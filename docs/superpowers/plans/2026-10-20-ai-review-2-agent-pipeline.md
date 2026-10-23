# AI Review — Plan 2: Agent and Review Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A headless, fully tested review engine: tools, the agent loop, repo context, the checklist, and the scout → reviewers (+ lookups, challenger) → verifier → assembler pipeline, including incremental re-review, driven by the app's code and testable end to end with a scripted fake LLM.

**Architecture:** Everything lives in `src/ai/` (`#[cfg(unix)]`). Agents are `run_agent` calls with an `AgentSpec` (prompt, tools, budget, finish check). Tools get a shared `ToolCx` with the sandbox, a `PrSnapshot` of the PR gathered once up front (no network inside tools), the run's `RunState`, and an `Interaction` trait the UI implements in Plan 3. `pipeline.rs` sequences the stages and exposes a `PipelineControl` handle (pause, notes, leads, cancel) for Plan 3's chat agent. Progress goes out as `PipelineEvent`s on an mpsc channel; Plan 3 maps them to runtime `Event`s with a `SessionId`.

**Tech Stack:** Rust 2024, tokio, `review-llm` and `review-sandbox` (Plan 1), rusqlite store (Plan 1), globset, serde_json.

**Spec:** `docs/superpowers/specs/2026-10-20-ai-review-design.md` (spec §N). **Depends on:** Plan 1 (`docs/superpowers/plans/2026-10-20-ai-review-1-foundation.md`) fully merged. Source to copy: `/Users/sercand/Developer/src/github.com/otsimo/automated-qa-framework/crates/qa-agent/src/tools/`.

## Global Constraints

- Everything from Plan 1's Global Constraints.
- **No GitHub writes and no merge/approve tool.** The tool registry is a fixed list; a test asserts no tool name contains `merge`, `approve`, `submit_review` (as a GitHub call), `post`, `resolve`, `edit_file`, or `write_file`.
- Tools never panic; bad input and failures are `ToolOutput::error(msg)` (copied behavior). Tool text output is cut to `MAX_TOOL_OUTPUT = 30_000` bytes with `truncate_middle`.
- Prompts and tool output aren't logged at info or above; logs carry ids, kinds and counts only.
- Budgets (spec §6.2): max turns scout 30, reviewer 40, challenger 20, lookup 15, verifier 10, assembler 5, chat 40; lookup answers ≤ 300 words; context threshold from `AiSettings.context_threshold` (default 0.7) of `ModelSettings.context_window`.
- Group sizing (spec §5.2): ≤ 8 files and ≤ 1,500 changed lines per group; the app splits larger groups.
- Reviewers run with at most `AiSettings.max_parallel_reviewers` (default 2) at once.

## Review Focus

1. **Option injection through read-only git tools.** A model passes `rev = "--output=/tmp/x"` or a path starting with `-`. Expect a tool error; no git call. Test in Task 2.
2. **A finding anchored to a line outside the diff, or to the wrong side.** Expect `record_finding` to reject it with a message naming the nearest diff lines. Test in Task 7.
3. **The model ends its turn without calling the finishing tool** (scout never calls `submit_brief`, assembler never calls `submit_review_draft`). Expect one reminder turn, then a clean stage failure, not a hang or a panic. Test in Task 9.
4. **A tool_use/tool_result pair split by context eliding**, leaving a `tool_result` without its `tool_use`. Providers reject that with a 400. Expect pairs to stay intact. Test in Task 9.
5. **The PR has 0 reviewable files** (only lockfiles/generated), or 1 file with a 20k-line patch. Expect a `skim`-only review that still produces a summary, and a big file split into hunk-range groups. Test in Task 11.

---

### Task 1: Tool framework and copied repo tools

**Files:**
- Create: `src/ai/tools/{mod.rs,truncate.rs,bash.rs,files.rs,search.rs}` ← adapted from `qa-agent/src/tools/` (not `edit_file`)
- Create: `src/ai/tools/tests.rs` ← adapted from `qa-agent/tests/tools.rs` (drop edit_file cases)
- Modify: `src/ai/mod.rs`

**Interfaces:**
- Produces:
  - `pub struct ToolOutput { pub parts: Vec<review_llm::ToolResultPart>, pub is_error: bool }` with `text(..)`, `error(..)` (copied)
  - `#[async_trait] pub trait Tool: Send + Sync { fn definition(&self) -> review_llm::Tool; async fn call(&self, input: serde_json::Value, cx: &ToolCx) -> ToolOutput; }`
  - `pub struct ToolCx { pub sandbox: Arc<dyn review_sandbox::SandboxOps>, pub repo_root: PathBuf, pub agent: AgentRef }`. Later tasks add these fields, by exactly these names: `snapshot: Arc<PrSnapshot>` (Task 4), `run: Arc<RunState>` (Task 7), `interaction: Arc<dyn Interaction>` (Task 3), `context: Arc<RepoContext>` (Task 5), `spawner: Arc<dyn Spawner>` (Task 12).
  - `#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)] pub struct AgentRef { pub id: u32, pub role: AgentRole, pub label: String }` and `pub enum AgentRole { Scout, Reviewer, Challenger, Lookup, Verifier, PriorCheck, Assembler, Chat }`
  - `pub struct Registry` with `Registry::new(tools: Vec<Arc<dyn Tool>>)`, `definitions() -> Vec<review_llm::Tool>`, `get(name) -> Option<Arc<dyn Tool>>`
  - Tool names: `bash`, `read_file`, `glob`, `grep`
  - `read_file` input: `{ path: string, offset?: integer (1-based line), limit?: integer (lines, default 2000) }`. Text output is `cat -n` style (`{n:>6}\t{line}`), with a trailing `"[showing lines a-b of N]"` when ranged. Images keep the copied behavior.

