# Hearken — Architecture

Hearken is a Cargo workspace with four crates, each with a clear responsibility. This document describes the internals in enough detail for someone with zero prior context to understand what every piece does, how the pieces fit together, and why key decisions were made.

---

## Workspace Layout

```
hearken/
├── Cargo.toml              # Workspace root + release profile (LTO, codegen-units=1, opt-level=3)
├── hearken-cli/             # CLI interface and orchestration
│   └── src/
│       ├── main.rs          # All commands, config loading, watch mode, pipeline
│       └── web.rs           # Axum web server (behind "web" feature)
├── hearken-core/            # Data models, mmap-based I/O, timestamp extraction
├── hearken-ml/              # Drain prefix tree, template matching, similarity
└── hearken-storage/         # SQLite persistence, FTS5 search, tags, time-series
```

---

## 1. `hearken-core` — Data Models & I/O

**Dependencies:** `memmap2`, `serde`, `serde_json`, `chrono`, `thiserror`

### Data Models

- **`LogSource`** — Represents a tracked log file: `id`, `file_path`, `file_group_id`, `last_processed_position` (byte offset for resume), `file_hash`.
- **`LogTemplate`** — A discovered pattern: `id`, `template` (space-joined token string, with `<*>` for variable positions). Scoped to a file group.
- **`LogOccurrence`** — A single log entry matched against a template: `id`, `log_source_id`, `pattern_id`, `timestamp`, `variables`, `raw_message`.

### `tokenize(input)` — Delimiter-Aware Tokenizer

Splits text on whitespace **except** inside balanced `()` and `[]` delimiters. This preserves tokens like `invoke0(Native Method)` and `[Background Worker Pool Thread]` as single tokens rather than splitting them on the internal spaces. The depth tracker clamps at zero for unbalanced delimiters. Used everywhere tokens are produced: CLI primary lines, CLI continuation lines, and ML template loading from the database.

### `extract_timestamp(line)` — Timestamp Extraction

Attempts to extract a Unix timestamp (seconds since epoch) from the beginning of a log line. Tries 6 formats in order:

| Index | Format | Example |
|---|---|---|
| 0 | ISO 8601 with `T` separator | `2026-01-15T08:00:00.123Z` |
| 1 | ISO 8601 space + comma frac | `2026-01-15 08:00:00,123` |
| 2 | ISO 8601 space + dot frac | `2026-01-15 08:00:00.123` |
| 3 | Syslog-style (assumes current year) | `Mar 15 08:00:00` |
| 4 | Common log format | `15/Mar/2026:08:00:00 +0000` |
| 5 | Unix epoch (10-digit number at start) | `1737043200` |

**Thread-local format cache:** A `thread_local! { static LAST_TS_FORMAT: Cell<usize> }` stores the index of the last successful format. On the next call, that format is tried first, cycling through the remaining formats only on miss. This is critical for performance: within a log file, every line uses the same timestamp format, so the cache hits on the first try for all but the first line.

### `LogReader` — Zero-Copy Mmap Reader

Opens the file with `memmap2::Mmap` and advises the kernel for sequential access (`madvise(MADV_SEQUENTIAL)` on Unix).

**`read_batch(start_pos, batch_size)`** returns up to `batch_size` lines as `Vec<(u64, &str, u64)>`:
- Tuple: `(line_start_byte_offset, line_content, next_line_start_byte_offset)`
- Lines are `&str` slices directly into the mmap — **zero allocation, zero copy**.
- Lines longer than **64 KB** are truncated at the byte level.
- Invalid UTF-8 sequences (binary dumps, mid-multibyte truncation) silently yield `""`.
- CR/LF and LF line endings are both handled (CR stripped before yielding).

---

## 2. `hearken-ml` — Pattern Recognition Engine

**Dependencies:** `hearken-core`, `rayon`, `thiserror`

