# Hearken — Architecture

Hearken is a Cargo workspace with four crates, each with a clear responsibility. This document describes the internals in enough detail for someone with zero prior context to understand what every piece does, how the pieces fit together, and why key decisions were made.

---

## Workspace Layout

```
hearken/
├── Cargo.toml              # Workspace root + release profile (LTO, codegen-units=1, opt-level=3)
├── hearken-cli/             # CLI interface and orchestration
├── hearken-core/            # Data models and mmap-based I/O
├── hearken-ml/              # Drain prefix tree and template matching
└── hearken-storage/         # SQLite persistence and FTS5 search
```

---

## 1. `hearken-core` — Data Models & I/O

**Dependencies:** `memmap2`, `serde`, `serde_json`, `chrono`, `thiserror`

### Data Models

- **`LogSource`** — Represents a tracked log file: `id`, `file_path`, `last_processed_position` (byte offset for resume), `file_hash`.
- **`LogTemplate`** — A discovered pattern: `id`, `template` (space-joined token string, with `<*>` for variable positions).
- **`LogOccurrence`** — A single log entry matched against a template: `id`, `log_source_id`, `pattern_id`, `timestamp`, `variables`, `raw_message`.

### `tokenize(input)` — Delimiter-Aware Tokenizer

Splits text on whitespace **except** inside balanced `()` and `[]` delimiters. This preserves tokens like `invoke0(Native Method)` and `[Background Worker Pool Thread]` as single tokens rather than splitting them on the internal spaces. The depth tracker clamps at zero for unbalanced delimiters. Used everywhere tokens are produced: CLI primary lines, CLI continuation lines, and ML template loading from the database.

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
-- Tracks processed log files and resume position
CREATE TABLE log_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT UNIQUE NOT NULL,
    last_processed_position INTEGER DEFAULT 0,
    file_hash TEXT
);

-- Discovered log templates with occurrence count
CREATE TABLE patterns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    template TEXT UNIQUE NOT NULL,
    occurrence_count INTEGER DEFAULT 0
);

-- Every matched log entry: one row per entry
CREATE TABLE occurrences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    log_source_id INTEGER NOT NULL,
    pattern_id INTEGER NOT NULL,
    timestamp INTEGER NOT NULL,        -- byte offset of entry in the file
    variables TEXT,                     -- tab-separated extracted variable values
    FOREIGN KEY(log_source_id) REFERENCES log_sources(id),
    FOREIGN KEY(pattern_id) REFERENCES patterns(id)
);

-- FTS5 index for full-text search across pattern templates
CREATE VIRTUAL TABLE patterns_fts USING fts5(
    pattern_id UNINDEXED,
    template
);