- [ ] **Step 1:** Copy and adapt the tools to `ToolCx`; port the copied tests (rename crate paths), and add:

```rust
#[tokio::test] async fn read_file_ranges_lines_with_numbers()   // 10-line file, offset 3 limit 2 → "     3\tl3\n     4\tl4\n[showing lines 3-4 of 10]"
#[tokio::test] async fn read_file_offset_past_end_is_an_error()
#[test] fn registry_has_no_write_or_merge_tools()                // over the full registry from `tools::all()` (grows in later tasks)
```

- [ ] **Step 2:** Run `cargo test --lib ai::tools` → new tests FAIL; **Step 3:** implement; **Step 4:** PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): tool framework and repo tools"`

---

### Task 2: `list_dir`, `asset_stats`, read-only git tools

**Files:**
- Create: `src/ai/tools/inspect.rs`, `src/ai/tools/git.rs`

**Interfaces:**
- Produces tools:
  - `list_dir { path }` → one line per entry: `name<TAB>size<TAB>dir|file`, sorted by name, max 500 entries (then `[N more]`).
  - `asset_stats { path }` → JSON text `{ "path", "bytes", "format", "width", "height", "siblings": { "count", "median_bytes", "max_bytes", "median_width", "median_height" }, "ratio_to_median": f64 }`. Siblings are the files in the same directory with the same extension (case-insensitive), excluding the file. Dimensions come from `image::image_dimensions`, `null` for non-images.
  - `git_show { rev, path? }`, `git_log { path?, n? (default 20, max 200) }`, `git_diff { range, path? }`. These run `git` as a child process in `repo_root` with `--no-pager`, `-c core.pager=cat`, an explicit `--` before paths, and no shell.
- `pub fn validate_rev(s: &str) -> Result<(), String>`: rejects empty, leading `-`, whitespace, control chars, and anything except `[A-Za-z0-9._/~^@{}:-]`. `range` may contain one `..` or `...`; each side is validated as a rev. Paths: leading `-` rejected; must resolve inside the repo (`Root::resolve`).

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn asset_stats_compares_with_siblings()   // dir with a.png 10x10 (~100B), b.png, c.png and big.png 2000x2000 → ratio_to_median > 10, median_width == 10
#[tokio::test] async fn list_dir_reports_sizes_and_kinds()
#[test] fn validate_rev_rejects_option_injection() { for r in ["--output=/tmp/x", "-p", "a b", "HEAD;rm", ""] { assert!(validate_rev(r).is_err(), "{r}") } for r in ["HEAD~2", "abc123", "origin/main", "HEAD@{1}", "v1.2.3^{}"] { assert!(validate_rev(r).is_ok(), "{r}") } }
#[tokio::test] async fn git_log_and_show_work_on_a_temp_repo()
#[tokio::test] async fn git_diff_path_outside_repo_is_rejected() // path "../x" → error
```

- [ ] **Step 2–4:** fail → implement → `cargo test --lib ai::tools` PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): list_dir, asset_stats and read-only git tools"`

---

### Task 3: `Interaction` trait and the approval-gated `bash`

**Files:**
- Create: `src/ai/interaction.rs`
- Modify: `src/ai/tools/bash.rs`, `src/ai/tools/mod.rs` (`ToolCx.interaction`, `ToolCx.allowed: Arc<Mutex<Vec<String>>>`)

**Interfaces:**
- Produces:
  - `pub struct Question { pub id: u64, pub text: String, pub options: Vec<String> }`
  - `pub enum ApprovalDecision { Once, ForReview, Deny { reason: Option<String> } }`
  - ```rust
    #[async_trait]
    pub trait Interaction: Send + Sync {
        async fn ask_user(&self, agent: &AgentRef, q: Question) -> String;
        async fn approve_command(&self, agent: &AgentRef, command: &str, reason: &str) -> ApprovalDecision;
        async fn approve_prepare(&self, agent: &AgentRef, reason: &str) -> bool;
    }
    ```
  - `pub struct ScriptedInteraction` (`#[cfg(test)]`, in `src/test_support.rs`): queued answers/decisions, and it records every call.
  - `ToolCx.allowed`: the repo's configured commands, seeded at run start. Commands approved `ForReview` are appended to it.
- `bash` behavior: `review_sandbox::approval::classify(cmd, &allowed)`. `Run` → exec. `Ask{reason}` → `approve_command`. `Once` → exec. `ForReview` → push the command's normalized text, then exec. `Deny` → `ToolOutput::error("the user declined this command" + optional ": reason")`. Exec timeout stays `DEFAULT_EXEC_TIMEOUT`.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn allowed_command_runs_without_asking()      // "ls" → no approve_command call
#[tokio::test] async fn unknown_command_asks_and_runs_once()       // "touch x" + Once → file exists; asked again next time
#[tokio::test] async fn for_review_is_remembered()                 // second identical call doesn't ask
#[tokio::test] async fn denied_command_returns_the_reason()        // Deny{Some("no")} → is_error, text contains "no", file absent
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): interaction trait and approval-gated bash"`

---

### Task 4: PR snapshot and GitHub read tools

**Files:**
- Modify: `src/github/checks.rs` (annotations)
- Create: `src/ai/snapshot.rs`, `src/ai/tools/gh.rs`
- Test: `tests/contract_check_annotations.rs`, inline tests