Implements a [Drain](https://jiemingzhu.github.io/pub/pjhe_icws2017.pdf)-inspired algorithm for online log template mining. The core data structure is a **prefix tree** that routes tokenized log lines to candidate templates for similarity comparison.

### Key Types

- **`InternalTemplate`** — A mutable template: `id: Option<i64>`, `tokens: Vec<String>`. Tokens are either literal strings, `<*>` wildcards, or `"\n"` (newline sentinel — see below).
- **`Node`** — Tree node: either `Internal(HashMap<String, Node>)` (keyed by token or `<*>`) or `Leaf(Vec<usize>)` (indices into `LogParser.templates`).
- **`LogParser`** — Owns the tree root, all templates, and tuning parameters (`max_depth`, `similarity_threshold`).

### `InternalTemplate::template_string()`

Converts the internal token vector to a DB-storable string. Iterates tokens: `"\n"` tokens become real newlines in the output; all other tokens are space-joined. This preserves the multi-line structure of stack traces in the database.

### Newline Token Architecture

Continuation lines (stack traces, `Caused by:` chains) are represented in the token stream by inserting a literal `"\n"` (ASCII 10) sentinel token before each continuation line's tokens. This means a 3-line stack trace like:

```
error message
\tat Foo.bar(Foo.java:123)
\tat Baz.qux(Baz.java:456)
```

becomes the token stream: `["error", "message", "\n", "at", "Foo.bar(Foo.java:123)", "\n", "at", "Baz.qux(Baz.java:456)"]`.

The `"\n"` sentinel is treated specially throughout the algorithm:
- **`is_variable("\n")`** returns `false` (no digits, no path separators — naturally safe).
- **`calculate_similarity()`** hard-rejects (returns 0.0) any comparison where `"\n"` appears in one sequence but not the other at the same position. This prevents entries with different continuation structures from merging.
- **`parse_tokens()`** skips `"\n"` during template evolution — it can never be wildcarded to `<*>`.
- **`add_template()`** round-trips from DB: splits the stored string on `\n`, inserts `"\n"` sentinel tokens between line groups, and uses `tokenize()` per line.

### Prefix Tree Structure

```
root: HashMap<token_count, Node>
  └─ token_count=8
       └─ "23.10.2024" or "<*>" (depth 1)
            └─ "00:00:00.001" or "<*>" (depth 2)
                 └─ "*INFO*" (depth 3)
                      └─ ... up to max_depth (15)
                           └─ Leaf([template_idx_0, template_idx_1, ...])
```

Lines are routed by token count first (exact match), then by the first `max_depth` tokens. Tokens identified as **variables** (contain digits, slashes, backslashes, or ≥2 dashes with length > 10) are mapped to `<*>` during tree navigation. If a literal key isn't found, the `<*>` key is tried as fallback.

### `is_variable(token)` Heuristic

Returns `true` if the token:
- Contains `/` or `\` (path separators — paths are always variables), OR
- Contains ≥ 2 dashes (`-`) and is longer than 10 characters (catches UUIDs), OR
- Has a **digit ratio ≥ 30%** (digits / total characters). This distinguishes truly variable tokens (timestamps like `2025-03-15` at 80%, IPs like `192.168.1.100` at 69%) from tokens where digits are incidental (stack frames like `Foo.java:538` at 5%, module versions like `[module:1.7.10]` at 14%). This ensures stack trace class names, method names, and file references are preserved as pattern content rather than wildcarded.

### `find_match(tokens)` — Immutable, Parallelizable

Navigates the tree to find the leaf node, then compares the input against up to **50 candidates** (capped for performance). Uses an **early exit** at 0.9 similarity. Returns the best match above `similarity_threshold`, or `None`.

**Similarity** is a simple match ratio: `(identical_tokens + wildcard_matches) / (total_tokens)`, where wildcard matches are positions where the template has `<*>` and the input has any token. This is NOT Jaccard — it counts token-by-token positional matches. Additionally, if `"\n"` appears at a position in either the template or the input but not both, similarity immediately returns 0.0 — this structurally prevents entries with different continuation line counts from merging into the same pattern.

### `parse_tokens(tokens, matched_idx)` — Mutable, Sequential

Called once per entry in the sequential phase:
1. If `matched_idx` is `None` (parallel phase didn't find a match), re-checks via `find_match()` — the tree may have new templates from earlier in this sequential pass.
2. If matched: compares token-by-token against the template. Differing tokens become `<*>` (**template evolution**), except `"\n"` sentinel tokens which are always preserved. If the template changed and has a DB ID, the ID is negated to flag it as "evolved" (needs a DB UPDATE).
3. If unmatched: creates a new template, applying `is_variable()` to each token. Inserts it into the tree immediately so subsequent lines can find it.

### Semantic Similarity (TF-IDF)

In addition to the structural `calculate_similarity()`, the ML module provides **TF-IDF cosine similarity** for cross-length template comparison:

- **`compute_idf(templates)`** — Computes inverse document frequency weights across all templates.
- **`semantic_similarity(a, b, idf)`** — Converts each template's tokens into a TF-IDF weighted vector and computes cosine similarity. Unlike structural similarity, this works across templates of different token lengths.

This powers `hearken-cli dedup --mode semantic`, which finds patterns that describe the same event but with structurally different templates.

### `extract_variables_from_tokens(tokens, template_tokens)`

Returns the subset of input tokens at positions where the template has `<*>`. Used to populate the `variables` column in `occurrences`.

---

## 3. `hearken-storage` — Persistence Layer

**Dependencies:** `hearken-core`, `rusqlite` (with `bundled` feature — plain SQLite, no encryption), `serde_json`, `thiserror`

### SQLite Configuration

Performance-critical pragmas applied at connection open:

| Pragma | Value | Why |
|---|---|---|
| `journal_mode` | WAL | Concurrent reads during writes, faster commits |
| `synchronous` | OFF | Maximum write speed (acceptable — DB is rebuilt from log files if corrupted) |
| `cache_size` | -1000000 | ~1 GB page cache in memory |
| `temp_store` | MEMORY | Temp tables/indexes in RAM |
| `locking_mode` | EXCLUSIVE | Single-writer optimization, avoids lock overhead |
| `page_size` | 16384 | 16 KB pages for large bulk inserts |

### Schema

```sql
-- Groups of related log files (e.g., error.log, access.log)
CREATE TABLE file_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL
);

