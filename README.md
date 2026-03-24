# Hearken 👂

> "If a tree falls in a forest and no one is around to hear it, does it make a sound?"

Hearken is a high-performance, unsupervised log analysis tool written in Rust. It acts as the "ear" for applications, listening to their "cries for help" buried in gigabytes of log files. It automatically discovers recurring patterns, tracks every occurrence, and provides actionable insights — all without any configuration, training data, or internet connection.

## Features

- **Extreme Efficiency:** Built in Rust with memory-mapped file I/O (`memmap2`) for processing multi-gigabyte log files (16 GB+) with minimal overhead. Release builds use LTO and single codegen unit for maximum throughput.
- **Unsupervised Pattern Recognition:** Uses a [Drain](https://jiemingzhu.github.io/pub/pjhe_icws2017.pdf)-inspired prefix tree algorithm to automatically discover log templates. No regex, no training, no prior knowledge of the log format required.
- **Multi-Line Entry Detection:** Automatically learns the structural format of log entries from a sample and groups continuation lines (stack traces, multi-line messages) with their parent entry — without any hardcoded patterns. Stack trace content is included in the pattern token stream so recurring exception shapes are discovered as first-class patterns.
- **Full-Text Search:** Integrated SQLite FTS5 index for fast searching across discovered patterns.
- **Resume Capability:** Tracks the last processed byte position per file, so interrupted runs pick up exactly where they left off.
- **100% Offline:** Designed for isolated environments; no internet connection required.

## Installation

```bash
cargo build --release
```

The binary will be at `target/release/hearken-cli`.

## Usage

### Process a Log File

```bash
# Process a file and discover patterns
./hearken-cli process /var/log/syslog

# Tune similarity threshold (0.0–1.0, default 0.5)
./hearken-cli process --threshold 0.4 server.log

# Adjust batch size (lines per batch, default 500,000)
./hearken-cli process --batch-size 1000000 server.log

# Use a custom database path
./hearken-cli -d my_analysis.db process server.log
```

Output includes per-batch timing breakdown (`parallel`, `sequential`, `db` phases in ms) and a summary of the top 10 most frequent patterns.

### Search Processed Patterns

```bash
# Search for patterns matching a keyword
./hearken-cli search "connection timeout"
```

### Clean State and Reprocess

```bash
# Delete old state and reprocess from scratch
rm hearken.db* && ./hearken-cli process server.log
```

## How It Works

### Pattern Discovery (Drain Algorithm)

Each log line is tokenized using a delimiter-aware tokenizer (splits on whitespace but preserves tokens containing spaces inside `()` and `[]`, such as `invoke0(Native Method)`) and routed through a **prefix tree** keyed by token count → first N tokens. Tokens that look like variables (≥30% digit ratio, or contain slashes, or UUID-like dashes) are mapped to `<*>`. Lines reaching the same leaf node are compared against existing templates using a simple **match ratio** (fraction of identical tokens). If similarity exceeds the threshold, the line is absorbed into the template; differing tokens become `<*>` wildcards. Otherwise, a new template is created.

### Multi-Line Entry Grouping

On the first batch, hearken samples every line and computes a **structural fingerprint** from the first two tokens' character-class shapes (digits → `d`, letters → `a`, punctuation kept, consecutive same-class collapsed). The dominant fingerprints (covering ≥90% of non-whitespace-leading lines) define what an "entry start" looks like. Lines with leading whitespace or non-matching fingerprints are grouped as continuations of the previous entry.

This is fully unsupervised — it works with any log format (ISO timestamps, European dates, syslog, custom formats) without any hardcoded patterns.

Continuation lines (stack traces, `Caused by:` chains, indented messages) are tokenized and appended to the parent entry's token stream, with a `"\n"` sentinel token inserted before each continuation line to preserve multi-line structure. The Drain tree naturally discovers recurring stack trace patterns — identical exception shapes with variable line numbers and versions are collapsed via `<*>` wildcards, while the `"\n"` sentinels ensure stack traces are stored with proper line breaks in the database. Entries with different continuation depths never merge, so each distinct stack trace shape stays as its own pattern. The combined token stream is capped at 1024 tokens.

### Processing Pipeline

Each batch of lines goes through three phases:

1. **Parallel Phase** (rayon): Tokenize every entry and search the prefix tree for matches. The tree is immutable during this phase, so all entries are processed concurrently.
2. **Sequential Phase**: For unmatched entries, re-check the tree (which now includes templates created earlier in this pass) and either match, evolve an existing template, or create a new one.
3. **DB Phase**: Insert new patterns, update evolved templates, write occurrence rows (pattern ID + file position + extracted variables), and advance the checkpoint position — all in a single transaction.

After all batches, occurrence counts are written to `patterns.occurrence_count` and the FTS5 index is rebuilt.

## Database Schema

State is stored in a plain SQLite database (`hearken.db` by default) with WAL mode and aggressive performance pragmas.

| Table | Purpose |
|---|---|
| `log_sources` | Tracks processed files and last byte position for resume |
| `patterns` | Discovered templates with `occurrence_count` for quick ranking |
| `occurrences` | Every matched entry: `pattern_id`, byte offset (`timestamp` column), extracted `variables` (tab-separated) |
| `patterns_fts` | FTS5 virtual table mirroring `patterns` for full-text search |

## Architecture

Hearken is a Cargo workspace with four crates:

| Crate | Role |
|---|---|
| `hearken-cli` | CLI interface, orchestration, multi-line grouping, parallel/sequential pipeline |
| `hearken-core` | Data models (`LogSource`, `LogTemplate`, `LogOccurrence`), mmap-based `LogReader` |
| `hearken-ml` | Drain prefix tree, template matching, similarity calculation, variable extraction |
| `hearken-storage` | SQLite persistence, schema management, FTS5 search, performance pragmas |

See [ARCHITECTURE.md](./ARCHITECTURE.md) for detailed internals.

## License

MIT / Apache-2.0