**Interfaces:**
- Produces:
  - `GithubClient::check_run_annotations(&self, repo: &RepoRef, check_run_id: u64) -> Result<Vec<CheckAnnotation>, GhError>` → `GET /repos/{o}/{r}/check-runs/{id}/annotations` (all pages). `pub struct CheckAnnotation { pub path: String, pub start_line: u32, pub end_line: u32, pub level: String, pub title: Option<String>, pub message: String }`. Also make `CheckRun` carry `id: u64` and `output_summary: Option<String>`, `output_text: Option<String>` if it doesn't yet (read from the existing JSON).
  - `pub struct PrSnapshot { pub repo: RepoRef, pub number: u64, pub metadata: PrMetadata, pub files: Vec<LoadedFile>, pub threads: Vec<ReviewThread>, pub conversation: Vec<…existing issue comment type…>, pub checks: Vec<CheckSummary> }` with `pub struct CheckSummary { pub name: String, pub conclusion: Option<String>, pub summary: Option<String>, pub text: Option<String>, pub annotations: Vec<CheckAnnotation> }`
  - `pub async fn gather_snapshot(client: &GithubClient, repo: &RepoRef, number: u64, files: Vec<LoadedFile>, metadata: PrMetadata) -> Result<PrSnapshot, GhError>`: the UI already has files and metadata loaded, so they're passed in. It fetches threads, conversation and check runs, plus annotations for runs whose conclusion is `failure`, `neutral`, `timed_out`, `action_required` or `cancelled`.
  - `PrSnapshot::diff_lines(&self, path: &str) -> Vec<(Side, u32)>`: every commentable line (from the parsed hunks)
  - Tools (read from `cx.snapshot`, no network): `gh_pr {}` (title, author, base/head refs+SHAs, labels, body), `gh_pr_files {}` (path, status, +/−, one per line), `gh_pr_patch { path }` (the raw patch with new-side line numbers prefixed), `gh_threads { path? }` (existing threads: path:line, author, body, resolved/outdated), `gh_checks {}` (name, conclusion, summary, annotations as `path:line level: message`).

- [ ] **Step 1: Failing tests**: a wiremock contract test for annotations (exact path, paging); `diff_lines_cover_added_context_and_removed_sides`; a `gh_pr_patch` unknown-path error; `gh_checks_lists_failed_annotations`.
- [ ] **Step 2–4:** fail → implement → `cargo test --test contract_check_annotations && cargo test --lib ai::snapshot ai::tools::gh` PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): PR snapshot with CI annotations and GitHub read tools"`

---

### Task 5: Repo context (REVIEW.md, AGENTS.md/CLAUDE.md, skills)

**Files:**
- Create: `src/ai/context.rs`, `src/ai/tools/skills.rs`

**Interfaces:**
- Produces:
  - `pub struct RepoCheck { pub id: String /* "repo:1".. */, pub text: String, pub glob: Option<String> }`
  - `pub struct InstructionFile { pub dir: String /* "" for root, "src/api" */, pub path: String, pub content: String /* imports expanded */ }`
  - `pub struct SkillInfo { pub name: String, pub description: String, pub dir: String }`
  - `pub struct RepoContext { pub review_md: Option<String>, pub checks: Vec<RepoCheck>, pub instructions: Vec<InstructionFile>, pub skills: Vec<SkillInfo> }`
  - `pub fn load_repo_context(root: &Path) -> RepoContext` (never fails; unreadable files are skipped with a `tracing::debug!`)
  - `RepoContext::instructions_for(&self, paths: &[&str]) -> Vec<&InstructionFile>`: root files plus nested ones whose `dir` is an ancestor of any path
  - `RepoContext::skill_index(&self) -> String`: `- name: description`, one per line
  - Tool `load_skill { name }` → the SKILL.md body without frontmatter, followed by `\n[skill files are under <dir>/; read them with read_file]`
- Rules (spec §6.6):
  - `REVIEW.md` lives at `.github/REVIEW.md`.
  - Checks are the top-level bullets under the first `## Checks` heading (case-insensitive), up to the next `##`. A bullet starting with `[<glob>] ` is scoped by that glob.
  - Instruction files are `AGENTS.md` and `CLAUDE.md` at any depth, skipping dirs ignored by `.gitignore` (use `ignore::WalkBuilder`) and capped at 50 files. When both exist in a dir, both are kept.
  - A line that is exactly `@relative/path.md` is replaced by that file's contents, one level deep. The path is resolved relative to the file and must stay inside the repo; otherwise the line is kept as-is.
  - Skills are `.claude/skills/*/SKILL.md` with YAML-ish frontmatter `name:` / `description:` between `---` lines (parse just those two keys; the folder name is the fallback name).

- [ ] **Step 1: Failing tests**

```rust
#[test] fn review_md_checks_parse_with_optional_globs()   // "## Checks\n- Use X\n- [assets/**/*.png] ≤ 200 KB\n## Other\n- not a check" → 2 checks, second glob Some
#[test] fn nested_agents_md_scopes_to_paths()              // root + src/api/AGENTS.md; instructions_for(["src/api/x.rs"]) has both, (["web/y.ts"]) only root
#[test] fn claude_md_import_is_expanded_once_and_confined() // CLAUDE.md "@AGENTS.md" → content; "@../../etc/passwd" stays literal
#[test] fn skills_index_reads_frontmatter()
#[tokio::test] async fn load_skill_returns_body_and_unknown_is_error()
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): repo context — REVIEW.md checks, AGENTS.md scoping, skills"`

---

### Task 6: Checklist and file kinds

**Files:**
- Create: `src/ai/checklist.rs`