-- Tracks processed log files and resume position
CREATE TABLE log_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT UNIQUE NOT NULL,
    file_group_id INTEGER NOT NULL,
    last_processed_position INTEGER DEFAULT 0,
    file_hash TEXT,
    FOREIGN KEY(file_group_id) REFERENCES file_groups(id)
);

-- Discovered log templates with occurrence count, scoped per file group
CREATE TABLE patterns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_group_id INTEGER NOT NULL,
    template TEXT NOT NULL,
    occurrence_count INTEGER DEFAULT 0,
    FOREIGN KEY(file_group_id) REFERENCES file_groups(id),
    UNIQUE(file_group_id, template)
);

-- Every matched log entry: one row per entry
CREATE TABLE occurrences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    log_source_id INTEGER NOT NULL,
    pattern_id INTEGER NOT NULL,
    byte_offset INTEGER NOT NULL,
    entry_timestamp INTEGER,              -- Unix seconds, extracted from log line
    variables TEXT,                        -- tab-separated extracted variable values
    FOREIGN KEY(log_source_id) REFERENCES log_sources(id),
    FOREIGN KEY(pattern_id) REFERENCES patterns(id)
);

-- User-assigned tags on patterns for suppression and categorization
CREATE TABLE tags (
    pattern_id INTEGER NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY(pattern_id, tag),
    FOREIGN KEY(pattern_id) REFERENCES patterns(id)
);

-- FTS5 index for full-text search across pattern templates
CREATE VIRTUAL TABLE patterns_fts USING fts5(
    pattern_id UNINDEXED,
    template
);

