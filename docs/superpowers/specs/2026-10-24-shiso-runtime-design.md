# shiso-runtime: the in-process engine, with IR types in shiso-ir

Date: 2026-10-24. Status: approved in conversation. Supersedes Part 1 of
`2026-10-23-decision-model-classifier-design.md` (the `shiso-decision` crate is dropped).

## Problem

`shiso-decision` (shiso-llm PR #50, unmerged) duplicated what `shiso-server` already has — model loading,
a second engine thread, a second set of decision types, a second pull path — and only did decisions.
Embedders need text generation, image generation/edit and decisions from the same engine, in-process,
with no HTTP.

`shiso-ir` already is the narrow waist between wire formats and the engine (neutral `Request`,
`Response`, `StreamEvent`, embeddings; no serde by design). What blocks embedding today: decisions and
images have no IR (the engine-facing types are `pub(crate)` wire structs), boot / `model_factory` /
`resolve_serve_model` take clap's `ServeArgs`, and the pull logic is tied to CLI output.

## Design

**Three crates, one direction:** `shiso-ir` (types) ← `shiso-runtime` (engine) ← `shiso-server` (HTTP + CLI).

1. **IR in `shiso-ir`** (neutral, no serde, same style as the existing modules):
   - `shiso_ir::decision` — request (input/state, optional media, questions: choice / yes-no / score
     with name, instructions, options with optional descriptions; temperature) and response (per question:
     name, chosen value, per-option probabilities in the caller's order, confidence, score extras the routes
     report today).
   - `shiso_ir::image` — generate request, edit request (with reference images), result (images + the
     metadata the routes report today), and a typed step event for streaming.
   The server's routes parse wire → IR and encode IR → wire; wire formats do not change; wire structs stay
   in the server.
2. **`shiso-runtime`** (new library crate; no axum, tower, tower-http, clap): the non-HTTP core moved out
   of `shiso-server` by `git mv` — `scheduler/`, `state.rs` (`AppState`), `model_factory.rs`, boot
   (`boot_language_model`, `boot_image`, `boot_audio`, `boot_embedding`, `open_backend`, `build_renderer`,
   `scheduler_config`), `tokenizer/`, `media*.rs`, `vision.rs`, `image/`, `embed/`, `audio/`, `ids.rs`,
   `profile.rs`, `api/{pipeline,blocking,inference_types}.rs`, `decision/{core,head,readout}` + the math /
   score / literal encoder from `shiso-decision`, hub resolution + pull, and `error.rs` (`AppError`) minus
   its `IntoResponse` impl. Edits are limited to visibility, `use` paths and the seams below. Backend
   features `metal` / `cuda` live here; `shiso-server`'s forward to them.
3. **`shiso-server`** becomes the HTTP + CLI adapter: router, axum handlers (wire → IR → runtime core →
   IR → wire), SSE framing, wire structs (`*_wire`, `*_to_ir`, `*_encode`), misc handlers, service tier,
   response store, CLI, binary, and the `IntoResponse` wrapper for `AppError`.
4. **Seams in the runtime:**
   - `ServeOptions` — the clap-free serve fields; boot and `model_factory` take `&ServeOptions`; recipe
     defaults that read `ArgMatches` take an explicitly-set set. `ServeArgs` (server) keeps CLI-only flags
     and `#[command(flatten)] opts: ServeOptions` (clap derive on `ServeOptions` behind a runtime feature
     `clap` that only the server enables — or a server-side mirror; pick the smaller).
   - Typed cores on IR: `decision::decide(&AppState, ir::decision::Request) -> Result<ir::decision::Response>`
     (the head / readout / generic-label dispatch leaves the axum `answer()`), `image::{generate,
     generate_stream, edit}` on IR, generation via the existing `pipeline::start` + `collect_response`.
   - One pull: `hub::{resolve, pull}(model_ref, &HubConfig, progress)` used by `shiso pull`, `shiso
     serve` and embedders; the CLI only renders progress. It carries PR #50's improvements: cancellable
     progress (~200 ms poll), hub client connect/read timeouts, a bad install discarded under locks and
     never accepted later, index tolerance of unknown tasks, parse-before-cache.
5. **`Runtime`** (`shiso_runtime::Runtime`): wraps `AppState` + the engine thread + the install lock.
   ```rust
   let rt = Runtime::open(Source::Registry("d1-3b"), RuntimeOptions { home, serve: ServeOptions::default(), .. },
                          &mut |done, total| true)?;          // blocking; pulls if missing; Source::Dir(path) too
   rt.generate(shiso_ir::Request)          -> Result<shiso_ir::Response, AppError>  // + _blocking, stream
   rt.images(shiso_ir::image::Generate)    -> Result<shiso_ir::image::Images, AppError>
   rt.edit_image(shiso_ir::image::Edit)    -> Result<shiso_ir::image::Images, AppError>
   rt.decide(shiso_ir::decision::Request)  -> Result<shiso_ir::decision::Response, AppError> // + _blocking
   ```
   One `Runtime` serves what the model directory provides (LLM, image, or both); a missing capability is a
   typed error. Blocking calls inside a tokio task return an error (no panic); drop joins the engine thread
   (documented). `shiso serve` builds its `AppState` through the same boot fns.
6. **`shiso-decision` is deleted**; PR #50 is closed in favour of the new PR. The literal-encoder fix
   (caller text never becomes any added token, offset-based detection) is kept.

## review-app

- Depend on `shiso-runtime` (+ `shiso-ir`); features `local-classifier` → `shiso-runtime/metal`,
  `local-classifier-cuda` → `shiso-runtime/cuda`, internal `_classifier`.
- `weights.rs` → `shiso_runtime::hub::{resolve, pull}`; `loader.rs` → `Runtime::open(Source::Dir(..))`
  then the schema probe; `decision.rs` builds a `shiso_ir::decision::Request` (one choice question from
  the schema, input as today) and calls `rt.decide_blocking`. Everything else in review-app is unchanged.

## Non-goals

No wire-format or CLI change; no new family (SmolLM2 separate); CUDA stays untested here.

## Verification

- shiso: every existing server test passes unmodified (test files may move with their code); wire → IR →
  wire round-trip tests for decisions and images equal today's output; `cargo tree -p shiso-runtime -e
  normal --features metal` has no axum / tower / tower-http / clap; `Runtime` tests (capability errors,
  blocking-in-task, drop-in-flight, Send + Sync, contract pins); device: `Runtime::decide` on d1-3b equals
  `/v1/decisions` (≤ 1e-4); `scripts/check.sh` green.
- review-app: CI green with the feature off; `local-classifier` build + tests; the d1-3b eval reproduces
  (benign 195→196, 0 Runs on the unsafe sets, Unavailable < 5 %).
