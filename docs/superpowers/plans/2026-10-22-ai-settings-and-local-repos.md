# AI Settings Tab, Per-PR Repo Folder, Local Repos List — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move provider/model setup into a Settings "AI" tab with header-row tables and edit dialogs, manage the repo↔folder link from the PR's AI tab and a new "Local repos" tab on the PR list (accordion rows), and stop treating the folder as global config.

**Architecture:** Storage stays `settings.ai.repos` / `providers` / `models` (no migration). Pure helpers (remote parsing, link/unlink, start pre-checks, presets, accordion reducer) are unit-tested; widgets return action enums that `app.rs` applies; all git/keychain/GitHub work runs on the worker as new `Command`/`Event` pairs. Everything is `#[cfg(unix)]` like the rest of the AI code.

**Tech Stack:** Rust, egui/eframe 0.37, tokio worker (`src/runtime.rs`), `egui_kittest` shots, `wiremock` contract tests.

**Spec:** `docs/superpowers/specs/2026-10-22-ai-settings-and-local-repos-design.md`. Approved mockups: `docs/design/mockups/ai-settings/`.

## Global Constraints

- Follow `AGENTS.md`: the UI thread does no I/O (git, keychain, directory listing, GitHub); widgets return action enums; decision logic is pure and unit-tested; PR-scoped events carry `SessionId`.
- Non-PR-scoped requests (folder probe, repo PR searches) carry their own `u64` token/generation and replies with an older one are dropped.
- Keys live only in the OS keychain (`llm:<provider>`); never in `settings.toml`, session files, logs or events that are persisted.
- Unlink clears only `RepoAiSettings.path`; commands, prepare and extra paths are kept. Repos with an empty `path` are not listed in Local repos.
- Repo keys are `host/owner/name`, stored lowercase, matched case-insensitively.
- At most 4 repo PR searches in flight at once.
- New code is `#[cfg(unix)]`; Windows build and clippy must still pass.
- CI must stay green: `cargo fmt --check`, `cargo clippy --workspace --all-targets --features ai-testing -- -D warnings`, `cargo test --workspace --features ai-testing`.
- Develop on branch `ai-review/ui` directly (no worktrees). Run every cargo command from the repo root.

## Review Focus

- A folder whose `origin` is missing, ambiguous (several remotes, none named `origin`), on another host, or unparseable: Add/Change says why and links nothing.
- Linking `Acme/Widgets` when `acme/widgets` is already linked: one entry, no duplicate row.
- Provider rename where the copy of the key fails, or the settings write fails afterwards: old key and old settings are intact.
- A linked repo whose folder was deleted: still listed (red "folder missing"), its PRs still load, Start is blocked with the reason.
- A slow repo-PR reply arriving after the user switched tabs or refreshed: dropped, no stale counts.

## File Structure

- `src/ai/checkout.rs` (modify): add `read_origin_repo`.
- `src/ai/repo_link.rs` (create): pure link/unlink/list/start-check helpers over `AiSettings`.
- `src/ai/presets.rs` (create): model presets.
- `src/runtime.rs` (modify): new commands/events.
- `src/ui/widgets/settings_ai_tab.rs` (create): providers/models tables and dialogs; `settings_window.rs` (modify): tab bar, remove old AI section and Repositories list.
- `src/ui/widgets/ai_panel.rs`, `ai_dialogs.rs`, `ai_repo_dialog.rs` (create): Local checkout row and repo commands dialog.
- `src/state/local_repos.rs` (create): Local repos reducer; `src/ui/widgets/local_repos.rs` (create): table + accordion.
- `src/ui/screens/pr_list.rs`, `src/state/pr_list.rs`, `src/persist/settings.rs`, `src/app.rs`, `src/ui/shots.rs` (modify).

---

### Task 1: Origin parsing and repo-link helpers