**Interfaces:**
- Produces:
  - `pub enum FileKind { Source, Test, Asset, Config, Generated }`
  - `pub fn file_kind(path: &str, is_binary: bool) -> FileKind`: Generated = lockfiles (`Cargo.lock`, `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `go.sum`, `Gemfile.lock`, `poetry.lock`), `*.min.*`, `*.pb.*`, `*_generated.*`, `vendor/**`, `dist/**`; Test = `tests/**`, `test/**`, `**/__tests__/**`, `*_test.*`, `*.test.*`, `*.spec.*`, `test_*.py`; Asset = binary or an image/font/media extension; Config = `.github/**`, `*.toml`, `*.yaml`, `*.yml`, `*.json` (not lockfiles), `Dockerfile*`, `Makefile`, `*.ini`, `.env*`; else Source
  - `pub struct ChecklistItem { pub id: String, pub title: String, pub instruction: String }`
  - `pub fn builtin_items() -> Vec<ChecklistItem>`: ids and instructions exactly as the spec §5.3 table (`intent`, `correctness`, `callers`, `consistency`, `duplication`, `tests`, `api_surface`, `security`, `performance`, `assets`, `ci`)
  - `pub fn default_items(kind: FileKind) -> &'static [&'static str]`: Source → all except `assets`; Test → `intent, correctness, consistency`; Asset → `assets, consistency`; Config → `intent, api_surface, security`; Generated → `intent`
  - `pub fn assign(path: &str, kind: FileKind, repo_checks: &[RepoCheck], scout_extra: &[String]) -> Vec<String>`: defaults ∪ matching repo checks (unscoped = all files; scoped = glob match) ∪ scout extras. Deduplicated, in builtin order then repo then extras.

- [ ] **Step 1: Failing tests**: a `file_kind_table` over ~20 paths; `assign_merges_defaults_repo_checks_and_extras`; `scout_cannot_remove_defaults` (extras only add).
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): checklist items and per-file-kind assignment"`

---

### Task 7: Findings, coverage and the run state

**Files:**
- Create: `src/ai/review.rs` (types + `RunState`), `src/ai/tools/record.rs`
- Modify: `src/ai/store.rs` (run/finding/coverage/qa/message APIs)

**Interfaces:**
- Produces:
  - `pub enum Severity { Bug, Risk, Nit, Question }` (serde lowercase)
  - `pub struct Evidence { pub path: String, pub line: Option<u32>, pub note: String }`
  - `pub enum FindingState { Draft, Kept, DroppedByUser, DroppedByVerifier, Posted, Addressed, StillPresent, Outdated }` (serde snake_case, the spec §11 spelling)
  - `pub enum FindingAnchor { Line(crate::model::comment::Anchor), Summary { path: String } }`
  - `pub struct Finding { pub id: i64, pub run_id: i64, pub agent: AgentRef, pub anchor: FindingAnchor, pub severity: Severity, pub title: String, pub body: String, pub evidence: Vec<Evidence>, pub checklist_item: Option<String>, pub state: FindingState, pub state_reason: Option<String>, pub pending_local_id: Option<u64> }`
  - `pub enum CheckResult { Issue, Ok, NotApplicable, Skipped }`
  - `pub struct CoverageEntry { pub file: String, pub item: String, pub result: CheckResult, pub note: String }`
  - `pub struct RunState` (`Arc`-shared, internally `Mutex`ed; persists every mutation through `Arc<Store>`):
    - `new(store, session_id, run_id, events: mpsc::UnboundedSender<PipelineEvent>)`
    - `add_finding(&self, f: NewFinding) -> i64`, `set_finding_state(&self, id, state, reason: Option<String>)`, `findings(&self) -> Vec<Finding>`
    - `record_check(&self, e: CoverageEntry)`, `coverage(&self) -> Vec<CoverageEntry>`, `missing(&self, assigned: &[(String, String)]) -> Vec<(String, String)>`
    - `add_qa(&self, q: &str, a: &str)`, `qa(&self) -> Vec<(String, String)>`
  - `Store` additions: `run_begin(session_id, kind: RunKind, base_sha: Option<&str>, head_sha) -> i64`, `run_end(run_id, status: &str, stop_reason: Option<&str>, usage_json)`, `finding_insert/update/list(session_id)`, `coverage_insert/list(run_id)`, `qa_insert/list(session_id)`, `message_append(run_id, agent_id, role, content_json)`, `messages(run_id, agent_id)`, `session_set_last_reviewed(session_id, sha)`, `session_brief(session_id) / set_brief(json)`. `pub enum RunKind { Full, Incremental }`.
  - Tools:
    - `record_finding { path, line, start_line?, side ("RIGHT" default | "LEFT"), severity, title, body, evidence: [{path, line?, note}], checklist_item? }`. Validation (spec §5.4/§5.6): `evidence` is non-empty, each note non-empty; `(side, line)` and `start_line` are in `cx.snapshot.diff_lines(path)`. Failures are an error naming up to 10 nearest valid lines on that side. If `path` is in the PR but has **no** diff lines (binary/asset), the finding is stored as `FindingAnchor::Summary`. A path not in the PR is an error ("anchor on the PR line that introduces it; put the other location in evidence").
    - `record_check { file, item, result ("issue"|"ok"|"n/a"), note }`: `ok`/`n/a` need a non-empty note; `item` must be assigned to this agent (the assignment list is on `ToolCx.assigned: Arc<Vec<(String, String)>>`, empty = anything allowed).
    - `done { summary? }` sets a flag on the agent's turn state (read by the loop).
  - `PipelineEvent` is defined in Task 15; until then `RunState` takes the sender type from there. Declare `pub enum PipelineEvent` in `src/ai/events.rs` now with `FindingAdded { id: i64 }`, `FindingChanged { id: i64 }`, `Coverage { file: String, item: String }`; Task 15 adds the rest.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn finding_without_evidence_is_rejected()
#[tokio::test] async fn finding_outside_the_diff_lists_nearest_lines()   // patch lines 10-14 RIGHT; line 30 → error contains "10" and "14"
#[tokio::test] async fn left_side_anchor_validates_against_old_lines()
#[tokio::test] async fn binary_file_finding_becomes_a_summary_item()
#[tokio::test] async fn ok_check_needs_a_note_and_an_assigned_item()
#[test]        fn missing_lists_unrecorded_pairs()
#[test]        fn run_state_persists_and_reloads()                      // in-memory Store: add finding + coverage, list via Store
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): findings, coverage, run state and record tools"`

---

### Task 8: `Llm` trait and the scripted fake

**Files:**
- Create: `src/ai/model.rs`
- Modify: `src/test_support.rs` (`ScriptedLlm`)

**Interfaces:**
- Produces:
  - `#[async_trait] pub trait Llm: Send + Sync { async fn stream(&self, req: review_llm::GenerateRequest) -> review_llm::Result<BoxStream<'static, review_llm::Result<review_llm::StreamEvent>>>; fn model(&self) -> &ModelSettings; }`
  - `pub struct ClientLlm { client: review_llm::Client, model: ModelSettings }` implementing it via `generate_stream`. It fills `model`, `max_tokens`, and `reasoning` (Low/Medium/High → `review_llm::Reasoning` with the matching effort). Cache hints are set on `system` + `tools` when the API is Anthropic.
  - `ScriptedLlm` (test only): `ScriptedLlm::new(model, Vec<ScriptStep>)` with `pub enum ScriptStep { Text(String), ToolCalls(Vec<(String /*name*/, serde_json::Value)>), Fail(review_llm::Error) }`. It turns each step into a stream (`MessageStart`, deltas, `BlockStop`, `Done(Reply)`) with deterministic tool-use ids `tu_<n>`, records every `GenerateRequest` it received, and panics with "script exhausted" when it runs out. Supports keyed scripts per agent label: `ScriptedLlm::per_agent(BTreeMap<String /*label prefix*/, Vec<ScriptStep>>)`. The agent label is passed via the request's system prompt first line, `# agent: <label>`, which Task 10's prompts always emit.

