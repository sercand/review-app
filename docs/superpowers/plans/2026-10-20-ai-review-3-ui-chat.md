# AI Review — Plan 3: UI, Chat Agent and Re-review — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the engine in front of the user: worker wiring with the stale-event rule, the checkout/restore/recovery dialogs, the ✦ AI tab with one session-like timeline, inline AI drafts with Keep/Edit/Drop/Why?, tree markers, the btw-style chat agent with steering tools, re-review from the UI, and the starter `.github/REVIEW.md`.

**Architecture:** The worker owns an `AiHub`: AI runs keyed by `PrKey`, each with its `PipelineControl`, a `UiInteraction` (implements Plan 2's `Interaction` with oneshot channels answered by UI commands), and the `SessionId` it currently reports to. A run outlives the PR screen. Reopening a PR re-attaches (`AiAttach`) and replays state from SQLite. UI state lives in `state/ai_review.rs` (a pure reducer over `Event`s). Widgets return `AiPanelAction`s that `app.rs` applies.

**Tech Stack:** egui 0.37, tokio, Plans 1–2 modules, `rfd` (native folder picker, unix only).

**Spec:** `docs/superpowers/specs/2026-10-20-ai-review-design.md` (spec §N). **Depends on:** Plans 1 and 2 merged.

## Global Constraints

- Everything from Plans 1–2's Global Constraints.
- **Stale-event rule:** every AI `Command`/`Event` that concerns a PR carries the `SessionId`; the UI drops events whose session isn't the open one. A run keeps going in the worker when the PR is left; only Finish, Cancel or app quit stop it (spec §4 step 7).
- Nothing reaches GitHub until the user submits the review themselves. **Keep** only moves a draft into the local `PendingReview` (or the review body for summary items).
- Widgets never mutate app state; decision logic lives in pure functions with unit tests (repo convention).
- The AI UI is `#[cfg(unix)]`. On Windows the app looks exactly as before. `DetailsTab::Ai` exists on all platforms (it's serialized in `Layout`), but it's never shown on Windows and loads as `Overview` there.
- Colors come from the theme palette (`crate::ui::theme::palette_of`); add one `ai` accent per theme, purple-ish as in the chosen mockup (option A), and don't hard-code colors in widgets.

## Review Focus

1. **The user quits the app mid-run.** Expect the pipeline to be cancelled, the restore dialog shown before exit (or, if the window is force-closed, crash recovery on the next launch), and the repo never left stashed silently. Test in Task 2 (recovery) and Task 8 (quit flow).
2. **Events from a run arrive after the user opened a different PR.** Expect them to be dropped by the UI but persisted, and to show when the first PR is reopened. Test in Task 1.
3. **The user answers a question after the run was cancelled.** Expect a no-op, with no panic from a dropped oneshot. Test in Task 1.
4. **Keep on a draft whose line vanished after the diff reloaded** (head moved). Expect Keep disabled with the pinned-commit banner, not a pending comment on a wrong line. Test in Task 5.
5. **A huge PR (≥ 50k rows) with many AI drafts.** Expect the diff to stay virtualized: AI rows are spliced like pending rows, and `tests/perf_large_pr.rs` still passes. Test in Task 5.

---

### Task 1: `AiHub` in the worker, commands and events

**Files:**
- Create: `src/ai/hub.rs`, `src/ai/ui_interaction.rs`
- Modify: `src/runtime.rs` (`ExecCtx.ai: AiHub` under cfg unix; `Command`/`Event` variants; dispatch)

**Interfaces:**
- Consumes: `start_pipeline`, `PipelineControl`, `PipelineEvent`, `Interaction` (Plan 2); `Store`, checkout functions (Plan 1).
- Produces:
  - `Command` (cfg unix), all carrying `session: SessionId, key: PrKey`:
    - `AiAttach`
    - `AiPlanCheckout { pr: PrTarget }`
    - `AiStart { pr: PrTarget, snapshot_inputs: SnapshotInputs, mode: RunMode }`, where `SnapshotInputs { files: Vec<LoadedFile>, metadata: PrMetadata }`
    - `AiAnswer { question_id: u64, answer: String }`
    - `AiApprove { approval_id: u64, decision: ApprovalDecision }`
    - `AiApprovePrepare { approval_id: u64, ok: bool }`
    - `AiUserMessage { text: String }`
    - `AiPause`, `AiResume`, `AiCancel`
    - `AiFinish { discard: Option<bool> }`
    - `AiFindingState { id: i64, state: FindingState, pending_local_id: Option<u64> }`
  - `Event` (cfg unix), all carrying `session: SessionId`:
    - `AiSnapshot(AiSnapshotData)`: full state on attach: findings, coverage, timeline entries from `ai_messages`, brief, run status, checkout record
    - `AiCheckoutPlanned(CheckoutPlan)`
    - `AiCheckoutFailed(String)`
    - `AiPrepareOutput(Vec<PrepareOutput>)`
    - `AiPipeline(PipelineEvent)`
    - `AiQuestion { id, agent: AgentRef, text, options }`
    - `AiApprovalRequest { id, agent, kind: ApprovalKind /* Command{command,reason} | Prepare{reason, commands} */ }`
    - `AiChat(ChatEvent)` (Task 7)
    - `AiRestoreNeedsDecision(Vec<String>)`
    - `AiRestored(RestoreOutcome)`
    - `AiUnrestored(Vec<CheckoutRecord>)`: not PR-scoped; sent once at startup
  - `AiHub`:
    - `runs: Mutex<HashMap<PrKey, RunHandle>>`
    - `RunHandle { control: Option<PipelineControl>, session: Arc<Mutex<SessionId>>, interaction: Arc<UiInteraction>, checkout: CheckoutRecord, chat: Option<ChatHandle> }`
    - A forwarding task maps `PipelineEvent` → `Event::AiPipeline` using the run's *current* `session` (rebound by `AiAttach`).
  - `UiInteraction` implements `Interaction`: each call allocates an id, stores a `oneshot::Sender`, emits `AiQuestion`/`AiApprovalRequest`, and awaits. A dropped sender (cancel) resolves to the empty string / `Deny{None}` / `false`. Answers for unknown ids are ignored.
- `AiStart`, in order:
  1. `execute_checkout` (blocking pool).
  2. `run_prepare` if configured, emitting `AiPrepareOutput`. A failure stops here and restores.
  3. Build the sandbox: `agent_env` + `sandboxed_launch`. `SandboxUnavailable` → `bash_disabled = Some(reason)`.
  4. `gather_snapshot`.
  5. `start_pipeline`.
  6. Forward events.

- [ ] **Step 1: Failing tests** (worker-level, with `ScriptedLlm`, `MemoryStore`, in-memory `Store`, a temp git fixture repo)

```rust
#[tokio::test] async fn start_checks_out_runs_and_forwards_events_with_the_session()
#[tokio::test] async fn attach_rebinds_the_session_and_sends_a_snapshot()
#[tokio::test] async fn answer_after_cancel_is_ignored()
#[tokio::test] async fn sandbox_unavailable_disables_bash_and_says_why()   // force via a test hook returning SandboxUnavailable::NoBwrap
#[tokio::test] async fn finish_restores_and_reports_needs_decision_when_dirty()
```

- [ ] **Step 2–4:** fail → implement → `cargo test --lib ai::hub` PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): worker hub, commands and events for AI runs"`

---

### Task 2: AI review state reducer

**Files:**
- Create: `src/state/ai_review.rs`
- Modify: `src/state/mod.rs`, `src/state/pr_detail.rs` (`PrSession.ai: AiReviewState`, cfg unix)

**Interfaces:**
- Produces:
  - `pub enum TimelineEntry`:
    - `Stage { stage: StageKind, status: StageStatus }`
    - `Agent { agent: AgentRef, group: Option<String>, tools: Vec<(String, String)>, text: String, stop: Option<String> }`
    - `Question { id, agent, text, options, answer: Option<String> }`
    - `Approval { id, agent, kind: ApprovalKind, decided: Option<String> }`
    - `Finding { id }`
    - `ChatUser { text }`
    - `ChatReply { text }`
    - `Notice { text }`: fallbacks, bash disabled, budget
  - `pub struct AiReviewState { pub phase: AiPhase, pub timeline: Vec<TimelineEntry>, pub findings: BTreeMap<i64, Finding>, pub coverage: Vec<CoverageEntry>, pub draft: Option<ReviewDraft>, pub usage: (u64, u64), pub checkout: Option<CheckoutRecord>, pub plan: Option<CheckoutPlan>, pub restore_decision: Option<Vec<String>>, pub chat_input: String, pub last_reviewed_sha: Option<String> }`
  - `pub enum AiPhase { Idle, Planning, CheckingOut, Running, Paused, Done, Restoring, Finished, Failed(String) }`
  - `impl AiReviewState { pub fn apply(&mut self, ev: &AiEventRef) ; pub fn stage_strip(&self) -> Vec<(StageKind, StageStatus, String /* "2/3" */)>; pub fn counts(&self) -> FindingCounts; pub fn drafts_for(&self, path: &str) -> impl Iterator<Item=&Finding>; pub fn can_continue(&self, head_sha: &str) -> bool }`
  - The `AgentText` deltas of the same agent coalesce into its `Agent` entry. `ToolCall` appends to `tools`. Entries stay in arrival order.
  - `FindingCounts { by_severity: [u32; 4], dropped_by_verifier: u32, skipped_checks: u32, prior: [u32; 3] /* addressed, still, outdated */ }`
  - In `app.rs`: `Event::AiUnrestored` at startup opens the recovery dialog (Task 4).

- [ ] **Step 1: Failing tests**: `text_deltas_coalesce_per_agent`; `question_then_answer_marks_entry`; `counts_track_states`; `can_continue_only_when_head_moved_and_not_running`; `snapshot_replaces_state`.
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): AI review state and timeline reducer"`