**Files:**
- Modify: `src/ai/checkout.rs` (next to `find_remote`, reusing `split_remote_url`)
- Create: `src/ai/repo_link.rs`; register in `src/ai/mod.rs`
- Modify: `src/ui/widgets/ai_dialogs.rs` (`save_folder` moves into `repo_link`; keep callers compiling)

**Interfaces:**
- Produces: `pub struct RemoteRepo { pub host: String, pub owner: String, pub name: String }` (lowercase) with `fn key(&self) -> String` (`host/owner/name`);
  `pub fn read_origin_repo(folder: &Path) -> Result<RemoteRepo, CheckoutError>` (new variants `NoRemote`, `AmbiguousRemote(Vec<String>)`, `UnparseableRemote(String)`);
  in `repo_link.rs`: `pub fn link_repo(ai: &mut AiSettings, repo: &RemoteRepo, path: PathBuf)`, `pub fn unlink_repo(ai: &mut AiSettings, key: &str)`, `pub fn linked_repos<'a>(ai: &'a AiSettings, host: &str) -> Vec<&'a RepoAiSettings>` (non-empty `path`, matching host, sorted by key), `pub fn folder_status(path: &Path) -> FolderStatus` (`Ok | Missing | NotARepo`).

- [ ] **Step 1: Write failing tests** in `checkout.rs` tests using temp git repos: `origin_https_ssh_scp_and_dot_git_forms_parse` (all four URL forms → `github.com/acme/widgets`), `no_origin_single_remote_is_used`, `no_origin_two_remotes_is_ambiguous`, `no_remote_errors`, `garbage_url_is_unparseable`. In `repo_link.rs`: `link_is_case_insensitive_and_deduplicates` (link `Acme/Widgets` after `acme/widgets` → one entry), `unlink_keeps_commands_and_prepare`, `linked_repos_skips_empty_path_and_other_hosts`, `folder_status_missing_and_not_a_repo`.
- [ ] **Step 2:** Run `cargo test --lib ai::checkout::tests::origin ai::repo_link` → FAIL (not defined).
- [ ] **Step 3:** Implement the signatures above. `read_origin_repo`: prefer remote `origin`; else the only remote; use `split_remote_url` for parsing. `link_repo` updates an existing entry's `path` in place (keeping commands), else pushes a new entry.
- [ ] **Step 4:** Run the same tests → PASS; `cargo clippy --workspace --all-targets --features ai-testing -- -D warnings` clean.
- [ ] **Step 5:** Commit `feat(ai): origin parsing and repo link helpers`.

### Task 2: Worker folder probe

**Files:** Modify `src/runtime.rs`; test in its `#[cfg(test)]` module or `tests/`.

**Interfaces:**
- Consumes: `read_origin_repo`, `folder_status` (Task 1).
- Produces: `Command::AiProbeFolder { token: u64, path: PathBuf }` → `Event::AiFolderProbed { token: u64, path: PathBuf, result: Result<RemoteRepo, String> }` (string is the user-facing reason). Runs in `spawn_blocking`.

- [ ] **Step 1:** Write failing test `probe_folder_reports_remote_and_reason`: a temp repo with origin → `Ok(key)`; a plain dir → `Err` containing "not a git repository" or the `CheckoutError` text.
- [ ] **Step 2:** Run it → FAIL. **Step 3:** Implement the handler. **Step 4:** PASS. **Step 5:** Commit `feat(runtime): folder probe command`.

### Task 3: Provider save/rename/delete worker ops

**Files:** Modify `src/runtime.rs`, `src/auth/store.rs` (add `copy_llm_key`/reuse `set_llm_key`, `clear_llm_key`, `llm_key`).