- [ ] **Step 1: Failing test** `scripted_llm_emits_tool_calls_and_records_requests`; **Steps 2–4**; **Step 5: Commit** `git commit -am "feat(ai): Llm trait, client adapter and scripted fake"`

---

### Task 9: The agent loop

**Files:**
- Create: `src/ai/agent.rs`

**Interfaces:**
- Consumes: `Llm` (Task 8), `Registry`/`ToolCx` (Task 1), `RunState` (Task 7).
- Produces:
  - `pub enum FinishCheck { Accept, Continue(String /* user-role reminder text */) }`
  - `pub struct AgentSpec { pub agent: AgentRef, pub system: String, pub initial: Vec<review_llm::Message>, pub registry: Registry, pub max_turns: u32, pub finish_tool: Option<&'static str>, pub on_finish: Arc<dyn Fn() -> FinishCheck + Send + Sync>, pub max_finish_reminders: u32 /*2*/ }`
  - `pub struct AgentControl { pub cancel: CancellationToken, pub pause: tokio::sync::watch::Receiver<bool>, pub notes: Arc<Mutex<VecDeque<String>>> }`
  - `pub enum AgentStop { Finished, MaxTurns, Cancelled, Failed(String), NoFinish }`
  - `pub struct AgentOutcome { pub stop: AgentStop, pub usage: review_llm::Usage, pub last_text: String, pub turns: u32 }`
  - `pub async fn run_agent(spec: AgentSpec, cx: ToolCx, llm: Arc<dyn Llm>, ctl: AgentControl, events: mpsc::UnboundedSender<PipelineEvent>) -> AgentOutcome`
- Behavior (spec §9.2, §6.2):
  1. Each turn: wait while `pause` is `true` (checked before each model call). Drain `notes` into one user text block `"[note from the user] …"` appended before the call. Elide if needed (rule 4). Then stream; forward `TextDelta` as `PipelineEvent::AgentText { agent, delta }` (Task 15 adds this variant). Build the reply.
  2. Tool calls run **in order**, each raced against `cancel`; results go back as one user message (tool_result blocks in call order). Emit `PipelineEvent::ToolCall { agent, name, summary }`, where `summary` is the first 120 chars of the JSON input.
  3. Finishing: a turn with no tool calls, or a turn where `finish_tool` was called successfully, triggers `on_finish()`. `Accept` → `Finished`. `Continue(text)` → push `text` as a user message and keep going, at most `max_finish_reminders` times, then `Finished` (the caller records skips). With `finish_tool = Some(name)` and a turn without tool calls, the reminder is `"Call {name} to finish."`; after the reminders run out → `NoFinish`.
  4. Context eliding: before each call, estimate tokens as `bytes / 4` over system + messages. Above `threshold × context_window`, replace the text of the **oldest** `tool_result` contents (never the latest two turns) with `"[elided: <tool> <first 80 chars of its input>]"` until under the threshold. `tool_use` blocks and their ids are never removed.
  5. Every message is persisted with `Store::message_append` (via `cx.run`).
  6. A model error after `review-llm`'s own retries → `Failed(msg)`.

- [ ] **Step 1: Failing tests** (all with `ScriptedLlm` + `ScriptedInteraction` on a temp repo)

```rust
#[tokio::test] async fn runs_tools_in_order_and_finishes_on_text()
#[tokio::test] async fn finish_check_continue_reprompts_then_accepts()
#[tokio::test] async fn missing_finish_tool_gets_one_reminder_per_allowance_then_no_finish()
#[tokio::test] async fn max_turns_stops()
#[tokio::test] async fn cancel_during_a_tool_stops_promptly()   // bash "sleep 30"; cancel after 100ms → Cancelled within 2s
#[tokio::test] async fn pause_holds_before_the_next_model_call()
#[tokio::test] async fn notes_are_delivered_on_the_next_turn()   // the recorded request contains "[note from the user] x"
#[tokio::test] async fn eliding_keeps_tool_use_result_pairs()    // tiny context_window; every tool_result id in the request has a matching tool_use earlier
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): agent loop with finish checks, pause, notes and eliding"`