---

### Task 3: The ✦ AI tab

**Files:**
- Create: `src/ui/widgets/ai_panel.rs`
- Modify: `src/ui/widgets/metadata_panel.rs` (`DetailsTab::Ai`, its tab, counts), `src/ui/theme.rs` (`ai` palette color), `src/ui/screens/pr_detail.rs`, `src/app.rs`, `src/ui/shots.rs`

**Interfaces:**
- Consumes: `AiReviewState` (Task 2).
- Produces:
  - `pub enum AiPanelAction`:
    - `Start`, `Continue`
    - `Answer { id, answer }`
    - `Approve { id, decision: ApprovalDecision }`, `ApprovePrepare { id, ok }`
    - `SendChat(String)`
    - `Pause`, `Resume`, `Cancel`, `Finish`
    - `JumpToFinding(i64)`
    - `ShowDropped(bool)`
  - `pub fn render(ui: &mut egui::Ui, st: &AiReviewState, show_dropped: bool) -> Vec<AiPanelAction>`
  - Pure helpers (tested): `pub fn stage_label(stage, status, progress) -> String`, `pub fn entry_title(e: &TimelineEntry) -> String` (e.g. `"Reviewer · notifications"`), `pub fn approval_buttons(kind) -> &'static [&'static str]` (`["Once", "For this review", "No"]` for commands, `["Run", "No"]` for prepare), `pub fn chat_enabled(phase) -> bool` (`Running | Paused | Done`)
