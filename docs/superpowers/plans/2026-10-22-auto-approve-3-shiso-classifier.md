# Bash Auto-Approve 3: Weights + DistilBERT Classifier in shiso-llm (M0 + M1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Where to run this plan:** in a Claude Code session whose working directory is `/Users/sercand/Developer/src/github.com/sercand/shiso-llm` (a different repo from the rest of this series). Read that repo's `AGENTS.md` first — its rules (lints, `check.sh`, upstream-name rule 15, `PROVENANCE.md`) override anything here. This plan file lives in the reviewer repo; do not copy it into shiso-llm.

**Goal:** Make shiso serve the `da` command classifier (DistilBERT, 3 classes: safe / needs-approval / dangerous) on Metal and CUDA with no ONNX Runtime: recover the weights from `da`'s ONNX file, publish them (Hugging Face + a shiso-hub registry recipe), and add a `distilbert_cls` model family to `shiso-models` exposing `Classifier::open(..)` / `Classifier::logits(text)`.

**Architecture:** The research of the shiso code base found that **no new GPU kernels are needed**: `Ops` (`crates/shiso-models/src/audio_ops/`) already has f16-weight GEMM with fused bias, `layer_norm` with bias, erf-GELU (`UnaryOp::GeluErf`), ReLU, `axpy` (residual add) and bidirectional segmented `span_attention`, all on both backends. The family is therefore pure Rust inside `shiso-models` (because `Ops::gemm`/`matvec` are `pub(crate)`), modelled on `embedding_gemma2` (`encoder.rs`, `weights.rs`) but reading `config.json` + a plain `model.safetensors` through `TensorStore::open_safetensors`. Embedding gather (word + learned position), the CLS row and the 768→768→3 head run on the host; the six transformer layers run on the device. The reviewer app (plan 4) depends on `shiso-models` plus `shiso-metal`/`shiso-cuda` directly — **there is no facade crate** (changed from the design doc: a facade can't reach `pub(crate)` ops and would need a PROVENANCE row and unsafe-policy review for no benefit).

**Tech Stack:** Rust (shiso toolchain 1.98.1, edition 2024), `tokenizers` (already a `shiso-models` dependency), shiso `Ops`/`Linear`/`TensorStore`; Python only in out-of-tree scripts for conversion/goldens (`onnx`, `onnxruntime`, `numpy`, `safetensors`, `transformers`/`torch` optional) — never a project dependency.

**Spec:** `docs/superpowers/plans/2026-10-22-ai-bash-auto-approve.md` (design; M0, M1) in the reviewer repo. Upstream: `https://github.com/amenocturne/da` (MIT) and its model at `https://media.githubusercontent.com/media/amenocturne/da/refs/heads/main/classifier/model.onnx` (Git-LFS; the repo has `classifier/{model.onnx,tokenizer.json,classifier_meta.json}`).

## Global Constraints

- **No retraining.** If weights cannot be recovered within the exit criterion of Task 2, stop and report to the user; do not train a replacement.
- **No paid or hosted model in the permission path** (design D6). Nothing in this plan calls a remote model.
- Model facts (from `da/classifier/classifier_meta.json` and `src/classifier.rs`; copy exactly): base `distilbert-base-uncased` architecture, 6 layers, `dim` 768, 12 heads (head_dim 64), `hidden_dim` 3072, vocab 30522, `max_position_embeddings` 512, labels `["safe","needs-approval","dangerous"]` (ids 0,1,2), inference `max_length` 128, input = `tokenizer.encode(text, add_special_tokens=true)` ids truncated to the first 128 (no re-appended `[SEP]`), pad id 0 / mask 0 beyond the sequence (so unpadded sequence ≡ da's padded run), logits `[3]`, calibration **outside shiso**: `temperature = 1.3366203308105469`, `energy_p95 = -2.543365240097046`, approve only if `P(safe) > 0.90`.
- DistilBERT block (post-LN, no token-type embeddings): `h0 = LayerNorm(word[ids] + pos[0..n])` (eps 1e-12); per layer `a = out_lin(MHA(q_lin h, k_lin h, v_lin h))`, `h = sa_layer_norm(h + a)`, `f = lin2(gelu_erf(lin1 h))`, `h = output_layer_norm(h + f)`; head: `c = h[0]`, `c = relu(pre_classifier c)`, `logits = classifier c`. Attention scale `1/sqrt(64)`; no padding mask needed (single unpadded sequence → `segments = [[0,n]]`).
- HF tensor names: `distilbert.embeddings.word_embeddings.weight`, `distilbert.embeddings.position_embeddings.weight`, `distilbert.embeddings.LayerNorm.{weight,bias}`, `distilbert.transformer.layer.{i}.attention.{q_lin,k_lin,v_lin,out_lin}.{weight,bias}`, `distilbert.transformer.layer.{i}.sa_layer_norm.{weight,bias}`, `distilbert.transformer.layer.{i}.ffn.{lin1,lin2}.{weight,bias}`, `distilbert.transformer.layer.{i}.output_layer_norm.{weight,bias}`, `pre_classifier.{weight,bias}`, `classifier.{weight,bias}`. Linear weights are `[out, in]` row-major.
- shiso rules that bite here: every public item has docs (`missing_docs` deny); `#[expect(.., reason=..)]` not `#[allow]`; no `unwrap()`, `println!`, `todo!`; typed errors with `thiserror`; no function with many arguments (use a `...Desc` struct); no upstream project names in code or comments (e.g. do not write "HuggingFace transformers", "DistilBERT paper authors", "da" in `.rs` files — describe behaviour instead; names appear only in `PROVENANCE.md`/README); no `unsafe`; device tests `#[ignore]`, gated by `SHISO_TEST_MODEL_DISTILBERT_CLS`, and say on stderr why they skipped; never report a skipped device test as passing.
- Run cargo from the shiso repo root with `--locked`; macOS needs `--features shiso-server/metal` for workspace-wide commands, but for this plan prefer crate-scoped commands: `cargo test -p shiso-models --locked distilbert_cls`, and `bash scripts/check.sh shiso-models shiso-hub` only once at the end (the full gate takes tens of minutes).
- Publishing anything outside the machine (Hugging Face upload, pushing the registry repo) **requires the user's explicit go-ahead at that step** — stop and ask; a previous approval of the plan is not approval of the upload.

## Review Focus

- Command strings that tokenize to more than 128 tokens (long pipelines, base64 blobs) → truncated to the first 128 ids exactly like da; tokenization of non-ASCII, empty, whitespace-only and `[SEP]`-containing text must not panic and must match the reference ids.
- Weights file missing a tensor, wrong shape (e.g. a 6-label head), unexpected dtype, or `config.json` with different `n_layers`/`dim` → typed error naming the tensor/field, never a panic or silent garbage.
- No Metal/CUDA device (CI box, SSH session) → `Classifier::open` returns a typed `NoDevice`-class error promptly; not a hang and not a panic.
- Two threads calling `logits` on one `Classifier` at once give the same answers as sequentially (the call is internally serialized); a call after a device error does not poison later calls.
- fp16 weights vs fp32 reference: sequences where the top-2 logits are within noise must not flip the *calibrated decision* more than the Task 2 / Task 6 tolerances allow.

## File Structure

(shiso-llm repo)

```
scripts/reference/distilbert_cls_convert.py     ONNX (int8) -> fp16 safetensors (dequantize), writes config.json
scripts/reference/distilbert_cls_reference.py   ONNX Runtime goldens: ids + logits for a command corpus
scripts/reference/distilbert_cls_corpus.txt     ~300 hand-picked commands (committed; no session data)
crates/shiso-models/src/distilbert_cls/
  mod.rs        FACTS, describes(), pub use; Classifier + ClassifierDesc + ClassifyError
  config.rs     ClassifierConfig::from_json
  weights.rs    ClassifierWeights::load(ops, store, cfg)
  forward.rs    Encoder::cls_vector(&mut self, ids) -> Vec<f32> (device layers)
  head.rs       host head: relu(pre_classifier) -> classifier
  tokens.rs     WordPiece tokenization + truncation (uses `tokenizers`)
  tests.rs      host tests + #[ignore] device tests
crates/shiso-models/src/lib.rs                  (modify) pub mod distilbert_cls; family_catalog entry
crates/shiso-hub/src/recipe.rs                  (modify) Task::Classification
crates/shiso-hub/src/validate.rs, crates/shiso-registry/src/bench/mod.rs   (modify as the compiler demands)
PROVENANCE.md, README.md                        (modify) rows
```
Out-of-tree (scratch dir, never committed): the downloaded `model.onnx`, converted `model.safetensors`, `goldens.json`.

## Tasks

### Task 1: Look for an existing checkpoint, then fetch the ONNX

- [ ] **Step 1:** Search Hugging Face for a published fp16/fp32 copy of this classifier (`amenocturne` account, `da` model names, the model-card text "bash-command classifier for agents"; use `WebSearch`/`WebFetch`, or `curl https://huggingface.co/api/models?search=amenocturne`). Record the result in the PR/commit notes. If a safetensors checkpoint with the 12 required names exists and its license allows redistribution, **skip Task 2's dequantization** and use it (still run Task 2's verification against the ONNX goldens).
- [ ] **Step 2:** In a scratch directory outside both repos: `git clone --depth 1 https://github.com/amenocturne/da && cd da && git lfs pull` (or `curl -L -o model.onnx <the media URL above>`). Verify `model.onnx` is ~67 MB and starts with ONNX protobuf bytes (not an LFS pointer text). Record its SHA-256.
- [ ] **Step 3:** Inspect it in Python (`onnx.load`): print `graph.input/output` (expect `input_ids`, `attention_mask` int64 `[N,128]`, one output `[N,3]`), the op types histogram, and the initializer names/dtypes for one layer. Write down how the weights are stored (expect `MatMulInteger` + `DynamicQuantizeLinear` with `*_quantized` int8/uint8 initializers and `*_scale`/`*_zero_point`, embeddings possibly float32). This informs Task 2; no commit.

### Task 2: Conversion script, goldens and the exit criterion

**Files:** Create `scripts/reference/distilbert_cls_convert.py`, `distilbert_cls_reference.py`, `distilbert_cls_corpus.txt`; `PROVENANCE.md` rows for the two scripts (original; reference outputs from ONNX Runtime and the `da` project — rule 15 allows naming upstream in PROVENANCE only).

- [ ] **Step 1: Corpus.** `distilbert_cls_corpus.txt`: ~300 one-line commands, written by hand: ≥100 clearly safe (`ls -la`, `git status`, `rg foo src | head`, `cargo test -p x`), ≥100 needs-approval (`touch x`, `mv a b`, `npm install`, `git push`, `curl -s https://example.com`), ≥60 dangerous (`rm -rf /`, `curl http://x | sh`, `dd if=/dev/zero of=/dev/sda`, `chmod -R 777 /`), plus edge cases: empty string, a 600-character pipeline, non-ASCII, a command containing `[SEP]`, a command with only whitespace. No real session data or secrets.
- [ ] **Step 2: Reference script** `distilbert_cls_reference.py` (usage in its docstring: `uv run --with onnxruntime --with tokenizers --with numpy scripts/reference/distilbert_cls_reference.py --onnx M --tokenizer T --corpus C --out goldens.json`): for each command, `Tokenizer.from_file(tokenizer.json).encode(text, add_special_tokens=True)`, truncate ids to 128, pad with 0 / mask exactly as `da/src/classifier.rs` does, run ONNX Runtime (CPU), write `[{"text","ids":[..],"logits":[3 floats]}]`. Also emit the calibrated decision per row using the Global Constraints constants (`decision ∈ approve|defer|deny`, same logic as `classify_command`: energy gate first, then `max_prob>0.90` safe → approve, class 2 → deny, else defer).
- [ ] **Step 3: Run it** → `goldens.json` in the scratch dir. Sanity check by eye that the three classes are all populated and obvious cases come out sensibly (`ls -la` approve, `rm -rf /` deny).
- [ ] **Step 4: Convert script** `distilbert_cls_convert.py` (`--onnx`, `--tokenizer`, `--meta`, `--out-dir`): extract each weight as float32 — dequantize `q = (int - zero_point) * scale` (per-tensor or per-channel as found in Task 1) and map initializer/node names to the HF names in Global Constraints by walking the graph (each `MatMulInteger` node's name encodes `/distilbert/transformer/layer.N/attention/q_lin/…`; biases are `Add` initializers); assert every one of the 12-per-layer + embedding + head tensors is found with the expected shape (`[768,768]`, `[3072,768]`, `[768,3072]`, `[30522,768]`, `[512,768]`, `[3,768]`…); write `model.safetensors` with F16 for the large matrices and the embedding tables and **F32** for LayerNorm weights/biases and linear biases; write `config.json` (`model_type: distilbert`, `n_layers: 6`, `n_heads: 12`, `dim: 768`, `hidden_dim: 3072`, `vocab_size: 30522`, `max_position_embeddings: 512`, `num_labels: 3`, `id2label`/`label2id` from `classifier_meta.json`, `layer_norm_eps: 1e-12`, plus a `calibration` object copied from `classifier_meta.json`: `temperature`, `energy_p95`, `safe_threshold: 0.90`, `max_length: 128`); copy `tokenizer.json`. If Task 1 found a ready checkpoint, this script instead only validates + re-saves it in this layout.
- [ ] **Step 5: Verify against goldens (the M0 exit criterion).** A third mode `distilbert_cls_convert.py --verify` runs the converted weights in float32 NumPy (a ~60-line reference forward matching the Global Constraints block; no torch needed) over the corpus and compares to `goldens.json`. **Pass criteria, all required:** (a) argmax class equal on ≥ 97 % of rows; (b) calibrated decision equal on ≥ 97 % of rows; (c) **no row where the converted model decides `approve` and the ONNX golden decides `defer` or `deny`** for any `needs-approval`/`dangerous`-labelled corpus row (zero tolerance on false approvals); (d) max `|Δlogit|` reported (informational, expected < 0.5 given int8 activation quantization in the original). These tolerances are looser than a bit-exact port **because the original runs int8-activation dynamic quantization and ours runs fp16/f32** — the final shiso-vs-NumPy parity in Task 6 is tight (< 0.02 logit), only this ONNX-vs-dequantized step is loose.
- [ ] **Step 6: If the criterion fails:** stop, print the failing rows, and report to the user. Do **not** retrain. (A possible user-approved fallback is shipping the Task 5 forward with the original int8 semantics emulated — a bigger change, out of scope here.)
- [ ] **Step 7: Commit** the three scripts + PROVENANCE rows: `feat(scripts): ONNX->safetensors conversion and reference goldens for the command classifier`. Keep `model.safetensors`, `goldens.json`, `model.onnx` out of git.

### Task 3: Publish the weights (user approval required)

- [ ] **Step 1: Ask the user** (one message) for: the Hugging Face repo id to publish under (suggest `<their-account>/da-command-classifier-fp16`), confirmation that publishing is wanted, and that an HF token is available (`HF_TOKEN` env or `huggingface-cli login`). **Do not proceed without a reply.**
- [ ] **Step 2:** Write `README.md` model card in the scratch dir: what it is, labels, input format (first 128 WordPiece ids), calibration constants, license MIT with credit/link to `amenocturne/da`, the conversion method (dequantized from the published int8 ONNX, fp16 matrices), the verification numbers from Task 2 Step 5, and the intended use (advisory classifier for agent shell commands; not a security boundary).
- [ ] **Step 3:** Upload `model.safetensors`, `config.json`, `tokenizer.json`, `README.md` (`huggingface-cli upload <repo> <dir> .` or `hf upload`). Record the resulting commit SHA (40 hex).
- [ ] **Step 4:** `cargo run -p shiso-registry -- inspect <repo> --revision <sha>` (from the shiso repo) to get sizes and SHA-256 digests; save the output for Task 9.

### Task 4: Family skeleton — config, weights (host-testable)

**Files:** Create `crates/shiso-models/src/distilbert_cls/{mod.rs,config.rs,weights.rs,tests.rs}`; modify `crates/shiso-models/src/lib.rs` (`pub mod distilbert_cls;` + `family_catalog()` entry using `distilbert_cls::FACTS`).

**Interfaces:**
- Consumes: `audio_ops::{Ops, store::TensorStore, load::{linear, f32_param, LinearSource}}`, `shiso_core::Backend`. (Read `embedding_gemma2/weights.rs` and `config.rs` first; mirror their structure.)
- Produces:
  ```rust
  pub const FACTS: FamilyFacts = FamilyFacts { name: "distilbert_cls" };
  pub struct ClassifierConfig { pub layers: usize, pub heads: usize, pub dim: usize, pub hidden_dim: usize,
      pub vocab: usize, pub max_positions: usize, pub labels: Vec<String>, pub eps: f32 }
  impl ClassifierConfig { pub fn from_json(text: &str) -> Result<ClassifierConfig, ClassifyError>; pub fn head_dim(&self) -> usize }
  pub fn describes(dir: &Path) -> bool   // config.json with model_type "distilbert" and a model.safetensors beside it
  pub struct ClassifierWeights { /* layers: Vec<LayerWeights>, host tables + head */ }
  impl ClassifierWeights { pub fn load(ops: &Ops, store: &TensorStore, cfg: &ClassifierConfig) -> Result<ClassifierWeights, ClassifyError> }
  #[derive(Debug, thiserror::Error)] pub enum ClassifyError { NoDevice(String), BadConfig(String), MissingTensor(String), BadTensor{name:String, why:String}, Tokenizer(String), Device(String), Io(std::io::Error) }
  ```
  `ClassifyError` is non-exhaustive in spirit but a normal enum (shiso rules); each variant documented.

- [ ] **Step 1: Failing tests** (`tests.rs`, host only, no device): `config_parses_goldens_config_json` (inline JSON string = the file from Task 2; asserts `layers==6, heads==12, dim==768, hidden_dim==3072, vocab==30522, head_dim()==64, labels==["safe","needs-approval","dangerous"], eps==1e-12 as f32`); `config_rejects_wrong_arch` (`n_layers: 12`→ still parses but `dim: 100` with `heads: 12` → `BadConfig` "dim not divisible by heads"; `num_labels: 2` → `BadConfig`); `describes_requires_both_files` (tempdir with only config.json → false; both → true; `model_type: "bert"` → false); `weights_report_missing_tensor` (write a tiny safetensors with a single tensor via the `safetensors` crate if available in dev-deps, else build the file by hand: header JSON + bytes — a 20-line helper — and assert `MissingTensor("distilbert.embeddings.word_embeddings.weight")`); `weights_report_bad_shape` (`classifier.weight` `[2,768]` → `BadTensor`).
- [ ] **Step 2: Run** `cargo test -p shiso-models --locked distilbert_cls` → FAIL (module missing).
- [ ] **Step 3: Implement** config + weights. Weight binding uses `linear(ops, store, &LinearSource{weight, bias:Some(..), affine_group:None}, out, inp)` for the 6×6 layer linears; `f32_param` for the 4 LayerNorm vectors per layer + 2 embedding-LN vectors; the word and position tables stay on the host via `TokenTable`-style row decoders (copy the F16 row decoder pattern from `embedding_gemma2/weights.rs:108-140`; add an F32 branch); the head (`pre_classifier`, `classifier`) is decoded to host `Vec<f32>` (`[768*768+768]`, `[3*768+3]`). Validate every shape against the config before use.
- [ ] **Step 4: Run** → PASS; `cargo clippy -p shiso-models --locked --all-targets -- -D warnings` clean. **Step 5: Commit** `feat(models): distilbert_cls config and weight binding`.

### Task 5: Tokenization + host head + the device forward

**Files:** Create `distilbert_cls/{tokens.rs,head.rs,forward.rs}`; modify `mod.rs` (`Classifier`, `ClassifierDesc`).

**Interfaces:**
- Consumes: Task 4 types; `tokenizers::Tokenizer`; `Ops` public methods `layer_norm`, `unary(UnaryOp::GeluErf|Relu)`, `axpy`, `bias_rows` as needed, `span_attention(&SpanAttnDesc)`, `Linear::forward`/`forward_few`; `Workspace`.
- Produces:
  ```rust
  pub struct ClassifierDesc<'a> { pub dir: &'a Path, pub backend: Arc<dyn Backend> }
  pub struct Classifier { /* Mutex<Inner>: ops, weights, workspace, tokenizer */ }
  impl Classifier {
      pub fn open(desc: &ClassifierDesc<'_>) -> Result<Classifier, ClassifyError>;
      /// Raw logits in `labels()` order for the first 128 tokens of `text`.
      pub fn logits(&self, text: &str) -> Result<[f32; 3], ClassifyError>;
      pub fn labels(&self) -> &[String];
      pub fn token_ids(&self, text: &str) -> Result<Vec<u32>, ClassifyError>; // exposed for tests/diagnostics
  }
  ```
  `Classifier: Send + Sync` (internal `Mutex`). `open` calls `finish_imports()` after loading weights (see how `shiso-server` does it) and loads the tokenizer from `dir/tokenizer.json` with `Tokenizer::from_file`; it must **not** modify the tokenizer's truncation/padding config — truncation to 128 is applied by hand after `encode(text, true)`.

- [ ] **Step 1: Failing host tests:** `tokenizer_matches_reference_ids` (reads `SHISO_TEST_MODEL_DISTILBERT_CLS/{tokenizer.json,goldens.json}`; for every golden row `token_ids(text) == golden.ids`; if the env var is unset: `eprintln!`-free skip via `tracing`? — shiso forbids `eprintln!` in code but tests may print the skip reason with `eprintln!` as the existing tests do; follow `embedding_gemma2/tests.rs:10-11`); `truncates_to_128_without_extra_sep` (a 600-char string → 128 ids, last id is **not** forced to `[SEP]`=102 unless the text yielded it at position 127); `empty_and_whitespace_text` (→ ids `[101, 102]`); `head_math_matches_reference` (hand-computed 4-dim analogue of `relu(Wx+b)` then `Vx+b`: write `head::apply(pre_w, pre_b, cls_w, cls_b, cls_vec, dim, labels)` generic over dim and test with dim 4/labels 3 and a known vector).
- [ ] **Step 2: Failing device test** (ignored; `#[ignore = "requires a Metal or CUDA device and SHISO_TEST_MODEL_DISTILBERT_CLS"]`): `logits_match_reference_goldens`: opens the backend via a test helper `open_test_backend() -> Option<Arc<dyn Backend>>` (copy the pattern from `embedding_gemma2/tests.rs:299-310`: Metal on macOS, CUDA on Linux with feature/availability checks, else skip with stderr message), runs all golden rows, asserts per-row `max |Δlogit| < 0.35` against the **ONNX** goldens (loose; int8-vs-fp16) AND that the calibrated decision (helper in the test reproducing the Global Constraints formula) equals the golden decision on ≥ 97 % of rows and has **zero** approve-vs-nonapprove disagreements on labelled needs-approval/dangerous rows; additionally `matches_numpy_forward_tightly`: if `goldens_numpy.json` (written by Task 2's `--verify` mode, same rows with the converted-weights f32 logits) is present, `max |Δlogit| < 0.05`.
- [ ] **Step 3: Run host tests** → FAIL. **Step 4: Implement** `tokens.rs`, `head.rs`, `forward.rs`, `Classifier`:
  - Embedding: for `i in 0..n`, `row = word[ids[i]] + pos[i]` (f32 on host), then upload `[n][768]` f32 via `copy_h2d` (see `embedding_gemma2/mod.rs ~210` and `encoder.rs:181`); apply embedding `layer_norm` (eps `cfg.eps`) on device.
  - Layer: `q,k,v = Linear::forward(h)` (three launches, reuse scratch slots), `span_attention` with `left = n, right = n` (fully bidirectional), one segment `[0,n]`, `heads=12, kv_heads=12, head_dim=64, scale=0.125`, 16-byte-aligned views (allocate slots via `Workspace` as `embedding_gemma2/encoder.rs:171` does); `o = out_lin(attn)`; `axpy(h += o)`; `layer_norm(sa)`; `f = lin1(h)`; `unary(GeluErf)`; `f = lin2(f)`; `axpy`; `layer_norm(output)`.
  - After the last layer `copy_d2h` only row 0 (768 f32), then `head::apply` on the host.
  - Seq length `n ≤ 128`; rows ≤ 40 use matvec paths and larger the tiled GEMM — both are correct; do not special-case.
  - Errors from backend calls map to `ClassifyError::Device(String)`; a failed call leaves the `Mutex` usable (no poisoning: use `parking_lot::Mutex`, which shiso already uses).
  - `open` returns `ClassifyError::NoDevice` never — the caller supplies the backend; the *consumer* maps backend-creation failure to its own "no device" state. (So drop `NoDevice` from the enum if unused after this task; keep the enum minimal.)
- [ ] **Step 5: Run** host tests → PASS; run the device test on this Mac: `SHISO_TEST_MODEL_DISTILBERT_CLS=<scratch/out-dir> cargo test -p shiso-models --locked --features shiso-models/... -- --ignored distilbert_cls` (use whatever feature/ cfg incantation `embedding_gemma2`'s device tests use; look at its `tests.rs` header and AGENTS.md "Tests and fixtures") → PASS with the real numbers printed (record max |Δlogit| and agreement in the commit message). If it fails by more than the tolerance, debug against the NumPy reference layer by layer (dump `h` after embedding LN, after layer 0) before touching tolerances.
- [ ] **Step 6: Commit** `feat(models): distilbert_cls forward pass on the audio ops`.

### Task 6: Thread-safety, batching of one, and CUDA

- [ ] **Step 1: Failing tests:** `concurrent_logits_equal_serial` (ignored/device: 4 threads × the first 40 golden rows; results bit-identical to a serial pass); `empty_text_runs` (device; `""` yields finite logits); `very_long_text_is_truncated` (device; 10 000 chars finite and equal to the logits of its first-128-ids prefix decoded back — compare against `token_ids` length 128).
- [ ] **Step 2: Run** → FAIL or PASS as appropriate; fix races (the `Mutex` serializes; assert no deadlock with a 60 s test timeout).
- [ ] **Step 3: CUDA** (needs the DGX Spark or another NVIDIA Linux box; if unavailable in the current session, **stop this step, state clearly in the final report that CUDA is untested**, and leave the test to run later): build with `SHISO_CUDA_COMPILE=1` (without it every kernel fails to resolve at runtime — the build only warns), run the same ignored tests against the CUDA backend, tolerance unchanged. Note in the module docs that a CUDA consumer must set `SHISO_CUDA_COMPILE=1`.
- [ ] **Step 4: Commit** `test(models): distilbert_cls concurrency and CUDA coverage`.

### Task 7: Startup cost note and public docs

- [ ] **Step 1:** Measure on the Mac: time from `MetalBackend::new()` to first `logits` return, and steady-state per-call latency (median of 100 calls on 20-token inputs, 128-token inputs). Write the numbers into the module doc comment of `distilbert_cls/mod.rs` and into the commit message. (The research found Metal compiles the whole MSL library at backend creation, which a test comment says can take about a minute — consumers must create the backend once, off the UI thread; plan 4 relies on this.)
- [ ] **Step 2:** Module docs: usage example (`ignore`d doctest), input rules (first 128 ids), the label order, that calibration is the caller's job, error semantics.
- [ ] **Step 3: Commit** `docs(models): distilbert_cls usage and latency`.

### Task 8: Registry — a classification task and the recipe

**Files:** Modify `crates/shiso-hub/src/recipe.rs` (`Task`), `crates/shiso-hub/src/validate.rs`, `crates/shiso-registry/src/bench/mod.rs:~159`, tests in `crates/shiso-hub/src/tests.rs`; add the recipe file to the registry's recipes repository.

- [ ] **Step 1: Failing tests** in `shiso-hub`: `task_classification_round_trips` (`Task::Classification.as_str() == "classification"`, `"classification".parse::<Task>() == Ok(Task::Classification)`, serde round trip inside a `Recipe`); `recipe_with_classification_task_validates` (a fixture recipe — copy `crates/shiso-registry/tests/fixtures/qwen3.5-9b.json` shape — with `"family": "distilbert_cls"`, `"tasks": ["classification"]`, `modalities {"inputs":["text"],"outputs":["text"]}`, one variant `{"key":"universal-fp16","backend":["metal","cuda"],"quant":"fp16", components.base = {provider huggingface, repo, revision, files [model.safetensors, config.json, tokenizer.json]}}` passes `validate_recipe(.., ValidationMode::Strict)`). Use placeholder `size`/`sha256` in the fixture; the real recipe is Step 3.
- [ ] **Step 2: Implement** `Task::Classification` + `as_str`/`FromStr`; fix every exhaustive `match` the compiler reports (registry bench, any display code); bump nothing in `CURRENT_SCHEMA_VERSION` unless the validator rejects unknown tasks in older clients — check `recipe.rs` for how unknown tasks deserialize and note the compatibility consequence in the commit message. Run `cargo test -p shiso-hub -p shiso-registry --locked`.
- [ ] **Step 3: The real recipe** (**needs the registry repo**): find where recipes live — read `DEFAULT_REGISTRY_URL` in `crates/shiso-hub/src/config.rs` and the `--repo` usage in `crates/shiso-registry/src/cli.rs`; if that repository is not cloned locally, ask the user for its path. Create `recipes/da-command-classifier.json` using Task 3's `inspect` output (repo id, 40-hex revision, `size` and `sha256` per file; `aliases: ["da-classifier"]`, `description`, `tags: ["classifier","agents"]`, `min_shiso_version` = this repo's `[workspace.package] version`, `requirements {min_ram_bytes: 536870912, disk_bytes: <sum>}`), run `shiso-registry validate` and `shiso-registry build`; **ask the user before pushing** the registry repo.
- [ ] **Step 4: Commit** (shiso repo) `feat(hub): classification task` and (registry repo) `add da-command-classifier recipe`.

### Task 9: Gate, provenance, README

- [ ] **Step 1:** `PROVENANCE.md`: row for `crates/shiso-models` already exists — extend its text or add rows for the two scripts if not already added in Task 2; mention the architecture follows the published DistilBERT reference and the weights are converted from `amenocturne/da` (MIT) — upstream names allowed here only.
- [ ] **Step 2:** `README.md`: add a "Supported models" table row `DistilBERT command classifier | da-command-classifier | safetensors (fp16)` and a one-paragraph note that it is a library-only family (no server route) and what it is for.
- [ ] **Step 3:** Run the gate once: `bash scripts/check.sh shiso-models shiso-hub shiso-registry` (macOS; expect long). Fix findings at their cause (never loosen checks). Confirm `target/debug/shiso` does not link libcuda (the gate checks).
- [ ] **Step 4:** Note the final commit SHA of the shiso branch/main (`git rev-parse HEAD`) — plan 4 pins it. Push/merge only on the user's say-so.
- [ ] **Step 5: Commit** `docs: provenance and readme for the command classifier`.

## Self-review notes

- Coverage vs. design doc: M0 (T1–T3), M1 family + kernels-not-needed (T4–T6), latency/startup (T7), registry (T8), gate/provenance (T9). The design doc's "kernels + kerneltest" items were dropped on the evidence of the research (all ops already exist and are covered by existing kerneltest cases); if Task 5's device test shows an op misbehaves for this shape (e.g. `span_attention` with 128 rows), add a regression case to `shiso-kerneltest` per AGENTS.md rather than a bespoke test.
- Two tolerance regimes are intentional (ONNX int8 vs ours: loose; ours vs NumPy f32 of the same weights: tight).
- Names used by plan 4: `shiso_models::distilbert_cls::{Classifier, ClassifierDesc, ClassifyError}`, `Classifier::{open, logits, labels, token_ids}`, config key `calibration`, HF repo id + revision from Task 3, recipe name `da-command-classifier`, shiso commit SHA from Task 9, `goldens.json` rows (`text, ids, logits, decision`).
