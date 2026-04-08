# Hearken — Developer Guidelines

## Build & Test Commands

```bash
cargo fmt --all -- --check          # Check formatting
cargo clippy --workspace --all-targets -- -D warnings   # Lint (default features)
cargo clippy --workspace --all-targets --features web -- -D warnings  # Lint (web feature)
cargo clippy --workspace --all-targets --features jira -- -D warnings # Lint (jira feature)
cargo test --workspace              # Run all tests
cargo build --release               # Release build (LTO enabled)
```

## Architecture

Five-crate workspace with clear separation of concerns:

| Crate            | Responsibility                                        |
|-----------------|-------------------------------------------------------|
| `hearken-core`   | Data models (`LogSource`, `LogTemplate`, `LogOccurrence`), mmap I/O, timestamp extraction |
| `hearken-ml`     | Drain prefix tree algorithm, template matching, TF-IDF similarity |
| `hearken-storage` | SQLite persistence, FTS5 full-text search, tags, time-series |
| `hearken-jira`   | JIRA REST API integration (Cloud v3 / Server v2), behind `jira` feature |
| `hearken-cli`    | CLI commands, config loading, watch mode, web server (behind `web` feature) |

See `ARCHITECTURE.md` for deep internals.

## Code Style

- **Lints:** Clippy pedantic enabled at workspace level. Selective allows in root `Cargo.toml` — remove them incrementally as code improves.
- **Formatting:** `rustfmt.toml` sets `max_width = 100`. Run `cargo fmt --all` before committing.
- **Toolchain:** Pinned to stable via `rust-toolchain.toml`.
- **Commits:** Use conventional commit prefixes (`feat:`, `fix:`, `refactor:`, etc.). See `CONTRIBUTING.md`.
- **Edition:** Rust 2024 across all crates.

## CI/CD

- **CI (`ci.yml`):** Runs on push/PR to `main`/`develop`. Gates: fmt check, clippy (zero warnings), tests (Linux/macOS/Windows), release build.
- **Release (`release.yml`):** Tag-triggered (`v*`). Multi-platform builds, changelog via `git-cliff`, auto-merge to `main`, GitHub Release with artifacts.

## Key Gotchas

- `hearken-cli/src/main.rs` is large (~3600 lines). When adding features, prefer extracting to a new module or crate.
- `hearken-core` uses `memmap2` for zero-copy file reading. Lines are `&str` slices into the mmap — do not hold references across batch boundaries.
- Thread-local timestamp format cache in `extract_timestamp()` — first line probes all formats, subsequent lines hit the cache.
- SQLite connections are not `Send`. Storage operations happen on the calling thread.