- Layout (spec §12, option A):
  - stage strip
  - the timeline (collapsible agent entries showing their tool calls; question cards with option buttons and a free-text field; approval cards)
  - findings summary (severity counts; "Dropped by verifier (n)" toggle; "Skipped checks (n)"; on re-review, the prior states)
  - usage, the chat input, then Pause/Resume · Finish · Cancel
- `app.rs` maps actions to `Command`s with the open session's `SessionId`. `Start` → `AiPlanCheckout` (or, with no repo folder configured, opens the folder dialog from Task 4 first). `SendChat` → `AiUserMessage`.

- [ ] **Step 1: Failing tests** for the pure helpers; `ai_tab_hidden_on_windows_loads_as_overview` (`#[cfg(not(unix))]`, deserialize `details_tab = "ai"` → `Overview`).
- [ ] **Step 2–4:** fail → implement → PASS. Add shots `pr-ai-running` and `pr-ai-done` (light/dark) using a fixture `AiReviewState`.
- [ ] **Step 5: Commit** `git commit -am "feat(ui): AI tab with session timeline, questions and approvals"`

---

### Task 4: Top-bar button, checkout, restore and recovery dialogs

**Files:**
- Create: `src/ui/widgets/ai_dialogs.rs`
- Modify: `src/ui/widgets/top_bar.rs`, `src/app.rs`, `Cargo.toml` (`[target.'cfg(unix)'.dependencies] rfd = "<latest>"`)

