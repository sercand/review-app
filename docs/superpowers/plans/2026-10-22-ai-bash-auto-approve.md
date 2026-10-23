# AI review: bash auto-approve (tree-sitter + allowlist + local DistilBERT classifier; no LLM, no paid API)

Date: 2026-10-22. Branch: `ai-review/ui`. Touches `crates/sandbox`, `src/ai/tools/bash.rs`,
`src/ai/interaction.rs`, settings, and (in a separate repo) `shiso-llm`.

## Goal

Today `review_sandbox::classify` (`crates/sandbox/src/approval.rs`) is a hand-rolled tokenizer plus a
27-word read-only list. Everything else — including every `cd` — parks on a human approval. Replace it
with a layered decision pipeline modelled on [amenocturne/da](https://github.com/amenocturne/da):

```
command
  │
  ▼
1. Parse (tree-sitter-bash)      unparseable / $(..) / heredoc / subshell ──────────────► Ask
  │ segments (a && b | c; d)
  ▼
2. Deny rules                    secret paths, known-dangerous argv ───────────────────► Deny
  │
  ▼
3. Rule policies (allowlist)     every segment vouched for ────────────────────────────► Run
  │ some segment unknown
  ▼
4. Local classifier (DistilBERT on shiso: CUDA / Metal)
  │   safe (P>0.90, in-distribution) ──► Run      dangerous ──► Deny     else ▼
  ▼
5. Human approval (existing dialog)    nobody watching (headless) ───────────────────────► Deny
```

**The sandbox stays the real boundary.** Stage 4 only decides whether to *interrupt the user*. They
never widen what the sandbox permits. Stages 1–3 are deterministic; 4 is advisory and fails closed
(error, timeout, model missing, no GPU → fall through to the next stage, never to `Run`).

## Implementation plans (execute in this order; each is self-contained for a fresh session)

| Plan | Covers | Repo / working dir | Depends on |
|------|--------|--------------------|------------|
| `2026-10-22-auto-approve-1-rules-engine.md` | M2: tree-sitter front end, da's engine + rule policies, deny paths, wire `bash` | reviewer | - |
| `2026-10-22-auto-approve-2-model-stage.md` | M3: classifier trait, never-approve table, cache, timeouts, settings | reviewer | plan 1 |
| `2026-10-22-auto-approve-3-shiso-classifier.md` | M0 + M1: weights recovery, HF publish, `distilbert_cls` family, registry recipe | **shiso-llm** | none (parallel to 1-2) |
| `2026-10-22-auto-approve-4-classifier-integration.md` | M4 + M5: link shiso, download, loader, UI, evaluation, promotion | reviewer | plans 2 and 3 |

**Changes from this document found while planning (the plans win):** shiso needs **no new kernels** (all ops exist on both
backends) and **no facade crate** (the family lives in `shiso-models`; the reviewer links `shiso-models`/`shiso-hub` and opens the
Metal/CUDA backend itself); the registry needs a new `Task::Classification`; the engine is `da`'s own (ported onto a tree-sitter front end
rather than re-written); a model "dangerous" verdict becomes an *ask with a warning*, not a hard deny; shiso's CUDA kernels only exist when
the consumer builds with `SHISO_CUDA_COMPILE=1`.

## Findings that shape the plan

1. **da's parser is hand-written (`src/shparse`, ~1.0k lines), not tree-sitter.** The request says
   tree-sitter, so we use `tree-sitter-bash` and port da's *policy* logic (`src/policies.rs`, 1.6k lines;
   `src/engine.rs`) onto its AST. da's `shparse` corpus tests (`corpus_tests.rs`) are reusable as
   behaviour fixtures: same inputs, same expected defer/approve.
2. **The classifier is DistilBERT** (66M params, 3 classes safe / needs-approval / dangerous, max_len 128,
   temperature 1.3366, energy OOD cutoff −2.5434, approve only if P(safe) > 0.90, test macro-F1 0.933). It
   ships as an **int8-quantized ONNX (67 MB, Git LFS)**. Shiso cannot read ONNX and we are not adding ONNX
   Runtime, so the weights have to come out of ONNX/training into safetensors (see M0).
