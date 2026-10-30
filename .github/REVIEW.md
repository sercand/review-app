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
- No change may make `review_approval::classify` return `Run` for a command containing an unhandled construct; every new policy needs a table test and a deny/defer counter-example.
- The approval parser puts a value in place of `$NAME` only from a `NAME=literal` statement (or a literal `for` word) certain to have run first, and forgets it after anything that may assign it; command substitutions other than `$(pwd)` and `$(git rev-parse --show-toplevel)` still bail.
- A new `CommandClassifier` implementation must keep returning `Unavailable` on any failure. The model is asked only after `classify` returns `Ask` (never on `Run` or `Deny`), sees the whole command, and its `Safe` runs it unless `floor_reason` (`crates/approval/src/floor.rs`) holds it; a deny rule must never be skipped because of a model answer, and a change that lets a dangerous or adversarial fixture past the floor must add it to `left_to_model.txt` deliberately (`floor_holds_everything_else`).
- `[ai.auto_approve].mode = "lenient"` runs a classifier's unsure answer only through the floor, never an `Unavailable` or skipped one. `mode = "trust"` (full trust) bypasses the approval engine and the sandbox's secret-read denies only at the `bash` tool and hub boundaries; no other tool may consult it, and it never skips prepare-command approval.
- `local_classifier` (decision-model) code must return `Unavailable` rather than propagate errors, keep a present-but-invalid `decision_schema.json` from falling back to the built-in schema, and never log command text above debug; the decision input must stay one `cmd:` line (command text escaped).
- [src/ai/**] No tool may write to GitHub, merge, approve, or edit files.
- [src/diff/**] Changes keep the diff virtualized; check `tests/perf_large_pr.rs`.
- New `pub` items have doc comments; tests use `src/test_support.rs` helpers instead of redefining them.