**Interfaces:**
- Produces:
  - Top bar: **✦ AI review**, shown when unix and `settings.ai.models.default` is set. The label is **Continue review** when `st.can_continue(head)`. It's disabled while `Running`/`Paused`.
  - `pub enum AiDialog { PickFolder { key: PrKey }, ConfirmCheckout(CheckoutPlan), Restore { lines: Vec<String> }, Recovery(Vec<CheckoutRecord>), Held { pr_number: u64 } }`
  - `pub enum AiDialogAction { FolderChosen(PathBuf), ConfirmCheckout, CancelCheckout, RestoreDiscard, RestoreKeep, RecoverRestore(i64), RecoverLater }`
  - `pub fn folder_error(path: &Path, key: &PrKey) -> Option<String>`: pure wrapper over `find_remote`. Returns the message shown when the folder's remotes don't match.
- The confirm dialog shows: current ref → PR `head_branch @ short sha`, a "your uncommitted changes will be stashed and restored afterwards" line when dirty, the exact `plan.commands`, and the repo's prepare commands.
- `FolderChosen` saves `RepoAiSettings { repo: "host/owner/name", path, .. }` into settings. Suggesting the commands (spec §4 step 1) is the scout's job: the first scout's brief carries `suggested_commands`/`suggested_prepare` (Plan 2 Task 11), shown as a Notice with an "Add to settings" action.
- `RestoreKeep` → stop: leave the checkout as is, show a Notice explaining the state, and mark the record `Failed`.

- [ ] **Step 1: Failing tests**: `folder_error_for_non_repo_and_wrong_remote`; `top_bar_label_and_enabled_table` (pure fn `ai_button(st, head, configured) -> Option<(&str, bool)>`).
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ui): AI review button, checkout, restore and recovery dialogs"`

---

### Task 5: Inline AI drafts and tree markers

**Files:**
- Modify: `src/diff/row_model.rs` (`RowRef::AiDraft { finding: i64 }`), `src/diff/annotate.rs` (`place_ai_draft`), `src/ui/widgets/diff_view.rs`, `src/ui/widgets/file_tree.rs` (`TreeRowVm.ai_drafts: u32`), `src/state/pr_detail.rs`, `src/app.rs`

