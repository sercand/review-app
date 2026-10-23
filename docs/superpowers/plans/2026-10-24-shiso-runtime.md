# shiso-runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `shiso-decision` with `shiso-runtime` — the non-HTTP core of `shiso-server` as a library
for in-process generation, images and decisions — with decision/image IR in `shiso-ir` and
`shiso-server` as the HTTP + CLI adapter; then point review-app at it.

**Architecture:** `shiso-ir` (types) ← `shiso-runtime` (engine, moved from the server by `git mv`) ←
`shiso-server` (router, handlers, wire structs, CLI). Seams: IR for decisions/images, typed cores on IR,
`ServeOptions` instead of clap's `ServeArgs`, one hub pull, a `Runtime` facade.

**Tech Stack:** Rust 2024 (shiso toolchain 1.98.1), tokio, serde (wire only), shiso crates; review-app.

**Spec:** `docs/superpowers/specs/2026-10-24-shiso-runtime-design.md` (review-app).

## Global Constraints

- shiso-llm AGENTS.md rules bind (missing_docs, `#[expect]`, no unwrap/println/todo, thiserror, no upstream
  names in prose, `--locked`, toolchain 1.98.1, metal|cuda exclusive, device tests `#[ignore]` + stderr skip
  reason, gate `bash scripts/check.sh <crates>`).
- `shiso-ir` stays serde-free and dependency-light (its Cargo.toml rule); new IR follows the existing style.
- Moves are `git mv`; edits limited to visibility, `use` paths and the named seams. Every existing
  `shiso-server` test keeps its assertions (files may move with their code). Wire formats, CLI flags and
  route behaviour are unchanged.
- `cargo tree -p shiso-runtime -e normal --features metal` contains no `axum`, `tower`, `tower-http`, `clap`
  (HTTP *client* crates via shiso-hub are fine).
- Branch: shiso-llm `feat/shiso-runtime` from `feat/shiso-decision` (e9417ee), keeping PR #50's shiso-hub
  and literal-encoder fixes; the final diff vs master contains no `crates/shiso-decision`.
- review-app: CI with the feature off; `--ignore-rust-version` (before `--`).

## Review Focus

- A model dir with only an LLM, only an image model, or both → `Runtime::open` serves what exists; the
  missing capability returns a typed error. Test in R6.
- `Runtime` dropped mid-call on another thread; blocking calls inside a tokio task → error, no panic/hang. R6.
- `shiso serve <name>` / `shiso pull <name>`: same output and same recipe-default handling of explicitly set
  flags. R2/R3 (existing CLI/hub tests unmodified).
- Image SSE framing byte-identical after the typed-stream split. R5 (existing images API tests unmodified).
- `/v1/decisions` and `Runtime::decide` answer identically for the same request (d1-3b parity ≤ 1e-4). R4/R6.

---

## Part R — shiso-llm

### Task R1: `shiso-runtime` crate — move the non-HTTP core

- Create `crates/shiso-runtime`; `git mv` from `crates/shiso-server/src/`: `scheduler/`, `state.rs`,
  `model_factory.rs`, `tokenizer/`, `media.rs`, `media_cache.rs`, `vision.rs`, `image/`, `embed/`, `audio/`,
  `ids.rs`, `profile.rs`, `api/{pipeline,blocking,inference_types}.rs`, `decision/{core,head,readout}.rs`,
  `hub.rs`'s resolution part, `error.rs` (keep `status()`; `IntoResponse` moves to a server wrapper), and
  the boot fns from `lib.rs` (→ `shiso_runtime::boot`). `axum::body::Bytes` → `bytes::Bytes`. `ServeArgs`
  may stay temporarily reachable (R2 removes clap). Backend features `metal`/`cuda` move to the runtime;
  the server forwards. Wire structs stay in the server.
- [ ] `cargo check -p shiso-server --locked --features shiso-server/metal`; then `cargo test -p shiso-server -p shiso-runtime --locked --features shiso-server/metal` with the same total pass count as before (record both).
- [ ] Commit `refactor(runtime): move shiso-server's non-HTTP core into shiso-runtime`.

### Task R2: `ServeOptions` — boot without clap

- `crates/shiso-runtime/src/options.rs`: `pub struct ServeOptions` (clap-free serve fields, `Default`,
  `validate()`), `pub struct ExplicitlySet` (option names the user set). Boot, `model_factory`, hub
  resolution take `&ServeOptions` (+ `&ExplicitlySet` where recipe defaults apply). Server `ServeArgs {
  #[command(flatten)] opts, CLI-only flags }` — clap derive via a runtime `clap` feature enabled only by the
  server, or a server-side mirror converted with `From`; choose the smaller and say why.
- [ ] Tests: `serve_options_default_validates`; recipe defaults never override an explicitly set option (port the ArgMatches test if present).
- [ ] `cargo tree -p shiso-runtime -e normal --features metal | grep -c clap` → 0 (with the `clap` feature off). Existing CLI/boot tests unmodified. Commit `refactor(runtime): ServeOptions; boot without clap`.

### Task R3: One pull

- `shiso_runtime::hub::{resolve(model_ref, &HubConfig) -> Result<Option<InstalledModel>, AppError>, pull(model_ref, &HubConfig, &mut dyn FnMut(u64,u64)->bool) -> Result<InstalledModel, AppError>}`,
  `pub struct InstalledModel { pub dir: PathBuf, pub total_bytes: u64, /* lock guard */ }`. `shiso pull` and
  `shiso serve` call these and only render progress. Bring over `shiso-decision/src/pull.rs`'s behaviour
  and tests (cancel tick, timeouts already in shiso-hub, discard-bad-install under locks, offline when
  installed, host-backend variant selection, NoDevice before download without a backend feature).
