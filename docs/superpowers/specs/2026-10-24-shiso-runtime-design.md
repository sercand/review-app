# shiso-runtime: one in-process engine for generation, images and decisions

Date: 2026-10-24. Status: design approved in conversation. Supersedes Part 1 of
`2026-10-23-decision-model-classifier-design.md` (the `shiso-decision` crate is dropped).

## Problem

`shiso-decision` (shiso-llm PR #50, unmerged) re-implemented things `shiso-server` already has: model
loading (LFM2-only copy of `open_backend`/`model_factory`), a second engine thread beside the scheduler,
a second set of decision types beside `decisions_wire.rs`, and a second pull path beside `run_pull`. It
also only does decisions. Embedders need text generation, image generation/edit and decisions from one
engine, in-process, with no HTTP.

What blocks embedding `shiso-server` today is **not axum** (only the router and the thin handler files use
it) but **clap**: boot, `model_factory` and `resolve_serve_model` take `ServeArgs`; and the decision wire
types are `pub(crate)`.

## Design

**Crates (shiso-llm):**

- **`shiso-runtime`** (new library; no axum, tower, hyper-server, clap): everything in `shiso-server`
  that is not HTTP — `scheduler/`, `state.rs` (`AppState`), `model_factory.rs`, boot (`boot_language_model`,
  `boot_image`, `boot_audio`, `boot_embedding`, `open_backend`, `build_renderer`, `scheduler_config`),
  `tokenizer/`, `media*.rs`, `vision.rs`, `image/`, `embed/`, `audio/`, `response_store.rs`, `ids.rs`,
  `profile.rs`, `api/{pipeline,blocking,inference_types}.rs`, all `*_wire` / `*_to_ir` / `*_encode` files,
  `decision/{core,head,readout,math,…}`, `hub` resolution + pull, and `error.rs` (`AppError`) minus its
  `IntoResponse` impl. Files move by `git mv`; edits are limited to visibility, imports and the seams below.
- **`shiso-server`** (thin adapter + binary): `router.rs`, the axum handlers (parse → call runtime →
  encode/stream), `api/{sse,misc_handlers,service_tier}.rs`, `cli.rs`, `main.rs`, and the `IntoResponse`
  wrapper for `AppError`. Its `metal`/`cuda` features forward to `shiso-runtime`.
- **`shiso-decision`** is deleted. Its moved pieces (math, score, literal encoder) go back under
  `shiso_runtime::decision`; its duplicates (schema, load, worker, decider, pull) are deleted; the genuinely
  new pull behaviour merges into the one pull (below). PR #50 is closed in favour of the new PR.

**Seams:**

1. **`ServeOptions`** — the clap-free fields of `ServeArgs` (model/config/tokenizer paths, ctx, KV and
   checkpoint knobs, scheduler config, engine options, image model + overrides, sampling preset, chat
   template, media decode policies). `ServeArgs` keeps CLI-only flags and `#[command(flatten)] opts:
   ServeOptions`. Every `&ServeArgs` in boot/model_factory/hub becomes `&ServeOptions`; `validate` moves
   with it. Recipe defaults that used `ArgMatches` take an "explicitly set" set instead (the CLI computes
   it from `ArgMatches`; the SDK passes what the caller set).
2. **One pull** — `shiso_runtime::hub::pull(model_ref, &HubConfig, progress) -> Result<InstalledModel,
   AppError>` and `resolve(model_ref, &HubConfig) -> Option<InstalledModel>`. `shiso pull` and `shiso
   serve` call them and only render progress; the SDK calls the same. It carries the improvements made in
   PR #50: cancellable progress (callback returns `false`, polled ~200 ms), hub client connect/read
   timeouts, a bad install discarded (variant dir + bad blobs, under locks) and never accepted later,
   index tolerance of unknown tasks, parse-before-cache. (The shiso-hub parts of #50 are kept as they are.)
3. **One set of types** — decision request/response become `pub` typed structs with serde both ways:
   `DecisionsRequest` (today's `DecisionsBody` shape) and `DecisionsResponse` (today's JSON map shape as
   a struct). `/v1/decisions` deserializes/serializes them; the SDK uses them directly. `/v1/systemone`
   likewise (`SystemOneRequest`/`SystemOneResponse`). Generation uses `shiso_ir::Request`/`Response`; images
   use the existing `ImageGenerationRequest`/`ImageGenerationResponse`/`EditRequest`. Request types gain
   `Serialize` + `Default` where cheap so Rust callers can build them.
4. **Typed cores** — each handler family exposes a core fn in the runtime that the axum handler calls:
   - generation: existing `pipeline::start` + `blocking::collect_response` (no change);
   - images: `images::generate(&AppState, ImageGenerationRequest) -> Result<ImageGenerationResponse>` and
     `generate_stream(...) -> impl Stream<Item = ImageStepEvent>` (typed events; SSE framing stays in the
     server); edits take `EditRequest` + decoded reference images;
   - decisions: `decision::decide(&AppState, DecisionsRequest) -> Result<DecisionsResponse>` (the dispatch
     between head / readout / generic label scoring moves out of the axum `answer()`), same for systemone.
   Response store and `HeaderMap` service-tier handling stay in the server.

**Embedder API** (`shiso_runtime::Runtime`, wraps `Arc<AppState>` + the engine thread + the install lock):

```rust
let rt = Runtime::open(RuntimeSource::Registry("d1-3b"), RuntimeOptions { home, pull_if_missing: true,
                       progress: Some(&mut |done, total| true), ..Default::default() })?;   // blocking
// or RuntimeSource::Dir(path)
rt.generate(shiso_ir::Request)          -> Result<shiso_ir::Response, AppError>   // async + _blocking
rt.generate_stream(shiso_ir::Request)   -> impl Stream<Item = StreamEvent>
rt.images(ImageGenerationRequest)       -> Result<ImageGenerationResponse, AppError>
rt.edit_image(EditRequest, refs)        -> Result<ImageGenerationResponse, AppError>
rt.decide(DecisionsRequest)             -> Result<DecisionsResponse, AppError>    // async + _blocking
```

- One `Runtime` serves whatever the model directory provides (LLM, image, or both), as `serve` does
  today; calling a capability the model lacks returns a typed `AppError`.
- Blocking variants must not be called inside an async task (documented; returns an error, no panic).
  Drop joins the engine thread (documented: don't drop on an executor thread).
- Decisions keep the literal-aware encoding fix from PR #50 (caller text never becomes any added token,
  offset-based detection).

## review-app changes

- Dependency `shiso-decision` → `shiso-runtime` (same features scheme: `local-classifier` → `metal`,
  `local-classifier-cuda` → `cuda`, internal `_classifier`).
- `weights.rs`: `shiso_runtime::hub::pull` / `resolve`; `loader.rs` engine: `Runtime::open(Dir(installed))`
  then the schema probe; `decision.rs`: build a `DecisionsRequest` (one `choice` question from the schema),
  call `rt.decide_blocking`, read the answer from `DecisionsResponse`. Everything else (schema loading +
  default, verdict mapping, escaping, busy guard, breaker, loader state machine, settings, UI, eval) is
  unchanged; tests that faked `Decider` fake the new seam.

## Non-goals

No change to wire formats, CLI flags, model families, or review-app's policy. No new HTTP endpoint. CUDA
stays untested here. SmolLM2 family is separate.

## Verification

- shiso: `scripts/check.sh` for runtime + server + hub + bench green; every existing server test passes
  unmodified (moves of test files are fine); new runtime tests: `Runtime::open` on d1-3b + `decide` equals
  `/v1/decisions` for the same request (parity, ≤1e-4), a `generate` smoke test on a small chat model if one
  is installed (else `#[ignore]` documented), an `images` smoke test likewise; `cargo tree -p shiso-runtime
  -e normal` contains no axum/tower-http/clap.
- review-app: CI set green with the feature off; `local-classifier` build + tests; the d1-3b device eval
  reproduces (benign 195→196, 0 Runs on the three unsafe sets).
