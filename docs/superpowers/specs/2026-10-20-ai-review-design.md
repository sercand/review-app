# Design Spec — AI review agent for `gh-review-desktop`

- **Status:** Draft
- **Date:** 2026-10-20
- **Topic:** An LLM agent that reviews a PR in the user's local checkout and
  drafts a review for the user to approve, edit, and submit.
- **Builds on:** `2026-10-19-gh-review-desktop-design.md` (cited below as
  "base spec §N").

Calls marked **[call]** were made by the designer, not the user. They are
listed together in §17 so they can be checked.

---

## 1. Why

The app makes reviewing a PR fast, but the reviewer still does all the
thinking. We want an agent that does a first, real review of the PR, so the
human starts from a draft of concrete, evidence-backed comments.

The model we'll run is about as capable as Claude Haiku. A small model given
"review this diff" in one prompt reads the changed lines, finds nothing
obviously broken, and approves. It isn't that the model *can't* see problems.
Nothing makes it go look. A good human reviewer finds most issues by
**comparing the change with code outside the diff**:

- a new file named differently from its siblings
- logic that duplicates a helper that already exists in the repo
- a PNG ten times bigger than the other images in the same folder
- a changed function whose callers weren't updated
- behavior that isn't covered by a test

We can't make the model smarter, but we can **teach it a method and enforce it
in code**. That method (§5) is the heart of this design. Everything else is
plumbing to run it safely in the user's checkout.

## 2. Goals, non-goals, anti-goals

### Goals

1. **Review like a careful human.** Look outside the diff, compare with the
   neighbors, trace callers, check the tests, and back every finding with
   evidence (a file and line it compared against).
2. **Draft, never post.** The agent produces a *draft review*: inline comments
   tagged by severity, a summary, and a suggested verdict. Nothing reaches
   GitHub until the user keeps a comment and submits the review themselves.
3. **Run unattended once started.** After the user clicks **✦ AI review** and
   confirms the checkout, the agent runs to the end. It stops only to ask the
   user a question or to get approval for a command.
4. **Use the repo's own guidance:** `.github/REVIEW.md`, `AGENTS.md` /
   `CLAUDE.md`, and skills in `.claude/skills/`.
5. **Use CI.** Failed checks and their annotations are inputs to the review.
6. **Continue where it left off.** When the author pushes new commits, a
   re-review looks at what changed since the last review and checks whether
   the earlier findings were addressed.
7. **Chat.** The user can follow progress, answer questions, and afterwards
   ask follow-ups ("why did you flag this?", "look harder at the migration").

### Non-goals

- **Merging.** The agent can't merge in any way (§8).
- **Approving or requesting changes by itself.** It only *suggests* a verdict,
  and the user presses the button.
- **Editing code or pushing fixes.** It's a reviewer. It has no file-edit tool,
  and it can't commit (§8).
- **Untrusted PRs.** It's for reviewing the user's own team's PRs. Hardening
  against malicious PR authors (prompt injection, hostile build scripts) is out
  of scope. The sandbox (§8) limits the damage but isn't designed for it.
- **Windows.** AI review is unix-only (macOS and Linux). On Windows it's
  compiled out, and the app builds and works as before.
- **OpenAI Chat Completions, Ollama-native, Bedrock.** Only Anthropic Messages
  and OpenAI Responses (both with a configurable base URL).
- **Several AI reviews at once in the same local repo.** There's one working
  checkout, so there's one review at a time per repo.
- **Moving the existing JSON session files into SQLite** **[call]**. The new
  AI data goes in SQLite. The existing per-PR JSON files stay as they are.

### Anti-goals: failures even if it technically works

- **A rubber stamp.** An agent that approves without showing what it checked.
  A clean result has to come with a coverage log (§5.4) that shows the checks
  were run.
- **Noise.** A review full of style nits, things CI already reports, or
  "consider adding a comment here". Findings are filtered by a verifier (§5.5).
  CI output is used as input, not repeated.
- **Repeating existing comments.** The agent sees the PR's existing review
  threads and mustn't re-raise what a human already said.
- **Touching the user's checkout without asking**, or leaving it in a
  different state than it was found.
- **A context-stuffed single agent.** One agent that reads everything, runs out
  of context, and loses the plot.

## 3. Constraints and decisions

| Decision | Source |
|---|---|
| Copy `qa-llm` from `otsimo/automated-qa-framework/crates/qa-llm`, and the tools from `qa-agent/src/tools` plus the local half of `qa-sandbox`, into this crate. The user owns that code and clears it to be relicensed under this repo's MIT license | user |
| The team's repos have **private dependencies**, and builds must work despite the credential-free sandbox (§8.5) | user |
| Move the crate to **edition 2024** and set `rust-version = "1.99"` (the copied code uses let-chains etc. and needs at least 1.98) | user |
| LLM APIs: **Anthropic Messages** and **OpenAI Responses** only | user |
| AI review is **unix-only**, behind `#[cfg(unix)]` | user |
| The agent works in the user's **real local checkout** of the repo, not a separate worktree | user |
| The app (not the agent) stashes, fetches, checks out the PR head, and restores afterwards, **after the user confirms** | user |
| Network is **on** in the sandbox (builds download dependencies) | user |
| Provider config is on disk in `settings.toml`; **only API keys** go in the OS keychain | user |
| AI session data (transcripts, findings, coverage) goes in **SQLite** | user |
| The chat lives in a new **✦ AI tab in the right panel**; draft comments are inline in the diff (layout option A) | user |
| **The app's code orchestrates the agents, not a model** | agreed in discussion |
| Haiku-class target model: every agent gets a narrow task and a fresh, small context | user (model class), design consequence |

### What we take from `automated-qa-framework`

- **`qa-llm` (~4.6k LOC), nearly whole.** Keep: `client.rs`, `anthropic.rs`,
  `openai.rs`, `http.rs`, `stream.rs`, `types.rs`, `tools.rs`, `retry.rs`,
  `error.rs`, `history.rs`, `media.rs`, `trace.rs`. Drop the QA "roles" layer
  and the proprietary decisions API (`role.rs`, `decision.rs`, `decisions.rs`,
  the role parts of `llm.rs` and `config.rs`). We build `ClientConfig` straight
  from our own settings (§10). `dns.rs` (SSRF guard) is optional. It's the
  builder's call whether it's worth keeping.