CREATE INDEX idx_occ_pattern ON occurrences(pattern_id);
CREATE INDEX idx_occ_source ON occurrences(log_source_id);
```

### Key Methods

- **`get_or_create_log_source(path)`** — Upserts a log source and returns it with the last processed position.
- **`search_patterns(query)`** — Full-text search via `patterns_fts MATCH`.
- **`get_top_patterns(limit)`** — Returns the N most frequent patterns by `occurrence_count`.

---

## 4. `hearken-cli` — Orchestration

**Dependencies:** `hearken-core`, `hearken-ml`, `hearken-storage`, `clap`, `rayon`, `rusqlite`, `anyhow`, `ahash`

### CLI Commands

| Command | Description |
|---|---|
| `process <file>` | Process a log file and discover patterns |
| `search <query>` | Full-text search across discovered patterns |
| `report` | Generate a self-contained HTML report from the database |

**Process options:**
- `--threshold <f64>` — Similarity threshold for pattern matching (default: 0.5)
- `--batch-size <usize>` — Lines per batch (default: 500,000)
- `-d, --database <path>` — Database file path (default: `hearken.db`)

**Report options:**
- `--output <path>` — Output HTML file path (default: `report.html`)
- `--samples <n>` — Sample occurrences per pattern (default: 5)
- `--top <n>` — Maximum patterns to include, ranked by occurrence count (default: 500)
- `-d, --database <path>` — Database file path (default: `hearken.db`)

### Multi-Line Entry Grouping (Unsupervised)

Before processing, hearken auto-detects the structural format of log entries:

1. **`token_shape(token)`** — Collapses a token to its character-class skeleton: digits → `d`, letters → `a`, punctuation kept as-is, consecutive same-class collapsed. Examples: `"23.10.2024"` → `"d.d.d"`, `"ERROR"` → `"a"`, `"00:00:00.001"` → `"d:d:d.d"`.

2. **`line_prefix_fingerprint(line)`** — Joins the shapes of the first two whitespace-delimited tokens with `|`. Also checks for leading whitespace (tab/space). Example: `"23.10.2024 00:00:00.001 *INFO* ..."` → `"d.d.d|d:d:d.d"`.

3. **`detect_entry_fingerprints(lines)`** — Samples all lines in the first batch. Counts fingerprint frequencies among non-whitespace-leading lines. Selects the top fingerprints covering ≥90% as "entry-start" patterns. Lines with leading whitespace are always continuations.

4. **`group_entries(lines, fingerprints)`** — Merges continuation lines with their parent entry into a `GroupedEntry` struct containing the primary line and all continuation line slices. Orphaned continuation lines at batch boundaries are skipped.

During tokenization, all continuation lines are tokenized using the delimiter-aware `tokenize()` function and appended to the primary line's token stream, with a `"\n"` sentinel token inserted before each continuation line's tokens. The combined stream is capped at 1024 tokens. This means stack traces, `Caused by:` chains, and other multi-line content become part of the pattern — the Drain tree naturally discovers recurring stack trace shapes (e.g., `at com.example.app... at com.example.db...`) with variable parts (line numbers, versions) replaced by `<*>` wildcards, while the `"\n"` sentinels preserve the line structure so stack traces display correctly in the database.

### Processing Pipeline (`process_log`)

```
┌─────────────────────────────────────────────────────────┐
│  Startup                                                │
│  1. Open/create DB, load existing patterns into parser  │
│  2. Read last_processed_position for resume             │
│  3. Memory-map the log file                             │
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
     ┌────────────────▼────────────────────────────┐
     │  Step 3: DB Phase (single transaction)      │
     │  • INSERT new patterns                      │
     │  • UPDATE evolved patterns                  │
     │  • INSERT occurrence rows (per entry)       │
     │  • UPDATE last_processed_position           │
     └────────────────┬────────────────────────────┘
                      │
          ┌───────────▼──────────────┐
          │  Next batch or finish    │
          └───────────┬──────────────┘
                      │
     ┌────────────────▼────────────────────────────┐
     │  Finalization                               │
     │  • Write occurrence_count to patterns table │
     │  • Rebuild FTS5 index                       │
     │  • Print top 10 patterns summary            │
     └─────────────────────────────────────────────┘
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

- **Top N patterns** (default 500, configurable via `--top`) ranked by occurrence count, with template text and count.
- **Sample occurrences** (default 5 per pattern, configurable via `--samples`) showing representative variable values. Fetched via the `idx_occ_pattern` index — one indexed query per pattern.
- **Summary statistics**: total pattern count, total occurrence count, processed source files.

This keeps the output at ~3-5 MB even for databases with millions of occurrences.

### HTML Architecture

The HTML template is compiled into the binary via `include_str!("report_template.html")`. At report time:

1. Query summary stats, patterns, and samples from the DB.
2. Serialize the data as JSON via `serde_json`.
3. Inject the JSON into the HTML template as `const REPORT_DATA = {...};`.
4. Write the complete HTML file.

All rendering is done client-side with vanilla JavaScript (no frameworks). Features:

- **Summary cards** with total patterns, occurrences, top pattern count, and source count.
- **Searchable/sortable pattern table** with rank, template preview, count, percentage, and distribution bar.
- **Expandable detail per pattern** showing the full template (with preserved newline structure for stack traces) and sample variable values.
- **Copy-to-clipboard button** per pattern that formats the template, count, and samples into Jira-friendly text.

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