---

### Task 10: Prompts

**Files:**
- Create: `src/ai/prompts.rs`, `tests/fixtures/prompts/*.txt` (golden)

**Interfaces:**
- Produces `pub fn system_prompt(role: AgentRole, label: &str) -> String`. The first line is always `# agent: <label>`. The bodies are `const` strings per role.
- Every reviewer-type prompt (Reviewer, Challenger, Chat) must include, in substance: work file by file; the checklist with "what to compare against" (the spec §5.3 table wording); **the evidence rule**; the record_check rule; don't repeat CI output or existing threads; ask the user only when the answer changes the review; one example of a good finding (with evidence) and one bad one ("consider adding a comment").
- The Challenger prompt adds the adversarial framing from spec §5.4. The Verifier prompt says to re-open every cited location and answer keep/drop/downgrade with a reason. Lookup: answer in ≤ 300 words with file:line references. Assembler: the verdict rules from spec §5.6.
- Golden test `prompts_match_golden_files`: compares each role's prompt with `tests/fixtures/prompts/<role>.txt`. With `UPDATE_GOLDEN=1` it rewrites them.

- [ ] **Steps 1–4:** test fails (no files) → write prompts + generate goldens → PASS. Also `reviewer_prompt_states_the_evidence_rule` (asserts the phrase `"Every finding must cite evidence"`).
- [ ] **Step 5: Commit** `git commit -am "feat(ai): system prompts for every agent role"`

---

### Task 11: Scout stage and the brief

**Files:**
- Create: `src/ai/stages/mod.rs`, `src/ai/stages/scout.rs`, `src/ai/tools/brief.rs`

**Interfaces:**
- Produces:
  - `pub enum Risk { High, Medium, Low }`
  - `pub struct Group { pub id: String /* "g1".. */, pub title: String, pub risk: Risk, pub reason: String, pub files: Vec<GroupFile>, pub leads: Vec<String>, pub extra_checks: Vec<String>, pub ci_notes: Vec<String>, pub skim: bool }` and `pub struct GroupFile { pub path: String, pub hunks: Option<(u32, u32)> /* hunk index range when split */ }`
  - `pub struct Brief { pub intent: String, pub description_matches: bool, pub unexplained: Vec<String>, pub groups: Vec<Group>, pub suggested_commands: Vec<String>, pub suggested_prepare: Vec<String> }` (the two suggestion lists are filled only when the repo has no configured commands; spec §4 step 1) (serde; stored via `Store::set_brief`)
  - Tool `submit_brief { intent, description_matches, unexplained[], suggested_commands?[], suggested_prepare?[], groups: [{title, risk, reason, files[], leads[], extra_checks[], ci_notes[], skim}] }`. Validation: every PR file appears in exactly one group, otherwise an error listing missing/duplicate paths.
  - `pub fn normalize_groups(brief: &mut Brief, snapshot: &PrSnapshot)`: assigns ids, forces Generated-kind files into a `skim` group, and splits any group over 8 files or 1,500 changed lines (a single file over 1,500 lines is split by hunk ranges). Sub-groups keep the parent's risk, leads and checks, with titles `"<title> (part k)"`.
  - `pub async fn run_scout(inputs: &StageInputs, prev: Option<&Brief>) -> Result<Brief, StageError>` where `pub struct StageInputs { pub cx: ToolCx /* template */, pub llm: Arc<dyn Llm>, pub ctl: AgentControl, pub events: …, pub settings: AiSettings }` and `pub enum StageError { NoFinish, Failed(String), Cancelled }`
  - Scout initial message: PR overview, file list with +/− and kinds, the CI summary, `review_md`, root instructions, repo commands, plus previous brief + new commits when `prev` is given. Tools: read-only repo tools, `gh_*`, `load_skill`, `submit_brief`.

- [ ] **Step 1: Failing tests**

```rust
#[test] fn submit_brief_rejects_missing_and_duplicate_files()
#[test] fn normalize_splits_large_groups_and_huge_files()       // 12 files → 2 groups; one 20k-line file → hunk-range parts
#[test] fn lockfile_only_pr_becomes_a_skim_group()
#[tokio::test] async fn scout_returns_a_normalized_brief()      // ScriptedLlm: read_file then submit_brief
#[tokio::test] async fn scout_without_submit_is_no_finish()
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): scout stage and review brief"`

---

### Task 12: Reviewer stage, lookups, `ask_user`, `request_prepare`

**Files:**
- Create: `src/ai/stages/reviewer.rs`, `src/ai/tools/interact.rs`

**Interfaces:**
- Produces:
  - `#[async_trait] pub trait Spawner: Send + Sync { async fn lookup(&self, from: &AgentRef, question: &str) -> String; }`. The pipeline implements it (Task 15). The reviewer stage gets it via `ToolCx.spawner`.
  - Tools:
    - `ask_lookup { question }` → `spawner.lookup`. Unavailable to lookups themselves: not in their registry.
    - `ask_user { question, options? }` → `interaction.ask_user`, then `run.add_qa`.
    - `request_prepare { reason }` → `interaction.approve_prepare`; if true, the stage's `prepare` closure runs the repo's configured prepare commands via `crate::ai::checkout::run_prepare` on the blocking pool and returns its output (truncated). If false → error "the user declined".
  - `pub async fn run_reviewer(inputs: &StageInputs, brief: &Brief, group: &Group) -> Result<ReviewerOutcome, StageError>` with `pub struct ReviewerOutcome { pub skipped: Vec<(String, String)>, pub findings: Vec<i64> }`
  - Reviewer initial message: brief intent, the group (files, leads, CI notes), each file's patch (or hunk range), the assigned checklist per file (`checklist::assign` + `builtin_items` text), `instructions_for(group paths)`, the skill index, the existing threads on these files, and user Q&A so far. `ToolCx.assigned` holds the (file, item) pairs.
  - `on_finish`: `run.missing(assigned)` empty → `Accept`; else `Continue("You haven't recorded these checks yet: …. Record each with record_check.")`. After the reminders run out, every missing pair is recorded as `Skipped` with note `"not checked"`.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn reviewer_is_reprompted_for_missing_checks_then_skips_are_recorded()