- **Tools** (`qa-agent/src/tools`): `bash`, `read_file`, `glob`, `grep`,
  `truncate`. Not `edit_file`. Extend `read_file` with `offset`/`limit` (small
  models need line ranges).
- **Sandbox** (`qa-sandbox`): `ops.rs`, `paths.rs`, `files.rs`, `search.rs`,
  `bash.rs` (PTY bash, unix). Drop `connector.rs` and everything gRPC.
- **Not taken:** `qa-agent`'s session loop, hub, jobs, recorder (Postgres,
  gRPC, QA server). The agent loop is rewritten (§9.2). It's about 150 lines
  of real logic in the original.
- **New dependencies** this brings in: async-openai 0.42 (responses, byot,
  middleware, rustls), futures, eventsource-stream, base64, rand, schemars,
  secrecy, tower, http, url, async-trait, tokio-util, globset, grep-regex,
  grep-searcher, ignore, nix, portable-pty, plus `rusqlite` (bundled) for
  storage. reqwest is already 0.13 in both. Unix-only crates go under
  `[target.'cfg(unix)'.dependencies]`.
- **Not present in the source, so new code:** permission prompts, asking the
  user questions, subagents, skills, context compaction, and the sandbox
  profile (§8). The original bash is explicitly unrestricted.

## 4. User flow

1. **Setup (once).** In Settings → AI: add a provider (name, API type, base
   URL, API key → keychain) and pick a model (§10). Per repo, set the local
   folder. The first time a repo is reviewed, the scout proposes the repo's
   build/test/lint commands from `Cargo.toml` / `package.json` / `Makefile` /
   CI workflows, and the user confirms or edits them. These become the
   commands the agent may run without asking (§8.1).
2. **Start.** On an open PR, the user clicks **✦ AI review** in the top bar.
   - No local folder for this repo yet → ask for one (folder picker), and
     check that one of its git remotes matches the PR's `host/owner/repo`.
   - Show the **checkout confirmation**: current branch → PR head
     (`feature/x @ a1b2c3d`), whether there are uncommitted changes that will
     be stashed, and the exact git commands the app will run.
3. **Checkout (by the app).** Stash if dirty, fetch the PR head, check it out,
   verify `HEAD` = the PR's head SHA (§7). Then the agent session starts.
4. **Run.** Scout → reviewers → verifier → assembler (§5), with progress in
   the AI tab. The run stops only for an `ask_user` question or a command
   approval. Other agents keep going while one waits, where they can.
5. **Result.** Draft comments appear inline in the diff in a distinct AI style
   with **Keep / Edit / Drop / Why?**. Files with drafts get a ✦ marker in
   the tree. **Keep** moves the comment into the existing pending review
   (base spec §10.2) as a normal pending comment. The suggested summary and
   verdict pre-fill the review popover, and the user submits as usual.
6. **Follow-up.** The chat stays live. Messages keep going to the same chat
   agent (§6.5), which now has the finished findings. It can answer
   questions, dig further, and add new drafts.
7. **Finish.** The user clicks **Finish** in the AI tab, or **Cancel**, or
   quits the app. The app restores the original branch and pops the stash
   (§7.3). **Leaving the PR screen doesn't restore anything** **[call]**: the
   checkout is held by the AI session, not by the screen, so the run and the
   follow-up chat keep working when the user navigates away (§9.3). While the
   checkout is held, the repo is **locked**: starting an AI review on another
   PR in the same repo says which PR holds it and offers to Finish that one
   first. The AI tab of a finished session (after Finish or a quit) still shows
   the transcript and findings, and sending a follow-up message first goes
   through the checkout confirmation again **[call]**.
8. **Re-review.** When the PR head has moved since the last run, the AI tab
   offers **Continue review**. It goes through the same checkout confirmation
   and then runs incrementally (§5.7).

## 5. The review method

This is the core of the design. The model is never asked to "review the PR".
The app's own code runs a fixed pipeline of narrow agents. Each agent gets a
fresh context holding only what its task needs, and the app enforces the steps
the model would otherwise skip.

```
            ┌──────────┐   brief    ┌─────────────┐  findings  ┌──────────┐  kept   ┌───────────┐
 PR + repo ─▶  Scout   ├───────────▶ Reviewer ×N  ├───────────▶ Verifier ├────────▶ Assembler ├─▶ draft review
            └──────────┘            └──────┬──────┘  coverage  └──────────┘         └───────────┘
                                           │ ask
                                     ┌─────▼─────┐
                                     │ Lookup ×k │  (short answers, own context)
                                     └───────────┘
```

### 5.1 Inputs gathered by the app (no model)

Before any agent runs, the app collects these deterministically, through
existing client calls plus a few new ones:

- PR metadata: title, body, author, base/head refs and SHAs, labels.
- The changed files list with status and +/− counts, and per-file patches
  (already loaded by `PrSession`).
- Existing review threads and conversation comments (so nothing is repeated).
- CI: check runs and combined status (already loaded), plus **output
  summary/text and annotations for failed or neutral runs**. Annotations come
  from the new client call `GET /repos/{o}/{r}/check-runs/{id}/annotations`.
- Repo context from the checkout (§6.6): `.github/REVIEW.md`, root
  `AGENTS.md` / `CLAUDE.md`, the skill index.
- The repo's configured commands (§8.1).
- On a re-review: the previous brief, findings and their states, the user's
  answers to earlier questions, and the commits since the last reviewed SHA
  (§5.7).

### 5.2 Scout: orient and map

One agent with read-only tools. Its job is to understand the PR and plan the
review, not to find issues. It produces a **review brief** (structured output
via a `submit_brief` tool, validated by the app):

- **Intent:** what the PR is trying to do, in 2–4 sentences, and whether the
  description matches the diff (unexplained changes are noted).