**Interfaces:**
- Produces: `Command::AiKeyOp { token: u64, op: KeyOp }`, `enum KeyOp { Store { name: String, key: String }, Move { from: String, to: String, key: Option<String> }, Remove { name: String }, Undo { name: String } }`; `Event::AiKeyOpDone { token: u64, result: Result<(), String> }`. `Move` copies the stored (or supplied `key`) to `llm:<to>` and does **not** delete `from`; the app sends `Remove{from}` only after the settings write succeeded, and `Undo{to}` (delete the new entry) if it failed.
- Modify the test-connection command that `SettingsAction::TestProvider` maps to (see `apply_settings_actions`, `app.rs` ~979) to take `key: Option<String>`: a typed key is used when present, else the keychain.

- [ ] **Step 1:** Failing tests using the in-memory/mock keychain used by `auth/store.rs` tests: `move_copies_without_deleting_the_old_key`, `move_with_supplied_key_stores_that_key`, `undo_removes_only_the_new_entry`, `test_connection_prefers_the_typed_key` (wiremock, like `tests/contract_llm_test.rs`).
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. **Step 5:** Commit `feat(ai): keychain key ops and typed-key connection test`.

### Task 4: Model presets and start pre-check

**Files:** Create `src/ai/presets.rs`; add `start_blockers` to `src/ai/repo_link.rs`.

**Interfaces:**
- Produces: `pub struct ModelPreset { pub api: LlmApi, pub id: &'static str, pub label: &'static str, pub context_window: u32, pub max_tokens: u32 }`, `pub fn presets(api: LlmApi) -> &'static [ModelPreset]` (the Claude 5 family and current OpenAI ids; builder picks exact sizes from the vendor docs), `pub fn start_blockers(ai: &AiSettings, repo_key: &str, has_key: &dyn Fn(&str) -> bool) -> Vec<String>`: reasons in order — no default model, provider missing, key missing, repo not linked, folder missing/not a repo.

- [ ] **Step 1:** Failing tests: `presets_are_per_api_and_have_nonzero_sizes`, `start_blockers_lists_each_missing_piece`, `start_blockers_empty_when_ready`.
- [ ] **Step 2–5:** FAIL → implement → PASS → commit `feat(ai): model presets and start pre-check`. (`has_key` is injected; the app fills it from a cached keychain probe, never inside a frame.)

### Task 5: Settings AI tab — providers

**Files:** Create `src/ui/widgets/settings_ai_tab.rs`; modify `settings_window.rs` (tab bar General | Appearance | AI, AI tab unix-only; delete `ai_providers`, `RepoDraft`, repos UI, Browse popup, and the AI part of `SettingsVm`/`apply`).

**Interfaces:**
- Consumes: `Command::AiKeyOp`/`Event::AiKeyOpDone` (Task 3).
- Produces: `SettingsAction::SaveProvider { original: Option<String>, provider: ProviderSettings, key: Option<String> }`, `SettingsAction::DeleteProvider { name: String }`, `SettingsAction::TestProvider { provider, model, key: Option<String> }`; pure `pub fn provider_delete_blocker(ai: &AiSettings, name: &str) -> Option<String>` (names the model using it); pure `pub fn validate_provider(existing: &[ProviderSettings], original: Option<&str>, candidate: &ProviderSettings) -> Result<(), String>` (non-empty, no edge spaces, unique).
- App flow in `app.rs`: `SaveProvider` → if renamed, `AiKeyOp::Move` → on `Ok` patch `settings.ai.providers` (+ models referencing the old name) and save → on save `Ok` send `Remove{from}`, on save error send `Undo{to}` and toast; non-renamed with a typed key sends `Store`.

- [ ] **Step 1:** Failing tests: `delete_blocked_when_a_model_uses_the_provider`, `validate_provider_rejects_duplicates_and_edge_spaces`, `rename_updates_model_references`, and a `shots.rs` shot `settings-ai-providers` plus `settings-ai-provider-dialog` (kittest, no panic, no missing glyphs).
- [ ] **Step 2:** FAIL. **Step 3:** Implement the table (header row Name | API | Base URL | Key | Status; row click opens the dialog; "+ Add provider"), the dialog (fields per spec, Delete/Test/Cancel/Save, test result line), and the app flow. Saving General settings no longer touches AI. **Step 4:** PASS + full clippy. **Step 5:** Commit `feat(ui): settings AI tab with providers table and dialog`.