CREATE INDEX idx_occ_pattern ON occurrences(pattern_id);
CREATE INDEX idx_occ_source ON occurrences(log_source_id);
CREATE INDEX idx_occ_entry_ts ON occurrences(entry_timestamp);
CREATE INDEX idx_patterns_group ON patterns(file_group_id);
CREATE INDEX idx_tags_pattern ON tags(pattern_id);
```

**v2 schema additions:**
- `occurrences.entry_timestamp` — Stores the Unix timestamp extracted from each log line. Enables temporal bucketing, correlation analysis, and timeline visualizations. Indexed for efficient time-range queries.
- `tags` table — Stores per-pattern tags for categorization and suppression. Tags can be managed via the report UI, the REST API, or imported from `--tags-file` JSON files.

### Key Methods

- **`get_or_create_file_group(name)`** — Upserts a file group and returns its ID.
- **`get_or_create_log_source(path, file_group_id)`** — Upserts a log source and returns it with the last processed position.
- **`search_patterns(query)`** — Full-text search via `patterns_fts MATCH`.
- **`get_top_patterns(limit)`** — Returns the N most frequent patterns by `occurrence_count`.
- **`get_all_patterns_ranked(limit, filter, group_filter)`** — Returns top patterns with optional template and group filtering.
- **`get_pattern_samples(pattern_id, limit)`** — Returns sample variable strings with source file paths.
- **`get_pattern_trends(pattern_ids)`** — Returns per-source occurrence distribution for sparkline generation. Batches queries in chunks of 500 to stay within SQLite parameter limits.
- **`get_pattern_time_series(pattern_ids, bucket)`** — Returns time-bucketed occurrence counts using `strftime()` on `entry_timestamp`. Supports `"hour"`, `"day"`, and `"auto"` (switches to hourly if time span < 48 hours, daily otherwise).
- **`get_timed_occurrences()`** — Returns `(pattern_id, entry_timestamp)` pairs for correlation analysis.
- **`get_variable_index()`** — Builds an inverted index from variable values to pattern IDs for root-cause clustering.
- **`set_tags(pattern_id, tags)`** / **`add_tag()`** / **`remove_tag()`** — Full CRUD for the `tags` table.
- **`get_report_summary()`** / **`get_source_counts_per_group()`** — Aggregation queries for report header data.

---

## 4. `hearken-cli` — Orchestration

**Dependencies:** `hearken-core`, `hearken-ml`, `hearken-storage`, `clap`, `rayon`, `rusqlite`, `anyhow`, `ahash`, `toml`, `serde`, `notify`, `tokio`

**Optional `web` feature:** `axum`, `tower-http`, `tokio` — adds the `serve` command

### Configuration Loading

`load_config()` searches for `.hearken.toml` using hierarchical resolution:

1. Current working directory → `cwd/.hearken.toml`
2. Walk parent directories → `../`, `../../`, etc.
3. User home → `~/.config/hearken/config.toml`

The config file uses TOML with sections matching CLI behavior:

```toml
database = "my-project.db"
threshold = 0.4
batch_size = 1000000

[report]
output = "analysis.html"
top = 1000
samples = 10
bucket = "hour"
tags_file = "my-tags.json"

[export]
format = "json"
top = 500