- **Groups:** related changed files grouped together (e.g. a handler + its
  test + its migration). Each group has a **risk** (high / medium / low), a
  short reason, and its **files**. Generated files, lockfiles, and vendored
  code go in a `skim` group.
- **Leads per group:** specific things to look at that the scout noticed while
  skimming, e.g. "new `fetchUser` looks like `getUser` in `api/users.ts`", or
  "adds `assets/icons/big.png`, compare its size with the siblings". Leads are
  hints, not findings.
- **Checklist assignment:** which checklist items (§5.3) apply to each group.
  The app adds all items that are always required for the file kind, so the
  scout can add items but can't remove the defaults.
- **CI notes:** which failures relate to which group.

Groups are sized so a reviewer's context stays small. The builder picks the
exact limits. A rough target is ≤ ~8 files and ≤ ~1,500 changed lines per
group, splitting big files by hunk ranges if needed.

### 5.3 The checklist

The built-in checklist encodes what a careful human does. Each item says
**what to compare against**, because "look at the code" isn't an instruction a
small model acts on.

| Id | Item | What the reviewer must actually do |
|---|---|---|
| `intent` | Matches the PR's intent | Compare the change with the brief's intent; flag unexplained or unrelated changes. |
| `correctness` | Logic and edge cases | Read the whole changed function (not just the hunk); empty/null/error paths, off-by-one, ordering, concurrency. |
| `callers` | Callers and contracts | For every changed signature, exported symbol, or behavior: grep for call sites and open them. Flag callers that weren't updated and changed contracts. |
| `consistency` | Fits its neighbors | Open 1–3 sibling files in the same directory, or the closest analog elsewhere. Compare naming, file/function structure, error handling, logging, and patterns. Flag divergence. |
| `duplication` | Doesn't duplicate existing code | Grep for similar names and key identifiers or literals; look for existing helpers/utilities that already do this. Flag reimplementations. |
| `tests` | Tested | Is new or changed behavior covered by a test in this PR or an existing one? Were tests deleted or weakened? |
| `api_surface` | Public surface and config | Public APIs, schemas, migrations, config keys, env vars, docs: changed consistently and compatibly? |
| `security` | Security basics | Untrusted input, injection, authz checks, secrets in code or logs. |
| `performance` | Obvious performance traps | Work in loops (I/O, queries, allocations), unbounded growth, N+1. |
| `assets` | Binary/asset sanity | For images and other binaries: compare size, dimensions, and format with the siblings in the same folder (via `asset_stats`, §6.3). |
| `ci` | CI results | Explain failed checks that belong to this group; don't repeat linter output. Only report something CI *didn't* catch, or explain the root cause of a failure. |
| `repo:*` | Repo-specific checks | Items from `.github/REVIEW.md` (§6.6). |

Default assignment by file kind **[call]**: source → all except `assets`;
tests → `intent`, `correctness`, `consistency`; assets → `assets`,
`consistency`; config/CI → `intent`, `api_surface`, `security`; generated,
lock, and vendored → `intent` only (e.g. "lockfile changed without the
manifest"). The builder may tune this table.

### 5.4 Reviewer: one per group, with an enforced coverage log

A reviewer gets: the brief (intent + its own group + leads), its group's
patches, the checklist items assigned to its group, the relevant
`AGENTS.md` / `CLAUDE.md` content (§6.6), the skill index, the
existing threads on its files, and the CI notes for its group.

It works with tools (§6.3), and it reports through two tools:

- `record_finding { path, line or range, side, severity, title, body, evidence[], checklist_item }`
  Severity is one of `bug`, `risk`, `nit`, `question`. **Evidence is
  required:** at least one `{ path, line, note }` pointing at what it compared
  against or traced. A finding without evidence is rejected by the tool with a
  message saying why.
- `record_check { file, item, result: issue | ok | n/a, note }`
  One per (file, assigned checklist item). `ok` and `n/a` need a note saying
  what was checked, e.g. "grepped `parseDate`, 0 other definitions" or
  "siblings use snake_case too".

**Anti-rubber-stamp rule, enforced by the app:** when the reviewer says it's
done (ends its turn without tool calls, or calls `done`), the app checks the
coverage log. If any (file, item) pair is missing, the app replies with the
list of missing pairs and the reviewer continues. After two such rounds the
missing pairs are recorded as `skipped` and shown in the AI tab, so a gap is
visible rather than silent.

Reviewers may run in parallel (default 2, configurable) **[call]**.

#### Challenge pass

A Haiku-class model's typical failure is to finish a high-risk group with
zero findings. When a group the scout marked **high** risk ends with no
`bug`/`risk` finding, the app runs one more short agent on it with an
adversarial framing: "assume this group has a defect a senior reviewer would
catch; find it, or explain with evidence why there isn't one." Its findings go
through the same verifier, so false positives are filtered there **[call]**.

#### Lookup helpers

A reviewer can call `ask_lookup { question }` to spawn a lookup agent with a
fresh context and read-only tools, e.g. "how does this repo usually handle
errors in HTTP handlers?" or "is there an existing utility that formats
durations?". The helper answers in a short, bounded reply (default ≤ 300
words, with file:line references). The reviewer's context then holds the
answer, not the 20 files the helper read. Lookups can't spawn further lookups.

### 5.5 Verifier: drop the weak findings