### Task 6: Settings AI tab — models

**Files:** Modify `settings_ai_tab.rs`, `app.rs`.

**Interfaces:**
- Consumes: `presets` (Task 4). Produces: `SettingsAction::SaveModel { role: ModelRole, model: Option<ModelSettings> }` (`ModelRole::{Default, Lookup}`; `None` clears the role), pure `validate_model(ai: &AiSettings, model: &ModelSettings) -> Result<(), String>` (provider exists, model id non-empty, sizes in the existing ranges).

- [ ] **Step 1:** Failing tests: `validate_model_requires_an_existing_provider`, `choosing_a_preset_fills_sizes_and_keeps_custom_free_text`, shots `settings-ai-models`, `settings-ai-model-dialog`.
- [ ] **Step 2–5:** FAIL → implement table (Role | Provider | Model | Context | Max out | Reasoning; Lookup row may be empty) and dialog (provider dropdown, preset dropdown + free text, context, max output, reasoning segmented) → PASS → commit `feat(ui): settings AI models table and dialog`.

### Task 7: PR AI tab — Local checkout row and start gating

**Files:** Modify `src/ui/widgets/ai_panel.rs`, `src/ui/widgets/ai_dialogs.rs` (remove `AiDialog::PickFolder` and its first-run flow), `src/state/ai_review.rs`, `src/app.rs`; create `src/ui/widgets/ai_repo_dialog.rs`.

**Interfaces:**
- Consumes: `read_origin_repo`/probe (Tasks 1–2), `start_blockers` (4), `link_repo`/`unlink_repo`.
- Produces: `AiPanelAction::{ChooseFolder, UnlinkFolder, EditRepoCommands}`; `pub struct LocalCheckoutVm { pub repo: String, pub path: Option<PathBuf>, pub status: FolderStatus, pub error: Option<String>, pub blockers: Vec<String>, pub change_blocked: Option<String> }`; pure `pub fn change_blocked(ai_state: &AiReviewState, unrestored: &[CheckoutRecord], repo_key: &str) -> Option<String>` (running run or unrestored checkout for that repo).
- The repo commands dialog edits `commands`, `prepare`, `extra_write_paths`, `extra_deny_read` of the repo's entry (one per line text areas, buffers owned by the dialog state, not re-split per frame).
- The folder picker answer is stored with the repo key it was opened for (`folder_picker: Option<(PrKey, Receiver<..>)>`), a later answer for a different key is dropped.

- [ ] **Step 1:** Failing tests: `change_blocked_while_a_run_is_live_or_unrestored`, `start_disabled_with_reason_when_not_linked`, `a_late_picker_answer_for_another_repo_is_ignored`, shots `pr-ai-local-checkout` (linked, unlinked, missing folder).
- [ ] **Step 2–4:** FAIL → implement the row (Choose…/Change…/Unlink, clone check result, errors in the row, commands summary with Edit) and Start gating; remove the old PickFolder dialog and the toast path at `app.rs` ~1638 → PASS.
- [ ] **Step 5:** Commit `feat(ui): local checkout row in the PR AI tab`.

### Task 8: Local repos state and repo PR searches

**Files:** Create `src/state/local_repos.rs`; modify `src/runtime.rs`, `src/state/mod.rs`, `src/persist/settings.rs` (`Layout.pr_list_tab: ListTab`, default `All`, serde default so old files load).