#[tokio::test] async fn reviewer_can_record_a_finding_and_complete_coverage()
#[tokio::test] async fn ask_user_answer_is_stored_as_qa()
#[tokio::test] async fn request_prepare_declined_is_an_error_and_runs_nothing()
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): reviewer stage with enforced coverage, lookups and questions"`

---

### Task 13: Challenger, dedupe and verifier

**Files:**
- Create: `src/ai/stages/verify.rs`, `src/ai/stages/challenge.rs`, `src/ai/tools/verdict.rs`

**Interfaces:**
- Produces:
  - `pub fn needs_challenge(group: &Group, findings: &[Finding]) -> bool`: `risk == High` and no `Bug`/`Risk` finding from that group's reviewer
  - `pub async fn run_challenger(inputs, brief, group) -> Result<Vec<i64>, StageError>`: reviewer registry plus `done` as the finish tool; no coverage enforcement
  - `pub fn dedupe(findings: &[Finding]) -> Vec<Vec<i64>>`: clusters with the same path, overlapping line ranges and the same side. Singletons are clusters too.
  - Tool `submit_verdicts { verdicts: [{ finding_id, verdict: "keep"|"drop"|"downgrade", severity?, reason, duplicate_of? }] }`: every finding in the batch needs exactly one verdict
  - `pub async fn run_verifier(inputs, batch: Vec<Finding>) -> Result<(), StageError>`. Batches are per file, ≤ 8 findings. `keep` → state stays `Draft`; `drop` → `DroppedByVerifier` + reason; `downgrade` → change severity (store it, reason in `state_reason`); `duplicate_of` → `DroppedByVerifier` with reason `"duplicate of #<id>"`. The verifier also gets the existing threads and the CI annotations for that file (spec §5.5).

- [ ] **Step 1: Failing tests**: `challenge_only_for_high_risk_without_bugs`; `dedupe_clusters_overlapping_ranges`; `verifier_drops_with_reason_and_downgrades`; `verdicts_must_cover_the_batch`.
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): challenger pass, dedupe and verifier"`

---

### Task 14: Assembler and the draft review

**Files:**
- Create: `src/ai/stages/assemble.rs`, `src/ai/tools/draft.rs`

**Interfaces:**
- Produces:
  - `pub enum Verdict { Approve, Comment, RequestChanges }` (serde snake_case; maps to `ReviewEvent` in Plan 3)
  - `pub struct ReviewDraft { pub summary: String, pub verdict: Verdict, pub rationale: String, pub rewrites: BTreeMap<i64, String> /* finding id → new body */ }`
  - Tool `submit_review_draft { summary, verdict, rationale, rewrites: [{finding_id, body}] }`. Rewrites may only target kept (`Draft`) findings.
  - `pub fn verdict_floor(findings: &[Finding]) -> Verdict`: any `Bug` → `RequestChanges`, else `Comment` if any `Risk`/`Question`, else `Approve`. If the model's verdict is less severe than the floor, the app keeps the model's verdict but prefixes the rationale with `"(note: the findings suggest <floor>)"` **[builder note: spec §5.6 says "unless clearly trivial", so the model may soften it, but visibly]**.
  - `pub async fn run_assembler(inputs, brief, kept: &[Finding], coverage: &[CoverageEntry]) -> Result<ReviewDraft, StageError>`: no repo tools. The input includes coverage counts per item and the list of `Summary` findings. The summary must mention what was checked (the prompt says so; the test checks that the coverage digest is in the request).

- [ ] **Step 1: Failing tests**: `verdict_floor_table`; `assembler_draft_applies_rewrites_to_kept_only`; `assembler_request_includes_coverage_digest`.
- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): assembler and review draft"`

---

### Task 15: Pipeline orchestration and `PipelineControl`

**Files:**
- Create: `src/ai/pipeline.rs`
- Modify: `src/ai/events.rs`
- Test: `tests/ai_pipeline.rs` (`#![cfg(unix)]`, uses a temp git repo + `ScriptedLlm`). Since integration tests can't see `test_support`, put `ScriptedLlm`/`ScriptedInteraction` behind a `pub mod testing` in `src/ai/` gated by `#[cfg(any(test, feature = "ai-testing"))]`, and add `[features] ai-testing = []`, enabled for this test via `required-features`.

