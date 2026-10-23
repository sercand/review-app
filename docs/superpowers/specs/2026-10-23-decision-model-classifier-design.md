# Bash auto-approve: shiso decision models as the local classifier

Superseded by `2026-10-24-shiso-runtime-design.md` (the `shiso-decision` crate is replaced by `shiso-runtime`).

Date: 2026-10-23. Status: approved in conversation (sections 1 and 2); this file is the written spec.
Supersedes the model parts of `docs/superpowers/plans/2026-10-22-ai-bash-auto-approve.md` (M0, M1, M4):
the da DistilBERT classifier is dropped (its shiso change was reverted in shiso-llm #49).

## Goal

The bash auto-approve model stage asks a **shiso decision model**, run in-process through a Rust
library (no HTTP anywhere), whether an AI coding agent should run a shell command without asking:

```
input:    "cwd: <repo root>\ntask: unknown\ncmd: <command segment>"
question: choice "decision" — approve | ask | deny (instructions + descriptions = the model's rubric)
answer:   choice, per-option probabilities, confidence
```

Testing uses the registry model **`d1-3b`** (LiquidAI d1-3B, lfm2 family, Q8_0 GGUF, Metal, ~3.2 GiB).
Production will use the user's own SmolLM2-135M-based decision model; adding a shiso family for it is
**out of scope** — review-app only takes a registry model name.

## What stays as it is

Rules engine, deny paths (incl. the OS `.env*` deny), never-approve filter, `ModelPolicy` + decision
cache, `CommandClassifier::active()`, `LiveClassifier` + live flag, busy guard, 3-failure breaker,
loader state machine, `ClassifierState` events, settings UI section, `spawn_blocking` + timeout
fallback, "model deny → ask with a warning", off by default, download only when the user enables the
classifier, the M5 evaluation harness and its promotion rule.

## Part 1 — shiso-llm: `shiso-decision` crate

A new workspace crate `crates/shiso-decision` (library SDK; follows shiso's AGENTS.md rules).

**Dependencies:** `shiso-core`, `shiso-engine`, `shiso-models`, `shiso-hub`, the tokenizer, and the
backend crate selected by feature (`metal` | `cuda`, mutually exclusive as elsewhere in shiso). It must
not depend on `shiso-server`, axum, clap or any HTTP crate.

**Moved out of `shiso-server` into it (not copied):**
- the probability math (`decision/math.rs`: label log-probs, softmax with temperature, label mass,
  choice/score confidence);
- the three decision paths: the `DecisionLayout` head (Clef), the `DecisionReadout` (d1/LFM2) and
  generic label scoring for chat models;
- the model boot steps the decision path needs (backend open, `load_model`, vocabulary, engine spawn).
  Where these take clap-shaped `ServeArgs` today, introduce a plain options struct that both the server
  and the library build.

`shiso-server`'s `/v1/decisions` and `/v1/systemone` become thin callers of `shiso-decision`, so the
math and prompt building exist once. Their wire format and behaviour do not change.

**API (names may be adjusted to shiso conventions; shapes are fixed):**

```rust
pub struct OpenDesc<'a> { pub model_dir: &'a Path, pub ctx: usize /* default 2048 */ }
pub struct Decider { /* engine thread, layout/readout, vocabulary */ }       // Send + Sync
impl Decider {
    pub fn open(desc: &OpenDesc<'_>) -> Result<Decider, DecideError>;        // blocking: backend + load
    pub async fn decide(&self, req: &DecisionRequest) -> Result<Vec<Answer>, DecideError>;
    pub fn decide_blocking(&self, req: &DecisionRequest) -> Result<Vec<Answer>, DecideError>;
}
pub struct DecisionRequest { pub input: String, pub questions: Vec<Question>, pub temperature: Option<f32> }
pub enum Question {
    Choice { name: String, instructions: String, choices: Vec<Choice> },
    YesNo  { name: String, instructions: String, yes: Option<String>, no: Option<String> },
    Score  { name: String, instructions: String, levels: Vec<Choice> },
}
pub struct Choice { pub value: String, pub description: Option<String> }
pub struct Answer {
    pub name: String,
    pub choice: String,                      // first argmax
    pub probabilities: Vec<(String, f32)>,   // caller's option order
    pub confidence: f32,                     // same definition as /v1/decisions today
}
pub enum DecideError { NoDevice(String), Load(String), Layout(String), Engine(String), Io(std::io::Error), .. }
pub fn ensure_model(name: &str, home: &Path, progress: impl FnMut(u64, u64) -> bool)
    -> Result<PathBuf, DecideError>;          // registry name -> installed, verified model dir
```

- `Decider` owns the engine thread; dropping the last handle joins it. Concurrent `decide` calls queue in
  the engine. No panics across the API: load/score failures are `DecideError`s.
- `ensure_model` wraps shiso-hub (`RegistryClient` → resolve → manifest → `select_variant` →
  `Downloader` + `BlobStore`): resumable, SHA-256 verified, cancellable through `progress` returning
  `false`. It reports the variant's total size for the UI.
- Registry robustness (from the earlier review, now in this crate's path): index parsing must tolerate
  unknown task/modality strings, and a fresh index must be parsed before it replaces the cached one.
- Tests: the existing `/v1/decisions` and `/v1/systemone` tests pass unchanged after the move; new
  `#[ignore]` device tests run `Decider` on d1-3b (env-var model dir, shiso convention) and assert the
  answers equal what the endpoint returns for the same request; host tests for the moved math.
- Gate: shiso's `scripts/check.sh` for the touched crates; PROVENANCE/README rows for the new crate.

## Part 2 — review-app

**Removed:** `src/ai/local_classifier/calibrate.rs`, `tests/fixtures/classifier_goldens.json`, the
pinned-HF constants in `weights.rs`, all `distilbert_cls` use. The shiso git dependency moves to the
shiso-llm commit that adds `shiso-decision`; the app depends on `shiso-decision` only (plus whatever
it re-exports), never on `shiso-server`. Features: `local-classifier` (Metal) and
`local-classifier-cuda` stay.

**Trait change (crates/approval):** `CommandClassifier::classify` takes
`&ClassifyInput { segment: &str, cwd: Option<&Path> }`. `ModelPolicy::verify` already receives the cwd
and passes it. The decision cache key becomes `(cwd, segment)`.

**`DecisionClassifier: CommandClassifier`** (replaces `ShisoClassifier`'s logits path):
- builds `input = "cwd: {cwd or \"unknown\"}\ntask: unknown\ncmd: {segment}"`;
- asks one `Choice` question from the model's schema;
- verdict: choice `approve` and p(approve) ≥ `approve_threshold` → `Safe`; choice `deny` →
  `Dangerous` (becomes an Ask with the "flagged" warning); anything else → `NeedsApproval`; any error,
  non-finite probability, or a schema option missing from the answer → `Unavailable`;
- keeps the busy guard (busy → `Unavailable` immediately), `catch_unwind`, the 3-failure breaker and
  `active()`; never logs command text above `debug`.

**Decision schema shipped with the model:** `decision_schema.json` in the model's directory:

```json
{
  "question": "decision",
  "instructions": "Should an AI coding agent run this shell command without asking the user?",
  "choices": [
    {"value": "approve", "description": "<rubric>"},
    {"value": "ask",     "description": "<rubric>"},
    {"value": "deny",    "description": "<rubric>"}
  ],
  "approve_threshold": 0.6
}
```

- Loaded at model load; `approve_threshold` defaults to 0.60 when absent; the three values must be
  exactly `approve`, `ask`, `deny` or the model is `Unavailable("invalid decision schema")`.
- If the file is missing, the app uses its **built-in default schema** (a constant in review-app with
  the instructions above, an approve/ask/deny rubric we write, and `approve_threshold` 0.60) for any
  model. A file that is present but invalid is still `Unavailable("invalid decision schema")`; the
  default is never substituted for a broken file. Settings shows whether the model's own schema or the
  default is in use.

**Model selection and download:** new setting `ai.auto_approve.decision_model` (registry name, default
`d1-3b`, `#[serde(default)]`). The loader calls `shiso_decision::ensure_model(name, <config root>/models,
progress)` then `Decider::open`; changing the model name while enabled reloads. The Settings state line
takes the download size from the selected variant instead of a hard-coded figure. Changing the name
in Settings is a text field (validated non-empty, `[a-z0-9._-]`).

**Timeout:** default `classifier_timeout_ms` becomes 2000 (still clamped 50..=5000). Busy or timed-out
calls fall back to the no-model verdict as today.

**Evaluation (M5):** the harness and fixtures stay; the device eval runs `DecisionClassifier` on d1-3b
(`SHISO_TEST_DECISION_MODEL_DIR=<installed d1-3b dir>`), reports the same table (Run counts per set,
benign prompt reduction rules-only vs with model, every needs_approval Run), and asserts zero Runs on
dangerous + adversarial. The promotion rule (zero false approvals and ≥ +10 points) decides the default;
until then the classifier stays off.

**Docs:** README local-classifier section, AGENTS.md AI paragraph, `.github/REVIEW.md` check wording,
and the evaluation note updated for decision models; the da/DistilBERT text and the HF repo reference
are removed.

## Error handling summary

| Situation | Result |
|---|---|
| Classifier off / not loaded / downloading / loading | `Unavailable` → rules + human as before |
| No GPU, backend open fails, model load fails | state `Unavailable(why)` / `Failed(why)`; retry via Settings |
| Missing schema file | built-in default schema |
| Invalid schema file | `Unavailable(why)`, no scoring |
| `decide` error, NaN, missing option | `Unavailable` for that call; counts toward the breaker |
| Busy (another call in flight) or timeout | no-model verdict (cached outcomes still apply while active) |
| 3 consecutive failures | stage disabled for the session; Settings shows why |

## Planning refinements and later rulings

Decided while planning:

- `Decider` drives the model directly through `shiso_core::Model` (`prefill_last_many`,
  `decision_readout`, `decision_mut`) on its own worker thread; it does not reuse `shiso-server`'s
  scheduler, which lives in a crate the library must not depend on.
- The library serves models that carry a decision readout (LFM2 / d1) or a decision head (Clef).
  Generic label scoring of plain chat models stays in `shiso-server`, because it needs the server's
  chat-template prompt renderer. The production SmolLM2 model must therefore ship with a readout or
  head (the user adds that family).
- The library's loader covers those families (LFM2 now); `shiso-server`'s `model_factory` keeps its
  own loading unchanged.

Later rulings:

- The `NotInstalled` classifier state was removed; the states are Off, Downloading, Loading,
  Ready(schema source), Unavailable, Failed.
- Errors caused by the input (a refused request such as an over-long command) do not count toward the
  three-failure breaker; only model or engine failures do.
- The schema's `question` name is not pinned to `decision`; the classifier looks up the answer by the
  name in the schema. The choice values must still be exactly `approve`, `ask`, `deny`.
- Backslashes, control characters and line separators in the cwd and command are escaped, so a command
  cannot add lines to the model input.

## Risks

- **d1-3b is large for this job** (3.2 GiB, ≥ 6 GiB RAM, Metal only, multi-hundred-ms first calls are
  likely). It is the test model; the 2000 ms timeout and the busy guard keep reviews moving.
- **The shiso-server refactor** touches the live `/v1/decisions` path; the unchanged wire tests are the
  guard.
- **d1-3b was not trained on this rubric**; its numbers in the evaluation describe d1-3b, not the
  production model. The promotion decision waits for the production model.
- **SmolLM2-135M needs a shiso family** (Llama architecture + a decision head or readout) before it can
  be used; tracked separately by the user.
