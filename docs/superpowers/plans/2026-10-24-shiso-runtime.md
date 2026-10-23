# shiso-runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `shiso-decision` crate with `shiso-runtime`: the non-HTTP core of `shiso-server` as a
library that runs text generation, image generation/edit and decisions in-process, with `shiso-server`
reduced to an axum adapter; then point review-app at it.

**Architecture:** Mostly `git mv` of `shiso-server`'s non-HTTP modules into a new crate, plus four seams:
a clap-free `ServeOptions`, one hub pull, one set of pub typed request/response types, typed per-family core
functions. A small `Runtime` facade wraps `AppState` + the engine thread for embedders.

**Tech Stack:** Rust 2024 (shiso toolchain 1.98.1), tokio, serde, shiso crates; review-app (egui app).

**Spec:** `docs/superpowers/specs/2026-10-24-shiso-runtime-design.md` (review-app).

## Global Constraints

- shiso-llm AGENTS.md rules bind (missing_docs, `#[expect]`, no unwrap/println/todo, thiserror, no upstream
  names in prose, `--locked`, toolchain 1.98.1, metal|cuda exclusive, device tests `#[ignore]` + stderr skip
  reason, gate `bash scripts/check.sh <crates>`).
- Moves are `git mv` with edits limited to visibility, `use` paths and the named seams. No behaviour change:
  every existing `shiso-server` test passes; test files may move with their code, but assertions don't change.
- `shiso-runtime` normal deps contain no `axum`, `tower`, `tower-http`, `hyper` server, `clap`
  (`cargo tree -p shiso-runtime -e normal --features metal`). HTTP *client* crates via shiso-hub are fine.
- Wire formats, CLI flags and behaviour of every route are unchanged.
- Branch: shiso-llm `feat/shiso-runtime`, created from `feat/shiso-decision` (e9417ee) so PR #50's shiso-hub
  fixes and the literal-encoder fix are kept; the final diff against master must contain no
  `crates/shiso-decision`.
- review-app constraints from the previous plan still apply (CI with feature off; `--ignore-rust-version`).

## Review Focus

- A model directory with only an LLM, only an image model, or both → `Runtime::open` serves what exists;
  the missing capability returns a typed error, never a panic. Tests in R6.
- `Runtime` dropped while a call is in flight on another thread, and blocking calls made inside a tokio
  task → error, no panic or hang. Tests in R6.
- `shiso serve <name>` and `shiso pull <name>` behave exactly as before (same output, same recipe-default
  handling of explicitly set flags). Tests in R2/R3 (existing CLI/hub tests unmodified).
- Image streaming over SSE gives byte-identical event framing after the typed-stream split. Test in R5
  (existing images API tests unmodified).
- `/v1/decisions` and the SDK `decide` give identical answers for the same request (parity on d1-3b).
  Test in R4/R6.

---

## Part R — shiso-llm

### Task R1: Create `shiso-runtime` and move the non-HTTP modules

