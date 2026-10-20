# AI Review — Plan 1: Foundation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lay every non-agent foundation the AI reviewer needs: edition 2024, the copied LLM client and sandbox as workspace crates, the credential-free sandboxed shell, the command approval classifier, AI settings and API keys (with a Settings → AI page and "Test connection"), the SQLite store, and the app-run git checkout/restore.

**Architecture:** The repo becomes a Cargo workspace. `crates/llm` (from `qa-llm`) and `crates/sandbox` (from `qa-sandbox`, plus new `env`, `profile`, `approval` modules) are standalone libraries with their own tests. App-side code lives in `src/ai/` behind `#[cfg(unix)]` (`llm.rs` client builder, `store.rs` SQLite, `checkout.rs` git). Settings data structs are cross-platform so a Windows build never drops the `[ai]` table. Plans 2 (agent + pipeline) and 3 (UI, chat, re-review) build on this.

**Tech Stack:** Rust edition 2024 (rust-version 1.99), tokio, reqwest 0.13, async-openai 0.42, portable-pty, `sandbox-exec` (macOS) / `bwrap` (Linux), rusqlite (bundled), keyring, egui 0.37.

**Spec:** `docs/superpowers/specs/2026-10-20-ai-review-design.md` (cited as "spec §N"). Source code to copy: `/Users/sercand/Developer/src/github.com/otsimo/automated-qa-framework/crates/{qa-llm,qa-sandbox}`.

## Global Constraints

- Edition `2024`, `rust-version = "1.99"` for every package in the workspace.
- CI (`.github/workflows/ci.yml`) must stay green on Linux, macOS **and Windows**: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- AI code is unix-only: app modules under `src/ai/` are `#[cfg(unix)]`; unix-only crates go in `[target.'cfg(unix)'.dependencies]`. Settings structs are **not** cfg-gated.
- LLM APIs: only Anthropic Messages and OpenAI Responses. No Chat Completions, no decisions API.
- Secrets: API keys only in the OS keychain, service `gh-review-desktop`, account `llm:<provider name>`. Never in `settings.toml`, SQLite, logs, or the agent's environment.
- The agent's shell never sees `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, or `SSH_AUTH_SOCK`, and can't write `<repo>/.git/`.
- The copied code is the user's own and is relicensed MIT; each copied crate gets a `README.md` line saying it was copied from `otsimo/automated-qa-framework` by its author and is MIT-licensed here.
- Every `pub` item has a doc comment (the repo's existing style). Unit tests inline; shared helpers from `src/test_support.rs` (don't redefine `tempdir()`).
- Git operations by the app run `git` from `PATH` with the user's normal environment; they are never retried.

## Review Focus

1. **macOS path aliases in the Seatbelt profile.** `/var`→`/private/var`, `/tmp`→`/private/tmp`: an uncanonicalized path silently fails to match and blocks the repo or allows nothing. Expect all policy paths canonicalized. Test in Task 6.
2. **Paths with spaces, quotes or backslashes** in the repo or home path breaking the Seatbelt string. Expect correct escaping. Test in Task 6.
3. **Another stash pushed after ours** (by the user, mid-review) shifting `stash@{0}`. Restore must apply *our* stash by SHA and drop the right entry. Test in Task 11.
4. **Original state is a branch with slashes, or a detached HEAD.** Restore must return to exactly that. Test in Task 11.
5. **A settings file with an unknown `api` value** (e.g. hand-edited `chat-completions`). Today a parse error resets *all* settings to defaults; expect the file to load, with that provider marked unsupported. Test in Task 8.

---

### Task 1: Edition 2024 and workspace

**Files:**
- Modify: `Cargo.toml`
- Modify: `.github/workflows/ci.yml`
- Modify: `AGENTS.md` (Commands section: `--workspace`)
- Modify: any `src/**` / `tests/**` the migration touches

**Interfaces:**
- Produces: a workspace root with `members = ["crates/*"]` that later tasks add crates to.

- [ ] **Step 1: Run the edition migration on the 2021 tree**

Run: `cargo fix --edition --all-targets --allow-dirty`
Expected: completes; review the diff (likely `if let` scrutinee / tail-expression temporaries, RPIT `use<..>` bounds).

- [ ] **Step 2: Bump the manifest**

In `Cargo.toml` set `edition = "2024"`, add `rust-version = "1.99"`, and add:

```toml
[workspace]
members = ["crates/*"]
resolver = "3"
```

