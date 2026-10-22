# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

GH Review (`gh-review-desktop`) is a native Rust + egui (eframe 0.37) desktop app for reviewing GitHub pull requests. See `README.md` for features and `docs/superpowers/specs/2026-10-19-gh-review-desktop-design.md` for the full spec. Code comments cite spec sections (e.g. "spec §8.4"), so look there for intent.

## Commands

CI (`.github/workflows/ci.yml`) runs these on Linux, macOS and Windows; all must pass:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --release -- --ignored          # perf guardrail (tests/perf_large_pr.rs), Linux CI only
```

Other useful ones:

```sh
cargo run                                  # on macOS goes through scripts/macos-run.sh (re-sign + dev .app bundle)
cargo test --lib state::pr_detail          # unit tests in one module
cargo test --test contract_threads         # one integration test file
cargo test --lib shots -- --ignored        # offscreen screenshots → target/ui-shots/ (needs a GPU)
```

Linux builds need `pkg-config libdbus-1-dev libxkbcommon-dev libgtk-3-dev`.

macOS release: `scripts/macos/archive.sh` then `scripts/macos/notarize.sh` (outputs in `target/dist/`; setup in `scripts/macos/README.md`). `.cargo/config.toml` sets `scripts/macos-run.sh` as the macOS cargo runner — it re-signs the debug binary so the keychain's "Always Allow" survives rebuilds, and wraps it in `target/bundle/` via `scripts/macos/dev-app.sh`.

## Architecture

The crate is a library (`src/lib.rs`) plus a thin `src/main.rs` that calls `app::run()`, so integration tests can use the public module tree.

**UI ⇄ network split (`src/runtime.rs`).** The GUI thread never does I/O. The UI sends a `Command` to a `Worker` (its own multi-thread tokio runtime); results come back as `Event`s that `ReviewApp` drains once per frame. Adding a network feature usually means: a `Command` variant + handler in `runtime.rs`, an `Event` variant, a client method in `src/github/`, and handling in `app.rs` / `state/`.

**Stale-event rule.** Each opened PR gets a `SessionId`; every PR-scoped `Event` carries it, and events for a PR the user has left are dropped. Preserve this when adding PR-scoped commands/events.

**`src/app.rs` (`ReviewApp`)** owns screen routing (`ScreenKind`, `route`), global shortcut/menu/palette dispatch (action ids from `ui/command.rs` registry), settings/theme, and applies the action enums widgets return.

**State vs. UI.** `src/state/` holds screen state and reducers, kept pure and unit-tested; `state/pr_detail.rs` (`PrSession`) is the largest — it drives PR loading as a state machine (metadata first so the head SHA is known, then files page by page), optimistic comment posting, pending review, viewed marks. Widgets in `src/ui/widgets/` and screens in `src/ui/screens/` don't mutate app state directly: they return action enums (e.g. `PrDetailAction`, `DiffAction`) that `app.rs` applies. Widget decision logic is factored into pure functions so it's testable without a window.

**Diff pipeline (`src/diff/`).** `parse` (patch → hunks) → `anchor`/`annotate` (map review threads/comments to rows) → `row_model` (flattened rows with offsets for virtualization) → `ui/widgets/diff_view.rs` renders only visible rows through a bounded `highlight` cache (syntect). `expand` handles context expansion; `image` handles image diffs. Performance on huge PRs (≥50k rows) is guarded by `tests/perf_large_pr.rs`.

**GitHub client (`src/github/`).** Hand-written over reqwest, REST + GraphQL (PR list is one GraphQL search). Reads may be retried; writes never are. `error.rs` classifies failures into `GhError`. Supports GitHub Enterprise Server via `Endpoints`.

**Persistence (`src/persist/`).** `settings.toml` plus per-PR `sessions/<host>/<owner>/<repo>/<number>.json` (viewed marks, pending review, composer drafts) with atomic, debounced writes. Tokens live only in the OS keychain (`src/auth/store.rs`, service `gh-review-desktop`) — never in session files.

**AI review (`src/ai/`, `#[cfg(unix)]`).** The engine: `llm.rs` builds provider clients from the vendored `crates/llm` (Anthropic + OpenAI behind neutral types); `crates/sandbox` (vendored) runs the agent's bash on a PTY inside an OS sandbox (Seatbelt / bubblewrap) that can't write `.git/` and has no credentials, and classifies commands for approval; `tools/` are the agent tools, `agent.rs` the agent loop, `pipeline.rs` the scout → reviewers → challengers → verifiers → assembler orchestrator, `context.rs`/`checklist.rs` the repo context (`.github/REVIEW.md` `## Checks` → `repo:*` items, directory-scoped `AGENTS.md`/`CLAUDE.md`, skills index), `store.rs` the SQLite persistence (`ai.sqlite`), `checkout.rs` the app-run git checkout/restore and prepare commands, and `hub.rs` the worker-side hub that turns AI `Command`s into `Event`s like the rest of `runtime.rs`. The UI side is `state/ai_review.rs` plus `ui/widgets/ai_panel.rs` and `ai_dialogs.rs`, following the same action-enum and `SessionId` rules. The agent has no GitHub credentials and no write path: nothing reaches GitHub except the user's own review submit.

## Tests

- Unit tests live inline in each module. Shared helpers are in `src/test_support.rs` (`#[cfg(test)]`); read it before adding helpers and don't redefine existing ones. Load fixtures with `use crate::fixture; fixture!("name.json")` (compile-time embedded from `tests/fixtures/`).
- `tests/contract_*.rs` run `GithubClient` against `wiremock` and assert exact request bodies. Their shared helpers are in `tests/common/mod.rs` (integration tests can't see `test_support`).
- `src/ui/shots.rs` renders every screen with `egui_kittest` for comparison against the HTML mockups in `docs/design/mockups/`; missing glyphs fail the run.
- `.github/REVIEW.md` is this repo's own review checklist, consumed by the AI reviewer (`src/ai/context.rs` parses its `## Checks` bullets; `[glob]`-prefixed ones scope to matching paths). Keep it in sync when the rules above change; `context_parses_this_repos_review_md` guards it.
