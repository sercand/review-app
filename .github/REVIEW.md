# Reviewing this repo

GH Review (`gh-review-desktop`) is a native Rust + egui desktop app for
reviewing GitHub pull requests: PR lists, fast diffs, inline comments and
review submission, without a browser. The GUI thread never does I/O: the UI
sends a `Command` to a background `Worker` (its own multi-thread tokio
runtime) and results come back as `Event`s that `ReviewApp` drains once per
frame (`src/runtime.rs`).

## Checks

- Widgets and screens return action enums and never mutate app state.
- PR-scoped `Command`s/`Event`s carry `SessionId`; events for a left PR are dropped.
- The GUI thread never does I/O.
- Tokens and API keys only in the OS keychain, never in settings, session files, SQLite or logs.
- GitHub reads may be retried; writes never are.
- [src/ai/**] No tool may write to GitHub, merge, approve, or edit files.
- [src/diff/**] Changes keep the diff virtualized; check `tests/perf_large_pr.rs`.
- New `pub` items have doc comments; tests use `src/test_support.rs` helpers instead of redefining them.