- [ ] Moved wiremock tests pass against the new fns; existing `hub_cli_tests` unmodified (same stdout).
- [ ] Commit `refactor(runtime): one pull for the CLI and embedders`.

### Task R4: Decision IR, typed core, delete `shiso-decision`

- `crates/shiso-ir/src/decision.rs`: `Request { input: String, media: Vec<..>, questions: Vec<Question>, temperature: Option<f32> }`,
  `Question { name, instructions, kind: QuestionKind::{Choice(Vec<Option_>), YesNo { yes, no }, Score(Vec<Option_>)} }`,
  `Option_ { value, description: Option<String> }` (name it per shiso style), `Response { answers: Vec<Answer> }`,
  `Answer { name, choice, probabilities: Vec<(String, f32)>, confidence: f32, expected_score: Option<f32> }`
  + whatever extras `/v1/decisions` and `/v1/systemone` report today (usage/token counts) — derive the exact
  field list from `decisions_encode.rs` / `systemone_encode.rs`.
- `shiso_runtime::decision::{decide, systemone}` take IR, return IR; math / score / literal move from
  `shiso-decision` into `shiso_runtime::decision`; server handlers: wire → IR (`decisions_wire.rs`) → core →
  IR → wire (`decisions_encode.rs`). Delete `crates/shiso-decision`; update workspace `Cargo.toml`,
  `shiso-bench/tests/contracts.rs`, the parity test.
- [ ] Tests: wire → IR → wire round-trip equals today's encoder output for fixed answers; existing `decisions*`, `systemone*`, `decision_readout` tests unmodified; device parity `/v1/decisions` vs `decide` on d1-3b ≤ 1e-4.
- [ ] `git grep -n shiso_decision` → none outside history. Commit `refactor(runtime): decision IR and typed core; drop shiso-decision`.

### Task R5: Image IR and typed core

- `crates/shiso-ir/src/image.rs`: `Generate`, `Edit` (with reference images), `Images` (result), `Step`
  (stream event) — fields derived from `images_wire.rs` / `image_edits_wire.rs` / the SSE events.
- `shiso_runtime::image::{generate, generate_stream, edit, edit_stream}` on IR; server handlers keep
  multipart parsing and SSE framing.
- [ ] Existing images / images_edit API tests unmodified (SSE byte-identical); wire → IR → wire round trip. Commit `refactor(runtime): image IR and typed generate/edit cores`.

### Task R6: `Runtime` facade

- `shiso_runtime::Runtime`: `enum Source<'a> { Registry(&'a str), Dir(&'a Path) }`, `struct RuntimeOptions {
  home, pull_if_missing, serve: ServeOptions, .. }` (`Default`), `Runtime::open(source, opts, progress)`
  (blocking; resolve/pull, then the same boot fns `serve` uses), `generate`/`generate_blocking`/
  `generate_stream`, `images`/`edit_image`, `decide`/`decide_blocking`. Drop joins the engine thread;
  blocking fns inside a tokio task return `AppError`.
- [ ] Tests: capability errors (dir without LLM / without image model); blocking-in-task → error; drop-in-flight → error + join; `Runtime: Send + Sync`; contract pins in shiso-bench.
- [ ] Device (`#[ignore]`): `Runtime::open(Registry("d1-3b"))` with `SHISO_HOME=/private/tmp/claude-501/-Users-claw-Developer-src-github-com-sercand-review-app/21579605-a6a7-4c7d-b5a7-4cc4ec2a30c2/scratchpad/shiso-home` + `decide_blocking` equals the endpoint; `generate` / `images` smoke tests skip on stderr when no suitable model is installed.
- [ ] Commit `feat(runtime): Runtime facade for in-process generation, images and decisions`.

### Task R7: Gate, docs, PR

- [ ] README "Library use" (Runtime example, `ignore` doctest), PROVENANCE (shiso-runtime row; shiso-decision row removed), AGENTS.md crate map.
- [ ] `bash scripts/check.sh shiso-ir shiso-runtime shiso-server shiso-hub shiso-bench` green.
- [ ] Commit; ask the user before pushing / opening the PR and closing #50.

## Part S — review-app

### Task S1: Use `shiso-runtime`

- `Cargo.toml`: `shiso-runtime` + `shiso-ir` at the R7 SHA; `local-classifier` → `shiso-runtime/metal`,
  `local-classifier-cuda` → `shiso-runtime/cuda`, internal `_classifier`.
- `weights.rs` → `shiso_runtime::hub::{resolve, pull}`; `loader.rs` → `Runtime::open(Source::Dir(..))` + the
  schema probe; `decision.rs` → `shiso_ir::decision::Request` (one Choice question from the schema, input as
  today) → `rt.decide_blocking` → answer from `shiso_ir::decision::Response`. Fakes adapt; every existing
  classifier/loader/hub test keeps its assertions.
- [ ] Feature off: no shiso crate built; CI set green; `local-classifier` build + tests green. Commit `build: shiso-runtime instead of shiso-decision`.

### Task S2: Eval + docs

- [ ] `model_stage_real` (release, d1-3b) reproduces benign 195→196, 0 Runs on the unsafe sets, Unavailable < 5 %. README / AGENTS.md / spec mentions of `shiso-decision` → `shiso-runtime`. Commit `docs: shiso-runtime`.