3. **Shiso has no encoder-classifier family and no CPU backend.** Its closest relative is
   `embedding_gemma2` (bidirectional encoder, `embedder.rs` contract). `Backend` implementations are Metal
   and CUDA only. Consequences: (a) new family + head in `shiso-models`; (b) **machines without
   Metal/CUDA (CI, most Linux boxes) get no classifier** and skip stage 4 — this must be a first-class,
   tested path, not an error.
4. **The sandbox shares the network** (`profile.rs`: "Network stays shared"). da's own README warns the
   model approves `curl -s https://…` and `touch x` "by resemblance". Because there is no network
   boundary, a model `Run` verdict must **not** apply to segments that touch the network or interpreters
   (`curl`, `wget`, `nc`, `ssh`, `scp`, `python`, `node`, package managers, `sh -c` with unknown content…).
   A hard-coded "never model-approve" list sits in front of stage 4 and 5. PR content is
   attacker-influenced, so every classifier input is treated as adversarial.
5. **Shiso's rules** (its `AGENTS.md`): no family names in `shiso-core/engine`; `missing_docs` deny;
   kernel cases live in backend-neutral `shiso-kerneltest`; `PROVENANCE.md` row for new crates and copied
   code; "upstream names appear only in PROVENANCE.md/README". Work there goes through its own
   `scripts/check.sh`. `unsafe` stays confined to the existing crates.

## Decisions (my calls — change any of them)

