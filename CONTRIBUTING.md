# Contributing to Hearken

## Code Quality Standards

This project enforces **clippy pedantic** lints at the workspace level. All warnings are
treated as errors in CI (`-D warnings`). The allowed-lint list in the root `Cargo.toml` is
intentionally temporary — remove entries as code is cleaned up.

### Naming

- No single-letter variables outside closures, iterators, or trivial math.
- No bare abbreviations — spell out names clearly (e.g., `tp` -> `template`,
  `src` -> `source`). Domain abbreviations are allowed only if they are
  well-known in the codebase (`db`, `ml`, `ts`, `fts`).

### Magic Numbers

Any numeric literal that is not `0`, `1`, `-1`, `0.0`, or `1.0` must be a named
`const` with a comment explaining *why* that value was chosen.

### Function Length

- Functions over ~50 lines should be reviewed for split opportunities.
- Functions over 100 lines must be refactored before merge.

### Documentation

- All public types and functions must have `///` doc comments.
- Module-level `//!` docs must describe purpose and key concepts.
- Goal: incrementally enable `#![warn(missing_docs)]` per crate.

### Error Handling

- Use `thiserror` for library crate errors, `anyhow` for CLI.
- Prefer explicit error types over `.unwrap()` in library code.
- Guard against panics in numeric code (checked arithmetic, NaN prevention).

## Incremental Cleanup

No big-bang rewrite required. Files are cleaned up as they are touched:
- When editing a file, fix any new clippy warnings your changes introduce.
- If a file is already clean, leave it alone.
- Priority targets for cleanup: `hearken-cli/src/main.rs` (large file).

## Commit Messages

Use conventional commit prefixes:

| Prefix       | Purpose                         |
|-------------|----------------------------------|
| `feat:`     | New feature                      |
| `fix:`      | Bug fix                          |
| `perf:`     | Performance improvement          |
| `refactor:` | Code restructuring (no behavior change) |
| `doc:`      | Documentation only               |
| `chore:`    | Build, CI, deps, tooling         |
| `test:`     | Adding or updating tests         |
| `style:`    | Formatting, lint fixes           |
| `build:`    | Build system changes             |

These prefixes feed into `git-cliff` for auto-generated release notes.

## Development Workflow

```bash
# Check formatting
cargo fmt --all -- --check

# Run clippy (must pass with zero warnings)
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features web -- -D warnings

# Run tests
cargo test --workspace

# Build release
cargo build --release
```