**Files:** Create `crates/shiso-runtime/{Cargo.toml,src/lib.rs}`; `git mv` from `crates/shiso-server/src/`:
`scheduler/`, `state.rs`, `model_factory.rs`, `tokenizer/`, `media.rs`, `media_cache.rs`, `vision.rs`,
`image/`, `embed/`, `audio/`, `response_store.rs`, `ids.rs`, `profile.rs`, `api/{pipeline,blocking,inference_types}.rs`,
every `*_wire.rs` / `*_to_ir.rs` / `*_encode.rs` under `openai/`, `anthropic/`, `decision/`, and
`decision/{core,head,readout}.rs`; `error.rs` (keep `status()`; the `IntoResponse` impl moves to a server
`error.rs` wrapper). The boot fns from `lib.rs` move to `shiso_runtime::boot` (still taking `&ServeArgs`
until R2 — so `ServeArgs`' struct moves too in this task as `ServeOptions`-to-be; R2 removes clap from it).
Replace `axum::body::Bytes` with `bytes::Bytes`. `shiso-server` depends on `shiso-runtime` and keeps router,
handlers, sse, misc_handlers, service_tier, cli, main.

**Interfaces:** Produces `shiso_runtime::{state::AppState, error::AppError, boot::*, scheduler::*, …}` at
the same module paths they had under `shiso_server::` (re-exported from `shiso_server` only where the
server's own tests need them).

- [ ] Step 1: Move files and fix paths until `cargo check -p shiso-server --locked --features shiso-server/metal` passes. (clap may still be a runtime dep at the end of R1; R2 removes it.)
- [ ] Step 2: `cargo test -p shiso-server -p shiso-runtime --locked --features shiso-server/metal` → same pass count as before the move (record both numbers).
- [ ] Step 3: Commit `refactor(runtime): move shiso-server's non-HTTP core into shiso-runtime`.

### Task R2: `ServeOptions` — boot without clap

**Files:** `crates/shiso-runtime/src/options.rs` (new), `boot.rs`, `model_factory.rs`, `hub` resolution;
`crates/shiso-server/src/cli.rs`.

**Interfaces:** Produces `pub struct ServeOptions` (all clap-free serve fields, `Default`), `ServeOptions::validate()`, and
`pub struct ExplicitlySet` (which option names the user set; used where recipe defaults previously read
`ArgMatches`). `ServeArgs { #[command(flatten)] pub opts: ServeOptions, /* CLI-only flags */ }`. Every
`&ServeArgs` in runtime becomes `&ServeOptions`.

- [ ] Step 1: Test `serve_options_default_validates` and a test that recipe defaults don't override an explicitly set value (port the existing ArgMatches-based test if one exists; else write it).
- [ ] Step 2: Implement; `cargo tree -p shiso-runtime -e normal --features metal | grep -c clap` → 0.
- [ ] Step 3: Existing CLI/boot tests pass unmodified. Commit `refactor(runtime): ServeOptions, boot without clap`.

### Task R3: One pull

**Files:** `crates/shiso-runtime/src/hub.rs` (from server `hub.rs` resolution + `run_pull`'s core),
`crates/shiso-server/src/cli.rs`/`hub_cli` (progress rendering only); fold in `crates/shiso-decision/src/pull.rs`'s
behaviour.

**Interfaces:** Produces `pub fn resolve(model_ref: &str, cfg: &HubConfig) -> Result<Option<InstalledModel>, AppError>`,
`pub fn pull(model_ref: &str, cfg: &HubConfig, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<InstalledModel, AppError>`,
`pub struct InstalledModel { pub dir: PathBuf, pub total_bytes: u64, /* lock guard */ }`. `shiso pull` and
`shiso serve` call these. Behaviour from #50 kept: cancel via progress (~200 ms poll), bad install discarded
under locks, offline when installed, host-backend variant selection, NoDevice before download without a
backend feature.

- [ ] Step 1: Move `shiso-decision`'s pull tests (wiremock) to the runtime against the new fns; existing `hub_cli_tests` unmodified.
- [ ] Step 2: Implement; `shiso pull`'s stdout unchanged (compare the existing CLI test expectations).
- [ ] Step 3: Commit `refactor(runtime): one pull for the CLI and embedders`.

### Task R4: Decision types, typed core, and delete `shiso-decision`

**Files:** `crates/shiso-runtime/src/decision/{mod,wire,decide,math,score,literal}.rs` (math/score/literal
moved back from `shiso-decision`), `crates/shiso-server/src/decision/{decisions,systemone}.rs` (thin);
delete `crates/shiso-decision/`; update `crates/shiso-bench/tests/contracts.rs`, workspace `Cargo.toml`,
`crates/shiso-server/tests/decisions_in_process.rs` (→ runtime test).

**Interfaces:** Produces `pub struct DecisionsRequest` / `DecisionsResponse` (serde both ways; today's wire
shapes exactly), `pub fn decide(state: &AppState, req: DecisionsRequest) -> impl Future<Output = Result<DecisionsResponse, AppError>>`,
same for `SystemOneRequest`/`SystemOneResponse`/`systemone`. The axum handler: parse → `decide` → `Json`.

- [ ] Step 1: Test `decisions_response_round_trips` (serialize equals today's encoder output for a fixed answer); existing `decisions*`, `systemone*`, `decision_readout` tests unmodified.
- [ ] Step 2: Implement; delete `shiso-decision` (load/worker/decider/schema duplicates gone). `git grep shiso_decision` → only history.
- [ ] Step 3: Device parity test moved to runtime: `/v1/decisions` (router oneshot in a server test) vs `decide` on d1-3b within 1e-4.
- [ ] Step 4: Commit `refactor(runtime): typed decisions core; drop shiso-decision`.

### Task R5: Images — typed core and stream

**Files:** `crates/shiso-runtime/src/image/api.rs` (from `openai/images.rs` + `image_edits.rs` cores),
`crates/shiso-server/src/openai/{images,image_edits}.rs` (thin, SSE framing).

**Interfaces:** `pub async fn generate(state, ImageGenerationRequest) -> Result<ImageGenerationResponse, AppError>`,
`pub fn generate_stream(state, ImageGenerationRequest) -> impl Stream<Item = Result<ImageStepEvent, AppError>>`,
`pub async fn edit(state, EditRequest, Vec<InputReferenceImage>) -> …` (+ stream).

- [ ] Step 1: Existing images/images_edit API tests unmodified (SSE framing byte-identical).
- [ ] Step 2: Implement. Step 3: Commit `refactor(runtime): typed image generate/edit cores`.

### Task R6: `Runtime` facade

**Files:** `crates/shiso-runtime/src/runtime.rs`, tests.

**Interfaces:** `pub enum RuntimeSource<'a> { Registry(&'a str), Dir(&'a Path) }`; `pub struct RuntimeOptions`
(`home: PathBuf`, `pull_if_missing: bool`, `serve: ServeOptions`, …, `Default`); `Runtime::open(src, opts,
progress) -> Result<Runtime, AppError>` (blocking; resolves/pulls, boots via the same boot fns `serve` uses);
`generate`/`generate_blocking`/`generate_stream`, `images`/`edit_image`, `decide`/`decide_blocking`; `Drop`
joins the engine thread; blocking fns inside a tokio task return `AppError` (no panic).

- [ ] Step 1: Tests: open on a dir with no LLM → `generate` typed error; blocking call inside a task → error; drop-while-in-flight → error + join (fake engine or small model); `Runtime: Send + Sync`; contract pins in shiso-bench.
- [ ] Step 2: Device tests (`#[ignore]`): `Runtime::open(Registry("d1-3b"))` with the installed home (`SHISO_HOME=/private/tmp/claude-501/-Users-claw-Developer-src-github-com-sercand-review-app/21579605-a6a7-4c7d-b5a7-4cc4ec2a30c2/scratchpad/shiso-home`) + `decide_blocking` matches the endpoint; `generate`/`images` smoke tests run only if a suitable model is installed (stderr skip otherwise).
- [ ] Step 3: Commit `feat(runtime): Runtime facade for in-process generation, images and decisions`.

### Task R7: Gate, docs, PR

- [ ] README "Library use" (Runtime example, ignore doctest), PROVENANCE row for `shiso-runtime` (remove the shiso-decision row), AGENTS.md crate map.
- [ ] `bash scripts/check.sh shiso-runtime shiso-server shiso-hub shiso-bench` green (accepted failure only the pre-existing csf_reference, if it recurs).
- [ ] Commit; ask the user before pushing / opening the PR (and closing #50).

## Part S — review-app

### Task S1: Use `shiso-runtime`

**Files:** `Cargo.toml` (`shiso-runtime` at the R7 SHA; features as before), `src/ai/local_classifier/{weights,loader,decision}.rs`.

- [ ] `weights.rs`: `shiso_runtime::hub::{resolve, pull}`; `loader.rs`: `Runtime::open(Dir(installed))` + the schema probe; `decision.rs`: build `DecisionsRequest` (one `choice` question from the schema, input as today) → `rt.decide_blocking` → read the answer from `DecisionsResponse`. Fakes adapt to the new seam; every existing classifier/loader/hub test keeps its assertions.
- [ ] Feature off: no shiso crate built; CI set green; `local-classifier` build + tests green; commit `build: shiso-runtime instead of shiso-decision`.

### Task S2: Eval + docs

- [ ] Run `model_stage_real` (release, d1-3b): must reproduce benign 195→196, 0 Runs on the three unsafe sets, Unavailable < 5 %. Update README/AGENTS.md/spec mentions of `shiso-decision` → `shiso-runtime`. Commit `docs: shiso-runtime`.
