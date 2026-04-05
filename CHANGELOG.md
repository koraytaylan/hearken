# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [3.0.0] — 2026-04-05

### Added

#### JIRA Integration
- **`hearken-jira` crate** — New workspace crate (behind `--features jira`) providing JIRA Cloud (API v3, ADF) and Server/Data Center (API v2, wiki markup) support.
- **`jira status` command** — Shows sync status: which discovered patterns have corresponding JIRA tickets and which are untracked.
- **`jira sync` command** — Creates new JIRA tickets for untracked patterns. Supports filtering: `--anomalies-only`, `--tags`, `--exclude-tags`, `--min-occurrences`, `--new-only`, `--dry-run`.
- **`jira update` command** — Updates existing JIRA tickets with latest occurrence counts and timestamps, adds a change comment. Supports `--anomalies-only`, `--tags`, `--exclude-tags`, `--min-occurrences`, `--dry-run`.
- **`--jira-sync` flag** — Inline JIRA sync after `process` and `watch` commands.
- **Stateless sync design** — No local sync state stored; JIRA is the source of truth via embedded code-block markers (`{code:title=hearken-metadata}`) in ticket descriptions.
- **Rate limiting** — Respects JIRA `Retry-After` headers with max 5 retries on 429 responses.
- **`[jira]` config section** — Configure JIRA connection in `.hearken.toml` with `url`, `type` (cloud/server), `project`, `label`. Secrets via `HEARKEN_JIRA_USER` and `HEARKEN_JIRA_TOKEN` environment variables.

## [2.0.0] — 2025-07-05

### Added

#### Temporal Analysis
- **Timestamp extraction** — `extract_timestamp()` in `hearken-core` automatically detects and parses 6 timestamp formats: ISO 8601 with `T`, ISO 8601 with space + comma fractional, ISO 8601 with space + dot fractional, syslog, common log format, and Unix epoch. Uses a thread-local format cache for fast repeated parsing.
- **`entry_timestamp` column** — Occurrences now store the extracted Unix timestamp (seconds) in `occurrences.entry_timestamp` with a dedicated index (`idx_occ_entry_ts`).
- **Temporal bucketing** — `--bucket hour|day|auto` flag on `report` and `export` commands groups occurrences into time buckets. Auto-detection uses hourly buckets for spans < 48 hours, daily otherwise.
- **Sparkline trends** — HTML report includes inline SVG sparklines showing temporal occurrence distribution per pattern.
- **Timeline chart** — Interactive stacked bar chart in the HTML report showing top pattern distribution across time buckets.

#### CI/CD Integration
- **`check` command** — CI/CD quality gate: `hearken-cli check --max-anomaly-score X --max-new-patterns N --fail-on-pattern "..."`. Exits with code 0 (pass) or 1 (fail). Supports `--baseline`, `--tags-file`, `--group`, and `--format text|json`.
- **`baseline save` command** — `hearken-cli baseline save --output hearken-baseline.db` saves a copy of the current database as a named snapshot.
- **`baseline compare` command** — `hearken-cli baseline compare <baseline.db>` compares current state against a baseline, reporting new, resolved, and changed patterns. Supports `--format text|json`.
- **Configuration file** — `.hearken.toml` with hierarchical search: current directory → parent directories → `~/.config/hearken/config.toml`. Supports `database`, `threshold`, `batch_size`, `[report]`, `[export]`, and `[check]` sections. CLI flags always override config values.

#### Deeper Analysis
- **`correlate` command** — `hearken-cli correlate --window 60 --top 20 --min-count 10` performs sliding-window cross-group co-occurrence detection using `entry_timestamp` data. Surfaces patterns that consistently appear together within a configurable time window. Supports `--format text|json`.
- **`cluster` command** — `hearken-cli cluster --min-shared 3 --top 20 --pattern-limit 200` groups patterns by shared variable values (IPs, request IDs, threads) using Union-Find clustering. Supports `--group` filtering and `--format text|json`.
- **Semantic dedup mode** — `hearken-cli dedup --mode semantic` uses TF-IDF cosine similarity to find semantically similar patterns, complementing the existing structural similarity mode.

