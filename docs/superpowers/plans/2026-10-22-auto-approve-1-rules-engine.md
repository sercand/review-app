# Bash Auto-Approve 1: Tree-sitter Rules Engine (M2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `review_sandbox::classify` (a hand-rolled tokenizer + 27-word allowlist) with a new `review-approval` crate: a tree-sitter-bash front end feeding a rule engine ported from [amenocturne/da](https://github.com/amenocturne/da), so `cd`, `ls`, `git diff`, `rg … | head` and friends stop prompting, while anything unproven still asks.

**Architecture:** `da`'s engine and policies work on a small `Segment` model (argv, dynamic-word flags, redirects, env assignments). We copy that model, the engine (`engine.rs`) and the rule policies (`policies.rs`) nearly verbatim and replace only the hand-written lexer/parser (`shparse`) with a tree-sitter-bash front end that emits the same `Vec<Segment>` / `Bail`. Any tree-sitter node kind we don't explicitly handle becomes a `Bail` (fail closed → ask). The crate is pure (no I/O). `Policy` becomes a trait object so per-call data (the repo's allowed commands, deny patterns) can be carried — `da` used process globals for that.

**Tech Stack:** Rust 2024 (rust-version 1.99), `tree-sitter` + `tree-sitter-bash`, no `unsafe`.

**Spec:** `docs/superpowers/plans/2026-10-22-ai-bash-auto-approve.md` (design + decisions D1–D8; this plan is its M2). Upstream reference: `https://github.com/amenocturne/da` (MIT) — clone it to a scratch directory (outside this repo) for the files named below. Its corpus fixtures (`tests/corpus/*.tests`) and `tests/policy_table.rs` are behaviour fixtures we reuse.

## Global Constraints

- Follow `AGENTS.md` (repo root). CI must pass: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Copied `da` code must be made clippy-clean (edition 2021 → 2024); **never** add `#[allow]` to get past a lint other than where this plan says so.
- New crate `crates/approval`, package name `review-approval`, `edition = "2024"`, `rust-version = "1.99"`, `publish = false`, `license = "MIT"`, `[lints.rust] unsafe_code = "forbid"`, same clippy lint block as `crates/sandbox/Cargo.toml`. Root `Cargo.toml` already has `members = ["crates/*"]`.
- Cross-platform (the crate is not `cfg(unix)`; the root crate gates its use). No I/O, no globals, no `unwrap()` outside tests.
- **Fail closed.** Anything the parser or a policy cannot account for yields `Ask`, never `Run`. A tree-sitter `ERROR`/`MISSING` node, or any node kind not in the explicit handled-kinds table, is a `Bail`.
- Attribution: copied `da` code keeps a header `// Adapted from amenocturne/da (MIT), <file>`; `crates/approval/NOTICE.md` carries da's MIT licence text and copyright line `Copyright (c) 2026 amenocturne`; `da`'s `tests/corpus/NOTICE.md` (corpus provenance) is copied alongside the corpus.
- Behaviour that must not regress (from the current `approval.rs` tests): quoted/escaped separators don't split; `$(…)`, backticks, `<(…)`, heredocs, here-strings always ask; unbalanced quotes ask; redirects only to `/dev/null`, `$TMPDIR/…` or a bare fd (`2>&1`); `find -exec/-delete/-fprint` ask; `git push`, `git commit`, `npm install`, `curl … | sh`, `python3 -c` ask; repo `allowed` entries match by exact line or whole-word prefix.
- Intentional behaviour changes versus today (document each in `tests/compat.rs`, Task 7): `cd` runs; bare `FOO=bar` runs; `FOO=1 ls` runs (non-risky assignment + vouched command); `git -C DIR log`, `time ls`, `env FOO=1 ls`, `sh -c 'ls'` run when the inner command is vouched; `ls;;` asks (tree-sitter reports a syntax error).
- Default rule set is conservative: `read_only`, `git:read`, `mkdir_cwd`, plus the repo `allowed` prefixes and the deny rules. `cargo:local`, `git:add/commit/…`, `help_bypass` and `macos_only` exist but are **not** enabled by default (they are only used if a caller passes them).

## Review Focus

- A command with a syntax error or an unsupported construct (`if`, `for`, `case`, subshell `( … )`, `{ …; }`, function definition, `[[ ]]`, arithmetic `$(( ))`, array literal, `&`) → `Ask` with the construct named in the reason, never `Run`; a panic or hang on arbitrary bytes/very long input never happens.
- Multi-line commands (`ls\nrm -rf x`, `\r\n` line endings, trailing backslash-newline continuation) split into segments exactly like `;` and the whole line asks if any segment asks.
- Unicode and odd quoting: `echo 'it'"'"'s'`, `$'\x41'` (ANSI-C quoting, treated as literal), non-ASCII words, NUL byte in the string → either correct `Run` for harmless commands or `Ask`, never misparsed into something approvable.
- Secret paths hidden behind wrappers or redirects: `timeout 5 cat .env`, `xargs cat < .env`, `sh -c 'cat ~/.ssh/id_rsa'`, `ls && cat .env.local` → `Deny` (not a prompt).
- A word that expands at run time in command or flag position (`$CMD args`, `sort $ARGS`, `git log $X`, `ls {a,b}`) never gets approved by a policy that could be fooled by it.

## File Structure

```
crates/approval/
  Cargo.toml
  NOTICE.md                    da's MIT licence + corpus provenance
  src/lib.rs                   public API: classify(), Verdict, Decision, ApprovalConfig, re-exports
  src/model.rs                 Segment, Separator, Redirect, RedirOp, Bail (copied from da shparse/model.rs)
  src/parse/mod.rs             pub fn parse(src) -> Result<Vec<Segment>, Bail>
  src/parse/words.rs           tree-sitter word nodes -> (String value, bool dynamic)
  src/parse/walk.rs            node-kind table, segment building, separators, redirects
  src/engine.rs                adapted from da engine.rs (wrappers, structural rules)
  src/policy.rs                `SegmentPolicy` trait + `Policy` (static fn policies) + impl
  src/policies.rs              adapted from da policies.rs (rule policies; ML removed)
  src/repo.rs                  RepoAllow (prefix rule) and DenyPaths policies, per-call data
  tests/corpus/*.tests         copied from da (+ NOTICE.md)
  tests/policy_table.rs        adapted from da tests/policy_table.rs
  tests/compat.rs              the current approval.rs behaviour table (+ documented changes)
  tests/parse_robustness.rs    arbitrary-input safety
crates/sandbox/src/approval.rs     deleted (Task 8)
crates/sandbox/src/lib.rs          drop `mod approval` + re-exports (Task 8)
src/ai/tools/bash.rs               call review_approval::classify (Task 8)
src/ai/tools/mod.rs                ToolCx gains `deny_paths` (Task 8)
```

## Public API (fixed by this plan; later plans depend on it)

```rust
// review_approval (src/lib.rs)
pub enum Verdict { Run { source: &'static str }, Ask { reason: String }, Deny { reason: String } }

pub struct ApprovalConfig<'a> {
    /// Repo-configured commands: exact line or whole-word prefix, as today.
    pub allowed: &'a [String],
    /// Path-component deny patterns (trailing `*` = prefix), e.g. ".env*", ".ssh".
    pub deny_paths: &'a [String],
    /// Working directory for `mkdir_cwd`; None disables path-relative rules.
    pub cwd: Option<&'a std::path::Path>,
    /// Extra policies consulted after the rules (the model stage plugs in here, plan 2).
    pub extra: &'a [&'a dyn SegmentPolicy],
}
pub fn classify(command: &str, cfg: &ApprovalConfig<'_>) -> Verdict;
pub fn default_deny_paths() -> Vec<String>;

// review_approval::policy
pub trait SegmentPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn verify(&self, seg: &Segment, cwd: Option<&Path>) -> Option<Verdict2>; // Verdict2 = da's {Approve, Deny}
}
```
(`Verdict2` is da's `Verdict` enum; name it `PolicyVerdict { Approve, Deny }` in code to avoid clashing with the public `Verdict`.) `Run.source` is the name of the policy that vouched (`"read-only"`, `"git:read"`, `"repo-allow"`, `"structural"` for cd/assignments, `"ml"` later) so the UI can say why a command ran silently.

## Tasks

### Task 1: Crate skeleton, model, licence

**Files:**
- Create: `crates/approval/Cargo.toml`, `crates/approval/NOTICE.md`, `crates/approval/src/lib.rs`, `crates/approval/src/model.rs`

**Interfaces:**
- Produces: `model::{Segment, Separator, Redirect, RedirOp, Bail}` with the exact definitions in da's `src/shparse/model.rs` (fields `assigns: Vec<(String,String)>`, `argv: Vec<String>`, `dynamic: Vec<bool>`, `redirects: Vec<Redirect>`, `follows: Separator`; methods `is_dynamic(i)`, `has_dynamic()`), plus new `Bail` variants `Syntax` (tree-sitter ERROR/MISSING), `Background`, `FunctionDef`, `Unsupported(&'static str)` (node kind name). Keep da's variants.

- [ ] **Step 1: Write the failing test** in `crates/approval/src/model.rs` (`#[cfg(test)]`): `segment_dynamic_flags_default_literal` asserts a `Segment` with `dynamic: vec![]` reports `is_dynamic(3) == false` and `has_dynamic() == false`; and with `dynamic: vec![false, true]` reports `has_dynamic() == true`.
- [ ] **Step 2: Run** `cargo test -p review-approval model::` → FAIL (crate missing).
- [ ] **Step 3: Create the crate.** `Cargo.toml` mirrors `crates/sandbox/Cargo.toml` metadata and lint blocks; dependencies `tree-sitter` and `tree-sitter-bash` (pin the latest exact versions from crates.io that compile with the repo toolchain: run `cargo add tree-sitter tree-sitter-bash -p review-approval` and keep the resolved versions), `thiserror` only if needed. Copy `model.rs` from da with the added `Bail` variants. `lib.rs` re-exports the model and has a crate doc comment. `NOTICE.md`: da's `LICENSE` text and the corpus provenance note.
- [ ] **Step 4: Run** `cargo test -p review-approval model::` → PASS; `cargo clippy -p review-approval --all-targets -- -D warnings` → clean.
- [ ] **Step 5: Commit** `feat(approval): new review-approval crate with the segment model`.

### Task 2: Tree-sitter front end — words and simple commands

**Files:**
- Create: `crates/approval/src/parse/mod.rs`, `parse/words.rs`, `parse/walk.rs`
- Test: `crates/approval/src/parse/tests.rs` (declare `#[cfg(test)] mod tests;` in `mod.rs`)

**Interfaces:**
- Consumes: `model::*`.
- Produces: `pub fn parse(src: &str) -> Result<Vec<Segment>, Bail>`. A `Parser` is created per call (cheap) from `tree_sitter_bash::LANGUAGE`; a parse failure (`None`) → `Bail::Syntax`.

- [ ] **Step 1: Failing tests** in `parse/tests.rs` (use a helper `argv(src) -> Vec<Vec<String>>`):
  - `simple_command`: `parse("ls -la src")` → one segment, `argv == ["ls","-la","src"]`, `dynamic == [false,false,false]`.
  - `quotes_are_removed`: `parse("grep \"a | b\" 'c; d' e\\ f")` → one segment, argv `["grep","a | b","c; d","e f"]`.
  - `dynamic_words`: `echo $HOME "$X" "a$Y" {a,b} *.rs` → dynamic flags `[false,true,true,true,true,false]` — globs (`*.rs`) are **not** dynamic (matches da: they don't change argv0/flags shape); braces are.
  - `ansi_c_string_is_literal`: `echo $'a\\nb'` → value is the decoded-or-raw literal, `dynamic == false` for the arg (decide: keep raw text without the `$'…'` wrapper; documented in a comment).
  - `assignments_collected`: `FOO=1 BAR="x y" cmd a` → `assigns == [("FOO","1"),("BAR","x y")]`, `argv == ["cmd","a"]`; `FOO=bar` alone → `argv` empty, one assign.
  - `escaped_separators_do_not_split`: `echo a\\; ls` → one segment `["echo","a;","ls"]`.
- [ ] **Step 2: Run** `cargo test -p review-approval parse::` → FAIL.
- [ ] **Step 3: Implement** `parse` + `words.rs`. Approach: `words.rs::word_value(node, src) -> Result<(String, bool), Bail>` handles node kinds `word`, `number`, `string` (double-quoted; children `string_content`, `simple_expansion`/`expansion` → dynamic; `command_substitution` → `Bail::CommandSubstitution`), `raw_string`, `ansi_c_string`, `concatenation` (concatenate children, OR dynamic flags), `simple_expansion`/`expansion` (dynamic, value = source text), `brace_expression`/words containing unquoted braces `{a,b}` (dynamic), `command_substitution` → `Bail::CommandSubstitution`, `process_substitution` → `Bail::ProcessSubstitution`, `arithmetic_expansion` → `Bail::Arithmetic`. Backslash escapes are resolved per bash rules for unquoted words. `walk.rs` has a `const HANDLED: &[&str]` table of every node kind the walker understands; encountering any other kind returns `Bail::Unsupported(kind)`. Any node with `is_error() || is_missing()` (check `tree.root_node().has_error()` first) → `Bail::Syntax`.
- [ ] **Step 4: Run** → PASS. **Step 5: Commit** `feat(approval): tree-sitter words and simple commands`.

### Task 3: Tree-sitter front end — lists, pipelines, redirects, bails

**Files:**
- Modify: `parse/walk.rs`, `parse/tests.rs`

**Interfaces:**
- Produces: `Segment.follows` set from `;`, newline, `&&`, `||`, `|`, `|&`; `Segment.redirects` from `file_redirect` nodes (`>`, `>>`, `<`, `>&`, `<&`, `&>`, `&>>`, `>|`, with optional leading fd) — a `Redirect { fd: Option<u32>, op: RedirOp, target: String }`.

- [ ] **Step 1: Failing tests:**
  - `list_and_pipeline_segments`: `a && b | c; d` → 4 segments with `follows == [And, Pipe, Semi, End]`.
  - `newline_splits_and_trailing_separators_ok`: `"ls\nrm x\n"` → 2 segments; `"ls;"` → 1 segment; `""` and `"  "` → `Ok(vec![])`.
  - `crlf_and_line_continuation`: `"ls\r\nrm x"` → 2 segments (carriage return never ends up in an argv word); `"ls \\\n-la"` → one segment `["ls","-la"]`.
  - `redirect_forms`: `cat x > o`, `2>&1`, `&> f`, `>> f`, `< f`, `>| f`, `2> /dev/null` produce the expected `(fd, op, target)`; `cat x > 2> /dev/null` yields target `"2"` for the first redirect (bash semantics — matches the existing `digit_target_glued_to_next_redirect_asks` test).
  - `bail_table` (one assertion per row, exact `Bail`): `$(x)`→`CommandSubstitution`, `` `x` ``→`CommandSubstitution`, `"$(x)"`→`CommandSubstitution`, `<(x)`→`ProcessSubstitution`, `<<EOF…`→`Heredoc`, `<<<x`→`HereString`, `$((1+1))`→`Arithmetic`, `(a)`→`Subshell`, `{ a; }`→`BraceGroup`, `if a; then b; fi`/`for`/`while`/`case`→`CompoundCommand`, `f() { a; }`→`FunctionDef`, `A=(1 2)`→`ArrayLiteral`, `a &`→`Background`, `echo 'x`→`Syntax`, `ls;;`→`Syntax`, `[[ -f x ]]`→`Unsupported`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** in `walk.rs`: recursive walk over `program`/`list`/`pipeline`/`command`/`redirected_statement`/`variable_assignment`/`negated_command` (`!` → drop, keep command), mapping each to segments; `file_redirect` target via `word_value` (a dynamic target is kept with its source text — the engine rejects non-literal redirect targets). Background (`&` child of `list`/`program`) is a **bail**, unlike da which treats it as `;`. Add a depth cap (64) returning `Bail::Unsupported("depth")`.
- [ ] **Step 4: Run** → PASS. **Step 5: Commit** `feat(approval): lists, pipelines, redirects and bails`.

### Task 4: Differential corpus tests against da's fixtures

**Files:**
- Create: `crates/approval/tests/corpus/*.tests` + `NOTICE.md` (copy from da), `crates/approval/tests/corpus.rs`

- [ ] **Step 1: Write** `tests/corpus.rs` adapted from da's `src/shparse/corpus_tests.rs`: for the `IN_SCOPE` files the new `parse` must return `Ok` **or** a documented bail from an explicit allow-list of constructs we choose to bail on (e.g. `Background`, `Unsupported`) — print every divergence; for `BAIL_EXPECTED` files (`12_command_substitution`, `13_arithmetic`, `14_here_documents`, `15_process_substitution`) cases whose source contains an expansion/heredoc outside single quotes **must** bail. Load fixtures with `include_str!`.
- [ ] **Step 2: Run** `cargo test -p review-approval --test corpus` → FAIL or divergences listed. **Step 3:** fix real parser bugs; for each remaining *intentional* divergence add the construct to the allow-list with a one-line reason. Do not weaken the `BAIL_EXPECTED` assertions. **Step 4: PASS. Step 5: Commit** `test(approval): corpus differential tests`.

### Task 5: Engine and policy trait

**Files:**
- Create: `src/policy.rs`, `src/engine.rs`, `src/policies.rs`
- Test: inline `#[cfg(test)]` in each + Task 6 table

**Interfaces:**
- Consumes: `parse::parse`, `model::*`.
- Produces: `pub enum PolicyVerdict { Approve, Deny }`; `pub trait SegmentPolicy` (see Public API); `pub struct Policy { pub name: &'static str, pub verify: fn(&Segment, Option<&Path>) -> Option<PolicyVerdict> }` with `impl SegmentPolicy for Policy`; `pub enum Decision { Approve { source: &'static str }, Deny, Defer }`; `pub fn classify_stack(cmd: &str, cwd: Option<&Path>, stack: &[&dyn SegmentPolicy]) -> (Decision, Option<String> /*reason*/)`.

- [ ] **Step 1: Failing test** (in `engine.rs`): with an empty stack, `cd src && FOO=bar` → `Approve{source:"structural"}`, `ls` → `Defer`, `echo hi > out.txt` → `Defer`, `PATH=/tmp ls` → `Defer`, `python3 x.py` → `Defer`, `bash -c 'cd x'` → `Approve` (inner vouched structurally), `bash -c 'ls'` → `Defer`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Port** `da/src/engine.rs` and `da/src/policies.rs`: change `Policy` use to the trait (`&[&dyn SegmentPolicy]`), delete `ML`/`ml_verify`/`classifier` and the `ptr::eq` ML skip in `deny_or_defer` (the model stage is added as an ordinary last policy in plan 2, and `deny_or_defer` must skip policies whose `name() == "ml"` — keep that behaviour through the name, not pointer identity), delete `lookup()` and the CLI mapping, add `$TMPDIR/…` and `${TMPDIR}/…` (also quoted `"$TMPDIR"/x`, which the parser already unquotes to `$TMPDIR/x` with `dynamic=true` — special-case the exact prefixes) to `SAFE_REDIRECT_TARGETS` handling in `is_redirect_safe`, return the vouching policy's name for `Approve.source`, and track the first failing segment's text (joined argv) for the reason. Resolve every clippy/edition issue.
- [ ] **Step 4: Run** `cargo test -p review-approval` → PASS. **Step 5: Commit** `feat(approval): engine and rule policies ported from da`.

### Task 6: Port da's policy table; repo allow and deny policies

**Files:**
- Create: `src/repo.rs`, `tests/policy_table.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `pub struct RepoAllow<'a>(pub &'a [String])` and `pub struct DenyPaths<'a>(pub &'a [String])`, both `SegmentPolicy`. `RepoAllow` approves a segment when `argv.join(" ")` equals an entry or starts with `entry + " "` (entries whitespace-normalized, empty ignored; segments with any dynamic word never match — a deliberate tightening over today, where `cargo test $(evil)` already bailed but `cargo test $X` matched). `DenyPaths` returns `Deny` when any argv word or redirect target has a path component matching a pattern (components split on `/`, `~` and `=`; trailing `*` = prefix match; `.env*` catches `.env.local`) — port `token_matches_deny` from da `src/main.rs`.
- Produces: `pub fn default_deny_paths() -> Vec<String>` = `[".env*", ".ssh", ".aws", ".gnupg", ".netrc", ".npmrc", ".pypirc", ".kube", ".docker", "id_rsa*", "id_ed25519*", "*.pem", "*.p12", ".git-credentials", "Library/Keychains"]` (`*.pem`-style leading-star patterns: support a leading `*` suffix match too).
- Produces: `classify(command, cfg) -> Verdict` in `lib.rs`: builds the stack `[DenyPaths, RepoAllow, READ_ONLY, GIT_READ, MKDIR_CWD, ..cfg.extra]` (deny first), empty/whitespace command → `Run{source:"empty"}`; `Decision::Defer` → `Ask{reason}`, `Decision::Deny` → `Deny{reason}`; a `Bail` → `Ask{reason}` naming the construct in plain words (`"command substitution"`, `"heredoc"`, `"unsupported shell construct: if/for/while"`, `"syntax error"`, …).

- [ ] **Step 1: Failing tests.** (a) `tests/policy_table.rs`: copy da's `tests/policy_table.rs`, rename imports (`dabin::` → `review_approval::`), drop the ML/`--path` CLI-only sections; keep the tables verbatim. (b) `repo.rs` tests: `allow_prefix_boundaries` (`cargo test` entry: `cargo test --locked` runs; `cargo testify`, `cargo` ask — ported from today's test), `allow_rejects_dynamic_word` (`cargo test $X` asks), `deny_beats_allow` (entry `cat`, command `cat .env` → `Deny`), `deny_inside_wrapper` (`timeout 5 cat .env`, `sh -c 'cat ~/.ssh/id_rsa'`, `ls && cat .env.local` → `Deny`), `deny_redirect_target` (`echo x > .env` → `Deny`).
- [ ] **Step 2: Run** `cargo test -p review-approval` → FAIL. **Step 3: Implement** as specified above. **Step 4: PASS.** **Step 5: Commit** `feat(approval): repo allow + deny-path policies and public classify`.

### Task 7: Compatibility table and robustness

**Files:**
- Create: `tests/compat.rs`, `tests/parse_robustness.rs`

- [ ] **Step 1: `tests/compat.rs`:** port the whole `classify_table` and every named test from `crates/sandbox/src/approval.rs` (`quoted_separators_do_not_split`, `escaped_separators_do_not_split`, `unbalanced_quotes_and_dangling_escape_ask`, `redirect_targets`, `substitutions_and_heredocs_always_ask`, `find_flags`, `allowed_prefix_boundaries`, `input_redirection_is_read_only`, `digit_target_glued_to_next_redirect_asks`, `allowed_beats_leading_assignment`) onto `review_approval::classify`. Rows whose outcome changes intentionally (see Global Constraints) go in a separate `#[test] fn intentional_changes()` with a comment per row. `reason_names_first_failing_segment` becomes: `classify("ls; rm -rf /")` is `Ask` and its reason contains `rm -rf /`.
- [ ] **Step 2: `tests/parse_robustness.rs`:** (a) a seeded pseudo-random generator (no new dependency: xorshift) builds 5 000 strings of up to 200 bytes from a bash-heavy alphabet (`'"\\$(){}[]<>|&;` newline, letters, `=`, `~`, unicode) and asserts `classify` never panics and never returns `Run` for any string that contains an unquoted `$(` — use a tiny reference check `contains_unquoted_cmd_sub` to select those inputs; (b) 100 000-character command and 5 000-deep nested parens `((((…))))` return within 1 s (assert `elapsed < Duration::from_secs(1)`), result `Ask`; (c) NUL byte and invalid-UTF-8-adjacent input (`"ls \u{0}"`) → `Ask`.
- [ ] **Step 3: Run** `cargo test -p review-approval` → fix real failures in the engine/parser (not the tests). **Step 4: PASS. Step 5: Commit** `test(approval): compat table and robustness`.