**Interfaces:**
- Produces:
  - `PipelineEvent` (complete): `Stage { stage: StageKind, status: StageStatus }`, `AgentStarted { agent: AgentRef, group: Option<String> }`, `AgentFinished { agent: AgentRef, stop: String }`, `AgentText { agent, delta }`, `ToolCall { agent, name, summary }`, `Question { agent, question: Question }`, `Approval { agent, id: u64, command: String, reason: String }`, `FindingAdded { id }`, `FindingChanged { id }`, `Coverage { file, item }`, `BriefReady`, `DraftReady { draft: ReviewDraft }`, `Usage { input: u64, output: u64 }`, `RunEnded { status: RunStatus, reason: Option<String> }`; with `pub enum StageKind { Scout, Review, Challenge, Verify, PriorCheck, Assemble }`, `pub enum StageStatus { Started, Done, Failed(String) }`, `pub enum RunStatus { Completed, Cancelled, Failed, BudgetExhausted }`
  - `pub struct PipelineInputs { pub store: Arc<Store>, pub session_id: i64, pub snapshot: Arc<PrSnapshot>, pub repo_root: PathBuf, pub sandbox: Arc<dyn SandboxOps>, pub settings: AiSettings, pub repo_settings: RepoAiSettings, pub llm: Arc<dyn Llm>, pub lookup_llm: Option<Arc<dyn Llm>>, pub interaction: Arc<dyn Interaction>, pub mode: RunMode, pub bash_disabled: Option<String> }`. When `bash_disabled` is `Some(reason)`, no registry includes `bash` or `request_prepare`, and every agent's initial message says "bash is unavailable in this review: <reason>" (spec §8.2). `pub enum RunMode { Full, Incremental { since_sha: String } }`
  - `pub struct PipelineControl` (cheap clone): `pause()`, `resume()`, `cancel()`, `note_agent(&self, agent_id: u32, text)`, `note_all(text)`, `add_lead(group_id, text) -> Result<(), String>` and `add_check(group_id, item) -> Result<(), String>` (error if the group already started), `rerun_group(group_id)`, `skip_group(group_id)`, `status(&self) -> PipelineStatus` (stages, per-group state `Pending|Running|Done|Skipped|Failed`, running agents, finding counts by severity/state, open questions)
  - `pub fn start_pipeline(inputs: PipelineInputs) -> (PipelineControl, mpsc::UnboundedReceiver<PipelineEvent>, JoinHandle<RunStatus>)`
- Flow:
  1. `run_begin`.
  2. Scout. Its failure ends the run `Failed`.
  3. Groups go through reviewers under a semaphore of `max_parallel_reviewers`, skim groups first and then by risk High → Low. A failed reviewer marks its group `Failed`, and the run continues.
  4. Challengers for `needs_challenge` groups.
  5. Dedupe, then verifier batches.
  6. Assembler, with only `Draft`-state findings as "kept".
  7. `DraftReady`, `session_set_last_reviewed(head_sha)`, `run_end`.
  - Token budget: after each agent, sum usage. Over `run_token_budget` (when > 0), cancel the remaining agents. The run ends `BudgetExhausted` but still runs the assembler on what exists.
  - `Spawner::lookup` runs a Lookup agent with `lookup_llm` or `llm`, max 15 turns, and returns its `last_text` cut to 300 words.

- [ ] **Step 1: Failing end-to-end tests** in `tests/ai_pipeline.rs`

```rust
#[tokio::test] async fn full_review_produces_a_draft_with_verified_findings()
//   fixture repo: src/a.rs (changed, duplicates a helper in src/util.rs), assets/logo.png (large); scripted scout → 2 groups;
//   reviewer g1 records a duplication finding with evidence src/util.rs:3 + all checks; reviewer g2 asset finding (Summary);
//   verifier keeps both; assembler submits → DraftReady with 2 findings, verdict Comment, coverage complete, events in stage order
#[tokio::test] async fn reviewer_failure_fails_only_its_group()
#[tokio::test] async fn cancel_ends_the_run_cancelled_and_persists_partial_findings()
#[tokio::test] async fn pause_and_note_reach_a_running_reviewer()
#[tokio::test] async fn add_lead_after_group_started_is_rejected()
#[tokio::test] async fn token_budget_stops_early_but_still_assembles()
```

- [ ] **Step 2–4:** fail → implement → `cargo test --features ai-testing --test ai_pipeline` PASS; `cargo test --workspace` PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): review pipeline orchestration and control handle"`

---

### Task 16: Incremental re-review and the prior-findings check

**Files:**
- Create: `src/ai/stages/prior.rs`
- Modify: `src/ai/pipeline.rs`, `src/ai/stages/scout.rs`

**Interfaces:**
- Produces:
  - `pub fn incremental_files(repo: &Path, since: &str, head: &str) -> Result<IncrementalScope, String>` with `pub enum IncrementalScope { Files(Vec<String>), FullFallback { reason: String } }`. It uses `git merge-base --is-ancestor since head`: if not an ancestor → `FullFallback { reason: "force-push: <since> is not an ancestor of <head>" }`, else `git diff --name-only since..head`.
  - Tool `submit_prior_states { states: [{ finding_id, state: "addressed"|"still_present"|"outdated", reason, new_anchor?: {path, line, side} }] }`. `still_present` with `new_anchor` re-anchors the finding: the new anchor is validated like `record_finding`, and if it's invalid the finding becomes a `Summary` anchor.
  - `pub async fn run_prior_check(inputs, open: Vec<Finding>) -> Result<(), StageError>`: open = state in `Draft | Kept | Posted`
  - Pipeline with `RunMode::Incremental`:
    1. `run_begin(kind Incremental, base_sha since)`.
    2. Prior check.
    3. Scout with `prev = session_brief`. The initial message lists the changed files and `git log since..head`.
    4. Only groups containing a changed file are reviewed.
    5. Every agent's initial message includes `qa()` as "Known facts from the user".
    6. Assembler, then set last reviewed.
  - `FullFallback` → run as `Full`, and emit `Stage { stage: Scout, status: Started }` preceded by `AgentText` on a synthetic `AgentRef` labelled `app`, carrying the fallback reason.

- [ ] **Step 1: Failing tests**

```rust
#[test] fn incremental_scope_lists_changed_files_and_detects_force_push()   // temp repo: commit A, B; since=A head=B → Files; rewrite history → FullFallback
#[tokio::test] async fn prior_check_marks_and_reanchors()                  // still_present with a valid new anchor → anchor updated; invalid → Summary
#[tokio::test] async fn incremental_run_reviews_only_touched_groups_and_passes_qa()
```

- [ ] **Step 2–4:** fail → implement → PASS.
- [ ] **Step 5: Commit** `git commit -am "feat(ai): incremental re-review with prior-findings check"`