#### UX
- **`serve` command** — `hearken-cli serve --port 8080` (requires `--features web`) starts an Axum HTTP server with a live HTML dashboard and REST API endpoints: `/api/summary`, `/api/patterns`, `/api/anomalies`, `/api/tags`, `/api/export`.
- **`watch` command** — `hearken-cli watch *.log --alert-score 5.0` monitors files for changes using `notify`, incrementally processes new entries, and triggers OS notifications for high-scoring anomalies.
- **Stdin support** — `hearken-cli process - --group-name mygroup` reads from stdin, enabling piped input from `tail`, `kubectl logs`, etc.
- **Pattern suppression** — `--tags-file` and `--include-suppressed` flags on `report`, `export`, and `anomalies` commands. Suppressed patterns are excluded by default; with `--include-suppressed` they appear dimmed with a 🔇 indicator in the HTML report.
- **Suppression UI** — 🔇 toggle button in the HTML report to interactively show/hide suppressed patterns.
- **`tags` table** — New database table (`pattern_id`, `tag`) for persistent pattern tagging with full CRUD support.

#### Documentation
- Comprehensive README with usage examples for all 14 commands, configuration file format, command reference table, and updated roadmap.
- Updated ARCHITECTURE.md with sections for timestamp extraction, web server, watch mode, CI/CD integration, correlation analysis, root-cause clustering, temporal bucketing, and pattern suppression.
- Added CHANGELOG.md following Keep a Changelog format.

### Changed
- **`occurrences` schema** — Added `entry_timestamp INTEGER` column (nullable) and renamed `timestamp` to `byte_offset` for clarity.
- **Report generation** — Now includes temporal sparklines, timeline chart, suppression toggle, and time-bucketed trend data.
- **`dedup` command** — Added `--mode structural|semantic` flag (default: `structural`).
- **`anomalies` command** — Added `--tags-file` and `--include-suppressed` flags.
- **Processing pipeline** — Parallel phase now includes `extract_timestamp()` for each entry.

## [1.0.0] — 2025-06-01

### Added
- **Log processing** — `process` command with Drain-inspired prefix tree algorithm for unsupervised pattern discovery from log files.
- **Multi-file processing** — Process multiple log files at once with automatic file grouping by base name (stripping date patterns and numeric suffixes).
- **Multi-line entry detection** — Unsupervised structural fingerprinting to group continuation lines (stack traces, `Caused by:` chains) with their parent entry.
- **Memory-mapped I/O** — Zero-copy file reading via `memmap2` with `madvise(MADV_SEQUENTIAL)` for processing multi-gigabyte files.
- **Parallel processing** — Rayon-based parallel tokenization and template matching with sequential-only tree mutation.
- **Resume capability** — Tracks last processed byte position per file for interrupted run recovery.
- **Full-text search** — `search` command with integrated SQLite FTS5 index across discovered patterns.
- **HTML report** — `report` command generating a single self-contained HTML file with searchable/sortable pattern tables, file group filtering, sample occurrences with source provenance, and copy-to-clipboard.
- **Export** — `export` command with `--format json|csv`, filtering (`--filter`, `--group`), and sampling (`--samples`, `--top`).
- **Diff mode** — `diff` command comparing two databases to find new, resolved, and changed patterns.
- **Pattern deduplication** — `dedup` command using structural template similarity clustering with Union-Find.
- **Anomaly detection** — `anomalies` command flagging single-source patterns and statistical outliers (>3σ z-score).
- **Database statistics** — `stats` command showing pattern count, occurrence count, file groups, source files, and database sizes.
- **Pattern tagging** — Tag patterns in the report UI with persistence via sidecar JSON files.
- **Parallel group processing** — Multiple file groups processed concurrently with per-thread database connections.
- **Timeline visualization** — Interactive stacked bar chart in report showing pattern distribution across source files.
- **Trend tracking** — Per-source occurrence distribution with inline SVG sparklines in report.
- **Input validation** — File path validation, threshold range checking, graceful handling of unreadable files.
- **Integration tests** — End-to-end tests covering the full processing pipeline.