**Interfaces:**
- Produces: `pub enum ListTab { All, Local }`; `pub struct LocalReposState { pub expanded: BTreeSet<String>, pub generation: u64, pub repos: BTreeMap<String, RepoPrs>, queue: VecDeque<String>, in_flight: usize }`; `RepoPrs { Loading, Loaded { total: u32, items: Vec<PrSummary> }, Failed(GhError) }`; `pub enum LocalReposMsg { Refresh(Vec<String>), Toggle(String), Loaded { generation: u64, repo: String, page: SearchPage }, Failed { generation: u64, repo: String, error: GhError } }`; `pub fn reduce(state: &mut LocalReposState, msg: LocalReposMsg) -> Vec<Command>` emitting `Command::SearchRepoPrs { generation: u64, repo: String, query: String }` (query `is:pr is:open repo:owner/name`, via `ScopeConstraint::Repo`), never more than 4 in flight; new `Event::RepoPrsLoaded { generation, repo, result }`.

- [ ] **Step 1:** Failing tests: `refresh_starts_at_most_four_searches`, `stale_generation_replies_are_dropped`, `toggle_expands_and_collapses_without_refetching_loaded_repos`, `failed_repo_does_not_block_others`, `old_settings_file_defaults_the_tab_to_all`, contract test `repo_pr_search_sends_the_repo_qualifier` (wiremock, in `tests/contract_*.rs` style).
- [ ] **Step 2–5:** FAIL → implement → PASS → commit `feat(state): local repos reducer and repo PR search`.

### Task 9: Local repos tab UI and wiring

**Files:** Create `src/ui/widgets/local_repos.rs`; modify `src/ui/screens/pr_list.rs`, `src/app.rs`, `src/ui/shots.rs`.

**Interfaces:**
- Consumes: Tasks 1, 2, 8; `pr_row::render_row`. Produces: `LocalReposAction::{Toggle(String), OpenPr { repo: String, number: u64 }, AddRepo, ChangeFolder(String), Unlink(String)}`; pure `pub fn rows(ai: &AiSettings, host: &str, state: &LocalReposState) -> Vec<LocalRepoRowVm>` (`repo`, `path`, `status`, `open_prs: Option<u32>`, `expanded`).
- `pr_list.rs` shows the `All PRs | Local repos` switch (persisted); Local repos table per the approved mockup `local-repos-accordion.html`: header (chevron | Repository | Folder | Open PRs | ⋯), accordion rows, red "folder missing", ⋯ menu (Change folder, Unlink — disabled with reason via `change_blocked`), "+ Add local repo…" (native picker → `AiProbeFolder` → on `Ok` `link_repo`, on `Err` toast the reason, already linked → toast "already linked"), empty state text. Opening the tab or the refresh action sends `LocalReposMsg::Refresh`.

- [ ] **Step 1:** Failing tests: `rows_list_only_linked_repos_on_this_host_sorted`, `add_repo_links_on_ok_and_reports_errors`, `add_existing_repo_changes_nothing`, shots `pr-list-local-repos` (collapsed, one expanded, empty, missing folder).
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5:** Commit `feat(ui): local repos tab with accordion`.

### Task 10: Cleanup, docs, full verification

**Files:** Modify `AGENTS.md` (architecture lines on settings/repo link; add `--features ai-testing` to CI commands), `README.md` (AI setup section), `.github/REVIEW.md` only if a rule changed.

- [ ] **Step 1:** Remove dead code (old `RepoDraft`, hand-rolled Browse popup, `AiDialog::PickFolder` leftovers); grep for `Settings → AI` toast strings and update their wording.
- [ ] **Step 2:** Run `cargo fmt --check`, `cargo clippy --workspace --all-targets --features ai-testing -- -D warnings`, `cargo test --workspace --features ai-testing`, `cargo test --lib shots -- --ignored` (GPU). Expected: all pass, no missing-glyph failures.
- [ ] **Step 3:** Smoke test manually: add provider (rename it, confirm the key follows), add model, link a repo from a PR and from the Local repos tab, expand a row, unlink while idle, try Change during a run (blocked).
- [ ] **Step 4:** Commit `docs: AI settings and local repos`.