### Task 8: Wire the bash tool, delete the old classifier

**Files:**
- Modify: `src/ai/tools/bash.rs`, `src/ai/tools/mod.rs` (ToolCx), `src/ai/mod.rs` (declare `approval_setup`), `src/ai/tools/tests.rs`, `crates/sandbox/src/lib.rs`, root `Cargo.toml` (`review-approval = { path = "crates/approval" }` under `[target.'cfg(unix)'.dependencies]`)
- Delete: `crates/sandbox/src/approval.rs`
- Modify: wherever `ToolCx { … }` literals are built (compile errors will list them; `src/ai/testing.rs`, `pipeline.rs`, `tools/tests.rs`).

**Interfaces:**
- Consumes: `review_approval::{classify, ApprovalConfig, Verdict, default_deny_paths}`.
- Produces: `src/ai/approval_setup.rs` with `#[derive(Clone, Default)] pub struct ApprovalSetup { pub deny_paths: Vec<String> }` and `ApprovalSetup::effective_deny_paths(&self) -> Vec<String>` (= `default_deny_paths()` + the user's, de-duplicated); `ToolCx.approval: Arc<ApprovalSetup>` (one new field so plan 2 can extend the struct without touching every `ToolCx { … }` literal again — there are 8: `agent.rs`, `tools/gh.rs`, `tools/interact.rs`, `tools/prior.rs`, `chat.rs`, `pipeline.rs`, `stages/scout.rs`, `tools/tests.rs`; the compiler lists them). `ToolOutput` for a denied command reads `the command was blocked: <reason>` (`ToolOutput::error`), and the approval dialog is **not** shown. Silent `Run`s are logged with `tracing::debug!(source, command)`; no UI event in this plan.

- [ ] **Step 1: Failing tests** in `src/ai/tools/tests.rs` (follow the existing `ScriptedInteraction` style): `bash_runs_cd_and_read_only_without_asking` (`NoInteraction`, commands `cd . && ls` and `git status` in the temp repo run and return output — pre-change they were denied by `NoInteraction`), `bash_denies_secret_paths_without_prompting` (`cat .env` with a `ScriptedInteraction` that has **no** queued approvals → error text contains `blocked`, and the interaction saw zero approval requests), `bash_still_asks_for_unknown_commands` (existing `touch x` tests keep passing unchanged).
- [ ] **Step 2: Run** `cargo test --lib ai::tools::tests::bash` → FAIL. **Step 3: Implement:** `approve()` builds `ApprovalConfig { allowed: &allowed_snapshot, deny_paths: &cx.approval.effective_deny_paths(), cwd: Some(&cx.repo_root), extra: &[] }`, matches `Verdict::{Run, Ask, Deny}`; keep the `for_review_contains` exact-line check first. Remove `classify`/`Verdict` from `review-sandbox` (`lib.rs` doc comment mentions it — update). Update the module doc comment of `bash.rs` and the bash tool description (the `cd` advice stays; it is now merely unnecessary).
- [ ] **Step 4: Run** `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → all PASS (also `cargo clippy --workspace --all-targets --features ai-testing -- -D warnings`). **Step 5: Commit** `feat(ai): auto-approve bash commands the rule engine can prove safe`.

### Task 9: Docs and repo checklist

**Files:**
- Modify: `AGENTS.md` (Architecture → AI review paragraph: `crates/approval`, fail-closed rule), `.github/REVIEW.md` `## Checks` (add: "No change may make `review_approval::classify` return `Run` for a command containing an unhandled construct; every new policy needs a table test and a deny/defer counter-example"), `docs/superpowers/specs/2026-10-20-ai-review-design.md` §8.1 (one paragraph pointing at the new engine)
- Test: the existing `context_parses_this_repos_review_md` test.

- [ ] **Step 1:** Make the doc edits. **Step 2:** `cargo test --lib context_parses_this_repos_review_md` → PASS. **Step 3:** Manual check (needs a built app; record result in the commit message): `cargo run`, start an AI review on any PR, observe that `cd`, `ls`, `rg … | head`, `git diff` do not prompt and `touch x` does. **Step 4: Commit** `docs: describe the approval engine`.

## Self-review notes

- Coverage vs. the design doc's M2: parser (T2–T4), policies/deny/repo-allow (T5–T6), verdict + sources (T6), tests/fuzz (T7), switch + delete old (T8), docs (T9).
- Names consistent with plan 2: `SegmentPolicy`, `PolicyVerdict`, `ApprovalConfig.extra`, `Verdict::Run { source }`, `Decision`.
- `da`'s `--path` mkdir policy needs the cwd: wired through `ApprovalConfig.cwd`.