**Interfaces:**
- Produces:
  - `pub fn place_ai_draft(f: &Finding, files: &[LoadedFile]) -> Option<InlineItem>`: `FindingAnchor::Line(a)` → `Placement::AfterLine { side: a.side, line: a.line }` when the row exists. Otherwise `EndOfFile`. `Summary` anchors aren't placed inline.
  - Drafts are spliced with the same `annotate` pass as pending comments. Only `Draft`-state findings are shown, and `DroppedByVerifier` only when "show dropped" is on.
  - `pub enum DiffAction` additions: `AiKeep(i64)`, `AiEdit(i64, String)`, `AiDrop(i64)`, `AiWhy(i64)`
  - `pub fn keep_draft(session: &mut PrSession, id: i64) -> Result<KeepResult, String>` (pure over session state):
    - `Line` anchor → `PendingReview::add(anchor, body_with_severity_prefix, diff_head_sha)`. `PinnedToOtherCommit` → `Err("Submit or discard your pending review (pinned to <sha>) before keeping drafts from the new commit.")`.
    - `Summary` → append `"\n\n**<path>:** <title> — <body>"` to the review body.
    - The result carries `pending_local_id` for `AiFindingState { state: Kept }`.
  - Body format: `**<Severity>:** <body>` (e.g. `**Bug:** …`) **[call]**
  - Drop → `AiFindingState { DroppedByUser }`. Edit → updates the body locally and in the store. Why? → `SendChat("Why did you flag #<id> (<path>:<line>)?")` and switches to the AI tab.
  - The tree shows ✦ with `ai_drafts` beside the existing comment/pending badges.

- [ ] **Step 1: Failing tests**

```rust
#[test] fn ai_drafts_are_placed_after_their_line_and_summary_items_are_not()
#[test] fn keep_line_draft_adds_pending_with_severity_prefix()
#[test] fn keep_is_blocked_while_pending_is_pinned_to_another_commit()
#[test] fn keep_summary_item_appends_to_review_body()
#[test] fn tree_counts_open_ai_drafts()
```

and extend `tests/perf_large_pr.rs` with 500 AI drafts spread over the files (budget unchanged).

- [ ] **Step 2–4:** fail → implement → `cargo test --workspace` and `cargo test --release -- --ignored` PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ui): inline AI drafts with Keep/Edit/Drop/Why and tree markers"`

---

### Task 6: Pre-filled review and "posted" inference

**Files:**
- Modify: `src/ui/widgets/review_panel.rs`, `src/state/pr_detail.rs`, `src/app.rs`

**Interfaces:**
- Produces:
  - When `st.draft` becomes `Some` and the pending review body is empty, the body is set to `draft.summary` and the event to `map_verdict(draft.verdict)`: `Approve → ReviewEvent::Approve`, `Comment → Comment`, `RequestChanges → RequestChanges`. Never overwrite a body the user already typed. The popover shows `"✦ suggested: <verdict> — <rationale>"`.
  - On a successful review submit, every finding in state `Kept` whose `pending_local_id` was in the submitted draft → `AiFindingState { Posted }`.
- [ ] **Steps 1–4:** tests `prefill_only_when_body_empty`, `submit_marks_kept_findings_posted` → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): pre-filled review from the draft; mark posted findings"`

---

### Task 7: The chat agent (btw-style, full session)

**Files:**
- Create: `src/ai/chat.rs`, `src/ai/tools/steer.rs`
- Modify: `src/ai/hub.rs`, `src/ai/prompts.rs` (+ golden `chat.txt`)

**Interfaces:**
- Consumes: `PipelineControl` (Plan 2 Task 15), `run_agent`, reviewer read-only tools, `record_finding`.
- Produces:
  - `pub struct ChatHandle { tx: mpsc::UnboundedSender<String> }`. On the first `AiUserMessage` for a run, the hub spawns `run_chat(...)`, a long-lived task. Later messages go to `tx`. One message is one agent turn sequence (until the model stops calling tools), then it waits for the next message. History persists in `ai_messages` under the chat agent id, so after a restart a new chat task reloads it.
  - `pub enum ChatEvent { User(String), Text(String), ToolCall { name, summary }, TurnDone }` → `Event::AiChat`
  - Status digest (the user-role preamble of each turn): `PipelineStatus` rendered compactly (stages, group states, counts, open questions with ids), plus the brief's intent. No transcripts.
  - Steering tools:
    - `pipeline_status {}`
    - `get_finding { id }`
    - `get_agent_log { agent_id, last_n? (default 20 messages) }`
    - `add_lead { group, text }`, `add_check { group, item }`
    - `note_agent { agent_id, text }`
    - `rerun_group { group }`, `skip_group { group }`
    - `pause_run {}`, `resume_run {}`, `cancel_run {}`
    - `answer_question { question_id, answer }`: routes into `UiInteraction`
    - plus `ask_lookup`, the read-only repo tools, `gh_*`, `load_skill`, and `record_finding`
    - After the run, the pipeline tools return `"the review has finished"` except `get_*`.
  - Typing doesn't pause the run (spec §6.5 call). The chat model is `models.default`.