[check]
max_anomaly_score = 5.0
max_new_patterns = 50
baseline = "hearken-baseline.db"
```

CLI flags always override config file values. The config struct is deserialized into `HearkenConfig` with optional fields for each setting.

### CLI Commands

| Command | Description |
|---|---|
| `process <files...>` | Process one or more log files, auto-grouped by base name. Use `-` for stdin. |
| `search <query>` | Full-text search across discovered patterns |
| `stats` | Show database statistics |
| `report` | Generate a self-contained HTML report with temporal bucketing and suppression |
| `export` | Export patterns as JSON or CSV with temporal bucketing |
| `diff <before> <after>` | Compare two databases to find new, resolved, and changed patterns |
| `dedup` | Find near-duplicate patterns (structural or semantic mode) |
| `anomalies` | Detect anomalous patterns with optional suppression |
| `check` | CI/CD quality gate — exits 0 (pass) or 1 (fail) |
| `baseline save\|compare` | Save database snapshot or compare against a baseline |
| `correlate` | Find cross-group temporal correlations via sliding window |
| `cluster` | Root-cause clustering by shared variable values |
| `watch <files...>` | Live file monitoring with incremental processing |
| `serve` | Start web dashboard (requires `--features web`) |

**Process options:**
- `--threshold <f64>` — Similarity threshold for pattern matching (default: 0.5)
- `--batch-size <usize>` — Lines per batch (default: 500,000)
- `--group-name <name>` — Override derived group name (default for stdin: `"stdin"`)
- `-d, --database <path>` — Database file path (default: `hearken.db`)

### File Grouping

When multiple files are passed (e.g., `hearken-cli process ~/logs/*.log`), they are automatically grouped by base name using `derive_group_name()`. This function strips date patterns (YYYY-MM-DD, YYYYMMDD) and numeric suffixes to find the canonical group name:

| Input | Group |
|---|---|
| `error.log.2024-10-01` | `error.log` |
| `error.20241001.log` | `error.log` |
| `access.log.1` | `access.log` |
| `request.log` | `request.log` |

Each file group gets its own `LogParser` instance (Drain tree), seeded from any existing patterns for that group in the database. Files within a group are processed in sorted order (alphabetical ≈ chronological for date-suffixed files). This ensures patterns from different log types (error vs access vs request) never interfere with each other.

**Report options:**
- `--output <path>` — Output HTML file path (default: `report.html`)
- `--samples <n>` — Sample occurrences per pattern (default: 5)
- `--top <n>` — Maximum patterns to include, ranked by occurrence count (default: 500)
- `--group <name>` — Filter report to specific file group(s), comma-separated
- `--filter <text>` — Filter patterns by template content (e.g., `*ERROR*`), comma-separated
- `--tags-file <path>` — Path to tags JSON file for pattern tagging (created if absent)
- `--include-suppressed` — Include suppressed patterns (shown dimmed with 🔇 indicator)
- `--bucket <hour|day|auto>` — Time bucket for temporal trends (default: `auto`)
- `-d, --database <path>` — Database file path (default: `hearken.db`)

### Multi-Line Entry Grouping (Unsupervised)

Before processing, hearken auto-detects the structural format of log entries:

1. **`token_shape(token)`** — Collapses a token to its character-class skeleton: digits → `d`, letters → `a`, punctuation kept as-is, consecutive same-class collapsed. Examples: `"23.10.2024"` → `"d.d.d"`, `"ERROR"` → `"a"`, `"00:00:00.001"` → `"d:d:d.d"`.

2. **`line_prefix_fingerprint(line)`** — Joins the shapes of the first two whitespace-delimited tokens with `|`. Also checks for leading whitespace (tab/space). Example: `"23.10.2024 00:00:00.001 *INFO* ..."` → `"d.d.d|d:d:d.d"`.

3. **`detect_entry_fingerprints(lines)`** — Samples all lines in the first batch. Counts fingerprint frequencies among non-whitespace-leading lines. Selects the top fingerprints covering ≥90% as "entry-start" patterns. Lines with leading whitespace are always continuations.

4. **`group_entries(lines, fingerprints)`** — Merges continuation lines with their parent entry into a `GroupedEntry` struct containing the primary line and all continuation line slices. Orphaned continuation lines at batch boundaries are skipped.

During tokenization, all continuation lines are tokenized using the delimiter-aware `tokenize()` function and appended to the primary line's token stream, with a `"\n"` sentinel token inserted before each continuation line's tokens. The combined stream is capped at 1024 tokens. This means stack traces, `Caused by:` chains, and other multi-line content become part of the pattern — the Drain tree naturally discovers recurring stack trace shapes (e.g., `at com.example.app... at com.example.db...`) with variable parts (line numbers, versions) replaced by `<*>` wildcards, while the `"\n"` sentinels preserve the line structure so stack traces display correctly in the database.

### Stdin Support

When `"-"` is passed as a file path, hearken reads from standard input into a temporary on-disk file, then processes it normally. The group name defaults to `"stdin"` but can be overridden with `--group-name`. This enables piped workflows like `kubectl logs ... | hearken-cli process - --group-name k8s-app`.

### Processing Pipeline

```
┌──────────────────────────────────────────────────────────────────┐
│  Startup                                                         │
│  1. Load config (.hearken.toml hierarchy)                        │
│  2. Open/create DB                                               │
│  3. Derive file groups from filenames                            │
│  4. Pre-create file group IDs                                    │
│  5. If multiple groups: process groups in parallel threads       │
│     Each thread gets its own DB connection (WAL mode)            │
│     If single group: process directly (no thread overhead)       │
│  6. Per group:                                                   │
│     a. Create LogParser, seed with group's DB patterns           │
│     b. Process each file in sorted order (see below)             │
│  7. Rebuild FTS5 index once at the end                           │
└──────────────────────────────────────────────────────────────────┘

Per-File Processing (shared LogParser within group):
┌─────────────────────────────────────────────────────────┐
│  1. Read last_processed_position for resume             │
│  2. Memory-map the log file                             │
└─────────────────────┬───────────────────────────────────┘
                      │
          ┌───────────▼──────────────┐
          │  For each batch of lines │◄─── read_batch(pos, 500K)
          └───────────┬──────────────┘
                      │
     ┌────────────────▼────────────────────────────┐
     │  Step 0: Entry Grouping                     │
     │  Auto-detect fingerprints (first batch only)│
     │  Group continuation lines with parent entry │
     └────────────────┬────────────────────────────┘
                      │
     ┌────────────────▼────────────────────────────┐
     │  Step 1: Parallel Phase (rayon par_iter)    │
     │  • Tokenize each entry (delimiter-aware)    │
     │  • Insert \n sentinel before continuations  │
     │  • Append continuation tokens (cap 1024)    │
     │  • extract_timestamp() for entry_timestamp  │
     │  • find_match() against prefix tree (immut) │
     └────────────────┬────────────────────────────┘
                      │
     ┌────────────────▼────────────────────────────┐
     │  Step 2: Sequential Phase                   │
     │  For each entry:                            │
     │  • parse_tokens() — match / evolve / create │
     │  • Count occurrence in-memory HashMap       │
     │  • Track new and evolved patterns           │
     └────────────────┬────────────────────────────┘
                      │
     ┌────────────────▼────────────────────────────────┐
     │  Step 3: DB Phase (single transaction)          │
     │  • INSERT new patterns (with file_group_id)     │
     │  • UPDATE evolved patterns                      │
     │  • INSERT occurrence rows (per entry)            │
     │    - byte_offset, entry_timestamp, variables    │
     │  • UPDATE last_processed_position               │
     └────────────────┬────────────────────────────────┘
                      │
          ┌───────────▼──────────────┐
          │  Next batch or finish    │
          └──────────────────────────┘
```

### Template Evolution and ID Signaling

When `parse_tokens()` matches a line to an existing template but some tokens differ, those positions become `<*>`. If the template already has a DB ID, the ID is **negated** (e.g., `42` → `-42`) to signal that it needs a DB UPDATE. The CLI checks for negative IDs, writes the update, and restores the positive ID.

### Per-Batch Timing

Every batch prints:
```
Batch: parallel=Xms, sequential=Yms, db=Zms, templates=N
```
This immediately reveals which phase is the bottleneck for a given workload.

---

## Report Generation (`report` subcommand)

Generates a **single self-contained HTML file** from the database. The HTML includes all CSS, JavaScript, and data inline — no external dependencies, no server needed, works fully offline.

### Data Strategy

The database can contain millions of occurrence rows, but patterns themselves are compact. The report includes:

- **Top N patterns** (default 500, configurable via `--top`) ranked by occurrence count, with template text, count, and file group name.
- **Sample occurrences** (default 5 per pattern, configurable via `--samples`) showing reconstructed full log entries with source file provenance.
- **File group breakdown**: pattern count per group, listed in the header.
- **Summary statistics**: total pattern count, total occurrence count, processed source files, applied filters.

This keeps the output at ~3-5 MB even for databases with millions of occurrences.

### HTML Architecture

The HTML template is compiled into the binary via `include_str!("report_template.html")`. At report time:

1. Query summary stats, patterns, and samples from the DB (filtered by `--group` and `--filter` if provided).
2. Serialize the data as JSON via `serde_json`.
3. Inject the JSON into the HTML template as `const REPORT_DATA = {...};`.
4. Write the complete HTML file with file size embedded as a `data-file-size` body attribute.

All rendering is done client-side with vanilla JavaScript (no frameworks). Features:

- **Summary cards** with total patterns, occurrences, top pattern count, source count, and file groups.
- **Header pills** showing applied filters, limits, sample count, and command used.
- **Searchable/sortable pattern table** with rank, group, template preview, count, percentage, and distribution bar.
- **Group filter dropdown** to narrow the table to a specific file group.
- **Expandable detail per pattern** showing a collapsible template view and individually collapsible sample occurrences with source file attribution.
- **Copy-to-clipboard button** per pattern that formats the group, template, count, and samples into Jira-friendly text.

---

## Performance Design Decisions

| Decision | Rationale |
|---|---|
| Memory-mapped I/O with `madvise(Sequential)` | Zero-copy line reading; kernel prefetches pages ahead |
| Parallel tokenization + matching via rayon | Tree is immutable during this phase, safe for `par_iter` |
| Sequential-only for template mutation | Tree mutation is not thread-safe; kept to a fast single pass |
| Prefix tree with max depth 15, candidate cap 50 | Limits worst-case comparison cost per line |
| Early exit at 0.9 similarity | Skips remaining candidates when a strong match is found |
| In-memory occurrence counting | `patterns.occurrence_count` written once at the end |
| WAL + synchronous=OFF + exclusive locking | Maximum SQLite write throughput |
| LTO + codegen-units=1 in release profile | ~10-20% faster binaries via whole-program optimization |
| 64 KB line truncation | Prevents pathological lines (base64 blobs, minified JSON) from dominating |
| 1024-token cap per line | Same protection at the token level |
| Delimiter-aware tokenizer | Preserves Java stack frame tokens like `invoke0(Native Method)` as single tokens |
| `\n` sentinel newline protection | Entries with different stack trace depths never merge; `\n` can never become `<*>` |

---

## 5. CI/CD Integration (`check` and `baseline` commands)

### Check Command

The `check` command implements a CI/CD quality gate that evaluates one or more conditions and exits with code 0 (all pass) or 1 (any fail):

1. **Max anomaly score** (`--max-anomaly-score <f64>`) — Runs anomaly detection and fails if any pattern's composite anomaly score exceeds the threshold.
2. **Max new patterns** (`--max-new-patterns <N> --baseline <path>`) — Compares the current database against a baseline and fails if more than N new patterns appeared.
3. **Fail on pattern** (`--fail-on-pattern <substring>`) — Fails if any pattern template contains the given substring. Can be specified multiple times.

Each check runs independently. The command outputs a summary table showing pass/fail status for each check. The `--format json` flag produces machine-parseable output for CI systems.

Tags-file and group filtering are supported to narrow the scope of checks.

### Baseline Management

- **`baseline save --output <path>`** — Copies the current database file to create a named snapshot. Default output: `hearken-baseline.db`.
- **`baseline compare <baseline-path>`** — Opens both the current and baseline databases, compares pattern sets, and reports new patterns, resolved patterns, and changed patterns (templates that evolved). Supports `--format text|json`.

---

## 6. Correlation Analysis (`correlate` command)

Uses a sliding time window to detect cross-group pattern co-occurrence:

1. Fetches all `(pattern_id, entry_timestamp)` pairs from the database (requires timestamps).
2. For each pair of patterns from **different file groups**, counts how often they co-occur within the configured time window (default: 60 seconds).
3. Filters to pairs above `--min-count` (default: 10) co-occurrences.
4. Ranks by co-occurrence count and reports the `--top` N pairs (default: 20).

This surfaces operational correlations like "DB connection timeout in `app.log` always appears within 60s of retry storm in `worker.log`".

---

## 7. Root-Cause Clustering (`cluster` command)

Groups patterns by shared variable values using Union-Find:

1. Fetches the top `--pattern-limit` (default: 200) patterns and their extracted variable values.
2. Builds an inverted index from variable values → pattern IDs.
3. For each variable value shared by multiple patterns, unions those patterns.
4. Filters clusters to those with at least `--min-shared` (default: 3) shared variable values.
5. Ranks clusters by size and reports the top N with shared values listed.

This surfaces root-cause clusters like "these 5 patterns all share IP `10.0.0.42`, request ID `abc-123`, and thread `worker-7`".

---

## 8. Watch Mode (`watch` command)

Live file monitoring with incremental processing:

1. Uses `notify::RecommendedWatcher` to watch parent directories of all input files.
2. On file modification events, reads new data from the last processed position (resume-aware).
3. Processes new entries through the standard pipeline (grouping → parallel → sequential → DB).
4. If `--alert-score` is set, runs anomaly detection after each processing cycle and triggers an OS notification (via `notify-rust` or platform equivalent) for patterns exceeding the threshold.

The watcher runs in a loop until interrupted with Ctrl+C.

---

## 9. Web Dashboard (`serve` command, `--features web`)

An optional Axum-based HTTP server that exposes the database through a REST API and a live HTML dashboard.

### Architecture

- **Shared state:** `Arc<Mutex<Storage>>` — the database connection is shared across all request handlers with mutex-guarded access.
- **CORS:** Permissive CORS layer for cross-origin API access.
- **Dashboard:** The `/` route serves a self-contained HTML dashboard (similar to the static report but with live data fetching).

### API Endpoints

| Route | Handler | Response |
|---|---|---|
| `GET /` | `dashboard_handler` | HTML dashboard page |
| `GET /api/summary` | `summary_handler` | `{ pattern_count, total_occurrences, sources, file_groups, time_range }` |
| `GET /api/patterns?top=N&group=X&filter=Y` | `patterns_handler` | Array of patterns with samples, trends, distribution, and tags |
| `GET /api/anomalies` | `anomalies_handler` | Anomalous patterns |
| `POST /api/tags` | `tags_handler` | Update pattern tags |
| `GET /api/export?format=json\|csv` | `export_handler` | Pattern export |

---

## 10. Pattern Suppression

Pattern suppression allows known-noise patterns to be hidden from reports without deleting them:

1. **Tags source:** The `--tags-file` flag loads/creates a JSON file mapping pattern IDs to tag arrays. Tags can also be stored in the database `tags` table.
2. **Suppression convention:** Patterns tagged with specific suppression tags are treated as suppressed.
3. **Report behavior:** By default, suppressed patterns are excluded. With `--include-suppressed`, they appear with dimmed styling and a 🔇 indicator.
4. **Report UI:** The HTML report includes a 🔇 toggle button to show/hide suppressed patterns interactively.

---

## 11. Temporal Bucketing

The `--bucket` flag on `report` and `export` commands controls how occurrence timestamps are aggregated:

| Value | Behavior |
|---|---|
| `hour` | Groups by `%Y-%m-%d %H:00` |
| `day` | Groups by `%Y-%m-%d` |
| `auto` | If time span < 48 hours → hourly; otherwise → daily |

Auto-detection queries `MIN(entry_timestamp)` and `MAX(entry_timestamp)` from the `occurrences` table. The bucketed data powers:
- **Sparklines** — Inline SVG showing occurrence distribution over time per pattern.
- **Timeline chart** — Interactive stacked bar chart showing top pattern distribution across time buckets.