| # | Call | Why |
|---|------|-----|
| D1 | Parser/rules live in a **new crate `crates/approval`** (`review-approval`, no `unsafe`, no I/O); `review-sandbox::approval` becomes a thin re-export then is deleted | `classify` is pure policy; keeps tree-sitter's C build out of the PTY crate; lets the model stage plug in via a trait |
| D2 | Verdict grows to `Run \| Ask{reason} \| Deny{reason}`; `Deny` is returned to the agent as a tool error without bothering the user | da has three outcomes; secrets-path denies shouldn't prompt |
| D3 | The classifier is a **trait** (`CommandClassifier`) in `review-approval`; the shiso-backed impl lives in `src/ai/` behind `#[cfg(unix)]` and a cargo feature (`local-classifier`, default on for macOS, opt-in on Linux) | the app builds and passes CI with no shiso, no GPU |
| D4 | Shiso is a **git dependency pinned by `rev`**, through a new thin facade crate `shiso-classify` in shiso-llm (so we don't depend on `shiso-models` internals) | user asked to link by repo URL; a facade keeps our surface to ~3 functions |
| D5 | Weights are **not committed** here. Converted fp16 safetensors (~133 MB) + tokenizer/meta are published to **Hugging Face** (our own repo, unless an equivalent already exists there — check first) and described by a **shiso-hub recipe** in the shiso registry. The app fetches them through `shiso-hub` (`RegistryClient` → `select_variant` → `Downloader` → `BlobStore`: resumable, SHA-256-verified, content-addressed). Settings → AI shows state/progress | 133 MB in git is bad; shiso already has the download/verify/store machinery, so we write none of it |
| D6 | **No LLM in the permission path.** No paid or hosted model, no calls to the user's provider; permissions are decided only by rules + the local classifier | your call: cost, privacy (command text never leaves the machine), and no prompt-injection surface |
| D7 | Headless / nobody-watching mode (`NoInteraction`) already denies unanswered approvals; keep that. | matches "deny if nobody's watching" |
| D8 | Training/export scripts stay in `da`'s repo (MIT). We copy only runtime code + tokenizer + metadata; license text added to `PROVENANCE`/`NOTICE` | avoid carrying Python tooling |

## Milestones

### M0 — Weights and a ground truth (blocker for everything model-related)

- Clone `da`, `git lfs pull`, and in a throwaway Python venv (**not** in either repo) run the ONNX model
  over a set of ~2k commands (da's `classifier/scripts/extract-corpus.py` output if present, else
  da's corpus tests + our own list). Save `(command, logits[3])` as goldens in
  `shiso-llm/test-vectors/references/` — an independent implementation, as shiso's rules require.
- Get fp32 weights. **No retraining** (your call). Order: (1) search Hugging Face for an existing
  published checkpoint of this classifier (author's or a community conversion); if one exists, use it.
  (2) Otherwise dequantize the int8 ONNX (`MatMulInteger`/`DynamicQuantizeLinear` → fp32) — reproduces
  exactly what `da` runs, so goldens match tightly. If (2) drifts beyond the exit criterion below, stop
  and report rather than retrain.
- Write `model.safetensors` (fp16) + `tokenizer.json` + `classifier_meta.json` and **publish to
  Hugging Face** when (1) found nothing. Credit da's author and MIT license in the model card.
- Exit criterion: a script comparing fp16 safetensors forward (PyTorch) vs ONNX goldens: max |Δlogit| < 0.05
  and identical argmax on ≥99.5 % of the set.

### M1 — Shiso: DistilBERT sequence-classifier family (repo: shiso-llm)

Own branch + PR in shiso-llm, gated by its `scripts/check.sh`.

1. `crates/shiso-models/src/distilbert_cls/` — `config.rs` (dim 768, 6 layers, 12 heads, ffn 3072,
   vocab 30522, 3 labels), `weights.rs` (tensor roles from the HF names: `distilbert.embeddings.*`,
   `transformer.layer.N.attention.{q,k,v,out}_lin`, `sa_layer_norm`, `ffn.lin1/2`, `output_layer_norm`,
   `pre_classifier`, `classifier`), `encoder.rs`. Contract mirrors `embedder.rs`: not a `Model`; one call
   `classify(token_ids) -> [f32; 3]` logits. Host-side: sequence packing, mask, truncate to 128.
2. Kernels, backend-neutral cases first in `shiso-kerneltest`, then thin wrappers per backend. Reuse what
   exists (`kernels/cuda/dense`, `generic`; `crates/shiso-metal/metal/dense`, `generic`; embedding_gemma2's
   bidirectional attention) and add only what is missing. Expected new kernels:
   - token+position embedding gather + LayerNorm (fused)
   - bidirectional masked softmax attention for seq ≤ 128 (padding mask, no KV cache, no causal mask)
   - GELU(erf) FFN epilogue; LayerNorm residual
   - classification head: CLS row → pre_classifier → ReLU → classifier (tiny GEMM; can run on the
     generic path)
   Lead with correctness: one fused-per-layer implementation is a later optimisation; 128 tokens × 6 layers
   is microseconds–low ms on either GPU.
3. Tests: logits vs the M0 goldens (tolerance fixed from M0's fp16 result); `#[ignore]` device tests per
   shiso convention, driven by `SHISO_TEST_MODEL_DISTILBERT_CLS`, saying on stderr when skipped.
4. **Registry recipe.** Add a recipe for this model with `shiso-registry` (task: classification, roles:
   weights/tokenizer/meta, file SHA-256s from M0, HF commit sha). The recipe schema's `Task`/`Role`
   enums may need a `Classify` variant; extend them in `shiso-hub` if so.
5. `crates/shiso-classify` facade (new workspace member + `PROVENANCE.md` row):
   ```rust
   pub struct Classifier;                       // owns Arc<dyn Backend>, model, tokenizer
   impl Classifier {
       pub fn open(dir: &Path) -> Result<Self, ClassifyError>;   // picks Metal or CUDA; Err(NoDevice) otherwise
       pub fn logits(&self, text: &str) -> Result<[f32; 3], ClassifyError>;
   }
   ```
   Tokenizer: shiso already tokenizes for its servers; reuse its WordPiece/BERT support if present,
   otherwise add the `tokenizers` crate to the facade only. Calibration (temperature, softmax, energy
   gate) stays on **our** side so thresholds can change without a shiso release.
5. Features: `metal` / `cuda`, mutually exclusive, exactly as shiso requires. No default device feature.
6. `PROVENANCE.md`: the family follows the `distilbert-base-uncased` reference architecture; da's weights
   cited per D8.

Exit: `cargo test -p shiso-models distilbert_cls` + device tests green on a Mac (Metal) and a CUDA box
(DGX Spark); gate passes; merged; note the commit SHA for M4.

### M2 — Reviewer: tree-sitter parser and rule policies (no ML yet)

New crate `crates/approval`. Independent of M0/M1, so it can start immediately and ship alone.

1. Deps: `tree-sitter`, `tree-sitter-bash` (exact versions, per the repo's pinning habit).
2. `parse.rs`: tree-sitter CST → `Script { segments: Vec<Segment> }`, where a `Segment` is one simple
   command (`argv: Vec<Word>`, `redirects`, `assignments`). Words carry `Literal(String)` vs `Dynamic`
   (contains `$VAR`, glob/brace expansion). **Bail to `Ask` on:** any `ERROR`/`MISSING` node, command or
   process substitution, heredoc/herestring, subshell/`( )`, `{ }` groups, `if/for/while/case`, functions,
   backgrounding `&`, and `[[ ]]` — da's list, and what the current tokenizer already does.
   Allowed structure: `;`, `&&`, `||`, `|`, and `!`.
3. `policy/`: port from da `policies.rs`, one module each, each returning `Vouch | Abstain`:
   `read_only` (argument-aware: `sort -o` ≠ approve, `sed` script charset, `find -delete/-exec`, `awk`
   `system`), `git` (caps read/add/commit/…; inert `-c` only), `cargo`, `cd`+assignments (the original
   complaint), `mkdir` under cwd, `--help`/`--version` (off by default), wrappers (`env`, `time`,
   `timeout`, `xargs`, `sh -c`, `find -exec`) vouch only if the inner command does, `repo` allowlist from
   `.github/REVIEW.md` (existing `allowed` argument, same prefix semantics as now).
   Output-redirect rule: only `/dev/null`, fds, and the existing `$TMPDIR` carve-out.
   Env overrides that change what runs (`PATH`, `LD_*`, `GIT_PAGER`, `GIT_EXTERNAL_DIFF`, …) → Ask.
4. `deny.rs`: path-component deny (`.env*`, `.ssh`, `.aws`, `.netrc`, keychains, `id_*`) applied to argv and
   redirect targets, **including inside wrappers**, before allow rules (da semantics). Configurable list in
   settings with these as defaults.
5. `Verdict` + `Decision { verdict, source: Rule(&'static str) | Model | Deny, reason }` so the UI can
   say *why* something auto-ran.
6. Tests: port da's `corpus_tests.rs` expectations; keep every existing `approval.rs` test passing (they
   are behavioural spec); add property-style tests that no argv containing `$(`/backtick/heredoc ever
   returns `Run`; a fuzz-ish test that arbitrary bytes never panic.
7. Switch `src/ai/tools/bash.rs::approve` to the new crate; `Deny` → tool error without a prompt; delete
   `review_sandbox::approval`.

Exit: `cargo fmt/clippy/test` green; manual run shows `cd`, `ls`, `git diff`, `cargo test`, `rg … | head`
no longer prompt.

### M3 — Reviewer: model stage plumbing (trait, gating, settings, fallbacks)

1. `CommandClassifier` trait in `review-approval`: `fn classify(&self, segment: &str) -> ModelVerdict`
   (`Safe | NeedsApproval | Dangerous | Unavailable`). Only **unvouched segments** are sent, each
   separately; the command runs only if every segment resolves to `Run`.
2. **Never-model-approve filter** (finding 4): if a segment's binary is in {network, interpreters,
   package managers, `sudo`, `rm`, `chmod`/`chown`, `dd`, …} or it has a dynamic word in command
   position, the model may *deny* but never *approve*. Table-tested.
3. Stage orchestration in `src/ai/` (not in the pure crate, because it awaits): classifier → human,
   with a classifier timeout (200 ms, configurable) and a single decision cache keyed on the normalized segment (so `ls` isn't re-judged 100×).
4. Settings (`settings.toml`, AI tab from the previous plan): `auto_approve` master switch (default on for
   rule policies, **off** for the classifier until M5 evals pass), `local_classifier` on/off + download
   state, deny-path list, timeout. `NoInteraction` unchanged.
5. UI: approval dialog already exists; add a quiet "auto-approved: <source>" line on the bash tool
   card, and a "Why was I asked?" reason already coming from `Ask.reason`. No new screens beyond the
   Settings fields.

### M4 — Reviewer: shiso-backed classifier

1. `Cargo.toml`: optional `shiso-classify = { git = "https://github.com/sercand/shiso-llm", rev = "<M1 sha>", optional = true }`,
   feature `local-classifier` with `shiso-classify/metal` on macOS and `/cuda` on Linux
   (target-specific feature wiring; verify cargo resolves mutually exclusive features cleanly —
   otherwise two feature names `local-classifier-metal` / `-cuda`). Run `cargo deny` against the new tree.
2. `src/ai/local_classifier.rs`: load on a blocking thread at first AI review start (never on the GUI
   thread), holds `Classifier`, applies temperature 1.3366 / energy gate −2.5434 / P(safe) > 0.90 exactly
   as da's `classifier.rs`, returns `Unavailable` on `NoDevice`/load failure/hash mismatch (logged once).
3. Weights manager: a small wrapper over `shiso-hub` (`HubConfig`, `RegistryClient`, `Downloader`,
   `BlobStore`) — resolve the recipe, download, verify, hand the blob dir to `Classifier::open`. Progress
   from `ProgressEvent` becomes an app-global `Event` (not PR-scoped, so no `SessionId`).
4. Tests: logic around thresholds against canned logits (no GPU); an `#[ignore]` device test comparing the
   first 200 goldens end to end; CI keeps building with the feature **off** on Linux/Windows, plus a
   macOS job with it on if we have a macOS runner (the repo already runs macOS CI).

### M5 — Evaluation and docs

1. Eval harness `tests/auto_approve_eval.rs` (+ `#[ignore]` device variant): a labelled set — benign review
   commands, plus adversarial ones (injection strings in args, `curl|sh`, `git -c core.pager=…`,
   encoded payloads, `cat .env`). Metrics gating promotion of the classifier to default-on:
   **false-approve rate on the dangerous/adversarial set = 0**, and prompt reduction on a benign set
   (target ≥ 80 % fewer prompts vs today). Numbers recorded in the PR.
2. Update `.github/REVIEW.md`, `AGENTS.md` (architecture paragraph: new crate, stages, fail-closed rule),
   and the AI-review spec §8.1.

## Ordering and parallelism

```
M0 ──► M1 ───────────────┐
M2 ──► M3 ───────────────┴──► M4 ──► M5
```
M2/M3 deliver the user-visible win (no more `cd` prompts) with no shiso dependency, so ship them first.
M0→M1 run in parallel in the other repo. M4 can't start until M1 has a pinned commit.

## Risks and open items

- **Weights provenance (M0).** The training data is real agent sessions; only the quantized ONNX is
  published. If dequantizing drifts too far from the goldens, M4+ stops there (no retraining) and the
  classifier stage is dropped; the rules (M2) still work. Also confirm da's
  license covers redistributing the weights (repo is MIT) and keep the NOTICE.
- **No GPU → no classifier.** Linux CI and many users. Acceptable by design (stage skipped), but the
  headline "small local classifier" is Mac/NVIDIA only. A tiny CPU path in the facade is possible later;
  out of scope.
- **Metal device in the app process.** egui/wgpu already holds a Metal device; a second, independent
  `MTLDevice` is fine but keep the model off the render queue. Verify no startup-time regression.
- **Model false approvals.** Mitigated structurally (never-approve table, per-segment, only after
  rules abstain, off by default until M5 eval is clean) — but it stays advisory, not a security
  control. Say so in the Settings help text.
- **Adversarial command text.** PR content can steer the agent's commands toward whatever the
  classifier misjudges. Worst case is a `Run` of a command still confined by the sandbox and the
  never-approve table; the eval set includes adversarial strings.
- **tree-sitter grammar gaps** (bash edge cases, zsh-only syntax): any `ERROR` node → `Ask`, so gaps
  degrade to today's behaviour, never to a silent `Run`.