Create an empty `crates/` (add `crates/.gitkeep`; it's removed in Task 2).

- [ ] **Step 3: Reformat under style edition 2024**

Run: `cargo fmt` — expect a large mechanical diff (import sorting). Commit it separately from Step 1/2 so reviewers can skip it.

- [ ] **Step 4: Update CI and AGENTS.md**

In `ci.yml`: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. The perf step stays `cargo test --release -- --ignored`. Mirror in `AGENTS.md` Commands.

- [ ] **Step 5: Verify**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS, no new warnings.

- [ ] **Step 6: Commit**

```bash
git commit -am "build: migrate to edition 2024 and a cargo workspace"
git commit -am "style: rustfmt for style edition 2024"   # the Step 3 diff
```

---

### Task 2: Copy `qa-llm` as `crates/llm`

**Files:**
- Create: `crates/llm/` ← copy of `qa-llm/{src,tests}` (all files, all fixtures)
- Create: `crates/llm/Cargo.toml`, `crates/llm/README.md`
- Delete: `crates/.gitkeep`

**Interfaces:**
- Produces: package `review-llm`, lib name `review_llm`, initially the same public API as `qa_llm`.

- [ ] **Step 1: Copy and rename**

Copy the directories. Replace `qa_llm` → `review_llm` in `tests/**` and docs. Write `Cargo.toml` with `name = "review-llm"`, `edition = "2024"`, `rust-version = "1.99"`, `publish = false`, `license = "MIT"`, and the exact dependency versions/features from the source workspace (`async-openai = { version = "0.42.2", default-features = false, features = ["responses","byot","middleware","rustls"] }`, `reqwest = { version = "0.13", default-features = false, features = ["json","stream","rustls"] }`, `tokio = { version = "1.53", features = ["time","macros","rt-multi-thread"] }`, base64 0.22, eventsource-stream 0.2.3, futures 0.3.34, http 1.5, image 0.25.10 (png, jpeg, no default features), rand 0.9, schemars 1.2.3, secrecy 0.10.3 (serde), serde 1.0 (derive), serde_json 1.0, thiserror 2.0, tower 0.5.3 (no default features), url 2.5 (serde); dev: toml 1.1, wiremock 0.6.5). Add `[lints.rust] unsafe_code = "forbid"` and `[lints.clippy] all = { level = "warn", priority = -1 }`.

- [ ] **Step 2: Verify the copy is green as-is**

Run: `cargo test -p review-llm`
Expected: PASS (the `live.rs` tests are `#[ignore]`).

- [ ] **Step 3: Commit**

```bash
git add crates/llm && git rm -q crates/.gitkeep
git commit -m "feat(llm): copy qa-llm as crates/llm"
```

---

### Task 3: Trim `review-llm` to the two chat APIs

**Files:**
- Delete: `crates/llm/src/{decision.rs,decisions.rs,role.rs,config.rs,llm.rs}`, `crates/llm/tests/fixtures/decisions/`
- Create: `crates/llm/src/api.rs`
- Modify: `crates/llm/src/{lib.rs,client.rs,error.rs,types.rs,trace.rs}` and tests that referenced removed items

**Interfaces:**
- Produces (the crate's whole public API after this task):
  - `Client`, `ClientConfig` (unchanged fields; `ClientConfig::new(api: ApiType, key: SecretString)`, `.base_url(Url)`)
  - `ApiType { AnthropicMessages, OpenaiResponses }` with `as_str() -> &'static str` returning `"anthropic-messages"` / `"openai-responses"`
  - `Client::generate(&self, req: GenerateRequest) -> Result<Reply>` and `Client::generate_stream(&self, req: GenerateRequest) -> Result<BoxStream<'static, Result<StreamEvent>>>` (signatures as in the source)
  - `CallMeta { api, provider, model, latency }` (no `role`, no `from_default`)
  - `Error` without `RoleNotConfigured`, `MissingAnswer`, or any decisions-only variant
  - unchanged: `trim_history`, `RetryPolicy`, `ImageFormat`, `ImageOptions`, `Video`, `dns`, and every type in `types.rs`
  - `wire::{anthropic_request, responses_request, anthropic_reply, responses_reply}`

- [ ] **Step 1: Add a test that pins the API names**

In `crates/llm/src/api.rs`:

```rust
#[test]
fn api_names_are_the_settings_spelling() {
    assert_eq!(ApiType::AnthropicMessages.as_str(), "anthropic-messages");
    assert_eq!(ApiType::OpenaiResponses.as_str(), "openai-responses");
}
```

- [ ] **Step 2: Remove the decisions and roles layers**

Move `ApiType` into `api.rs` (drop `Decisions`, `Kind`, all role enums). Remove `Inner::Decisions`, `Client::decide`, `Tag`, and the role fields from `CallCtx`/`CallMeta`. Delete the listed files and their `pub use` lines. Delete or edit tests that exercise removed items (`decide*`, role routing, `LLMConfig` parsing).

- [ ] **Step 3: Verify**

Run: `cargo test -p review-llm && cargo clippy -p review-llm --all-targets -- -D warnings && ! grep -rniE "decision|ChatRole|LLMConfig" crates/llm/src crates/llm/tests`
Expected: PASS and no grep output.

- [ ] **Step 4: Add `crates/llm/README.md`** (provenance + MIT line per Global Constraints) and commit

```bash
git commit -am "refactor(llm): keep only Anthropic Messages and OpenAI Responses"
```

---

### Task 4: Copy `qa-sandbox` as `crates/sandbox` (local half)

**Files:**
- Create: `crates/sandbox/src/{lib.rs,ops.rs,paths.rs,files.rs,search.rs,bash.rs}` ← copied
- Create: `crates/sandbox/tests/{bash.rs,files.rs}` ← copied
- Create: `crates/sandbox/Cargo.toml`, `crates/sandbox/README.md`

**Interfaces:**
- Produces: package `review-sandbox`, lib `review_sandbox`, exporting `SandboxOps`, `LocalHost`, `Root`, `OpError`, `ExecOutput`, `Entry`, `GrepMatch`, `Listing`, `DEFAULT_EXEC_TIMEOUT`, `MAX_FILE_BYTES`, `MAX_GLOB`, `MAX_GREP`, `OUTPUT_CAP`, and `BashSession` (unix).

- [ ] **Step 1: Copy without the connector**

Don't copy `connector.rs` or `tests/connector.rs`; remove its `mod`/`pub use` lines and rewrite the crate doc comment (no server/connector wording). Rename `qa_sandbox` → `review_sandbox`. `Cargo.toml`: async-trait 0.1.92, globset 0.4.20, grep-regex 0.1.14, grep-searcher 0.1.17, ignore 0.4.33, rand 0.9, secrecy 0.10.3, thiserror 2.0, tokio 1.53 (sync, time, macros, rt-multi-thread, process), tokio-util 0.7.20, tracing 0.1; `[target.'cfg(unix)'.dependencies]` tempfile 3.27, nix 0.31 (signal, fs; no default features), portable-pty 0.9; dev: anyhow, serde_json, tempfile. Same lints and package metadata as Task 2. Rename the PTY thread to `"review-sandbox-pty"`.

- [ ] **Step 2: Verify on this OS and that Windows still compiles**

Run: `cargo test -p review-sandbox` → PASS. Run: `cargo check -p review-sandbox --target x86_64-pc-windows-msvc` if the target is installed; otherwise rely on CI. `tests/bash.rs` already has `#![cfg(unix)]`; add it to any other test that needs a shell.

- [ ] **Step 3: README + commit**

```bash
git commit -am "feat(sandbox): copy qa-sandbox's local ops as crates/sandbox"
```

---

### Task 5: Configurable shell launch and the agent environment

**Files:**
- Create: `crates/sandbox/src/env.rs`
- Modify: `crates/sandbox/src/bash.rs` (`BashSession::start`), `crates/sandbox/src/files.rs` (`LocalHost`)
- Test: `crates/sandbox/tests/launch.rs` (`#![cfg(unix)]`)

**Interfaces:**
- Produces:
  - `pub struct Launch { pub program: OsString, pub args: Vec<OsString>, pub env: BTreeMap<OsString, OsString> }`: the shell process to start, with a **complete** environment (the child starts from an empty env plus `env`, then `PS1= PS2= TERM=dumb`). `BashSession` appends `["--noprofile", "--norc"]` to `args`, so plain bash is `program = "bash", args = []` and a sandbox wrapper is e.g. `program = "sandbox-exec", args = ["-p", <profile>, "bash"]`.
  - `Launch::host() -> Launch`: today's behavior (`bash`, host env minus the existing `is_scrubbed` vars).
  - `BashSession::start_with(cwd: &Path, launch: &Launch) -> io::Result<BashSession>`; `start(cwd)` becomes `start_with(cwd, &Launch::host())`.
  - `LocalHost::with_launch(dir: &Path, launch: Launch) -> io::Result<LocalHost>`.
  - `pub struct EnvInput { pub host_env: Vec<(OsString, OsString)>, pub extra_allow: Vec<String>, pub tmp_dir: PathBuf, pub git_user: Option<(String, String)> }`
  - `pub fn agent_env(input: &EnvInput) -> io::Result<BTreeMap<OsString, OsString>>`: writes `<tmp>/gitconfig` and creates `<tmp>/gh/`.
  - `pub const DENIED_ENV: [&str; 5] = ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "SSH_AUTH_SOCK"];`

- [ ] **Step 1: Write the failing tests**

`env.rs` unit tests:

```rust
#[test]
fn agent_env_keeps_only_the_allowlist_and_sets_git_and_gh_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let env = agent_env(&EnvInput {
        host_env: vars(&[("PATH", "/usr/bin"), ("HOME", "/h"), ("GH_TOKEN", "x"),
                         ("SSH_AUTH_SOCK", "/s"), ("AWS_SECRET_ACCESS_KEY", "y"), ("FOO", "1")]),
        extra_allow: vec!["FOO".into(), "GH_TOKEN".into()],   // deny list wins
        tmp_dir: tmp.path().into(),
        git_user: Some(("Ada".into(), "ada@example.com".into())),
    }).unwrap();
    assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
    assert_eq!(get(&env, "FOO"), Some("1"));
    for k in ["GH_TOKEN", "SSH_AUTH_SOCK", "AWS_SECRET_ACCESS_KEY"] { assert_eq!(get(&env, k), None); }
    assert_eq!(get(&env, "GIT_TERMINAL_PROMPT"), Some("0"));
    assert_eq!(get(&env, "GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(get(&env, "CARGO_NET_OFFLINE"), Some("true"));
    assert_eq!(get(&env, "TMPDIR"), tmp.path().to_str());
    assert_eq!(get(&env, "GH_CONFIG_DIR"), tmp.path().join("gh").to_str());
    let cfg = std::fs::read_to_string(tmp.path().join("gitconfig")).unwrap();
    assert!(cfg.contains("name = Ada") && cfg.contains("email = ada@example.com"));
    assert!(cfg.contains("[credential]") && cfg.contains("helper ="));
}
```

The base allowlist is exactly `PATH, HOME, LANG, LC_ALL, LC_CTYPE, TERM, USER, LOGNAME, SHELL`; `GIT_CONFIG_GLOBAL` = `<tmp>/gitconfig`. (`vars`/`get` are local test helpers.)

`tests/launch.rs`:

```rust
#[tokio::test]
async fn start_with_uses_exactly_the_given_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var_os("PATH").unwrap());
    env.insert("MARK".into(), "42".into());
    let host = LocalHost::with_launch(dir.path(), Launch { program: "bash".into(), args: vec![], env }).unwrap();
    let out = host.exec("echo $MARK; echo ${HOME-unset}", false, T).await.unwrap();
    assert_eq!(out.output.trim(), "42\nunset");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p review-sandbox agent_env start_with` → FAIL (items don't exist).

- [ ] **Step 3: Implement** `Launch`, `start_with` (use `CommandBuilder::new(&launch.program)`, `env_clear()`, then `args` + `["--noprofile","--norc"]`, then `env`), `LocalHost::with_launch` (store the `Launch` and use it where the shell is started lazily), and `agent_env`. Re-export `Launch`, `EnvInput`, `agent_env`, `DENIED_ENV` from `lib.rs`.

- [ ] **Step 4: Verify**

Run: `cargo test -p review-sandbox` → PASS (the copied bash tests still pass through `Launch::host()`).

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(sandbox): configurable shell launch and credential-free agent env"
```

---

### Task 6: Sandbox profile (Seatbelt / bubblewrap)

**Files:**
- Create: `crates/sandbox/src/profile.rs`
- Test: inline unit tests + `crates/sandbox/tests/policy.rs` (`#![cfg(unix)]`)

**Interfaces:**
- Consumes: `Launch` (Task 5).
- Produces:
  - `pub struct SandboxPolicy { pub repo: PathBuf, pub tmp: PathBuf, pub write_paths: Vec<PathBuf>, pub deny_read: Vec<PathBuf> }`
  - `pub fn default_write_paths(home: &Path) -> Vec<PathBuf>`: `~/.cargo/registry`, `~/.cargo/git`, `~/.npm`, `~/.cache`, `~/Library/Caches`, `~/go/pkg/mod`, `~/.gradle` (spec §8.2)
  - `pub fn default_deny_read(home: &Path, app_config: &Path) -> Vec<PathBuf>`: `~/.ssh`, `~/.config/gh`, `~/.git-credentials`, `~/.netrc`, `~/.aws`, `~/.docker/config.json`, `~/Library/Keychains`, `app_config`
  - `pub fn seatbelt_profile(policy: &SandboxPolicy) -> String`: pure
  - `pub fn bwrap_args(policy: &SandboxPolicy) -> Vec<OsString>`: pure; only binds paths that exist
  - `#[derive(Debug, thiserror::Error)] pub enum SandboxUnavailable { #[error("bubblewrap (bwrap) is not installed")] NoBwrap, #[error("bubblewrap can't create a sandbox here: {0}")] BwrapBroken(String), #[error("sandboxing isn't supported on this OS")] Unsupported }`
  - `pub fn sandboxed_launch(policy: &SandboxPolicy, env: BTreeMap<OsString, OsString>) -> Result<Launch, SandboxUnavailable>`: macOS → `sandbox-exec -p <profile> bash`; Linux → `bwrap <args> -- bash` after probing `bwrap --ro-bind / / true`.

**Policy decisions to implement exactly:**
- Every path in the policy is canonicalized before use (`std::fs::canonicalize`, skipping missing paths for write/deny lists, keeping `repo`/`tmp` mandatory).
- Seatbelt: `(version 1)(allow default)(deny file-write*)` then one `(allow file-write* ...)` with `(require-all (subpath "<repo>") (require-not (subpath "<repo>/.git")))`, `(subpath "<tmp>")`, each write path, and `/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/ptmx`, `(regex #"^/dev/ttys[0-9]+$")`, `/dev/dtracehelper`; then `(deny file-read* ...)` for each deny path (`subpath` for dirs, `literal` for files). Network is not restricted.
- SBPL string escaping: `\` → `\\`, `"` → `\"`.
- bwrap: `--ro-bind / /`, `--dev /dev`, `--proc /proc`, `--bind <repo> <repo>`, `--ro-bind <repo>/.git <repo>/.git` (when it exists), `--bind <tmp> <tmp>`, `--bind` each existing write path, `--tmpfs <dir>` for each existing deny dir, `--ro-bind /dev/null <file>` for each existing deny file, `--die-with-parent`, `--chdir <repo>`. No `--unshare-net`.

- [ ] **Step 1: Write the failing unit tests**

```rust
#[test]
fn seatbelt_protects_dot_git_and_canonicalizes_paths() {
    let t = tempfile::tempdir().unwrap();          // on macOS this lives under /var → /private/var
    let repo = t.path().join("my \"repo\"");        // quote + space
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let tmp = t.path().join("tmp"); std::fs::create_dir_all(&tmp).unwrap();
    let p = seatbelt_profile(&SandboxPolicy { repo: repo.clone(), tmp, write_paths: vec![], deny_read: vec![] });
    let canon = std::fs::canonicalize(&repo).unwrap().display().to_string().replace('"', "\\\"");
    assert!(p.contains(&format!("(require-not (subpath \"{canon}/.git\"))")));
    if cfg!(target_os = "macos") { assert!(!p.contains("\"/var/")); }
}

#[test]
fn bwrap_binds_repo_rw_and_dot_git_ro_and_hides_denied_dirs() { /* assert the arg pairs listed above, in order --bind repo before --ro-bind repo/.git */ }

#[test]
fn missing_write_and_deny_paths_are_skipped() { /* a nonexistent path appears in neither output */ }
```

- [ ] **Step 2: Run to verify they fail**, then **Step 3: implement** `profile.rs` and re-export from `lib.rs`.

- [ ] **Step 4: Write the guarantee tests** in `tests/policy.rs`

Each test builds a temp repo (`git init`, one commit via `git -c user.name=t -c user.email=t@t commit`), a temp `tmp`, a temp "secret" dir with a file in it put in `deny_read`, then a `LocalHost::with_launch(repo, sandboxed_launch(...))`. When `sandboxed_launch` returns `NoBwrap`/`BwrapBroken`, the test prints `skipping: <reason>` and returns (CI's Ubuntu may block user namespaces).

```rust
#[tokio::test] async fn can_write_in_the_repo_and_tmp()        // echo hi > f && echo hi > "$TMPDIR/f" → exit 0
#[tokio::test] async fn cannot_write_dot_git()                 // touch .git/x → exit ≠ 0; and `git commit --allow-empty -m x` → exit ≠ 0, HEAD unchanged
#[tokio::test] async fn cannot_read_denied_paths()             // cat <secret>/file → exit ≠ 0
#[tokio::test] async fn cannot_write_outside_allowed_paths()   // touch <a sibling dir of repo>/x → exit ≠ 0
#[tokio::test] async fn agent_env_has_no_tokens_inside_the_sandbox()  // with GH_TOKEN in host_env → `printenv GH_TOKEN` exit 1
#[tokio::test] #[ignore = "needs network"] async fn network_is_allowed() // curl -sI https://github.com → exit 0
```

- [ ] **Step 5: Verify**

Run: `cargo test -p review-sandbox --test policy` → PASS on macOS; PASS or skip on Linux.

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(sandbox): seatbelt and bubblewrap policies for the agent shell"
```

---

### Task 7: Command approval classifier

**Files:**
- Create: `crates/sandbox/src/approval.rs`

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum Verdict { Run, Ask { reason: String } }`
  - `pub fn classify(command: &str, allowed: &[String]) -> Verdict`, where `allowed` = the repo's configured commands plus commands approved "for this review". The caller concatenates them.

**Rules (spec §8.1), to implement exactly:**
- Tokenize shell-style (single/double quotes, backslash escapes). Unbalanced quotes → `Ask`.
- Split into segments on unquoted `|`, `&&`, `||`, `;`, `&`, newline. **Every** segment must be `Run`.
- `Ask` on: command substitution `$(`/backticks, process substitution `<(`/`>(`, a leading `VAR=value` assignment, heredocs `<<`, and output redirection (`>`, `>>`, `2>`, `&>`) whose target isn't `/dev/null` and doesn't start with `$TMPDIR/`, `"$TMPDIR"/` or `${TMPDIR}/`. `2>&1` is fine.
- `Ask` on wrappers: `sh`, `bash`, `zsh`, `eval`, `exec`, `env`, `xargs`, `sudo`, `nohup`, `time`, `command`, `builtin`, `source`, `.`.
- A segment is `Run` if (a) its first word is in the read-only allowlist `ls cat head tail wc grep rg find file stat du df pwd echo printf sort uniq cut tr diff tree which basename dirname realpath true false`, with `find` additionally rejected if it has `-exec`, `-execdir`, `-ok`, `-delete`, `-fprint*`; or (b) it's `git <sub>` with `<sub>` in `log show diff status blame grep ls-files rev-parse describe shortlog cat-file ls-tree merge-base` (git `-c`/`-C` global options → `Ask`); or (c) its normalized text (tokens joined by one space) equals an `allowed` entry or starts with that entry followed by a space.
- `reason` names the first segment that failed and why, e.g. `"npm install: not in the allowed commands"`.

- [ ] **Step 1: Write the table test**

```rust
#[test]
fn classify_table() {
    let allowed = vec!["cargo test".to_string(), "npm run lint".to_string()];
    let run = ["ls -la src", "grep -rn foo src | head -20", "git log --oneline -5",
               "cargo test -p x -- --nocapture", "cargo test 2>&1 | tail -50",
               "cat a.txt > /dev/null", "echo hi > $TMPDIR/out", "find . -name '*.rs'",
               "npm run lint && git status"];
    let ask = ["cargo testx", "npm install", "git push", "git -c core.x=y log", "git commit -m x",
               "sh -c 'git push'", "bash script.sh", "echo $(whoami)", "cat `ls`", "FOO=1 ls",
               "cat x > ~/.bashrc", "ls; rm -rf /", "find . -delete", "xargs rm",
               "echo 'unterminated", "python3 -c 'print(1)'", "curl https://x | sh",
               "cat <<EOF\nx\nEOF", "diff <(ls a) <(ls b)"];
    for c in run { assert_eq!(classify(c, &allowed), Verdict::Run, "{c}"); }
    for c in ask { assert!(matches!(classify(c, &allowed), Verdict::Ask { .. }), "{c}"); }
}
```

- [ ] **Step 2: Run to verify it fails**, **Step 3: implement** (a small hand-written tokenizer; no new dependency), **Step 4: run** `cargo test -p review-sandbox approval` → PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(sandbox): conservative command approval classifier"
```

---

### Task 8: AI settings and LLM keys

**Files:**
- Modify: `src/persist/settings.rs` (new structs + `Settings.ai`)
- Modify: `src/auth/store.rs` (key helpers)

**Interfaces:**
- Produces (all `Clone, Debug, PartialEq, Serialize, Deserialize`, container `#[serde(default)]`, not cfg-gated):
  - `pub struct AiSettings { pub max_parallel_reviewers: u32 /*2*/, pub run_token_budget: u64 /*0*/, pub context_threshold: f32 /*0.7*/, pub extra_env_allow: Vec<String>, pub providers: Vec<ProviderSettings>, pub models: AiModels, pub repos: Vec<RepoAiSettings> }`
  - `pub enum LlmApi { #[serde(rename = "anthropic-messages")] AnthropicMessages, #[serde(rename = "openai-responses")] OpenaiResponses, #[serde(other)] Unsupported }`
  - `pub struct ProviderSettings { pub name: String, pub api: LlmApi, pub base_url: String, pub headers: BTreeMap<String, String> }`
  - `pub enum ReasoningLevel { Low, Medium, High }` (serde lowercase)
  - `pub struct ModelSettings { pub provider: String, pub model: String, pub context_window: u32 /*200_000*/, pub max_tokens: u32 /*8192*/, pub reasoning: Option<ReasoningLevel> }`
  - `pub struct AiModels { pub default: Option<ModelSettings>, pub lookup: Option<ModelSettings> }`
  - `pub struct RepoAiSettings { pub repo: String /* "host/owner/name" */, pub path: PathBuf, pub commands: Vec<String>, pub prepare: Vec<String>, pub extra_write_paths: Vec<PathBuf>, pub extra_deny_read: Vec<PathBuf> }`
  - `impl AiSettings { pub fn repo(&self, host: &str, owner: &str, name: &str) -> Option<&RepoAiSettings>; pub fn provider(&self, name: &str) -> Option<&ProviderSettings> }`: `repo` matches case-insensitively
  - `Settings { …, pub ai: AiSettings }`
  - In `auth/store.rs`: `pub fn llm_key(store: &dyn TokenStore, provider: &str) -> anyhow::Result<Option<String>>`, `pub fn set_llm_key(store: &dyn TokenStore, provider: &str, key: &str) -> anyhow::Result<()>`, `pub fn clear_llm_key(…)`. They use host `"llm"` and login `provider`, so the account is `llm:<provider>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ai_settings_parse_the_spec_example() {
    let s: Settings = toml::from_str(SPEC_EXAMPLE).unwrap();   // the §10 block, verbatim
    assert_eq!(s.ai.providers[0].api, LlmApi::AnthropicMessages);
    assert_eq!(s.ai.models.default.as_ref().unwrap().context_window, 200_000);
    assert_eq!(s.ai.repo("GitHub.com", "Owner", "Name").unwrap().prepare, vec!["cargo fetch"]);
}

#[test]
fn an_old_settings_file_without_ai_still_loads() { /* existing fixture text → ai == AiSettings::default(), other fields intact */ }

#[test]
fn an_unknown_api_does_not_reset_the_settings_file() {
    // write a settings.toml with theme = "dark" and a provider api = "chat-completions"; load_or_default
    // → theme is Dark and providers[0].api == LlmApi::Unsupported
}

#[test]
fn ai_settings_round_trip() { /* to_string → from_str equals */ }
```

and in `store.rs`: `llm_keys_use_the_llm_account` (MemoryStore: `set_llm_key(&s, "work", "k")` then `s.get("llm", "work") == Some("k")`).

- [ ] **Step 2: Run to verify they fail**, **Step 3: implement**, **Step 4: verify** `cargo test --lib persist::settings auth::store` → PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(settings): AI providers, models and per-repo settings; LLM keys in keychain"
```

---

### Task 9: LLM client builder and "Test connection" command

**Files:**
- Modify: `Cargo.toml` (`[target.'cfg(unix)'.dependencies] review-llm = { path = "crates/llm" }, review-sandbox = { path = "crates/sandbox" }, secrecy = "0.10.3", url = "2.5"`)
- Create: `src/ai/mod.rs`, `src/ai/llm.rs`
- Modify: `src/lib.rs` (`#[cfg(unix)] pub mod ai;`), `src/runtime.rs`
- Test: `tests/contract_llm_test.rs` (`#![cfg(unix)]`)

**Interfaces:**
- Consumes: `ProviderSettings`, `ModelSettings`, `llm_key` (Task 8); `review_llm::{Client, ClientConfig, ApiType, GenerateRequest}`.
- Produces:
  - `pub fn build_client(p: &ProviderSettings, key: SecretString) -> anyhow::Result<review_llm::Client>`: `LlmApi::Unsupported` → error `"provider <name> uses an unsupported API"`; `base_url` parsed with `url::Url`; `headers` copied; provider name = `p.name`.
  - `pub async fn test_connection(client: &review_llm::Client, model: &ModelSettings) -> Result<String, String>`: one non-streaming `generate` with user text `"Reply with the single word OK."`, `max_tokens: 16`. Returns `Ok(<model from reply meta>)` or `Err(<error display>)`. The error must not contain the key.
  - `Command::AiTestProvider { provider: ProviderSettings, model: ModelSettings }` (cfg unix) → worker reads the key with `llm_key(ctx.store, &provider.name)` (missing → `Err("no API key stored for <name>")`) → `Event::AiProviderTested { provider: String, result: Result<String, String> }`.

- [ ] **Step 1: Write the failing contract tests** (wiremock)

```rust
#[tokio::test] async fn anthropic_provider_hits_v1_messages_with_the_key()   // base_url = server.uri() + "/v1" → POST /v1/messages, header x-api-key: k; returns Ok("model-id")
#[tokio::test] async fn responses_provider_hits_v1_responses_with_bearer()   // POST /v1/responses, Authorization: Bearer k
#[tokio::test] async fn a_401_is_an_error_that_does_not_echo_the_key()       // Err(msg) && !msg.contains("sekrit-key")
#[test]        fn unsupported_api_is_rejected()
```

Use the request/response bodies from `crates/llm/tests/fixtures/{anthropic,responses}/text.response.json` for the mock replies.

- [ ] **Step 2: Run to verify they fail**, **Step 3: implement** `src/ai/llm.rs` and the runtime command/event pair (follow the existing `execute` match style; the command isn't PR-scoped, so no `SessionId`), **Step 4: verify** `cargo test --test contract_llm_test` → PASS, then `cargo test --workspace`.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(ai): build LLM clients from settings; Test connection command"
```

---

### Task 10: SQLite store

**Files:**
- Modify: `Cargo.toml` (`[target.'cfg(unix)'.dependencies] rusqlite = { version = "<latest>", features = ["bundled"] }`; use the newest release on crates.io)
- Create: `src/ai/store.rs`

**Interfaces:**
- Produces:
  - `pub struct Store` (holds `Mutex<rusqlite::Connection>`; `Send + Sync`)
  - `Store::open(path: &Path) -> anyhow::Result<Store>` (creates parents, `journal_mode=WAL`, `foreign_keys=ON`, runs migrations); `Store::open_in_memory() -> anyhow::Result<Store>`
  - `pub fn ai_db_path(config_root: &Path) -> PathBuf` → `<config>/ai.sqlite`
  - `pub struct PrKey { pub host: String, pub owner: String, pub repo: String, pub number: u64 }` (owner/repo stored lowercased)
  - `Store::session_id(&self, key: &PrKey) -> anyhow::Result<i64>`: get-or-create the `ai_sessions` row
  - `pub enum OriginalRef { Branch(String), Detached(String) }`
  - `pub enum CheckoutState { Started, Active, Restored, Failed }` (stored as text `started|active|restored|failed`)
  - `pub struct CheckoutRecord { pub id: i64, pub repo_path: PathBuf, pub repo: String, pub pr_number: u64, pub original: OriginalRef, pub stash_sha: Option<String>, pub head_sha: String, pub state: CheckoutState }`
  - `Store::checkout_begin(&self, rec: &CheckoutRecord) -> anyhow::Result<i64>` (ignores `rec.id`), `checkout_set_stash(id, sha)`, `checkout_set_state(id, CheckoutState)`, `checkout_holding(&self, repo_path: &Path) -> anyhow::Result<Option<CheckoutRecord>>` (state `Started`/`Active`: the repo lock), `checkouts_unrestored() -> anyhow::Result<Vec<CheckoutRecord>>`

**Schema (migration 1, tracked with `PRAGMA user_version`).** Create every spec §11 table now, so Plan 2 needs no migration: `ai_sessions`, `ai_runs`, `ai_messages`, `ai_findings`, `ai_coverage`, `ai_qa`, `ai_checkout`, with the columns listed in spec §11. Columns ending `_json` are TEXT; ids are `INTEGER PRIMARY KEY`; `ai_runs.session_id`, `ai_findings.session_id`, `ai_qa.session_id` reference `ai_sessions(id)`; `ai_messages.run_id`, `ai_coverage.run_id`, `ai_findings.run_id` reference `ai_runs(id)`. Unique index on `ai_sessions(host, owner, repo, number)`. `ai_checkout` additionally stores `repo_path` and `head_sha`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn session_id_is_stable_and_case_insensitive()      // same PrKey with "Owner"/"owner" → same id
#[test] fn checkout_lifecycle_and_repo_lock() {
    // begin → holding(path) is Some; set_stash; set_state(Active) → still holding;
    // set_state(Restored) → holding None and unrestored() empty
}
#[test] fn unrestored_survives_reopen()                      // file DB in tempdir(): begin, drop, reopen → unrestored().len() == 1
#[test] fn migrations_are_idempotent()                       // open twice on the same file → user_version == 1
#[test] fn original_ref_round_trips_branch_with_slashes_and_detached()
```

- [ ] **Step 2: Run to verify they fail**, **Step 3: implement**, **Step 4: verify** `cargo test --lib ai::store` → PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(ai): SQLite store with checkout records and full AI schema"
```

---

### Task 11: Checkout, restore and prepare (app-run git)

**Files:**
- Create: `src/ai/checkout.rs`
- Test: inline, using temp git repos

**Interfaces:**
- Consumes: `Store`, `CheckoutRecord`, `OriginalRef`, `CheckoutState` (Task 10).
- Produces:
  - `pub struct PrTarget { pub host: String, pub owner: String, pub name: String, pub number: u64, pub head_sha: String, pub head_branch: String }`
  - `#[derive(Debug, thiserror::Error)] pub enum CheckoutError { NotARepo(PathBuf), NoMatchingRemote { wanted: String }, Git { args: String, stderr: String }, HeadMismatch { expected: String, actual: String }, Held { pr_number: u64 }, NeedsDecision(Vec<String>), Store(anyhow::Error) }` with user-facing `#[error]` texts
  - `pub fn find_remote(repo: &Path, host: &str, owner: &str, name: &str) -> Result<String, CheckoutError>`: matches `https://host/owner/name(.git)`, `git@host:owner/name(.git)`, `ssh://git@host[:port]/owner/name(.git)`, case-insensitive, trailing `/` ignored. Read the **raw** URLs with `git config --get-regexp '^remote\..*\.url$'`, not `git remote -v`, which shows URLs after `insteadOf` rewriting.
  - `pub struct CheckoutPlan { pub repo_path: PathBuf, pub remote: String, pub original: OriginalRef, pub dirty: bool, pub commands: Vec<String> }`
  - `pub fn plan_checkout(repo: &Path, pr: &PrTarget, store: &Store) -> Result<CheckoutPlan, CheckoutError>`: fails with `Held` if `store.checkout_holding(repo)` is `Some` for a different PR. `dirty` means `git status --porcelain` is non-empty (untracked included). `commands` are the display strings, exactly: `git stash push --include-untracked -m "gh-review AI #<n>"` (only when dirty), `git fetch <remote> pull/<n>/head`, `git checkout --detach <head_sha>`.
  - `pub fn execute_checkout(plan: &CheckoutPlan, pr: &PrTarget, store: &Store) -> Result<CheckoutRecord, CheckoutError>`: order (spec §7.1): record (`Started`) → stash, then `git rev-parse stash@{0}` → `checkout_set_stash` → fetch → checkout → verify `HEAD == head_sha` (mismatch → run `restore`, return `HeadMismatch`) → `Active`. Any failure after the stash restores before returning the error.
  - `pub fn restore_preview(rec: &CheckoutRecord) -> Result<Vec<String>, CheckoutError>`: `git status --porcelain` lines
  - `pub enum RestoreOutcome { Restored, StashConflict { stash_sha: String } }`
  - `pub fn restore(rec: &CheckoutRecord, discard: bool, store: &Store) -> Result<RestoreOutcome, CheckoutError>`: non-empty preview and `!discard` → `NeedsDecision(lines)`; `discard` → `git reset --hard` + `git clean -fd` (never `-x`); then `git checkout <branch>` or `git checkout --detach <sha>`; then, if a stash exists, `git stash apply --index <sha>`. On success: find our entry's index in `git stash list --format=%H` and `git stash drop stash@{i}`, then state `Restored`. On apply failure: state `Failed`, return `StashConflict` (stash left in place).
  - `pub struct PrepareOutput { pub command: String, pub status: i32, pub output: String }`
  - `pub fn run_prepare(repo: &Path, commands: &[String]) -> Result<Vec<PrepareOutput>, PrepareOutput>`: runs each with `sh -c` in `repo` under the user's normal env, in order; the first failure is returned as `Err`.

All git calls go through one private `fn git(repo: &Path, args: &[&str]) -> Result<String, CheckoutError>` (std `Command`, `GIT_TERMINAL_PROMPT=0`, captures stderr). These functions are blocking; callers run them with `spawn_blocking` (Plan 3).

- [ ] **Step 1: Write the test fixture helper and the failing tests**

Helper `fn fixture() -> (TempDir, PathBuf /*clone*/, PrTarget)`: creates a bare `origin.git`, a clone with `main` + one commit, a commit on branch `feature/x` pushed as `refs/pull/7/head` (`git push origin feature/x:refs/pull/7/head`), then checks the clone back out on `main`. Uses `git -c user.name=t -c user.email=t@t`. The remote URL is rewritten with `git remote set-url origin https://github.com/acme/app.git` *after* the push, plus `git config url.<bare path>.insteadOf https://github.com/acme/app.git` so fetches still hit the bare repo.

```rust
#[test] fn find_remote_matches_https_ssh_and_scp_forms()        // table over three URL forms + case + trailing .git; a fork URL → NoMatchingRemote
#[test] fn clean_tree_checkout_and_restore_round_trip()          // plan: !dirty, 2 commands; execute → HEAD == head_sha; restore → on main, state Restored
#[test] fn dirty_tree_is_stashed_and_restored_with_untracked()   // modify tracked + add untracked → after checkout both gone; after restore both back
#[test] fn restore_applies_our_stash_even_if_another_was_pushed() // after checkout, user does `git stash push` of something else; restore → our changes back, the other stash still in list
#[test] fn restores_branch_with_slashes_and_detached_original()   // original "team/feature" and a detached SHA
#[test] fn changes_made_during_review_need_a_decision()          // write a file after checkout → restore(.., false) = NeedsDecision; restore(.., true) → Restored, file gone
#[test] fn stash_conflict_leaves_the_stash_and_marks_failed()    // edit the stashed file in a conflicting way on main before restore → StashConflict, state Failed, sha still in stash list
#[test] fn second_pr_in_same_repo_is_held()                       // plan_checkout for PR 8 while 7 is Active → Held { pr_number: 7 }
#[test] fn head_mismatch_restores_and_errors()                    // PrTarget.head_sha = a wrong sha → HeadMismatch, back on main
#[test] fn prepare_runs_in_order_and_stops_at_first_failure()     // ["echo a", "false", "echo c"] → Err(status 1), output of "echo c" never produced
```

- [ ] **Step 2: Run to verify they fail**, **Step 3: implement**, **Step 4: verify** `cargo test --lib ai::checkout` → PASS.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(ai): app-run checkout, restore and prepare with crash-safe records"
```

---

### Task 12: Settings → AI section

**Files:**
- Modify: `src/ui/widgets/settings_window.rs`
- Modify: `src/app.rs` (apply new actions; route `Event::AiProviderTested`)
- Modify: `src/ui/shots.rs` (add a `settings-ai` shot)

**Interfaces:**
- Consumes: `AiSettings` (Task 8), `Command::AiTestProvider` / `Event::AiProviderTested` (Task 9), `set_llm_key` (Task 8).
- Produces:
  - `SettingsVm { …, pub ai: AiVm }`, where `pub struct AiVm { pub providers: Vec<ProviderVm>, pub default_model: ModelSettings, pub repos: Vec<RepoAiSettings>, pub test_results: BTreeMap<String, Result<String, String>> }` and `pub struct ProviderVm { pub settings: ProviderSettings, pub key_input: String, pub key_stored: bool }`
  - `settings_vm(s)` fills `ai` (`key_stored` comes from a `&dyn Fn(&str) -> bool` parameter so it stays pure: change the signature to `settings_vm(s: &Settings, has_key: &dyn Fn(&str) -> bool)`). `SettingsVm::apply` writes `providers`, `models.default`, `repos` back. Provider names must be unique and non-empty; `apply` returns `Err(String)` otherwise. Change its return type to `Result<bool, String>` and update callers.
  - New `SettingsAction` variants: `SetLlmKey { provider: String, key: String }` (emitted on Apply for every non-empty `key_input`) and `TestProvider { provider: ProviderSettings, model: ModelSettings }`.
  - The AI section renders only `#[cfg(unix)]`; on Windows the VM still round-trips `ai` untouched.

UI layout (builder's call within the existing window style): providers list (name, API combo with the two values, base URL, key field showing "stored" when `key_stored`, Remove), Add provider, default model (provider combo, model id, context window, max tokens, reasoning), "Test connection" (shows the latest `test_results` entry), and the repos list (repo `host/owner/name`, folder path with a picker, commands and prepare as one-per-line text areas).

- [ ] **Step 1: Write the failing VM tests**

```rust
#[test] fn ai_vm_round_trips_through_apply()               // settings with 1 provider + 1 repo → vm → apply → equal ai
#[test] fn apply_emits_no_key_into_settings()              // key_input "sekrit" → toml::to_string(&settings) doesn't contain "sekrit"
#[test] fn duplicate_provider_names_are_rejected()         // apply → Err
#[test] fn key_stored_reflects_the_keychain_probe()        // has_key = |n| n == "work"
```

- [ ] **Step 2: Run to verify they fail**, **Step 3: implement** the VM, actions, rendering, and the app handling (`SetLlmKey` → `set_llm_key(store, …)` with a toast on error; `TestProvider` → `worker.send(Command::AiTestProvider{…})`; `AiProviderTested` → `vm.ai.test_results.insert`).

- [ ] **Step 4: Verify**

Run: `cargo test --lib ui::widgets::settings_window && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` → PASS. Optional (GPU): `cargo test --lib shots -- --ignored` and look at `target/ui-shots/settings-ai-*.png`.

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(settings): AI providers, model, repos and Test connection"
```

---

## What Plans 2 and 3 pick up

- **Plan 2 (agent + pipeline):** copy `qa-agent/src/tools` (`bash`, `read_file` + `offset`/`limit`, `glob`, `grep`, `truncate`; not `edit_file`) and add `list_dir`, `asset_stats`, `git_*`, `gh_*`, review-output tools. Then the agent loop (§9.2) with a scripted fake LLM, checklist/REVIEW.md/AGENTS.md/skills context, and scout → reviewers (coverage enforcement, challenge pass, lookups) → verifier → assembler, persisted in the Task 10 tables.
- **Plan 3 (UI + chat):** worker commands/events for checkout, prepare and runs (with `SessionId`), the checkout/restore/crash-recovery dialogs, the ✦ AI tab timeline, inline drafts with Keep/Edit/Drop/Why?, tree markers, the chat agent with steering tools and pause/resume, re-review, and the starter `.github/REVIEW.md`.