Each finding (batched by file, at the builder's discretion) goes to a verifier
with a fresh context: the finding, its evidence, the relevant patch, and
read-only tools. The verifier must **re-open the cited locations** and returns
`keep`, `drop`, or `downgrade` (e.g. bug → nit) with a one-line reason.

- Dropped findings are kept in storage and listed under "Dropped by verifier"
  in the AI tab, with the reason, so the user can see what was filtered out.
- The verifier also drops findings that duplicate an existing review thread,
  or a CI failure without adding anything to it.
- Duplicate findings across reviewers (same anchor, same issue) are merged by
  the app before the verifier runs (same path and overlapping lines, then the
  verifier judges whether they're the same issue).

### 5.6 Assembler: the draft review

One agent, no repo tools. Input: brief, kept findings, coverage summary, CI
state. Output via `submit_review_draft`:

- **Summary:** what the PR does, the main issues, and what was checked
  (from the coverage log, so a clean review still says what it looked at).
- **Suggested verdict:** `approve`, `comment`, or `request_changes`, with a
  one-line rationale. Rule given to it: any kept `bug` → `request_changes`
  unless clearly trivial; only `nit`s → `approve` or `comment`.
- It may rewrite finding bodies for tone and concision, but not change their
  anchors or severity.

**Anchoring.** GitHub only accepts inline comments on lines in the diff
(base spec §7.3), and the pending review only holds line comments. File-level
comments are always posted immediately (base spec §7.3), so they can't wait in
a draft. Therefore:

- Every finding must anchor to a line (or range) **in the diff**.
  `record_finding` rejects other anchors with a message telling the agent to
  pick the closest diff line it's about.
- A finding about code outside the PR (e.g. "this duplicates
  `utils/x.ts:40`") is anchored on the PR line that introduces the duplicate,
  and the outside location goes in its evidence.
- A finding that genuinely has no diff line (e.g. "this file should be
  deleted", a binary/asset finding with no patch) becomes a **summary item**.
  It's listed in the AI tab with Keep / Drop like a draft. Keep appends it to
  the review summary body under the file's name, rather than posting a
  file-level comment **[call]**.

### 5.7 Re-review: continue where it left off

The AI session for a PR is persistent (§11). When the PR head has moved:

1. The app records the new head, and the user confirms the checkout again (§7).
2. The **incremental diff** is `last_reviewed_sha..new_head`. If
   `last_reviewed_sha` isn't an ancestor of the new head (force-push), the
   incremental diff falls back to the full PR diff and the AI tab says so.
3. **Prior findings check:** a verifier-style agent gets every previous
   finding that is still open (drafted, kept, or posted) and the current code,
   and marks each `addressed`, `still_present`, or `outdated`. The AI tab shows
   these states. Posted comments that are now addressed are reported in the
   summary. The agent doesn't resolve threads on GitHub.
4. The scout gets its previous brief and the new commits, and updates the
   brief. Only groups touched by the new commits are re-reviewed.
5. The user's earlier answers to `ask_user` questions are passed to every
   agent as known facts, so it doesn't ask the same thing twice.
6. **Pending review pinned to the old head** **[call]**. The app's pending
   review is pinned to one commit (base spec §10.2), and new pending comments
   on a newer diff are blocked. Continue review is allowed anyway. While the
   pending review is still pinned to the old head, Keep on a new draft is
   disabled, and a banner offers the existing choices: submit the pending
   review against its pinned commit, or discard it. After either, Keep works on
   the new head. Old drafts the user never kept are re-checked in step 3 like
   any other open finding. The ones still present are re-anchored by the agent
   onto the new head; if a finding can't be re-anchored, it becomes a summary
   item.

## 6. Agents, context, and tools

### 6.1 Roles

| Role | Context | Tools | Ends with |
|---|---|---|---|
| Scout | PR inputs (§5.1), REVIEW.md, root AGENTS.md | read-only repo + GitHub read | `submit_brief` |
| Reviewer | brief slice, group patches, checklist, scoped AGENTS.md, skill index | read-only repo + bash + GitHub read + `ask_lookup` + `ask_user` + `load_skill` | coverage complete |
| Challenger | like reviewer, adversarial prompt | same as reviewer | `done` |
| Lookup | one question | read-only repo + `load_skill` | short answer |
| Verifier | finding(s) + evidence + patch | read-only repo | verdict per finding |
| Assembler | brief, kept findings, coverage, CI | none | `submit_review_draft` |
| Chat (spawned when the user types; §6.5) | status digest, brief, chat history | reviewer's read-only tools + `record_finding` + pipeline steering tools | user-driven |

All prompts are written for a small model: short, imperative, with an example
of a good finding and a bad one, and they state the evidence rule. The exact
wording is up to the builder. The system prompts live in source as `const`
strings and are covered by snapshot tests, so prompt changes show up in
review.

### 6.2 Budgets

Per agent: max turns (scout 30, reviewer 40, lookup 15, verifier 10 — the
builder may tune these), plus a token budget per whole run, set in settings,
default off. When a budget runs out, the run stops cleanly with what it has,
and the AI tab says why. Token usage per agent and per run is shown in the AI
tab.

**Context management:** tool output is truncated (existing `truncate_middle`,
30k bytes). When an agent's context goes past a threshold of the model's
configured context window (default 70%), old tool results are replaced by
one-line stubs (`[read src/x.rs:1-200, elided]`), keeping tool_use/tool_result
pairs intact (`trim_history` semantics). Summarizing compaction isn't needed
in v1 because agents are short-lived **[call]**.

### 6.3 Tools

**Local repo** (all confined to the repo root, from `qa-sandbox`):

- `read_file { path, offset?, limit? }`: text with line numbers; images
  returned as images (downscaled, existing behavior).
- `glob { pattern }`, `grep { pattern, path?, glob? }`: respect `.gitignore`.
- `list_dir { path }` **[call]**: entries with sizes. This makes "look at the
  siblings" one cheap call.
- `asset_stats { path }` **[call]**: size, image dimensions and format, plus
  the same stats for the other files of the same kind in its directory, and
  the median. This makes the "PNG too big" check deterministic.
- `git_show { rev, path }` / `git_log { path?, n }` / `git_diff { range, path? }`
  **[call]**: read-only git, implemented by the app (not through bash), so they
  work even when bash is blocked.
- `bash { command }`: sandboxed (§8), subject to the approval policy.

**GitHub read** (through the app's existing `GithubClient`, never through
`gh`):

- `gh_pr { }`: metadata and description.
- `gh_pr_files { }`, `gh_pr_patch { path }`.
- `gh_threads { path? }`: existing review threads.
- `gh_checks { }`: runs, status, failing annotations.

**Review output** (writes to app state only, never to GitHub):
`record_finding`, `record_check`, `submit_brief`, `submit_review_draft`,
`done`.

**Interaction:** `ask_user { question, options? }` (§6.4), `ask_lookup
{ question }`, `load_skill { name }` (§6.6), `request_prepare { reason }`
(§8.5).

**Deliberately absent:** file edit/write tools, any GitHub write (comment,
review, approve, merge, resolve). Approve and merge exist in the app's client
(`reviews.rs`, `merge.rs`) but are not wrapped as tools.

### 6.4 Asking the user

`ask_user` posts the question in the AI tab, optionally with quick-reply
buttons for `options`, and parks that agent until the user answers. Other
agents keep running. Prompts tell agents to ask only when the answer changes
the review (e.g. "is empty `links` allowed for drafts?"), not to confirm what
they can check themselves. Answers are stored and reused (§5.7).

### 6.5 One session, and the chat agent

**It looks like a single agent session (user).** The pipeline has many
agents, but the AI tab shows **one timeline**, the way a single agent
transcript reads. Each entry names its stage ("Scout: grouped 6 files into
3", "Reviewer · notifications: read `default-links.ts`, grepped
`withDefaultLinks`…", "Verifier dropped 1 finding"). Entries expand to the
tool calls behind them. Questions, approvals, and findings appear inline in
the same timeline. The user never has to think about which agent is which.

**The chat agent is spawned when the user interrupts (user).** The model for
it is the `/btw` side question in Claude Code / opencode: you ask while the
work keeps going, and it answers without blocking the main work. Unlike
`/btw`, it's a **full session** (user): it has tools, keeps its history
across messages, and can act on the pipeline. No model sits idle in the chat. When the user types a message (during or after the run),
the app spawns the **chat agent**, or wakes it if it already exists for this
session, and it answers in the timeline. It isn't the orchestrator; the app's
code still runs the pipeline. It's the user's handle on it:

- **Sees the state:** it gets a compact status digest from the app (stages,
  groups, findings so far, coverage, pending questions), not the agents'
  transcripts. It reads details through tools only when needed:
  `pipeline_status`, `get_finding`, `get_agent_log { agent }`.
- **Steers:** `add_lead { group, text }` and `add_check { group, item }`
  (for groups that haven't started yet), `note_agent { agent, text }`
  (delivered at that agent's next turn), `rerun_group`, `skip_group`,
  `ask_lookup`, `pause_run` / `resume_run`, `cancel_run`.
- **Relays questions:** when the user answers an agent's `ask_user` question
  in free text instead of the quick replies, the chat agent routes the answer
  to the agent that asked.
- **Reviews too:** it has the reviewer's read-only tools and `record_finding`,
  so "look at the migration again" can produce new drafts directly.

**Interrupting doesn't pause the run** **[call]**. Typing spawns the chat
agent while the pipeline keeps going. The chat agent can pause it when the
user asks ("stop, wait"), or the user can press **Pause** in the tab. A paused
run lets running tool calls finish, then holds every agent before its next
model call.

**After the run**, the same chat agent continues as the follow-up chat, with
its history, so "why did you flag this?" has the whole conversation behind
it. "Why?" on a draft sends that finding to it, quoted. Its history is
compacted like the others' (§6.2), and it persists in SQLite with the session.

### 6.6 Repo context

All read from the checked-out PR head.

- **`.github/REVIEW.md`:** free-form markdown given to the scout and all
  reviewers. Optional convention **[call]**: bullet items under a
  `## Checks` heading become checklist items `repo:1`, `repo:2`, …, assigned
  to every group. A bullet may start with a path glob in brackets to scope it,
  e.g. `- [assets/**/*.png] Images must be ≤ 200 KB and use the same size as
  their siblings.`, in which case the app assigns it only to groups with
  matching files.
- **`AGENTS.md` / `CLAUDE.md`:** the root files go to every agent except the
  assembler. Nested files (e.g. `src/api/AGENTS.md`) go only to reviewers with
  files under that directory. Lines of the form `@path/to/file.md` (Claude Code
  import syntax) are expanded one level deep, confined to the repo.
- **Skills:** `.claude/skills/*/SKILL.md`, frontmatter `name` and
  `description`. Agents see only the index (name + description). `load_skill
  { name }` returns the skill body, and files inside the skill folder can be
  read with `read_file`. Repo skills only in v1; `~/.claude/skills` isn't read
  **[call]**.

**Ship a starter `.github/REVIEW.md` for this repo** as part of the work
**[call]**, written from `AGENTS.md` and the base spec (e.g. "widgets return
action enums, never mutate app state", "PR-scoped events carry `SessionId`",
"no tokens outside the keychain"). It doubles as the example in the docs.

## 7. Local checkout handling (done by the app, not the agent)

### 7.1 Before the run

1. Check the folder is a git repo with a remote whose URL matches the PR's
   `host/owner/repo` (https or ssh forms). Remember which remote.
2. Record the original state: current branch (or the detached SHA), and
   whether the tree is dirty (`git status --porcelain`, including untracked
   files).
3. Show the confirmation dialog (§4 step 2) with the exact commands.
4. If dirty: `git stash push --include-untracked -m "gh-review AI #<n>"`, and
   record the **stash commit SHA** (not `stash@{0}`, which can shift).
5. `git fetch <remote> pull/<n>/head` (GHES: same refspec).
6. Check out the PR head **detached** at the fetched SHA **[call]**. This
   avoids colliding with, or modifying, a local branch of the same name. The
   dialog still shows the PR's branch name.
7. Verify `git rev-parse HEAD` equals the PR's head SHA. If not, abort and
   restore.
8. Run the repo's **prepare commands** (§8.5), if any, outside the sandbox
   with the user's credentials. These are listed in the confirmation dialog
   alongside the git commands.

The original state is written to SQLite **before** step 4 so a crash can be
recovered from.

### 7.2 During the run

The agent can't change git state (§8: `.git/` isn't writable inside the
sandbox), so `HEAD` stays put. Builds may write into the working tree (e.g.
`target/`, `node_modules/`, generated files).

### 7.3 Restore

On Finish, Cancel, or app quit (not on leaving the PR screen; §4 step 7):

1. `git status --porcelain`. If there are changes to **tracked** files or new
   untracked files that aren't ignored (something the build or agent
   produced), show them and ask: discard them, or stop and leave the checkout
   as is.
2. `git checkout <original branch or SHA>`.
3. If a stash was made: `git stash apply <stash SHA>`, and on success drop
   that stash entry. On conflict: stop, leave the stash in place, and tell the
   user exactly what state the repo is in and which stash holds their changes.
   Never force anything.

**Crash recovery:** on startup, if SQLite has an unrestored checkout, the app
offers to restore it.

Git runs as a child process (`git` on PATH) using the user's normal
environment and credentials. This is the app's own action, not the agent's,
so the sandbox doesn't apply.

## 8. Safety

Matching command text can't stop `git push` or `gh pr merge`: `sh -c`, a
script the agent writes, or a Python one-liner gets around it. So command
matching is only used to decide **ask vs. run**. The guarantee comes from the
agent having **no credentials** and a **sandbox** that can't write to `.git/`.

### 8.1 Approval policy (UX layer)

A command runs without asking if it's:

- a read-only command from a built-in allowlist: `ls`, `cat`, `head`, `tail`,
  `wc`, `grep`/`rg`, `find`, `file`, `stat`, `du`, `git log/show/diff/status/
  blame/grep/ls-files`, …, with no redirection to a file outside the temp dir;
  or
- one of the repo's configured commands (§4 step 1), matched as a prefix,
  e.g. `cargo test`, `npm run lint`.

Anything else waits for approval in the AI tab: **Once**, **For this review**,
or **No** (with an optional reason sent back to the agent). Pipelines and
`&&` chains are allowed only if every part is allowed. The parser is
conservative: anything it can't classify asks. **Always** (persist for the
repo) is just adding the command to the repo's configured commands in
settings **[call]**.

The classifier is the `review-approval` crate (`crates/approval`), a tree-sitter
bash parser plus a rule engine ported from `da`. `classify(command, config)`
returns `Run`, `Ask` or `Deny`: parse, then deny paths (`.env*`, `.ssh`, ...),
then the rule policies (read-only commands, read-only `git`, the repo's
configured commands, `mkdir` in the checkout), then ask. It fails closed:
anything the parser or a policy can't account for asks. `Deny` is returned to
the agent as a tool error without prompting.

### 8.2 Sandbox (the actual guarantee)

Every bash command runs inside an OS sandbox:

- **macOS:** `sandbox-exec` with a generated Seatbelt profile.
- **Linux:** `bwrap` (bubblewrap). If `bwrap` isn't installed, bash is
  disabled for the run, the AI tab says why, and the review continues with the
  other tools **[call]**. Landlock as an alternative is the builder's choice.

Profile:

| | Rule |
|---|---|
| Network | **allowed** |
| Write | the repo working tree **except `.git/`**; a per-run temp dir (exported as `TMPDIR`); package/build caches: `~/.cargo/registry`, `~/.cargo/git`, `~/.npm`, `~/.cache`, `~/Library/Caches`, `~/go/pkg/mod`, `~/.gradle`, plus a user-editable list in settings |
| Read | everything except the denied paths below |
| Read denied | `~/.ssh`, `~/.config/gh`, `~/.git-credentials`, `~/.netrc`, `~/.aws`, `~/.docker/config.json`, keychain databases, the app's own config dir, plus a user-editable list |

### 8.3 Clean environment

The bash session starts from a built environment, not the app's. It's
`PATH`, `HOME`, `LANG`, `TERM`, `TMPDIR`, `USER`, `SHELL`, plus an allowlist
the user can extend, with:

- no `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, or `SSH_AUTH_SOCK`
- `GH_CONFIG_DIR` pointing at an empty temp dir
- `GIT_TERMINAL_PROMPT=0`, `GIT_CONFIG_NOSYSTEM=1`
- `GIT_CONFIG_GLOBAL` pointing at a generated config that copies `user.name`
  and `user.email` but sets `credential.helper=` (empty)

### 8.4 Why this holds

- **Merge needs a GitHub credential.** The agent has none in its env, can't
  read the credential stores, and has no merge tool.
- **Commit, reset, and checkout need to write `.git/`.** The sandbox denies
  it.
- **The app's GitHub token** is used only by the app's client inside the
  worker, never put in the agent's environment, and never shown to a model.

Known gap (accepted, team-only PRs): with network on, code run by a build
could send repo source elsewhere.

### 8.5 Private dependencies

The team's repos pull private dependencies (cargo git deps, private npm/Go
modules over ssh or https). Inside the sandbox there are no credentials, so
fetching them would fail. The workaround **[call]**: **the app fetches
dependencies before the agent starts, outside the sandbox, as the user.**

- Each repo has **prepare commands** in settings, e.g. `cargo fetch`,
  `npm ci`, `go mod download`. The first-time command suggestion (§4 step 1)
  proposes them too. They run after checkout (§7.1 step 8) in the user's normal
  environment, with output shown in the AI tab. A failure stops the start and
  shows the output.
- After that, the agent's builds and tests run from the filled caches
  (`~/.cargo/registry`, `~/.cargo/git`, `node_modules`, `~/go/pkg/mod`), which
  are writable in the sandbox (§8.2). For cargo, the agent's environment sets
  `CARGO_NET_OFFLINE=true` **[call]** so a build fails fast with a clear
  error instead of trying to fetch.
- **Mid-run:** if a build fails for a missing dependency, the agent calls
  `request_prepare { reason }`. The app shows an approval card ("re-run
  prepare commands: `npm ci`?"), and on approval runs the *configured* prepare
  commands outside the sandbox. The agent can't choose what runs there; it can
  only ask for the fixed list. A re-review re-runs prepare after its checkout.
- **Accepted risk:** prepare commands execute the PR's dependency manifests
  with the user's credentials (e.g. npm install scripts). That's acceptable
  for team-only PRs (§2). The user can add `--ignore-scripts` etc. to the
  command.

**Deferred alternative,** if prepare isn't enough (e.g. builds that fetch at
build time in ways prepare can't cover): a **read-only git broker** run by
the app outside the sandbox. It's a local proxy that forwards only
`git-upload-pack` (fetch/clone) over ssh and https with the user's
credentials, and refuses `git-receive-pack` (push). The sandboxed git is
pointed at it via `GIT_SSH_COMMAND` / `url.<base>.insteadOf`. That keeps "the
agent can't push" while allowing private fetches. It isn't in v1.

Registry tokens in files like `~/.npmrc` and `~/.cargo/credentials.toml` stay
readable in v1, since some builds need them. They could allow publishing a
package but not merging. The user can add them to `extra_deny_read` (§10).

## 9. Architecture

### 9.1 Modules **[call: names and layout]**

```
src/ai/                    #[cfg(unix)] — whole tree
  llm/                     copied qa-llm (trimmed, §3)
  sandbox/                 copied qa-sandbox local ops + PTY bash + new profile.rs, env.rs
  tools/                   copied tools + new: list_dir, asset_stats, git_*, gh_*, review tools
  agent.rs                 the agent loop (§9.2)
  pipeline.rs              scout → reviewers → challenger → verifier → assembler orchestration
  prompts.rs               system prompts (const), snapshot-tested
  checklist.rs             built-in items, file-kind defaults, REVIEW.md parsing
  context.rs               REVIEW.md / AGENTS.md / skills discovery and scoping
  approval.rs              command classifier (§8.1)
  checkout.rs              git stash/fetch/checkout/restore (§7)
  store.rs                 SQLite (§11)
src/state/ai_review.rs     AI tab state + reducers (pure, unit-tested)
src/ui/widgets/ai_panel.rs the AI tab
src/ui/widgets/…           inline AI draft rows in diff_view, ✦ tree markers, checkout dialog, Settings → AI page
```

### 9.2 The agent loop

A rewrite of `qa-agent`'s loop without the DB/gRPC: build the request (system,
history, tools, cache hints on system + tools for Anthropic) → stream →
forward deltas as events → execute tool calls **in order** (the same as the
original) → append results → repeat until no tool calls, `done`, or the turn
or token budget runs out. Uses a `CancellationToken` checked around the stream
and every tool call. `ask_user` and approvals are tool calls that await a
oneshot channel answered from the UI.

### 9.3 Fitting the UI ⇄ worker split

Following the `runtime.rs` pattern (`AGENTS.md`):

- **Commands:** `AiStart { session, … }`, `AiAnswer { session, question_id,
  answer }`, `AiApprove { session, approval_id, decision }`, `AiUserMessage` (spawns or wakes the chat agent), `AiPause`, `AiResume`,
  `AiCancel`, `AiFinish`, `AiContinue`, plus checkout commands.
- **Events:** progress (stage/agent started/finished), streamed text for the
  follow-up chat, `AiQuestion`, `AiApprovalRequest`, `AiFinding` (new or state
  changed), `AiCoverage`, `AiDraftReady`, `AiUsage`, `AiError`, checkout
  results.
- **Every AI event carries the `SessionId`** (stale-event rule). An AI run
  isn't cancelled just because the user navigates away. It keeps running in
  the worker, its results go to SQLite, and when the PR is opened again the
  AI tab loads from SQLite and re-attaches to the live run **[call]**. Only
  Finish, Cancel, or quitting the app stops it.
- GitHub reads by agents go through the worker's existing `GithubClient`
  (reads may be retried; there are no writes).

## 10. Settings

In `settings.toml` (no secrets):

```toml
[ai]
max_parallel_reviewers = 2
run_token_budget = 0            # 0 = unlimited
context_threshold = 0.7

[[ai.providers]]
name = "work"
api = "anthropic-messages"      # or "openai-responses"
base_url = "https://llm.example.com"
headers = {}                    # optional extra headers

[ai.models.default]
provider = "work"
model = "model-id"
context_window = 200000
max_tokens = 8192
reasoning = "medium"            # optional: low | medium | high

# optional: a cheaper model for lookup helpers
# [ai.models.lookup]

[[ai.repos]]
repo = "github.com/owner/name"
path = "/Users/me/src/owner/name"
commands = ["cargo test", "cargo clippy", "cargo fmt --check"]
prepare = ["cargo fetch"]       # run outside the sandbox as the user (§8.5)
extra_write_paths = []
extra_deny_read = []
```

API keys: keychain service `gh-review-desktop`, account `llm:<provider name>`.

**Settings → AI** page: providers (add/edit/remove, "Test connection" makes
one tiny request), model selection, per-repo folders and commands.

## 11. Storage (SQLite)

`<config>/gh-review-desktop/ai.sqlite`, via `rusqlite` with bundled SQLite and
WAL mode. Rough schema **[call]** (the builder owns the exact columns):

- `ai_sessions (id, host, owner, repo, number, created_at, last_reviewed_sha, brief_json)`: one per PR.
- `ai_runs (id, session_id, kind full|incremental, base_sha, head_sha, status, started_at, ended_at, usage_json, stop_reason)`.
- `ai_messages (run_id, agent_id, role, seq, content_json)`: transcripts, for the timeline's expandable details and the chat agent's history.
- `ai_findings (id, session_id, run_id, path, anchor_json, severity, title, body, evidence_json, checklist_item, state, state_reason, pending_local_id)`: `state` ∈ draft, kept, dropped_by_user, dropped_by_verifier, posted, addressed, still_present, outdated.
- `ai_coverage (run_id, file, item, result, note)`.
- `ai_qa (session_id, question, answer, asked_at)`: the user's answers.
- `ai_checkout (repo, original_ref, stash_sha, pr_number, state)`: crash recovery (§7.3).

"Posted" is inferred when the pending review containing a kept finding is
submitted successfully.

No secrets in SQLite. Prompts and tool output are stored (they're the user's
own code). Logs carry only ids, kinds, and counts (same rule as `qa-llm`).

## 12. UI

Layout option A (chosen from mockups):

- **Top bar:** **✦ AI review** button (on unix builds with AI configured).
  It becomes **Continue review** when the head has moved since the last run.
- **Right panel → ✦ AI tab**, next to Overview / Conversation / Commits /
  Checks:
  - **one timeline** that reads like a single agent session (§6.5): stage
    entries with expandable tool calls, chat replies, questions (with quick
    replies), and command approval cards, in order; a compact stage strip
    above it (scout ✓, reviewer 2/3 ●, …)
  - findings summary: counts by severity, "Dropped by verifier (n)",
    "Skipped checks (n)", and on re-review prior findings with their states
  - token usage
  - chat input (typing spawns the chat agent), and Pause / Finish / Cancel
- **Diff:** AI drafts as their own rows, in a distinct ✦ style (purple in the
  mockup; the builder picks theme colors via the palette), with severity tag,
  body, evidence links (click opens that file/line if it's in the PR,
  otherwise shows the snippet), and **Keep / Edit / Drop / Why?**.
- **File tree:** ✦ count next to files with open drafts.
- **Checkout dialog** (§4 step 2) and **restore dialog** (§7.3).
- **Settings → AI** (§10).

Shots (`src/ui/shots.rs`) get the AI tab, an inline draft, and the checkout
dialog.

## 13. Errors

- **LLM errors:** `qa-llm` retries (429/5xx/timeouts). A failed agent fails
  its stage. Reviewer failures don't fail the run: the group is marked failed
  and the others continue. Scout or assembler failure ends the run with a
  message, keeping whatever was produced.
- **Tool errors** go back to the model as `is_error` results (existing
  behavior).
- **Git errors** in checkout or restore stop the flow, and the dialog shows
  the git output. The app never retries a git write.
- **Sandbox unavailable** (§8.2): bash disabled, with a message.

## 14. Testing

- **Copied code:** bring over the `qa-llm` tests (wiremock + golden fixtures)
  and the tool/sandbox tests, adjusted to the trimmed modules.
- **Scripted LLM:** a fake provider that replays a scripted sequence of
  replies (tool calls and text). The pipeline tests run scout → reviewer →
  verifier → assembler end to end on a fixture repo (a temp git repo created
  by the test) and assert the draft, the coverage enforcement (a reviewer that
  skips items gets re-prompted, then `skipped`), evidence rejection, anchoring
  rules, and re-review states.
- **Pure units:** the command classifier (§8.1) with a large table of
  allowed / ask cases, including `sh -c`, `;`, `$(...)`, redirections;
  REVIEW.md parsing; checklist assignment; group sizing; anchoring; AI tab
  reducers.
- **Sandbox:** integration tests (unix, local, may be `#[ignore]` on CI if
  `bwrap` isn't installed) that prove the guarantees: writing `.git/HEAD`
  fails, reading `~/.ssh` fails, `env` shows no tokens, network works.
- **Checkout:** temp git repos testing stash/checkout/restore, a dirty tree
  with untracked files, conflicting pop, and crash recovery.
- **Live (`#[ignore]`):** one real review of a small fixture PR against a
  configured provider, the same way `qa-llm`'s live tests work.

## 15. Milestones (suggested)

1. Edition 2024 migration; copy `qa-llm`, tools, and sandbox under
   `src/ai/` (cfg unix); tests green on all three CI OSes.
2. Settings → AI, keychain keys, "Test connection".
3. Checkout and restore with dialogs, SQLite store, crash recovery.
4. Sandbox profile + clean env + approval classifier.
5. Agent loop + tools + a single-agent "review one group" path, to prove the
   loop.
6. Full pipeline (scout, reviewers with coverage, lookups, verifier,
   challenger, assembler), plus the AI tab and inline drafts.
7. Chat agent (steering tools, pause/resume, follow-up); re-review.
8. Starter `.github/REVIEW.md` for this repo; docs.

## 16. Left to the builder

- Exact prompt wording, group-size limits, turn budgets, the default
  checklist-by-kind table.
- Verifier batching.
- The SQLite column details and migrations approach.
- Whether to keep `qa-llm`'s `dns.rs`.
- Linux sandbox: `bwrap` vs Landlock.
- Exact visual styling of AI rows (within the theme palette).
- How much of each agent's transcript the AI tab shows.

## 17. Calls made by the designer (please check)

1. The existing JSON session files stay JSON; only new AI data uses SQLite.
2. The PR head is checked out **detached** at the PR's head SHA, not as a
   local branch.
3. Default checklist-by-file-kind table (§5.3).
4. Two reviewers in parallel by default.
5. A challenge pass on high-risk groups that produced no `bug`/`risk`.
6. No summarizing compaction in v1, just eliding old tool results.
7. New tools: `list_dir`, `asset_stats`, read-only `git_*` outside bash.
8. Typing in the chat during a run doesn't pause it. The chat agent can
   pause on request, and there's a Pause button.
9. REVIEW.md `## Checks` bullets (optionally glob-scoped) become checklist
   items.
10. Only repo skills; `~/.claude/skills` isn't read.
11. A starter `.github/REVIEW.md` for this repo is part of the work.
12. Linux without `bwrap`: bash disabled for the run, rest of the review
    continues.
13. "Always allow" a command = add it to the repo's configured commands.
14. An AI run keeps going when the user navigates away from the PR and
    re-attaches when the PR is reopened. The checkout is held until Finish,
    Cancel, or quit, and while it's held the repo is locked to that PR. After
    a restore, a follow-up message goes through checkout again.
15. Findings must anchor to a diff line. Findings with no diff line become
    summary items, and Keep adds them to the review body instead of posting a
    file-level comment.
16. Continue review is allowed while a pending review is pinned to the old
    head, but Keep is blocked until that pending review is submitted or
    discarded.
17. Private dependencies: the app runs per-repo prepare commands
    (`cargo fetch`, `npm ci`, …) outside the sandbox before the agent starts.
    The agent can ask to re-run them, but only the configured list. A
    read-only git broker is deferred.
18. `CARGO_NET_OFFLINE=true` in the agent's environment.
19. Registry token files (`~/.npmrc`, `~/.cargo/credentials.toml`) stay
    readable by default.
