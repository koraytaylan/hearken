# Hearken 👂

> "If a tree falls in a forest and no one is around to hear it, does it make a sound?"

Hearken is a high-performance, unsupervised log analysis tool written in Rust. It acts as the "ear" for applications, listening to their "cries for help" buried in gigabytes of log files. It automatically discovers recurring patterns, tracks every occurrence, and provides actionable insights — all without any configuration, training data, or internet connection.

## Features

### Core Analysis

- **Extreme Efficiency** — Built in Rust with memory-mapped file I/O (`memmap2`) for processing multi-gigabyte log files (16 GB+) with minimal overhead. Release builds use LTO and single codegen unit for maximum throughput.
- **Multi-File Processing with File Groups** — Process multiple log files at once with `hearken-cli process ~/logs/*.log`. Files are automatically grouped by their base name (stripping dates and numeric suffixes), and each group gets its own independent pattern discovery tree. Multiple groups are processed in parallel threads.
- **Unsupervised Pattern Recognition** — Uses a [Drain](https://jiemingzhu.github.io/pub/pjhe_icws2017.pdf)-inspired prefix tree algorithm to automatically discover log templates. No regex, no training, no prior knowledge of the log format required.
- **Multi-Line Entry Detection** — Automatically learns the structural format of log entries from a sample and groups continuation lines (stack traces, multi-line messages) with their parent entry — without any hardcoded patterns.
- **Full-Text Search** — Integrated SQLite FTS5 index for fast searching across discovered patterns.
- **Anomaly Detection** — Flag single-source patterns and statistical outliers (>3σ) with anomaly scoring.
- **Pattern Deduplication** — Find near-duplicate patterns using structural template similarity or TF-IDF semantic similarity.
- **Diff Mode** — Compare two databases to find new, resolved, and changed patterns between runs.
- **Resume Capability** — Tracks the last processed byte position per file, so interrupted runs pick up exactly where they left off.
- **100% Offline** — Designed for isolated environments; no internet connection required.

### v2 — Temporal Analysis

- **Timestamp Extraction** — `extract_timestamp()` automatically detects and parses 6 timestamp formats (ISO 8601 variants, syslog, common log, Unix epoch) with a thread-local format cache for fast repeated parsing.
- **Temporal Bucketing** — `--bucket hour|day|auto` on report/export groups occurrences into time buckets for trend visualization with sparklines and timeline charts.
- **Pattern Suppression** — Tag patterns via `--tags-file` and suppress known-noise patterns from reports. The HTML report includes a 🔇 toggle button with dimmed rows for suppressed patterns; use `--include-suppressed` to show them.

### v2 — CI/CD Integration

- **Check Command** — `hearken-cli check` runs quality-gate checks (max anomaly score, max new patterns, fail-on-pattern substring match) and exits with code 0 (pass) or 1 (fail) for CI pipelines.
- **Baseline Management** — `hearken-cli baseline save/compare` snapshots the current database state and diffs against a previous baseline for regression detection.
- **Config File** — `.hearken.toml` with hierarchical search (cwd → parent directories → `~/.config/hearken/config.toml`) to persist defaults for threshold, batch size, report options, and check thresholds.

### v2 — Deeper Analysis

- **Correlation Analysis** — `hearken-cli correlate` uses a sliding time window to detect cross-group pattern co-occurrence (e.g., a DB timeout always followed by a retry storm).
- **Root-Cause Clustering** — `hearken-cli cluster` groups patterns by shared variable values (IPs, request IDs, threads) using Union-Find to surface root-cause clusters.
- **Semantic Grouping** — `hearken-cli dedup --mode semantic` uses TF-IDF cosine similarity to find semantically similar patterns even when template structure differs.

### v2 — UX

- **Web Dashboard** — `hearken-cli serve --port 8080` (behind `--features web`) starts an HTTP server with a live dashboard, REST API (`/api/summary`, `/api/patterns`, `/api/anomalies`, `/api/tags`, `/api/export`), and CORS support.
- **Watch Mode** — `hearken-cli watch *.log --alert-score 5.0` monitors files for changes with `notify`, incrementally processes new entries, and triggers OS notifications for high-scoring anomalies.
- **Stdin Support** — `hearken-cli process - --group-name mygroup` reads from stdin for piped input and streaming use cases.

## Installation

```bash
cargo build --release
```

The binary will be at `target/release/hearken-cli`.

To include the web dashboard:

```bash
cargo build --release --features web
```

To include JIRA integration:

```bash
cargo build --release --features jira
```

## Usage

### Process Log Files

```bash
# Process a single file
hearken-cli process /var/log/syslog

# Process multiple files — files are auto-grouped by base name
hearken-cli process ~/logs/*.log

# Tune similarity threshold (0.0–1.0, default 0.5)
hearken-cli process --threshold 0.4 server.log

# Adjust batch size (lines per batch, default 500,000)
hearken-cli process --batch-size 1000000 server.log

# Use a custom database path
hearken-cli -d my_analysis.db process server.log

# Read from stdin with a custom group name
tail -f /var/log/app.log | hearken-cli process - --group-name app-logs

# Pipe from another command
kubectl logs deploy/myapp | hearken-cli process - --group-name k8s-myapp
```

**File Grouping:** When multiple files are passed, they are grouped by their base name. Date patterns (YYYY-MM-DD, YYYYMMDD) and numeric suffixes are stripped:
- `error.log.2024-10-01`, `error.log.2024-10-02` → group `error.log`
- `error.20241001.log`, `error.20241002.log` → group `error.log`
- `access.log`, `access.log.1` → group `access.log`

Each group gets its own independent Drain tree, so patterns from `error.log` and `access.log` never interfere with each other.

### Search Processed Patterns

```bash
# Search for patterns matching a keyword
hearken-cli search "connection timeout"
```

### Generate HTML Report

```bash
# Generate report from default database
hearken-cli report

# Customize output and scope
hearken-cli report --output analysis.html --top 1000 --samples 10

# Filter patterns by substring
hearken-cli report --filter '*WARN*,*ERROR*'

# Filter by file group
hearken-cli report --group error.log,access.log

# Time bucket for trends (hour, day, or auto-detect)
hearken-cli report --bucket hour

# Load pattern tags and suppress known noise
hearken-cli report --tags-file my-tags.json

# Include suppressed patterns (shown dimmed)
hearken-cli report --tags-file my-tags.json --include-suppressed

# Report from a specific database
hearken-cli -d my_analysis.db report
```

The report is a single self-contained HTML file (all CSS/JS/data inline) that opens in any browser and works offline. It includes:
- Searchable/sortable pattern table with file group filtering
- Inline SVG sparklines showing temporal occurrence distribution
- Interactive timeline chart (stacked bars of top patterns across time buckets)
- Expandable details with reconstructed sample occurrences and source provenance
- Pattern tagging: add/remove tags in the UI, filter by tag, export tags as JSON
- 🔇 suppression toggle: dim known-noise patterns without deleting them
- Copy-to-clipboard for Jira ticket creation

### Database Statistics

```bash
# Show pattern count, occurrences, file groups, source files, DB sizes
hearken-cli stats
```

### Export Patterns

```bash
# Export as JSON (to stdout)
hearken-cli export

# Export as CSV to a file
hearken-cli export --format csv --output patterns.csv

# Filter and limit, with time bucketing
hearken-cli export --format json --top 100 --filter '*ERROR*' --samples 3 --bucket day
```

### Diff Two Databases

```bash
# Compare before/after databases to find new, resolved, and changed patterns
hearken-cli diff before.db after.db

# JSON output for scripting
hearken-cli diff before.db after.db --format json
```

### Pattern Deduplication

```bash
# Find near-duplicate patterns using structural similarity (default threshold: 0.95)
hearken-cli dedup

# Adjust similarity threshold
hearken-cli dedup --threshold 0.90

# Use TF-IDF semantic similarity instead of structural matching
hearken-cli dedup --mode semantic

# Check a specific group, JSON output
hearken-cli dedup --group error.log --format json
```

### Anomaly Detection

```bash
# Detect anomalous patterns (single-source or >3σ outliers)
hearken-cli anomalies

# Limit results, filter by group, suppress known patterns
hearken-cli anomalies --top 20 --group error.log --tags-file tags.json

# Include suppressed patterns, JSON output
hearken-cli anomalies --format json --tags-file tags.json --include-suppressed
```

### CI/CD Quality Gate (Check)

```bash
# Fail if any pattern has anomaly score > 8.0
hearken-cli check --max-anomaly-score 8.0

# Fail if more than 50 new patterns appeared vs baseline
hearken-cli check --max-new-patterns 50 --baseline hearken-baseline.db

# Fail if any pattern contains "FATAL" or "OOM"
hearken-cli check --fail-on-pattern "FATAL" --fail-on-pattern "OOM"

# Combine multiple gates with group filtering
hearken-cli check --max-anomaly-score 5.0 --max-new-patterns 20 \
    --baseline hearken-baseline.db --group error.log

# JSON output for CI parsing
hearken-cli check --max-anomaly-score 5.0 --format json
```

Exit codes: `0` = all checks pass, `1` = one or more checks failed.

**Example GitHub Actions step:**

```yaml
- name: Log quality gate
  run: |
    hearken-cli process logs/*.log
    hearken-cli check --max-anomaly-score 5.0 --max-new-patterns 30 \
        --baseline hearken-baseline.db
```

### Baseline Management

```bash
# Save the current database as a baseline snapshot
hearken-cli baseline save --output hearken-baseline.db

# Compare current state against a baseline
hearken-cli baseline compare hearken-baseline.db

# JSON output
hearken-cli baseline compare hearken-baseline.db --format json
```

### Correlation Analysis

```bash
# Find correlated patterns across groups (60s sliding window, top 20)
hearken-cli correlate

# Custom window size and minimum occurrence count
hearken-cli correlate --window 120 --top 50 --min-count 5

# JSON output
hearken-cli correlate --format json
```

### Root-Cause Clustering

```bash
# Cluster patterns by shared variable values (min 3 shared to link)
hearken-cli cluster

# Tune clustering parameters
hearken-cli cluster --min-shared 5 --top 30 --group error.log

# Limit patterns analyzed (default 200) and get JSON output
hearken-cli cluster --pattern-limit 500 --format json
```

### Watch Mode

```bash
# Watch log files for changes and process new entries live
hearken-cli watch /var/log/*.log

# Alert on high-scoring anomalies (triggers OS notification)
hearken-cli watch /var/log/app.log --alert-score 5.0

# Custom threshold and batch size
hearken-cli watch /var/log/*.log --threshold 0.4 --batch-size 100000
```

### Web Dashboard

Requires building with `--features web`:

```bash
# Start the web dashboard on port 8080
hearken-cli serve --port 8080

# Custom port
hearken-cli serve --port 3000
```

REST API endpoints:

| Endpoint | Method | Description |
|---|---|---|
| `/` | GET | Live HTML dashboard |
| `/api/summary` | GET | Database summary (patterns, occurrences, groups, time range) |
| `/api/patterns` | GET | Patterns with samples, trends, tags (`?top=N&group=X&filter=Y`) |
| `/api/anomalies` | GET | Anomalous patterns |
| `/api/tags` | POST | Update pattern tags |
| `/api/export` | GET | Export patterns (`?format=json\|csv`) |

### Configuration File

Hearken searches for `.hearken.toml` starting from the current directory, walking up to parent directories, then falling back to `~/.config/hearken/config.toml`. CLI flags always override config file values.

```toml
# .hearken.toml
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
samples = 5
bucket = "day"

[check]
max_anomaly_score = 5.0
max_new_patterns = 50
baseline = "hearken-baseline.db"
tags_file = "my-tags.json"

# Requires --features jira
[jira]
url = "https://your-instance.atlassian.net"
type = "cloud"             # "cloud" or "server"
project = "OPS"
label = "hearken"
# issue_type = "Bug"       # Default: "Bug"
# Credentials via env vars: HEARKEN_JIRA_USER, HEARKEN_JIRA_TOKEN
```

### Clean State and Reprocess

```bash
# Delete old state and reprocess from scratch
rm hearken.db* && hearken-cli process server.log
```

## Command Reference

| Command | Description |
|---|---|
| `process <files...>` | Process log files (use `-` for stdin) |
| `search <query>` | Full-text search across patterns |
| `stats` | Show database statistics |
| `report` | Generate self-contained HTML report |
| `export` | Export patterns as JSON or CSV |
| `diff <before> <after>` | Compare two databases |
| `dedup` | Find near-duplicate patterns (structural or semantic) |
| `anomalies` | Detect anomalous patterns |
| `check` | CI/CD quality gate with exit code 0/1 |
| `baseline save\|compare` | Save or compare database baselines |
| `correlate` | Find cross-group temporal correlations |
| `cluster` | Root-cause clustering by shared variables |
| `watch <files...>` | Live file monitoring with incremental processing |
| `serve` | Start web dashboard (requires `--features web`) |
| `jira status` | Show JIRA sync status for discovered patterns (requires `--features jira`) |
| `jira sync` | Create new JIRA tickets for untracked patterns (requires `--features jira`) |
| `jira update` | Update existing JIRA tickets with latest data (requires `--features jira`) |

**Global flag:** `-d, --database <path>` — Database file path (default: `hearken.db`)

## How It Works

### Timestamp Extraction

Every log line is scanned for a timestamp at the beginning. Six formats are tried in order, with a **thread-local cache** of the last successful format for fast repeated parsing:

| # | Format | Example |
|---|---|---|
| 0 | ISO 8601 with `T` | `2026-01-15T08:00:00.123Z` |
| 1 | ISO 8601 space + comma frac | `2026-01-15 08:00:00,123` |
| 2 | ISO 8601 space + dot frac | `2026-01-15 08:00:00.123` |
| 3 | Syslog | `Mar 15 08:00:00` |
| 4 | Common log format | `15/Mar/2026:08:00:00 +0000` |
| 5 | Unix epoch | `1737043200` |

Extracted timestamps are stored as Unix seconds in `occurrences.entry_timestamp` and enable all temporal features (bucketing, correlation, timeline charts).

### Pattern Discovery (Drain Algorithm)

Each log line is tokenized using a delimiter-aware tokenizer (splits on whitespace but preserves tokens containing spaces inside `()` and `[]`, such as `invoke0(Native Method)`) and routed through a **prefix tree** keyed by token count → first N tokens. Tokens that look like variables (≥30% digit ratio, or contain slashes, or UUID-like dashes) are mapped to `<*>`. Lines reaching the same leaf node are compared against existing templates using a simple **match ratio** (fraction of identical tokens). If similarity exceeds the threshold, the line is absorbed into the template; differing tokens become `<*>` wildcards. Otherwise, a new template is created.

### Multi-Line Entry Grouping

On the first batch, hearken samples every line and computes a **structural fingerprint** from the first two tokens' character-class shapes (digits → `d`, letters → `a`, punctuation kept, consecutive same-class collapsed). The dominant fingerprints (covering ≥90% of non-whitespace-leading lines) define what an "entry start" looks like. Lines with leading whitespace or non-matching fingerprints are grouped as continuations of the previous entry.

This is fully unsupervised — it works with any log format (ISO timestamps, European dates, syslog, custom formats) without any hardcoded patterns.

Continuation lines (stack traces, `Caused by:` chains, indented messages) are tokenized and appended to the parent entry's token stream, with a `"\n"` sentinel token inserted before each continuation line to preserve multi-line structure. The Drain tree naturally discovers recurring stack trace patterns — identical exception shapes with variable line numbers and versions are collapsed via `<*>` wildcards, while the `"\n"` sentinels ensure stack traces are stored with proper line breaks in the database. Entries with different continuation depths never merge, so each distinct stack trace shape stays as its own pattern. The combined token stream is capped at 1024 tokens.

### Processing Pipeline

Each batch of lines goes through three phases:

1. **Parallel Phase** (rayon): Tokenize every entry, extract timestamp, and search the prefix tree for matches. The tree is immutable during this phase, so all entries are processed concurrently.
2. **Sequential Phase**: For unmatched entries, re-check the tree (which now includes templates created earlier in this pass) and either match, evolve an existing template, or create a new one.
3. **DB Phase**: Insert new patterns, update evolved templates, write occurrence rows (pattern ID + byte offset + entry timestamp + extracted variables), and advance the checkpoint position — all in a single transaction.

After all batches, occurrence counts are written to `patterns.occurrence_count` and the FTS5 index is rebuilt.

## Database Schema

State is stored in a plain SQLite database (`hearken.db` by default) with WAL mode and aggressive performance pragmas.

| Table | Purpose |
|---|---|
| `file_groups` | Groups of related log files (e.g., `error.log`, `access.log`) — each group has independent pattern discovery |
| `log_sources` | Tracks processed files, their file group, and last byte position for resume |
| `patterns` | Discovered templates with `occurrence_count`, scoped to a `file_group_id` |
| `occurrences` | Every matched entry: `pattern_id`, `log_source_id`, byte offset, `entry_timestamp` (Unix seconds), extracted `variables` |
| `patterns_fts` | FTS5 virtual table mirroring `patterns` for full-text search |
| `tags` | User-assigned tags on patterns (`pattern_id`, `tag`) for suppression and categorization |

## Architecture

Hearken is a Cargo workspace with five crates:

| Crate | Role |
|---|---|
| `hearken-cli` | CLI interface, orchestration, multi-line grouping, parallel pipeline, all commands, watch mode, config loading |
| `hearken-core` | Data models (`LogSource`, `LogTemplate`, `LogOccurrence`), mmap-based `LogReader`, timestamp extraction |
| `hearken-ml` | Drain prefix tree, template matching, structural & semantic similarity, variable extraction |
| `hearken-storage` | SQLite persistence, schema management, FTS5 search, trend/time-series queries, tag CRUD, performance pragmas |
| `hearken-jira` | JIRA integration: ticket creation/update, Cloud (ADF) & Server (wiki markup) support, pattern filtering |

Optional `web` feature adds `hearken-cli/src/web.rs` with Axum-based HTTP server and REST API.

Optional `jira` feature adds JIRA ticket management via `hearken-jira` crate.

See [ARCHITECTURE.md](./ARCHITECTURE.md) for detailed internals. See [CHANGELOG.md](./CHANGELOG.md) for version history.

## Roadmap

### v1 — Foundation ✅

- [x] **Input Validation** — Validate file paths, threshold range, skip unreadable files with warnings
- [x] **Integration Tests** — End-to-end tests covering the full processing pipeline
- [x] **Stats Command** — Database summary: pattern/occurrence counts, file groups, DB sizes
- [x] **JSON/CSV Export** — `export` command with `--format json|csv`, filtering, and sampling options
- [x] **Diff Mode** — `diff` command to compare two databases, find new/resolved/changed patterns
- [x] **Trend Tracking** — Per-source occurrence distribution with inline SVG sparklines in report
- [x] **Pattern Deduplication** — `dedup` command using template similarity clustering with Union-Find
- [x] **Anomaly Detection** — `anomalies` command flagging single-source and z-score outliers
- [x] **Parallel Group Processing** — Process multiple file groups concurrently with per-thread DB connections
- [x] **Timeline Visualization** — Interactive stacked bar chart in report showing pattern distribution across sources
- [x] **Pattern Tagging** — Tag patterns in report UI, filter by tag, persist via sidecar JSON

### v2 — Temporal, CI/CD, Analysis, UX ✅

- [x] **Timestamp Extraction** — 6-format parser with thread-local cache, `entry_timestamp` stored per occurrence
- [x] **Temporal Bucketing** — `--bucket hour|day|auto` on report/export with sparklines and timeline
- [x] **Pattern Suppression** — `--tags-file`, `--include-suppressed`, 🔇 button in report UI
- [x] **Check Command** — CI/CD quality gate with exit code 0/1
- [x] **Baseline Management** — `baseline save/compare` for regression detection
- [x] **Config File** — `.hearken.toml` with hierarchical search
- [x] **Correlation Analysis** — Sliding-window cross-group co-occurrence detection
- [x] **Root-Cause Clustering** — Union-Find clustering by shared variables
- [x] **Semantic Grouping** — `--mode semantic` for dedup using TF-IDF cosine similarity
- [x] **Web Dashboard** — `serve` command with REST API and live dashboard (behind `--features web`)
- [x] **Watch Mode** — Live file monitoring with incremental processing and OS notifications
- [x] **Stdin Support** — `process -` for piped input
- [x] **Documentation** — Comprehensive README, ARCHITECTURE.md, and CHANGELOG.md

### v3 — JIRA Integration ✅

- [x] **`hearken-jira` crate** — New workspace crate behind `--features jira` with JIRA Cloud (API v3, ADF) and Server/Data Center (API v2, wiki markup) support
- [x] **`jira status`** — Show sync status: which patterns have JIRA tickets, which are untracked
- [x] **`jira sync`** — Create new JIRA tickets for untracked patterns with configurable filtering (anomalies-only, tags, exclude-tags, min-occurrences, new-only, dry-run)
- [x] **`jira update`** — Update existing JIRA tickets with latest occurrence counts, timestamps, and a change comment
- [x] **`--jira-sync` flag** — Inline sync after `process` and `watch` commands
- [x] **Stateless sync** — No local state; JIRA is the source of truth via embedded code-block markers in ticket descriptions
- [x] **Rate limiting** — Respects JIRA `Retry-After` headers with configurable max retries

## License

MIT / Apache-2.0