- [ ] **Step 1: Failing tests** (ScriptedLlm): `first_message_spawns_chat_and_later_messages_reuse_it`; `chat_can_pause_and_note_a_running_reviewer`; `answer_question_resolves_the_pending_ask`; `chat_after_run_can_record_a_new_draft`; `chat_history_survives_restart`.
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): btw-style chat agent with pipeline steering tools"`

---

### Task 8: Lifecycle: finish, quit, repo lock, re-review

**Files:**
- Modify: `src/app.rs`, `src/ai/hub.rs`

**Interfaces:**
- Produces:
  - **Finish:**
    1. `AiFinish { discard: None }` → hub cancels the pipeline and chat → `restore_preview`.
    2. Non-empty → `AiRestoreNeedsDecision` → Restore dialog → `AiFinish { discard: Some(bool) }` → `restore` → `AiRestored`.
    3. `StashConflict` shows a Notice with the stash SHA and the exact command to apply it later.
  - **Quit:** `app.rs` intercepts the window close (`ViewportCommand::CancelClose` while any run holds a checkout) and runs the Finish flow for each held checkout, then closes. Force-quit is covered by the startup recovery (`AiUnrestored`).
  - **Repo lock:** `AiPlanCheckout` failing with `CheckoutError::Held` → `AiDialog::Held { pr_number }` with "Finish that review first".
  - **Re-review:** **Continue review** → `AiPlanCheckout` → confirm → `AiStart { mode: Incremental { since_sha: last_reviewed_sha } }`. If the checkout is still held by this PR from the previous run, the hub skips stash and re-fetches the new head instead. Same dialog, same commands minus the stash line.
- [ ] **Step 1: Failing tests** (hub level): `finish_with_clean_tree_restores_without_asking`; `held_repo_rejects_second_pr`; `continue_while_held_refetches_without_stash`; `quit_with_held_checkout_runs_finish` (pure fn `close_requires_finish(hub_state) -> bool`).
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): finish, quit and re-review lifecycle"`

---

### Task 9: Starter `.github/REVIEW.md`, docs, final checks

**Files:**
- Create: `.github/REVIEW.md`
- Modify: `README.md` (AI review section: setup, sandbox guarantees, private dependencies/prepare), `AGENTS.md` (the `src/ai/` and `crates/` architecture paragraph)

**Content of `.github/REVIEW.md`:** short context (what the app is, the UI ⇄ worker split), then `## Checks`:
- Widgets and screens return action enums and never mutate app state.
- PR-scoped `Command`s/`Event`s carry `SessionId`; events for a left PR are dropped.
- The GUI thread never does I/O.
- Tokens and API keys only in the OS keychain, never in settings, session files, SQLite or logs.
- GitHub reads may be retried; writes never are.
- `[src/ai/**] ` No tool may write to GitHub, merge, approve, or edit files.
- `[src/diff/**] ` Changes keep the diff virtualized; check `tests/perf_large_pr.rs`.
- New `pub` items have doc comments; tests use `src/test_support.rs` helpers instead of redefining them.

- [ ] **Step 1:** Write the files; add `context_parses_this_repos_review_md` to `src/ai/context.rs` tests (loads the real file via `env!("CARGO_MANIFEST_DIR")` → ≥ 8 checks, two scoped).
- [ ] **Step 2: Full verification** — `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo test --release -- --ignored` → PASS. Then a manual smoke test on macOS with a real provider: run an AI review on a small PR of this repo, keep one draft, submit nothing, Finish, and check `git status`/`git stash list` are as before.
- [ ] **Step 3: Commit** `git commit -am "docs: REVIEW.md for this repo and AI review docs"`
