# GitHub PR Review Desktop Client — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `gh-review-desktop`, a Rust + egui desktop client for reviewing GitHub pull requests: log in with GitHub, list your open PRs, open one, browse a file tree and a virtualized diff, filter and mark files/folders viewed, and leave GitHub-compatible immediate and pending review comments.

**Architecture:** One Cargo package with a library target (all logic, `pub` modules) and a thin binary target. `ui`/`state` (egui widgets plus pure reducers) sit over `github`/`auth`/`diff`/`model` (pure, testable, no GUI dependency). The UI never touches the network: it emits `Command`s onto a channel and folds `Event`s back into state; a `tokio` worker performs all I/O. Diffs come from GitHub's precomputed patches; context expansion uses the head blob; only files GitHub sends without a patch are diffed locally.

**Tech Stack:** Rust (edition 2021), `eframe` (+ `eframe::egui`), `egui_extras`, `tokio`, `reqwest` (rustls), `serde`/`serde_json`/`toml`, `similar`, `syntect` (pure-Rust regex), `pulldown-cmark`, `keyring` (native backends), `directories`, `percent-encoding`, `thiserror`, `tracing`. Dev: `wiremock`, `tempfile`.

**Spec:** `docs/superpowers/specs/2026-10-19-gh-review-desktop-design.md`

## Global Constraints

- Rust edition 2021; one package named `gh-review-desktop` with `src/lib.rs` (library `gh_review_desktop`, every module `pub`) and `src/main.rs` (calls `gh_review_desktop::app::run()`).
- Use `egui` only through `eframe::egui` (no separate `egui` dependency, so versions cannot drift). Add crates with `cargo add` at their **newest** released versions.
- HTTP uses `reqwest` with its rustls TLS feature; `cargo tree -i openssl-sys` must print nothing. No `octocrab`.
- Every request sends `User-Agent: gh-review-desktop/<version>`, `X-GitHub-Api-Version: 2022-11-28`, and `Authorization: Bearer <token>`; REST requests default to `Accept: application/vnd.github+json`.
- Host → URL derivation is exactly spec §4.1.
- Auth is OAuth **Device Flow** with scope string exactly `repo read:org`, plus a PAT paste fallback. **No client secret may exist anywhere in the binary.**
- Keyring service exactly `gh-review-desktop`; keyring account exactly `{host}:{login}`. `keyring` must be built with `apple-native`, `windows-native`, `sync-secret-service` features.
- Config root `<config_dir>/gh-review-desktop/`; settings file `settings.toml`; session file `sessions/<host>/<owner>/<repo>/<pr-number>.json` with owner/repo lowercased. All file writes go through `persist::write_atomic`.
- Comment anchoring is exactly spec §7.3: single line `{path, line, side}`; a range adds `{start_line, start_side}` with `start_side == side` and `start_line < line` on **both** sides, within one hunk. `path` is the API `filename`. `commit_id` is the head SHA the displayed diff was loaded from (or the pending draft's pinned commit). File-level comments use REST `subject_type: "file"` and are never pending.
- Review events are exactly `COMMENT | APPROVE | REQUEST_CHANGES`. The PR author may not `APPROVE` or `REQUEST_CHANGES`. `REQUEST_CHANGES` needs a body; `COMMENT` needs a body or a comment.
- Scope search: one query per selected scope (spec §7.2), results merged and de-duplicated. Never join scope qualifiers into one query.
- Tokens live only in the OS keychain — never in `settings.toml`, session files, or logs. 401 and sign-out never delete drafts.
- One PR is open at a time. Screens are exactly `Login`, `PrList`, `PrDetail`; settings is a modal window.
- Every PR-scoped `Command`/`Event` carries a `SessionId`; events for a non-current session are dropped.
- Mutating requests (POST/DELETE/PATCH) are never retried automatically.
- Performance budget: model-level work for a visible frame stays under 16 ms on a synthetic 1500-file / 50000-line PR in release mode (Task 54), plus manual frame-time QA.
- **GUI verification rule:** egui has no stable snapshot harness, so all widget logic is extracted into pure view-model functions that are unit-tested; each UI task additionally ends with a recorded manual QA step.

### Conventions every task follows

- **Commits:** stage the exact files the task created or modified, then commit: `git add <files> && git commit -m "<msg>"`. **Never `git commit -am`** — it silently skips new files, and nearly every task creates new files.
- **Checks before commit:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` must pass.
- **Growing `Command`/`Event`:** Task 3 defines the envelopes with only a few variants. Any task that needs a new variant adds it to `src/runtime.rs` **and** the matching arm in `runtime::execute` (created in Task 10) in the same task. Interfaces below list the variants each task adds.
- **Growing `Settings`/`PrSession`:** fields added by later tasks are `#[serde(default)]` (settings) or initialized in the constructor (session); the task lists the field it adds.
- **Fixtures:** stored in `tests/fixtures/`. Unit tests load them with the `fixture!` macro (below), never with a relative `include_str!("../../…")` path (which breaks at different module depths).
- **Shared test helpers:** unit-test helpers live in `src/test_support.rs` (declared in `lib.rs` as `#[cfg(test)] pub mod test_support;`); integration-test helpers live in `tests/common/mod.rs`. When a test uses a helper from this table that does not exist yet, the task adds it.

| Helper | Where | Definition |
|---|---|---|
| `fixture!("name.json")` | test_support | `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/", $name))` |
| `tempdir()` | test_support | `tempfile::tempdir().unwrap()`; tests pass `dir.path()` |
| `repo()` | test_support | `RepoRef::new("o", "r")` |
| `pr_summary(number, title, comments)` | test_support | `PrSummary` in `o/r`, author `"a"`, not draft, `ci: None`, `review: None` |
| `file(path)` | test_support | `PrFile` with status `Modified`, `blob_sha: format!("b-{path}")`, patch `"@@ -1 +1 @@\n-a\n+b\n"`, `changes: 2` |
| `files_at(&[&str])` | test_support | `paths.iter().map(\|p\| file(p)).collect()` |
| `rows_from(patch)` | test_support | all rows of `parse_patch(patch).unwrap()`, flattened |
| `loaded(path, patch)` | test_support | `LoadedFile { file: PrFile { patch: Some(patch), ..file(path) }, diff: Some(FileDiff { hunks: parse_patch(patch).unwrap() }) }` |
| `line_anchor(path, line)` | test_support | single-line RIGHT `Anchor` |
| `range_anchor(path, start, end)` | test_support | RIGHT `Anchor` with `start_line: Some(start)`, `line: end` |
| `tree_at(&[&str])` | test_support | `build_tree(&files_at(paths), &\|_\| true)` |
| `find(root, path)` | test_support | depth-first search by `TreeNode.path` → `Option<&TreeNode>` |
| `meta(head_sha)` / `meta_by(author, head_sha)` | test_support | `PrMetadata` with the given head SHA (and author; default `"author"`) |
| `review(state)` | test_support | `Review` with id 1, author `"me"`, given state |
| `client(uri)` | tests/common | `GithubClient::with_endpoints(Endpoints { web: uri, rest: uri, graphql: format!("{uri}/graphql") }, "tok")` |

## Review Focus

Inputs/conditions the spec implies but no task's happy path exercises. Each is pinned by a test in the task named:

1. **Hostile/malformed comment markdown** (unterminated fences, pathological nesting, ~100 KB bodies) must render without panic. → Task 36.
2. **CRLF, missing trailing newline, empty context lines, and non-ASCII/emoji text in patches** must keep old/new numbering and anchors exact. → Task 16.
3. **Very long single lines** (minified bundles) must be truncated on a char boundary and skip highlighting. → Tasks 20 and 21.
4. **Renamed, deleted, and space/non-ASCII paths** must produce the correct `path` in comment and review payloads and be percent-encoded in contents URLs. → Tasks 17, 19, 31, 45.
5. **Token expiry (401) while pending comments exist** must preserve the draft and never drop comments. → Task 44.
6. **A pending review started in the GitHub web UI** must not make local submission fail opaquely. → Tasks 41 and 42.
7. **The PR head moving while a draft exists** must keep the draft pinned to its commit and only un-view changed files. → Tasks 25 and 44.

---

## Phase A — Foundations

### Task 1: Package scaffold, dependencies, and CI

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/test_support.rs`, `rust-toolchain.toml`, `.github/workflows/ci.yml`, `.gitignore`
- Test: `src/lib.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: nothing.
- Produces: library crate `gh_review_desktop` with `pub fn version() -> &'static str` and the module skeleton `pub mod app; pub mod runtime; pub mod auth; pub mod github; pub mod model; pub mod diff; pub mod state; pub mod persist; pub mod ui; #[cfg(test)] pub mod test_support;` (empty `mod.rs` files allowed). `src/main.rs` temporarily prints `version()`; Task 10 replaces it with `app::run()`.

- [ ] **Step 1: Write the failing test** in `src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn crate_exposes_version() {
        assert!(!super::version().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test crate_exposes_version`
Expected: FAIL to compile — `version` not defined.

- [ ] **Step 3: Implement the scaffold**

```bash
cargo init --lib --name gh-review-desktop      # then create src/main.rs by hand
# set edition = "2021" in Cargo.toml
cargo add eframe egui_extras
cargo add tokio --features rt-multi-thread,sync,time,macros
cargo add reqwest --no-default-features --features json,rustls-tls   # if the newest reqwest renamed its rustls feature, use the new name
cargo add syntect --no-default-features --features default-fancy     # pure-Rust regex, no onig C build
cargo add serde --features derive
cargo add serde_json toml similar pulldown-cmark directories percent-encoding thiserror anyhow tracing tracing-subscriber
cargo add keyring --features apple-native,windows-native,sync-secret-service
cargo add --dev wiremock tempfile
cargo tree -i openssl-sys   # must report nothing
```

Add `pub fn version() -> &'static str { env!("CARGO_PKG_VERSION") }`, the module skeleton, and `src/test_support.rs` containing the `fixture!` macro and `tempdir()`. `rust-toolchain.toml` pins `stable` with `rustfmt` and `clippy`. `ci.yml` runs on `ubuntu-latest`, `macos-latest`, `windows-latest`; the Ubuntu job first runs `sudo apt-get install -y pkg-config libdbus-1-dev libxkbcommon-dev libgtk-3-dev`. Steps: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and (Ubuntu only) `cargo test --release -- --ignored` for the performance guardrail. `.gitignore` contains `/target`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test crate_exposes_version && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src rust-toolchain.toml .github .gitignore
git commit -m "chore: scaffold gh-review-desktop package with CI"
```

---

### Task 2: `GhError` and response classification

**Files:**
- Create: `src/github/mod.rs`, `src/github/error.rs`
- Test: `src/github/error.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `#[derive(Debug, Clone, PartialEq, thiserror::Error)] pub enum GhError { Unauthorized, RateLimited { reset_at: Option<SystemTime> }, SsoRequired { url: Option<String> }, Forbidden(String), NotFound, Validation(String), Server(u16), Network(String), GraphQl(Vec<String>), Decode(String) }`
  - `#[derive(Default)] pub struct ResponseMeta { pub status: u16, pub ratelimit_remaining: Option<u32>, pub ratelimit_reset: Option<u64>, pub retry_after: Option<u64>, pub sso: Option<String> }` and `pub fn ResponseMeta::from_headers(status: u16, headers: &reqwest::header::HeaderMap) -> ResponseMeta`
  - `pub fn classify(meta: &ResponseMeta, body: &str, now: SystemTime) -> GhError`
  - `impl GhError { pub fn is_write_blocking(&self) -> bool /* RateLimited, Unauthorized */; pub fn is_retryable_read(&self) -> bool /* Server, Network */ }`

- [ ] **Step 1: Write the failing test**

```rust
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn m(status: u16) -> ResponseMeta { ResponseMeta { status, ..Default::default() } }

#[test]
fn classifies_responses() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    assert_eq!(classify(&m(401), "", now), GhError::Unauthorized);

    // x-ratelimit-reset is on every response; with quota left a 403 is a permission error
    let perm = ResponseMeta { status: 403, ratelimit_remaining: Some(10), ratelimit_reset: Some(2_000), ..Default::default() };
    assert_eq!(classify(&perm, r#"{"message":"Resource not accessible"}"#, now),
               GhError::Forbidden("Resource not accessible".into()));

    let primary = ResponseMeta { status: 403, ratelimit_remaining: Some(0), ratelimit_reset: Some(2_000), ..Default::default() };
    assert_eq!(classify(&primary, "", now),
               GhError::RateLimited { reset_at: Some(UNIX_EPOCH + Duration::from_secs(2_000)) });

    let secondary = ResponseMeta { status: 429, retry_after: Some(60), ..Default::default() };
    assert_eq!(classify(&secondary, "", now),
               GhError::RateLimited { reset_at: Some(now + Duration::from_secs(60)) });

    let sso = ResponseMeta { status: 403,
        sso: Some("required; url=https://github.com/orgs/acme/sso?authorization_request=x".into()), ..Default::default() };
    assert!(matches!(classify(&sso, "", now),
        GhError::SsoRequired { url: Some(u) } if u.starts_with("https://github.com/orgs/acme/sso")));

    assert_eq!(classify(&m(404), "", now), GhError::NotFound);
    assert_eq!(classify(&m(422),
        r#"{"message":"Validation Failed","errors":[{"message":"line must be part of the diff"}]}"#, now),
        GhError::Validation("Validation Failed: line must be part of the diff".into()));
    assert_eq!(classify(&m(502), "", now), GhError::Server(502));

    assert!(GhError::Server(502).is_retryable_read());
    assert!(!GhError::Validation(String::new()).is_retryable_read());
    assert!(GhError::Unauthorized.is_write_blocking());
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test classifies_responses`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `classify`: 401 → `Unauthorized`; 403/429 with `retry_after` → `RateLimited(now + retry_after)`; 403/429 with `ratelimit_remaining == Some(0)` → `RateLimited(reset)`; 403 with `sso` → `SsoRequired` (parse `url=`); other 403 → `Forbidden(message)`; 404 → `NotFound`; 422 → `Validation(message[: errors joined by "; "])` where each `errors[]` entry is a string or an object with `message`; 5xx → `Server(status)`. Implement `from_headers` (`x-ratelimit-remaining`, `x-ratelimit-reset`, `retry-after`, `x-github-sso`).

- [ ] **Step 4: Run test to verify it passes** — `cargo test classifies_responses`. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github && git commit -m "feat: add GhError and response classification"`.

---

### Task 3: `RepoRef`, `Command`/`Event` envelopes, and the worker bridge

**Files:**
- Create: `src/model/mod.rs`, `src/model/repo.rs`, `src/runtime.rs`
- Test: `src/runtime.rs`, `src/model/repo.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 2 (`GhError`).
- Produces:
  - `#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)] pub struct RepoRef { pub owner: String, pub name: String }` with `new(owner, name)` (preserves case), `id(&self) -> String` (lowercased `owner/name`, used for storage keys), and `Display` as `owner/name`.
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct SessionId(pub u64)`.
  - `pub enum Command { Ping(u32), Shutdown }` and `pub enum Event { Pong(u32), Toast(String), WorkerPanicked(String), Failed { session: Option<SessionId>, error: GhError } }` — later tasks add variants (see Conventions).
  - `pub fn event_channel() -> (EventSender, EventReceiver)` over `std::sync::mpsc`; `EventSender: Clone` with `send(&self, Event)` and `with_repaint(self, Arc<dyn Fn() + Send + Sync>) -> Self` (called after every send so egui wakes up — Task 10 passes `ctx.request_repaint()`); `EventReceiver::drain(&self) -> Vec<Event>` (non-blocking, called every frame) and `recv_timeout(&self, Duration) -> Option<Event>`.
  - `pub struct Worker` with `send(&self, Command)`; `pub fn spawn_worker<H, F>(events: EventSender, handler: H) -> Worker where H: Fn(Command, EventSender) -> F + Send + Sync + 'static, F: Future<Output = ()> + Send + 'static` — owns a multi-thread tokio runtime on a background thread, spawns one task per command, and turns a task panic (`JoinError::is_panic`) into `Event::WorkerPanicked`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn command_round_trips_through_worker() {
    let (tx, rx) = event_channel();
    let worker = spawn_worker(tx, |cmd, events| async move {
        if let Command::Ping(n) = cmd { events.send(Event::Pong(n)); }
    });
    worker.send(Command::Ping(7));
    assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Some(Event::Pong(7))));
}

#[test]
fn handler_panic_becomes_an_event() {
    let (tx, rx) = event_channel();
    let worker = spawn_worker(tx, |_cmd, _events| async move { panic!("boom") });
    worker.send(Command::Ping(1));
    assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Some(Event::WorkerPanicked(_))));
}

#[test]
fn sending_requests_a_repaint() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let (tx, _rx) = event_channel();
    let tx = tx.with_repaint(Arc::new(move || { h.fetch_add(1, Ordering::SeqCst); }));
    tx.send(Event::Pong(1));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
```

and in `src/model/repo.rs`:

```rust
#[test]
fn repo_ref_keeps_case_but_keys_lowercase() {
    let r = RepoRef::new("Octo", "Repo");
    assert_eq!(r.to_string(), "Octo/Repo");
    assert_eq!(r.id(), "octo/repo");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test command_round_trips_through_worker handler_panic_becomes_an_event sending_requests_a_repaint repo_ref_keeps_case_but_keys_lowercase`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the types and the worker bridge as specified.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/model src/runtime.rs src/lib.rs && git commit -m "feat: add Command/Event worker bridge"`.

---

### Task 4: Device-flow state machine (pure)

**Files:**
- Create: `src/auth/mod.rs`, `src/auth/device_flow.rs`
- Test: `src/auth/device_flow.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct DeviceCode { pub device_code: String, pub user_code: String, pub verification_uri: String, pub interval: Duration, pub expires_in: Duration }`
  - `pub enum PollOutcome { Pending, SlowDown { interval: Option<Duration> }, Denied, Expired, Fatal(String), Token(String) }` — `Fatal` covers `incorrect_client_credentials`, `device_flow_disabled`, `unsupported_grant_type`, `incorrect_device_code`.
  - `#[derive(Clone, Debug, PartialEq)] pub enum AuthFailure { Expired, Denied, Config(String), Http(String) }`
  - `pub enum AuthState { Idle, Polling { code: DeviceCode, interval: Duration }, Authenticated(String), Failed(AuthFailure) }`
  - `pub fn start(code: DeviceCode) -> AuthState` and `pub fn advance(state: AuthState, outcome: PollOutcome) -> AuthState`

- [ ] **Step 1: Write the failing test**

```rust
fn code(interval_secs: u64) -> DeviceCode {
    DeviceCode { device_code: "dc".into(), user_code: "ABCD-1234".into(),
        verification_uri: "https://github.com/login/device".into(),
        interval: Duration::from_secs(interval_secs), expires_in: Duration::from_secs(900) }
}

fn interval(s: &AuthState) -> u64 {
    match s { AuthState::Polling { interval, .. } => interval.as_secs(), _ => panic!("not polling") }
}

#[test]
fn polling_progresses_and_backs_off_by_five_seconds() {
    let s = start(code(5));
    let s = advance(s, PollOutcome::Pending);
    assert_eq!(interval(&s), 5);
    let s = advance(s, PollOutcome::SlowDown { interval: None });
    assert_eq!(interval(&s), 10);
    let s = advance(s, PollOutcome::SlowDown { interval: None });
    assert_eq!(interval(&s), 15);                         // +5 s each time (RFC 8628), not doubling
    let s = advance(s, PollOutcome::SlowDown { interval: Some(Duration::from_secs(30)) });
    assert_eq!(interval(&s), 30);                         // server-provided interval wins
    assert_eq!(advance(s, PollOutcome::Token("t".into())), AuthState::Authenticated("t".into()));
}

#[test]
fn terminal_outcomes_fail() {
    assert_eq!(advance(start(code(5)), PollOutcome::Expired), AuthState::Failed(AuthFailure::Expired));
    assert_eq!(advance(start(code(5)), PollOutcome::Denied), AuthState::Failed(AuthFailure::Denied));
    assert!(matches!(advance(start(code(5)), PollOutcome::Fatal("device_flow_disabled".into())),
                     AuthState::Failed(AuthFailure::Config(_))));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test polling_progresses_and_backs_off_by_five_seconds terminal_outcomes_fail`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `start` (→ `Polling` with the code's interval) and `advance` as tested. Derive `PartialEq, Debug` on `AuthState`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/auth && git commit -m "feat: add device flow state machine"`.

---

### Task 5: Token store (keyring + in-memory fake)

**Files:**
- Create: `src/auth/store.rs`
- Test: `src/auth/store.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub trait TokenStore: Send + Sync { fn get(&self, host: &str, login: &str) -> anyhow::Result<Option<String>>; fn set(&self, host: &str, login: &str, token: &str) -> anyhow::Result<()>; fn clear(&self, host: &str, login: &str) -> anyhow::Result<()>; }`
  - `pub struct KeyringStore` using `keyring::Entry::new("gh-review-desktop", &format!("{host}:{login}"))`; `get` maps `keyring::Error::NoEntry` to `Ok(None)`; `clear` treats `NoEntry` as success.
  - `#[derive(Default)] pub struct MemoryStore` (a `Mutex<HashMap>`) for tests.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn memory_store_round_trips_and_clears() {
    let s = MemoryStore::default();
    assert_eq!(s.get("github.com", "me").unwrap(), None);
    s.set("github.com", "me", "tok").unwrap();
    assert_eq!(s.get("github.com", "me").unwrap().as_deref(), Some("tok"));
    s.clear("github.com", "me").unwrap();
    s.clear("github.com", "me").unwrap();               // clearing twice is fine
    assert_eq!(s.get("github.com", "me").unwrap(), None);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test memory_store_round_trips_and_clears`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `KeyringStore` and `MemoryStore`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Manual QA** — run a scratch `#[ignore]`d test that sets, reads back, and clears a `KeyringStore` entry on the dev machine; confirm the entry is visible in the OS keychain UI while set (this catches a build that silently fell back to the `keyring` mock store).

- [ ] **Step 6: Commit** — `git add src/auth/store.rs src/auth/mod.rs && git commit -m "feat: add token store"`.

---

### Task 6: Settings file and atomic writes

**Files:**
- Create: `src/persist/mod.rs`, `src/persist/settings.rs`
- Test: `src/persist/mod.rs`, `src/persist/settings.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn config_root() -> PathBuf` (`directories::ProjectDirs::from("", "", "gh-review-desktop")` config dir).
  - `pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()>` — creates parent dirs, writes `<path>.tmp`, `sync_all`, renames over `path`.
  - `#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)] pub struct Account { pub login: String, pub name: Option<String>, pub avatar_url: Option<String>, pub scopes: Vec<String> }`
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(default)] pub struct Settings { pub host: String /* "github.com" */, pub client_id: Option<String>, pub account: Option<Account> }` with `pub fn load_or_default(dir: &Path) -> Settings` (missing or unparsable file → defaults, logged) and `pub fn save(&self, dir: &Path) -> anyhow::Result<()>` via `write_atomic`. Later tasks add `theme`, `default_scopes`, `layout`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn settings_default_round_trip_and_partial_files() {
    let dir = tempdir();
    let mut s = Settings::load_or_default(dir.path());
    assert_eq!(s.host, "github.com");
    s.account = Some(Account { login: "octocat".into(), ..Default::default() });
    s.save(dir.path()).unwrap();
    assert_eq!(Settings::load_or_default(dir.path()), s);

    std::fs::write(dir.path().join("settings.toml"), "host = \"ghe.example.com\"\n").unwrap();
    let partial = Settings::load_or_default(dir.path());
    assert_eq!(partial.host, "ghe.example.com");
    assert!(partial.account.is_none());

    std::fs::write(dir.path().join("settings.toml"), "not = [valid").unwrap();
    assert_eq!(Settings::load_or_default(dir.path()).host, "github.com");
}

#[test]
fn atomic_write_replaces_and_leaves_no_temp_file() {
    let dir = tempdir();
    let p = dir.path().join("nested/x.json");
    write_atomic(&p, b"one").unwrap();
    write_atomic(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
    assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test settings_default_round_trip_and_partial_files atomic_write_replaces_and_leaves_no_temp_file`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** as specified. Never put a token in `Settings`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/persist && git commit -m "feat: add settings file and atomic writes"`.

---

### Task 7: `GithubClient` REST core

**Files:**
- Create: `src/github/client.rs`, `tests/common/mod.rs`, `tests/contract_client.rs`
- Test: `src/github/client.rs` (`#[cfg(test)]`), `tests/contract_client.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct Endpoints { pub web: String, pub rest: String, pub graphql: String }` and `pub fn endpoints_for(host: &str) -> Endpoints` (spec §4.1).
  - `#[derive(Clone)] pub struct GithubClient` with `pub fn new(host: &str, token: &str) -> Self`, `pub fn with_endpoints(e: Endpoints, token: &str) -> Self`, and `pub fn endpoints(&self) -> &Endpoints`.
  - `pub async fn get_json<T: DeserializeOwned>(&self, path_or_url: &str) -> Result<(T, Option<String /* next link */>), GhError>` — retries `is_retryable_read` errors up to 3 times with backoff (250 ms, 500 ms, 1 s).
  - `pub async fn get_all_pages<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, GhError>` following `Link: rel="next"` (absolute URLs used as-is).
  - `pub(crate) async fn send_mutation(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, GhError>` — **no retry**.
  - `pub fn next_link(link_header: &str) -> Option<String>`
  - `pub struct GitHubUser { pub login: String, pub name: Option<String>, pub avatar_url: Option<String> }`, `pub struct Me { pub user: GitHubUser, pub scopes: Vec<String> }`, and `pub async fn me(&self) -> Result<Me, GhError>` (`scopes` from `X-OAuth-Scopes`, split on `,` and trimmed).
  - `tests/common/mod.rs`: `pub fn client(uri: &str) -> GithubClient` (see helper table).

- [ ] **Step 1: Write the failing tests**

In `src/github/client.rs`:

```rust
#[test]
fn derives_endpoints_per_host() {
    assert_eq!(endpoints_for("github.com"), Endpoints { web: "https://github.com".into(),
        rest: "https://api.github.com".into(), graphql: "https://api.github.com/graphql".into() });
    assert_eq!(endpoints_for("ghe.acme.io"), Endpoints { web: "https://ghe.acme.io".into(),
        rest: "https://ghe.acme.io/api/v3".into(), graphql: "https://ghe.acme.io/api/graphql".into() });
}

#[test]
fn parses_next_link() {
    let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
    assert_eq!(next_link(h).as_deref(), Some("https://api.github.com/x?page=2"));
    assert_eq!(next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#), None);
}
```

In `tests/contract_client.rs`:

```rust
mod common;
use common::client;
use gh_review_desktop::github::{client::*, error::GhError};
use serde_json::{json, Value};
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn me_sends_required_headers_and_reads_scopes() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/user"))
        .and(header("authorization", "Bearer tok"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200)
            .insert_header("x-oauth-scopes", "repo, read:org")
            .set_body_json(json!({"login": "octocat", "name": "Mona", "avatar_url": null})))
        .mount(&server).await;
    let me = client(&server.uri()).me().await.unwrap();
    assert_eq!(me.user.login, "octocat");
    assert_eq!(me.scopes, vec!["repo".to_string(), "read:org".to_string()]);
}

#[tokio::test]
async fn unauthorized_maps_to_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/user"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})))
        .mount(&server).await;
    assert_eq!(client(&server.uri()).me().await.unwrap_err(), GhError::Unauthorized);
}

#[tokio::test]
async fn follows_link_pagination() {
    let server = MockServer::start().await;
    let next = format!("<{}/items?page=2>; rel=\"next\"", server.uri());
    Mock::given(method("GET")).and(path("/items")).and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([3])))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/items"))
        .respond_with(ResponseTemplate::new(200).insert_header("link", next.as_str()).set_body_json(json!([1, 2])))
        .mount(&server).await;
    let all: Vec<Value> = client(&server.uri()).get_all_pages("/items").await.unwrap();
    assert_eq!(all, vec![json!(1), json!(2), json!(3)]);
}

#[tokio::test]
async fn retries_reads_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/user")).respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1).mount(&server).await;
    Mock::given(method("GET")).and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "octocat"})))
        .mount(&server).await;
    assert_eq!(client(&server.uri()).me().await.unwrap().user.login, "octocat");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test derives_endpoints_per_host parses_next_link && cargo test --test contract_client`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the client on `reqwest::Client` with the global headers, classification via `ResponseMeta::from_headers` + `classify`, read retries, and `send_mutation` without retries. `tracing` spans must never record the `Authorization` header.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github tests/common tests/contract_client.rs && git commit -m "feat: add GithubClient REST core"`.

---

### Task 8: Device-flow HTTP driver

**Files:**
- Create: `src/auth/device_http.rs`, `tests/contract_device_flow.rs`
- Test: `tests/contract_device_flow.rs`

**Interfaces:**
- Consumes: Tasks 4, 7 (`Endpoints`).
- Produces:
  - `pub struct HttpDeviceFlow` with `pub fn new(web_base: &str) -> Self`, `pub async fn request_code(&self, client_id: &str, scope: &str) -> Result<DeviceCode, AuthFailure>`, and `pub async fn poll_token(&self, client_id: &str, device_code: &str) -> Result<PollOutcome, AuthFailure>`. Both POST form bodies with `Accept: application/json`. (Inherent methods rather than an `async fn` trait — a public `async fn` in a trait trips the `async_fn_in_trait` lint under `-D warnings`.)
  - `pub async fn run_device_flow(flow: &HttpDeviceFlow, client_id: &str, scope: &str, on_code: impl FnOnce(&DeviceCode)) -> Result<String, AuthFailure>` — requests a code, calls `on_code`, then loops `poll_token` → `advance` with `tokio::time::sleep(interval)` until a terminal state or `expires_in` elapses (→ `AuthFailure::Expired`).

- [ ] **Step 1: Write the failing test**

```rust
use gh_review_desktop::auth::device_http::*;
use serde_json::json;
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn device_flow_requests_code_then_polls_until_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/login/device/code"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("client_id=cid"))
        .and(body_string_contains("scope=repo+read%3Aorg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc", "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900, "interval": 0 })))
        .mount(&server).await;
    Mock::given(method("POST")).and(path("/login/oauth/access_token"))
        .and(body_string_contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "authorization_pending"})))
        .up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_x", "token_type": "bearer", "scope": "repo,read:org" })))
        .mount(&server).await;

    let flow = HttpDeviceFlow::new(&server.uri());
    let mut shown = None;
    let token = run_device_flow(&flow, "cid", "repo read:org", |c| shown = Some(c.user_code.clone()))
        .await.unwrap();
    assert_eq!(token, "gho_x");
    assert_eq!(shown.as_deref(), Some("ABCD-1234"));
}

#[tokio::test]
async fn disabled_device_flow_is_a_config_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc", "user_code": "U", "verification_uri": "v", "expires_in": 900, "interval": 0 })))
        .mount(&server).await;
    Mock::given(method("POST")).and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "device_flow_disabled"})))
        .mount(&server).await;
    let err = run_device_flow(&HttpDeviceFlow::new(&server.uri()), "cid", "repo read:org", |_| {}).await;
    assert!(matches!(err, Err(AuthFailure::Config(_))));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test --test contract_device_flow`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** as specified, mapping the `error` field per spec §6 step 4.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/auth/device_http.rs src/auth/mod.rs tests/contract_device_flow.rs && git commit -m "feat: add device flow HTTP driver"`.

---

### Task 9: Login, token restore, sign-out, and the login view-model

**Files:**
- Create: `src/auth/session.rs`, `src/ui/mod.rs`, `src/ui/screens/mod.rs`, `src/ui/screens/login.rs`
- Test: `src/auth/session.rs`, `src/ui/screens/login.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 4, 5, 6, 7.
- Produces:
  - `pub fn complete_login(store: &dyn TokenStore, settings: &mut Settings, me: &Me, token: &str) -> anyhow::Result<()>` — stores the token under `{settings.host}:{me.user.login}` and caches `Account` in `settings`.
  - `pub fn restore_token(store: &dyn TokenStore, settings: &Settings) -> anyhow::Result<Option<String>>`
  - `pub fn handle_unauthorized(store: &dyn TokenStore, settings: &Settings) -> anyhow::Result<()>` — clears the keychain entry only (account kept to prefill the login screen; drafts untouched).
  - `pub fn sign_out(store: &dyn TokenStore, settings: &mut Settings) -> anyhow::Result<()>` — clears the keychain entry and `settings.account`; drafts untouched.
  - `pub struct LoginVm { pub user_code: Option<String>, pub verification_uri: Option<String>, pub spinner: bool, pub error: Option<String>, pub needs_client_id: bool }` and `pub fn login_vm(state: &AuthState, settings: &Settings) -> LoginVm` — `needs_client_id` is true for a non-`github.com` host without a `client_id` override (spec §4 prerequisite).
  - `pub fn render(ui: &mut egui::Ui, vm: &LoginVm, form: &mut LoginForm, actions: &mut Vec<LoginAction>)` with `LoginAction { StartDeviceFlow, SubmitPat(String), SetHost(String), SetClientId(String), CopyCode, OpenBrowser }`.

- [ ] **Step 1: Write the failing tests**

```rust
fn me(login: &str) -> Me {
    Me { user: GitHubUser { login: login.into(), name: None, avatar_url: None }, scopes: vec!["repo".into()] }
}

#[test]
fn login_persists_token_and_account_then_restores() {
    let store = MemoryStore::default();
    let mut settings = Settings::default();
    complete_login(&store, &mut settings, &me("octocat"), "tok").unwrap();
    assert_eq!(store.get("github.com", "octocat").unwrap().as_deref(), Some("tok"));
    assert_eq!(settings.account.as_ref().unwrap().login, "octocat");
    assert_eq!(restore_token(&store, &settings).unwrap().as_deref(), Some("tok"));
}

#[test]
fn unauthorized_and_sign_out_only_touch_the_token_and_account() {
    let store = MemoryStore::default();
    let mut settings = Settings::default();
    complete_login(&store, &mut settings, &me("octocat"), "tok").unwrap();

    handle_unauthorized(&store, &settings).unwrap();
    assert_eq!(store.get("github.com", "octocat").unwrap(), None);
    assert!(settings.account.is_some());                 // prefill for re-login

    complete_login(&store, &mut settings, &me("octocat"), "tok2").unwrap();
    sign_out(&store, &mut settings).unwrap();
    assert_eq!(store.get("github.com", "octocat").unwrap(), None);
    assert!(settings.account.is_none());
}
```

and in `src/ui/screens/login.rs`:

```rust
#[test]
fn enterprise_without_client_id_asks_for_one() {
    let mut s = Settings { host: "ghe.acme.io".into(), ..Default::default() };
    assert!(login_vm(&AuthState::Idle, &s).needs_client_id);
    s.client_id = Some("Iv1.abc".into());
    assert!(!login_vm(&AuthState::Idle, &s).needs_client_id);
    assert!(!login_vm(&AuthState::Idle, &Settings::default()).needs_client_id);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test login_persists_token_and_account_then_restores unauthorized_and_sign_out_only_touch_the_token_and_account enterprise_without_client_id_asks_for_one`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and the login screen: user-code display, "Copy code", "Open in browser", spinner, PAT entry, host selector, client-ID field (shown when `needs_client_id`).

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/auth src/ui && git commit -m "feat: add login, restore, and sign-out"`.

---

### Task 10: App shell, screen routing, command executor, and toasts

**Files:**
- Create: `src/app.rs`, `src/ui/widgets/mod.rs`, `src/ui/widgets/toast.rs`
- Modify: `src/main.rs`, `src/runtime.rs`
- Test: `src/app.rs`, `src/ui/widgets/toast.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 3, 5, 6, 7, 8, 9.
- Produces:
  - `pub enum ScreenKind { Login, PrList, PrDetail }`, `pub fn initial_screen(has_token: bool) -> ScreenKind`, and `pub fn route(current: ScreenKind, event: &Event) -> ScreenKind` — any `Event::Failed { error: GhError::Unauthorized, .. }` → `Login`; `Event::LoggedIn { .. }` → `PrList`.
  - `pub struct ExecCtx { pub client: RwLock<Option<GithubClient>>, pub store: Arc<dyn TokenStore> }` and `pub async fn execute(ctx: Arc<ExecCtx>, cmd: Command, events: EventSender)` in `runtime.rs` — the single place where `Command`s become network calls.
  - New variants: `Command::StartDeviceFlow { host: String, client_id: String }`, `Command::SubmitPat { host: String, token: String }`; `Event::DeviceCode(DeviceCode)`, `Event::LoggedIn { me: Me, token: String }`, `Event::AuthFailed(AuthFailure)`. On success the executor validates with `me()`, installs the client in `ExecCtx`, and stores the token; the UI thread then calls `complete_login` bookkeeping on `settings` and saves it.
  - `pub struct Toasts` with `push(&mut self, msg: String, now: Instant)` and `visible(&self, now: Instant) -> Vec<&str>` (5 s lifetime, max 3 shown).
  - `pub struct ReviewApp` implementing `eframe::App`: drains events every frame, routes screens, renders toasts, and on `Unauthorized` calls `handle_unauthorized`. `pub fn run() -> eframe::Result<()>` initializes `tracing`, installs a panic hook that logs, restores the token, and starts eframe. `main.rs` becomes `fn main() -> eframe::Result<()> { gh_review_desktop::app::run() }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn routing_follows_auth_events() {
    assert!(matches!(initial_screen(false), ScreenKind::Login));
    assert!(matches!(initial_screen(true), ScreenKind::PrList));
    let unauthorized = Event::Failed { session: None, error: GhError::Unauthorized };
    assert!(matches!(route(ScreenKind::PrDetail, &unauthorized), ScreenKind::Login));
    let other = Event::Failed { session: None, error: GhError::NotFound };
    assert!(matches!(route(ScreenKind::PrDetail, &other), ScreenKind::PrDetail));
}
```

and in `src/ui/widgets/toast.rs`:

```rust
#[test]
fn toasts_expire_and_cap() {
    let t0 = Instant::now();
    let mut t = Toasts::default();
    for i in 0..4 { t.push(format!("m{i}"), t0); }
    assert_eq!(t.visible(t0), vec!["m1", "m2", "m3"]);    // newest three
    assert!(t.visible(t0 + Duration::from_secs(6)).is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test routing_follows_auth_events toasts_expire_and_cap`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the shell, executor arms for the new commands, toasts, and `run()`. `Event::WorkerPanicked` and `Event::Toast` become toasts. `PrList`/`PrDetail` render placeholders until Tasks 14 and 23.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — fresh keychain: `cargo run`, complete the device flow in a browser, confirm the app lands on the (placeholder) PR list; relaunch and confirm no login prompt; sign out (temporary button) and confirm the keychain entry is gone; repeat with a PAT.

- [ ] **Step 6: Commit** — `git add src/app.rs src/main.rs src/runtime.rs src/ui && git commit -m "feat: add app shell, routing, and executor"`.

---

## Phase B — PR list

### Task 11: Scope search-query builder

**Files:**
- Create: `src/github/search.rs`
- Test: `src/github/search.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `RepoRef`.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] pub enum Scope { Assigned, Authored, ReviewRequested, Involves, Mentions }`
  - `#[derive(Clone, Debug, PartialEq)] pub enum ScopeConstraint { None, Repo(RepoRef), Org(String) }`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum Sort { #[default] Updated, Created, Comments }`
  - `pub fn build_queries(scopes: &[Scope], constraint: &ScopeConstraint, login: &str, sort: Sort) -> Vec<String>` — one query per scope (spec §7.2); with no scope and a constraint, a single "all open PRs there" query; with neither, an empty list.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn one_query_per_scope_never_joined() {
    use ScopeConstraint as C;
    assert_eq!(build_queries(&[Scope::Assigned], &C::None, "mona", Sort::Updated),
               vec!["is:pr is:open assignee:mona sort:updated-desc"]);
    assert_eq!(build_queries(&[Scope::Involves, Scope::Mentions], &C::None, "mona", Sort::Updated),
               vec!["is:pr is:open involves:mona sort:updated-desc",
                    "is:pr is:open mentions:mona sort:updated-desc"]);
    assert_eq!(build_queries(&[Scope::ReviewRequested], &C::Repo(RepoRef::new("Octo", "Repo")), "mona", Sort::Comments),
               vec!["is:pr is:open review-requested:mona repo:Octo/Repo sort:comments-desc"]);
    assert_eq!(build_queries(&[Scope::Authored], &C::Org("acme".into()), "mona", Sort::Created),
               vec!["is:pr is:open author:mona org:acme sort:created-desc"]);
    assert_eq!(build_queries(&[], &C::Org("acme".into()), "mona", Sort::Updated),
               vec!["is:pr is:open org:acme sort:updated-desc"]);
    assert!(build_queries(&[], &C::None, "mona", Sort::Updated).is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test one_query_per_scope_never_joined`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `build_queries`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/search.rs src/github/mod.rs && git commit -m "feat: add per-scope search query builder"`.

---

### Task 12: GraphQL transport and PR search

**Files:**
- Create: `src/github/graphql.rs`, `src/model/pr.rs`, `tests/fixtures/search_prs.json`, `tests/contract_graphql.rs`
- Test: `src/github/graphql.rs` (`#[cfg(test)]`), `tests/contract_graphql.rs`

**Interfaces:**
- Consumes: Tasks 7, 11.
- Produces:
  - `GithubClient::graphql<T: DeserializeOwned>(&self, query: &str, variables: serde_json::Value) -> Result<T, GhError>` — POSTs `{query, variables}` to `endpoints.graphql`, returns `data` deserialized; a non-empty `errors[]` maps to `GhError::RateLimited { reset_at: None }` if any error has `type == "RATE_LIMITED"`, else `GhError::GraphQl(messages)`. Read-only queries use the read retry policy.
  - In `model::pr`: `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum CiState { Pending, Success, Failure }` (`statusCheckRollup.state`: `SUCCESS` → Success; `FAILURE`/`ERROR` → Failure; `PENDING`/`EXPECTED` → Pending; null → `None`) and `pub enum ReviewDecision { Approved, ChangesRequested, ReviewRequired }`.
  - `#[derive(Clone, Debug, PartialEq)] pub struct PrSummary { pub repo: RepoRef, pub number: u64, pub title: String, pub author: String /* "ghost" if null */, pub draft: bool, pub updated_at: String, pub created_at: String, pub additions: u32, pub deletions: u32, pub comments: u32, pub ci: Option<CiState>, pub review: Option<ReviewDecision>, pub url: String }`
  - `pub const SEARCH_PRS: &str` (spec §7.1 selection, `first: 50`, `after: $after`), `pub struct SearchPage { pub items: Vec<PrSummary>, pub issue_count: u32, pub end_cursor: Option<String>, pub has_next: bool }`, `pub fn parse_search(data: &serde_json::Value) -> Result<SearchPage, GhError>` (skips empty nodes), and `GithubClient::search_prs(&self, query: &str, after: Option<&str>) -> Result<SearchPage, GhError>`.
  - New variants: `Command::SearchPrs { request: u64, query: String, after: Option<String> }`; `Event::PrSearchPage { request: u64, query: String, page: SearchPage }`, `Event::PrSearchFailed { request: u64, error: GhError }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parses_search_page_with_badges_and_skips_empty_nodes() {
    let body: serde_json::Value = serde_json::from_str(fixture!("search_prs.json")).unwrap();
    let page = parse_search(&body["data"]).unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.issue_count, 137);
    assert!(page.has_next);
    assert_eq!(page.end_cursor.as_deref(), Some("Y3Vyc29yOjI="));
    let first = &page.items[0];
    assert_eq!(first.repo.to_string(), "octo/repo");
    assert_eq!((first.number, first.additions, first.deletions), (12, 40, 3));
    assert_eq!(first.ci, Some(CiState::Failure));
    assert_eq!(first.review, Some(ReviewDecision::ChangesRequested));
    assert!(page.items[1].draft);
    assert_eq!(page.items[1].author, "ghost");            // null author
}
```

Create `search_prs.json` as a full GraphQL response with three `search.nodes`: a PR with `statusCheckRollup.state: "ERROR"` and `reviewDecision: "CHANGES_REQUESTED"`, an empty `{}` node, and a draft PR with `author: null` and no commits rollup.

In `tests/contract_graphql.rs`:

```rust
#[tokio::test]
async fn graphql_rate_limit_error_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"q": "is:pr is:open author:mona sort:updated-desc"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded"}]})))
        .mount(&server).await;
    let err = client(&server.uri()).search_prs("is:pr is:open author:mona sort:updated-desc", None).await.unwrap_err();
    assert!(matches!(err, GhError::RateLimited { .. }));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test parses_search_page_with_badges_and_skips_empty_nodes && cargo test --test contract_graphql`. Expected: FAIL.

- [ ] **Step 3: Implement** the transport, DTOs, parser, `search_prs`, and the `SearchPrs` executor arm.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github src/model tests/fixtures/search_prs.json tests/contract_graphql.rs src/runtime.rs && git commit -m "feat: add GraphQL transport and PR search"`.

---

### Task 13: PR list state and reducer

**Files:**
- Create: `src/state/mod.rs`, `src/state/pr_list.rs`
- Test: `src/state/pr_list.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 11, 12.
- Produces:
  - `pub struct QueryCursor { pub query: String, pub end_cursor: Option<String>, pub has_next: bool, pub total: u32 }`
  - `#[derive(Default)] pub struct PrListState { pub scopes: Vec<Scope>, pub constraint: ScopeConstraint, pub search: String, pub sort: Sort, pub items: Vec<PrSummary>, pub queries: Vec<QueryCursor>, pub request: u64, pub loading: bool, pub error: Option<GhError> }` (`ScopeConstraint` gets `Default = None`).
  - `pub enum PrListMsg { SetScopes(Vec<Scope>), SetConstraint(ScopeConstraint), SetSort(Sort), SetSearch(String), Refresh, PageLoaded { request: u64, query: String, page: SearchPage }, LoadFailed { request: u64, error: GhError } }`
  - `pub fn reduce(state: &mut PrListState, msg: PrListMsg, login: &str) -> Vec<Command>` — scope/constraint/sort/refresh bump `request`, clear items, rebuild `queries` via `build_queries`, and return one `SearchPrs` per query; `SetSearch` is local and returns nothing; pages for an old `request` are ignored; items are de-duplicated by `(repo.id(), number)`.
  - `pub fn load_more(state: &PrListState) -> Vec<Command>` — one `SearchPrs` per query with `has_next`.
  - `pub fn visible_items(state: &PrListState) -> Vec<&PrSummary>` — local text filter (title, repo, author; case-insensitive) then sort by `sort` descending.
  - `pub fn counter_label(state: &PrListState) -> String` — `"N of M"`, or `"N of ≈M"` when more than one query is merged.
  - Settings gains `#[serde(default)] pub default_scopes: Vec<Scope>` (default: ReviewRequested, Assigned, Authored).

- [ ] **Step 1: Write the failing tests**

```rust
fn page(numbers: &[u64], total: u32, cursor: Option<&str>) -> SearchPage {
    SearchPage { items: numbers.iter().map(|n| pr_summary(*n, "t", 0)).collect(), issue_count: total,
                 end_cursor: cursor.map(Into::into), has_next: cursor.is_some() }
}

#[test]
fn scope_change_starts_one_search_per_scope() {
    let mut s = PrListState::default();
    let cmds = reduce(&mut s, PrListMsg::SetScopes(vec![Scope::Assigned, Scope::Authored]), "mona");
    assert_eq!(cmds.len(), 2);
    assert!(s.loading && s.items.is_empty() && s.error.is_none());
}

#[test]
fn pages_merge_dedupe_and_ignore_stale_requests() {
    let mut s = PrListState::default();
    reduce(&mut s, PrListMsg::SetScopes(vec![Scope::Assigned, Scope::Authored]), "mona");
    let (req, q0, q1) = (s.request, s.queries[0].query.clone(), s.queries[1].query.clone());
    reduce(&mut s, PrListMsg::PageLoaded { request: req, query: q0.clone(), page: page(&[1, 2], 2, Some("c1")) }, "mona");
    reduce(&mut s, PrListMsg::PageLoaded { request: req, query: q1, page: page(&[2, 3], 2, None) }, "mona");
    assert_eq!(s.items.len(), 3);                         // #2 appears once
    assert_eq!(counter_label(&s), "3 of ≈4");
    let more = load_more(&s);
    assert!(matches!(&more[..], [Command::SearchPrs { query, after: Some(c), .. }] if *query == q0 && c == "c1"));

    reduce(&mut s, PrListMsg::PageLoaded { request: req - 1, query: "old".into(), page: page(&[9], 1, None) }, "mona");
    assert_eq!(s.items.len(), 3);
}

#[test]
fn search_text_filters_locally_and_sort_reorders() {
    let mut s = PrListState::default();
    reduce(&mut s, PrListMsg::SetScopes(vec![Scope::Assigned]), "mona");
    let (req, q) = (s.request, s.queries[0].query.clone());
    let mut p = page(&[], 2, None);
    p.items = vec![pr_summary(1, "fix bug", 5), pr_summary(2, "Add search", 9)];
    reduce(&mut s, PrListMsg::PageLoaded { request: req, query: q, page: p }, "mona");

    assert!(reduce(&mut s, PrListMsg::SetSearch("SEARCH".into()), "mona").is_empty());
    assert_eq!(visible_items(&s).len(), 1);
    reduce(&mut s, PrListMsg::SetSearch(String::new()), "mona");
    s.sort = Sort::Comments;                              // local re-sort of loaded rows
    assert_eq!(visible_items(&s)[0].number, 2);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test scope_change_starts_one_search_per_scope pages_merge_dedupe_and_ignore_stale_requests search_text_filters_locally_and_sort_reorders`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the reducer, `load_more`, `visible_items`, `counter_label`, and the settings field.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state src/persist/settings.rs && git commit -m "feat: add PR list state and reducer"`.

---

### Task 14: PR list screen and row view-model

**Files:**
- Create: `src/ui/screens/pr_list.rs`, `src/ui/widgets/pr_row.rs`
- Modify: `src/app.rs`
- Test: `src/ui/widgets/pr_row.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 12, 13.
- Produces:
  - `pub enum Badge { Draft, CiPending, CiSuccess, CiFailure, Approved, ChangesRequested, ReviewRequired }`
  - `pub struct PrRowVm { pub title: String, pub repo_number: String, pub author: String, pub badges: Vec<Badge>, pub additions: u32, pub deletions: u32, pub comments: u32, pub updated_at: String }`
  - `pub fn pr_row_vm(pr: &PrSummary) -> PrRowVm`
  - The screen: scope chips, repo/org constraint input, search box, sort dropdown, an `egui_extras::TableBuilder` over `visible_items`, the counter label, a "Load more" row calling `load_more` when scrolled into view, an error banner, and row click opening the PR (wired in Task 22).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn row_vm_surfaces_draft_ci_and_review() {
    let pr = PrSummary { draft: true, ci: Some(CiState::Failure),
        review: Some(ReviewDecision::ChangesRequested), ..pr_summary(7, "t", 3) };
    let vm = pr_row_vm(&pr);
    assert_eq!(vm.badges, vec![Badge::Draft, Badge::CiFailure, Badge::ChangesRequested]);
    assert_eq!(vm.repo_number, "o/r#7");
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test row_vm_surfaces_draft_ci_and_review`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `pr_row_vm` and the screen; route `Event::PrSearchPage`/`PrSearchFailed` into `reduce` in `app.rs`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Manual QA** — with a real token, verify each scope returns PRs, multiple scopes show the union (not the intersection), search narrows locally, sort reorders, "Load more" fetches subsequent pages, and the counter matches GitHub's totals.

- [ ] **Step 6: Commit** — `git add src/ui src/app.rs && git commit -m "feat: add PR list screen"`.

---

## Phase C — Diff engine and PR detail

### Task 15: Unified patch parser core

**Files:**
- Create: `src/diff/mod.rs`, `src/diff/parse.rs`, `src/model/diff.rs`
- Test: `src/diff/parse.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces (in `model::diff`, all `#[derive(Clone, Debug, PartialEq)]`): `#[derive(Copy, Eq, Hash, Serialize, Deserialize)] #[serde(rename_all = "UPPERCASE")] pub enum Side { Left, Right }`, `pub enum LineKind { Context, Added, Removed }`, `pub enum RowOrigin { Patch, Expanded, Local }`, `pub struct DiffRow { pub kind: LineKind, pub old_no: Option<u32>, pub new_no: Option<u32>, pub text: String, pub no_newline_at_eof: bool, pub origin: RowOrigin }`, `pub struct Hunk { pub old_start: u32, pub new_start: u32, pub rows: Vec<DiffRow> }`, `pub struct FileDiff { pub hunks: Vec<Hunk> }`.
- Produces (in `diff::parse`): `pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ParseError>` and `pub fn row_anchor(row: &DiffRow) -> Option<(Side, u32)>` (Added → Right/new; Removed → Left/old; Context → Right/new).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn numbers_rows_like_github() {
    let patch = "@@ -1,3 +1,3 @@ fn main()\n ctx\n+added\n-removed\n ctx2\n";
    let hunks = parse_patch(patch).unwrap();
    assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));
    let rows = &hunks[0].rows;
    assert_eq!((rows[0].old_no, rows[0].new_no), (Some(1), Some(1)));
    assert_eq!((rows[1].old_no, rows[1].new_no), (None, Some(2)));
    assert_eq!((rows[2].old_no, rows[2].new_no), (Some(2), None));
    assert_eq!((rows[3].old_no, rows[3].new_no), (Some(3), Some(3)));
    assert_eq!(rows[1].text, "added");                    // sign stripped
    assert_eq!(row_anchor(&rows[1]), Some((Side::Right, 2)));
    assert_eq!(row_anchor(&rows[2]), Some((Side::Left, 2)));
    assert_eq!(row_anchor(&rows[0]), Some((Side::Right, 1)));   // context defaults to Right
}

#[test]
fn multiple_hunks_restart_numbering() {
    let hunks = parse_patch("@@ -1 +1 @@\n-a\n+b\n@@ -10,2 +10,2 @@\n x\n-y\n+z\n").unwrap();
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[1].rows[2].new_no, Some(11));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test numbers_rows_like_github multiple_hunks_restart_numbering`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the parser: skip lines before the first `@@` (`diff --git`, `---`, `+++`, `index`), parse each header, track independent old/new cursors, strip the sign, set `origin: Patch`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff src/model/diff.rs src/model/mod.rs && git commit -m "feat: add unified patch parser"`.

---

### Task 16: Parser edge cases (Review Focus #2)

**Files:**
- Modify: `src/diff/parse.rs`
- Test: `src/diff/parse.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 15.
- Produces: no new public API; hardens `parse_patch` per spec §8.2.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn handles_edge_cases_without_shifting_numbers() {
    // the no-newline marker flags the previous row and consumes no number
    let r = &parse_patch("@@ -1,1 +1,1 @@\n-old\n+new\n\\ No newline at end of file\n").unwrap()[0].rows;
    assert_eq!(r.len(), 2);
    assert!(r[1].no_newline_at_eof && r[1].new_no == Some(1));

    // abbreviated header with omitted counts
    assert_eq!(parse_patch("@@ -5 +5,2 @@\n ctx\n+added\n").unwrap()[0].rows[1].new_no, Some(6));

    // non-ASCII / emoji content keeps anchors
    assert_eq!(parse_patch("@@ -1,1 +1,2 @@\n old\n+répertoire 🎉\n").unwrap()[0].rows[1].new_no, Some(2));

    // CRLF input: header and rows both parse, one trailing \r stripped
    let r = &parse_patch("@@ -1,1 +1,1 @@\r\n-a\r\n+b\r\n").unwrap()[0].rows;
    assert_eq!((r[1].text.as_str(), r[1].new_no), ("b", Some(1)));

    // a fully empty line inside a hunk is an empty context line
    let r = &parse_patch("@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n").unwrap()[0].rows;
    assert_eq!((r[1].kind.clone(), r[1].old_no, r[1].new_no), (LineKind::Context, Some(2), Some(2)));
    assert_eq!(r[3].new_no, Some(3));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test handles_edge_cases_without_shifting_numbers`. Expected: FAIL on at least the CRLF, no-newline, and empty-line cases.

- [ ] **Step 3: Implement** the edge cases. Never panic: malformed headers return `ParseError`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/parse.rs && git commit -m "fix: harden patch parser edge cases"`.

---

### Task 17: Files API → `PrFile`, status, patch state, and paging (Review Focus #4)

**Files:**
- Create: `src/github/files.rs`, `tests/fixtures/pr_files_page.json`
- Test: `src/github/files.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 7, 15, 16.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)] pub enum FileStatus { Added, Removed, Modified, Renamed, Copied, Changed, Unchanged }`
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum PatchState { Present, Binary, TooLarge }` — absent patch with `changes == 0` → `Binary`, absent with `changes > 0` → `TooLarge` (spec §7.5).
  - `#[derive(Clone, Debug, PartialEq)] pub struct PrFile { pub path: String, pub previous_path: Option<String>, pub status: FileStatus, pub additions: u32, pub deletions: u32, pub changes: u32, pub patch: Option<String>, pub patch_state: PatchState, pub blob_sha: String }`
  - `#[derive(Clone, Debug)] pub struct LoadedFile { pub file: PrFile, pub diff: Option<FileDiff> }` (diff parsed on the worker; `None` unless `Present`).
  - `pub fn parse_files_page(body: &str) -> Result<Vec<PrFile>, GhError>`, `pub fn display_path(f: &PrFile) -> String` (`old → new` for renames), `pub fn anchor_path(f: &PrFile) -> &str` (always `f.path`, the API `filename`).
  - New variants: `Command::LoadPrFiles { session: SessionId, repo: RepoRef, number: u64 }`; `Event::PrFilesPage { session: SessionId, files: Vec<LoadedFile>, done: bool }`. The executor fetches `/repos/{o}/{r}/pulls/{n}/files?per_page=100` page by page, parses each patch, and emits one event per page.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn statuses_paths_and_patch_states() {
    let files = parse_files_page(fixture!("pr_files_page.json")).unwrap();
    let renamed = files.iter().find(|f| f.status == FileStatus::Renamed).unwrap();
    assert_eq!(display_path(renamed), "src/old name.rs → src/new name.rs");
    assert_eq!(anchor_path(renamed), "src/new name.rs");

    let deleted = files.iter().find(|f| f.status == FileStatus::Removed).unwrap();
    assert_eq!(anchor_path(deleted), "src/gone.rs");

    let binary = files.iter().find(|f| f.path.ends_with(".png")).unwrap();
    assert_eq!((binary.patch.is_none(), binary.patch_state), (true, PatchState::Binary));

    let huge = files.iter().find(|f| f.path.ends_with("huge.json")).unwrap();
    assert_eq!(huge.patch_state, PatchState::TooLarge);

    let unicode = files.iter().find(|f| f.path.contains("ünïcode")).unwrap();
    assert_eq!(anchor_path(unicode), unicode.path);

    let changed = files.iter().find(|f| f.path == "bin/tool").unwrap();
    assert_eq!(changed.status, FileStatus::Changed);      // mode change
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test statuses_paths_and_patch_states`. Expected: FAIL.

- [ ] **Step 3: Implement** the DTO, parser, and executor arm. Create the fixture with: a renamed file with spaces (`previous_filename` set), a removed file, a PNG with no `patch` and `changes: 0`, a `huge.json` with no `patch` and `changes: 40000`, a non-ASCII path, and a `changed` mode-only file.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/files.rs tests/fixtures/pr_files_page.json src/runtime.rs && git commit -m "feat: parse and page PR files"`.

---

### Task 18: PR metadata

**Files:**
- Create: `src/github/metadata.rs`, `tests/fixtures/pr_metadata.json`
- Test: `src/github/metadata.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 7.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct PrMetadata { pub node_id: String, pub number: u64, pub title: String, pub body: String /* null → "" */, pub author: String, pub state: String, pub draft: bool, pub base_ref: String, pub base_sha: String, pub head_ref: String, pub head_sha: String, pub additions: u32, pub deletions: u32, pub changed_files: u32, pub requested_reviewers: Vec<String> /* users, then "team:<slug>" */, pub labels: Vec<String>, pub milestone: Option<String>, pub html_url: String }`
  - `pub fn parse_metadata(body: &str) -> Result<PrMetadata, GhError>`
  - New variants: `Command::LoadPrMetadata { session: SessionId, repo: RepoRef, number: u64 }`; `Event::PrMetadataLoaded { session: SessionId, meta: PrMetadata }`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn parses_metadata() {
    let m = parse_metadata(fixture!("pr_metadata.json")).unwrap();
    assert_eq!((m.head_sha.as_str(), m.base_sha.as_str()), ("sha_head", "sha_base"));
    assert_eq!(m.requested_reviewers, vec!["alice".to_string(), "team:core".to_string()]);
    assert_eq!(m.labels, vec!["bug".to_string()]);
    assert_eq!(m.body, "");                               // null body
    assert!(m.draft);
    assert_eq!(m.changed_files, 3);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test parses_metadata`. Expected: FAIL.

- [ ] **Step 3: Implement** the parser and executor arm; create the fixture (draft PR, `body: null`, one requested user and one requested team, one label).

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/metadata.rs tests/fixtures/pr_metadata.json src/runtime.rs && git commit -m "feat: add PR metadata"`.

---

### Task 19: Comment coordinate mapping and multi-line ranges (Review Focus #4)

**Files:**
- Create: `src/model/comment.rs`, `src/diff/anchor.rs`
- Test: `src/diff/anchor.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 15, 17.
- Produces:
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] pub struct Anchor { pub path: String, pub side: Side, pub line: u32, pub start_line: Option<u32>, pub start_side: Option<Side> }` — a line anchor only.
  - `#[derive(Clone, Debug, PartialEq)] pub enum CommentTarget { Line(Anchor), File { path: String } }`
  - `#[derive(Debug, PartialEq)] pub enum AnchorError { NoCoordinateOnSide, EmptyRange, StartNotBeforeEnd, MixedSides }`
  - `pub fn single(f: &PrFile, row: &DiffRow, side: Side) -> Result<Anchor, AnchorError>`
  - `pub fn range(f: &PrFile, hunk: &Hunk, rows: RangeInclusive<usize>, side: Side) -> Result<Anchor, AnchorError>` — rows are indices into one hunk (so a range can never cross hunks); the first and last rows must have a coordinate on `side`; a one-row range returns a single-line anchor.
  - `pub fn validate(a: &Anchor) -> Result<(), AnchorError>` — if `start_line` is set: `start_side == Some(side)` and `start_line < line` (both sides).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn ranges_respect_github_constraints_and_paths() {
    let f = PrFile { previous_path: Some("src/old name.rs".into()), status: FileStatus::Renamed, ..file("src/new name.rs") };
    let h = &parse_patch("@@ -0,0 +1,3 @@\n+a\n+b\n+c\n").unwrap()[0];

    let a = range(&f, h, 0..=1, Side::Right).unwrap();
    assert_eq!(a, Anchor { path: "src/new name.rs".into(), side: Side::Right, line: 2,
                           start_line: Some(1), start_side: Some(Side::Right) });
    assert!(validate(&a).is_ok());

    let one = range(&f, h, 2..=2, Side::Right).unwrap();
    assert_eq!((one.line, one.start_line), (3, None));    // one row → single-line comment

    // a removed row has no RIGHT coordinate
    let h2 = &parse_patch("@@ -1,2 +1,1 @@\n-x\n+y\n-z\n").unwrap()[0];
    assert_eq!(range(&f, h2, 0..=1, Side::Right), Err(AnchorError::NoCoordinateOnSide));
    assert_eq!(single(&f, &h2.rows[0], Side::Left).unwrap().line, 1);

    // start_line must be before line on BOTH sides
    let left = |start, line| Anchor { path: "x".into(), side: Side::Left, line, start_line: Some(start), start_side: Some(Side::Left) };
    assert!(validate(&left(1, 3)).is_ok());
    assert_eq!(validate(&left(3, 1)), Err(AnchorError::StartNotBeforeEnd));
    assert_eq!(validate(&left(2, 2)), Err(AnchorError::StartNotBeforeEnd));
    let mixed = Anchor { start_side: Some(Side::Right), ..left(1, 3) };
    assert_eq!(validate(&mixed), Err(AnchorError::MixedSides));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test ranges_respect_github_constraints_and_paths`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `single`/`range`/`validate` on top of `row_anchor` and `anchor_path`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/model/comment.rs src/model/mod.rs src/diff/anchor.rs src/diff/mod.rs && git commit -m "feat: add comment anchoring with multi-line ranges"`.

---

### Task 20: Syntax-highlight cache (lazy, LRU, long-line guard)

**Files:**
- Create: `src/diff/highlight.rs`
- Test: `src/diff/highlight.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct HighlightSpan { pub range: Range<usize> /* byte range */, pub color: [u8; 3], pub bold: bool, pub italic: bool }`
  - `pub trait Highlighter: Send { fn highlight(&self, syntax: &str, text: &str) -> Vec<HighlightSpan>; }`
  - `#[derive(Clone, Debug, PartialEq, Eq, Hash)] pub struct HighlightKey { pub syntax: String, pub text_hash: u64 }` with `pub fn new(syntax: &str, text: &str) -> Self`.
  - `pub const MAX_HIGHLIGHT_CHARS: usize = 2_000;`
  - `pub struct HighlightCache` with `pub fn new(capacity: usize, h: Box<dyn Highlighter>) -> Self`, `pub fn spans(&mut self, syntax: &str, text: &str) -> &[HighlightSpan]` (returns `&[]` without calling the highlighter for text longer than `MAX_HIGHLIGHT_CHARS`), `len`, `hits`, `misses`.
  - `pub struct SyntectHighlighter` (`SyntaxSet::load_defaults_newlines` + a theme, both loaded once in `new()`), and `pub fn syntax_for_path(path: &str) -> String` (extension-based, `"txt"` fallback).

- [ ] **Step 1: Write the failing test**

```rust
#[derive(Default)]
struct CountingHighlighter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Highlighter for CountingHighlighter {
    fn highlight(&self, _: &str, text: &str) -> Vec<HighlightSpan> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        vec![HighlightSpan { range: 0..text.len(), color: [0, 0, 0], bold: false, italic: false }]
    }
}

#[test]
fn caches_by_key_evicts_and_skips_long_lines() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut c = HighlightCache::new(2, Box::new(CountingHighlighter(calls.clone())));
    c.spans("rs", "let x=1;");
    c.spans("rs", "let x=1;");
    assert_eq!((c.hits(), c.misses()), (1, 1));
    c.spans("rs", "b");
    c.spans("rs", "c");
    assert!(c.len() <= 2);

    let minified = "a".repeat(MAX_HIGHLIGHT_CHARS + 1);
    assert!(c.spans("js", &minified).is_empty());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);   // long line never highlighted
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test caches_by_key_evicts_and_skips_long_lines`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the LRU (a `HashMap` + recency queue is enough), the guard, and `SyntectHighlighter` (per-line, stateless — spec §8.4 limitation).

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/highlight.rs src/diff/mod.rs && git commit -m "feat: add LRU syntax highlight cache"`.

---

### Task 21: Index-based row model, heights, and long-line truncation (Review Focus #3)

**Files:**
- Create: `src/diff/row_model.rs`
- Test: `src/diff/row_model.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 15, 17.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub enum RowRef { FileHeader { file: u32 }, Placeholder { file: u32 } /* binary / too large */, Line { file: u32, hunk: u32, row: u32 }, Gap { file: u32, before_hunk: u32 } /* expandable context, Task 45 */, Thread { thread: u32 }, Pending { local_id: u64 }, Composer }` — the last three are inserted by Task 37.
  - `pub struct RowModel { pub rows: Vec<RowRef>, pub file_ranges: Vec<Range<usize>> /* indexed like files */ }` with private `heights: Vec<f32>` and `prefix: Vec<f32>`.
  - `pub fn build(files: &[LoadedFile]) -> RowModel` — per file: header, then `Placeholder` (no diff) or the hunks' `Line` rows with a `Gap` before every hunk that does not start at line 1.
  - `impl RowModel { pub fn heights(&self) -> &[f32]; pub fn set_height(&mut self, idx: usize, h: f32); pub fn offset_of(&self, idx: usize) -> f32; pub fn visible_range(&self, scroll_y: f32, viewport_h: f32) -> Range<usize> /* binary search on prefix sums */ }` with default heights `HEADER_H`, `ROW_H`.
  - `pub const MAX_DISPLAY_CHARS: usize = 10_000;` and `pub fn display_text(text: &str) -> Cow<'_, str>` — truncates on a `char` boundary and appends `" … (line truncated)"`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn builds_ranges_placeholders_and_gaps() {
    let mut bin = loaded("logo.png", "@@ -1 +1 @@\n-a\n+b\n");
    bin.diff = None;
    let m = build(&[loaded("src/a.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n"), bin]);
    assert_eq!(m.rows[..2], [RowRef::FileHeader { file: 0 }, RowRef::Gap { file: 0, before_hunk: 0 }]);
    assert_eq!(m.file_ranges[0], 0..5);                   // header + gap + 3 lines
    assert_eq!(m.rows[m.file_ranges[1].start + 1], RowRef::Placeholder { file: 1 });
}

#[test]
fn visible_range_uses_prefix_sums() {
    let files: Vec<_> = (0..100).map(|i| loaded(&format!("f{i}.rs"), "@@ -1 +1 @@\n-a\n+b\n")).collect();
    let mut m = build(&files);
    assert_eq!(m.visible_range(0.0, 1.0), 0..1);
    let idx = m.file_ranges[50].start;
    assert_eq!(m.visible_range(m.offset_of(idx), 1.0).start, idx);
    let before = m.offset_of(idx);
    m.set_height(0, HEADER_H + 100.0);
    assert_eq!(m.offset_of(idx), before + 100.0);
}

#[test]
fn display_text_truncates_on_char_boundaries() {
    assert!(display_text(&"a".repeat(500_000)).chars().count() <= MAX_DISPLAY_CHARS + 32);
    let wide = "é🎉".repeat(10_000);                      // multi-byte; must not panic
    assert!(display_text(&wide).ends_with("(line truncated)"));
    assert_eq!(display_text("short"), "short");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test builds_ranges_placeholders_and_gaps visible_range_uses_prefix_sums display_text_truncates_on_char_boundaries`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the model. No per-row `String` clones.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/row_model.rs src/diff/mod.rs && git commit -m "feat: add index-based row model"`.

---

### Task 22: `PrSession` and load orchestration

**Files:**
- Create: `src/state/pr_detail.rs`
- Test: `src/state/pr_detail.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 3, 17, 18, 21.
- Produces:
  - `pub enum LoadPhase { Header, Files, Ready, Failed }`
  - `pub struct PrSession { pub id: SessionId, pub host: String, pub repo: RepoRef, pub number: u64, pub metadata: Option<PrMetadata>, pub diff_head_sha: Option<String> /* head the displayed diff came from */, pub files: Vec<LoadedFile>, pub row_model: RowModel, pub phase: LoadPhase, pub error: Option<GhError>, pub head_moved: bool }` — later tasks add `viewed`, `filters`, `threads`, `pending`, `reviews`.
  - `pub fn open(id: SessionId, host: String, repo: RepoRef, number: u64) -> (PrSession, Vec<Command>)` — emits `LoadPrMetadata` only.
  - `pub fn on_event(&mut self, e: &Event) -> Vec<Command>` — ignores events whose session ≠ `self.id`; on metadata: records it, sets `head_moved` if `diff_head_sha` is set and differs, and emits `LoadPrFiles` (files are fetched **after** metadata so `diff_head_sha` matches the files); on a files page: appends and rebuilds the row model; on `done`: `diff_head_sha = metadata.head_sha`, `head_moved = false`, phase `Ready`; on `Failed`: sets `error`.
  - `pub fn refresh(&mut self) -> Vec<Command>` — re-fetches metadata (and, via `on_event`, files).
  - Later tasks append their initial commands (threads, reviews, commits, checks…) to `open`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn metadata_then_files_then_ready() {
    let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
    assert!(matches!(&cmds[..], [Command::LoadPrMetadata { .. }]));
    let cmds = s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta("sha1") });
    assert!(cmds.iter().any(|c| matches!(c, Command::LoadPrFiles { .. })));
    s.on_event(&Event::PrFilesPage { session: SessionId(1), files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")], done: false });
    assert!(matches!(s.phase, LoadPhase::Files));
    s.on_event(&Event::PrFilesPage { session: SessionId(1), files: vec![loaded("b.rs", "@@ -1 +1 @@\n-a\n+b\n")], done: true });
    assert!(matches!(s.phase, LoadPhase::Ready));
    assert_eq!(s.row_model.file_ranges.len(), 2);
    assert_eq!(s.diff_head_sha.as_deref(), Some("sha1"));
}

#[test]
fn stale_session_events_are_dropped() {
    let (mut s, _) = PrSession::open(SessionId(2), "github.com".into(), repo(), 7);
    s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta("old") });
    assert!(s.metadata.is_none());
    s.on_event(&Event::Failed { session: Some(SessionId(1)), error: GhError::NotFound });
    assert!(s.error.is_none());
}

#[test]
fn refresh_detects_a_moved_head() {
    let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
    s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta("sha1") });
    s.on_event(&Event::PrFilesPage { session: SessionId(1), files: vec![], done: true });
    s.refresh();
    s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta("sha2") });
    assert!(s.head_moved);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test metadata_then_files_then_ready stale_session_events_are_dropped refresh_detects_a_moved_head`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `PrSession`. In `app.rs`, clicking a PR row allocates the next `SessionId`, calls `open`, sends its commands, and switches to `PrDetail`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state/pr_detail.rs src/state/mod.rs src/app.rs && git commit -m "feat: add PR session load orchestration"`.

---

### Task 23: Virtualized diff view widget

**Files:**
- Create: `src/ui/widgets/diff_view.rs`, `src/ui/screens/pr_detail.rs`
- Modify: `src/app.rs`
- Test: `src/ui/widgets/diff_view.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 20, 21, 22.
- Produces:
  - `pub struct DiffViewState { pub mode: DiffMode, pub wrap: bool }`, `pub enum DiffMode { Unified, Split }`
  - `pub fn row_height(wrap: bool, text: &str, char_width: f32, width: f32) -> f32` — `ROW_H` unwrapped; `ROW_H * ceil(chars * char_width / width)` wrapped (on `display_text`).
  - `pub fn split_pairs(rows: &[DiffRow]) -> Vec<(Option<&DiffRow>, Option<&DiffRow>)>` — pairs each removed run with the following added run; context rows pair with themselves.
  - `pub fn placeholder_text(f: &PrFile) -> &'static str` — "Binary file not shown" / "Large diff not shown — Load diff". (Task 49 replaces the binary placeholder with the image viewer for image files.)
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum HeaderButton { CopyPath, OpenInBrowser, CommentOnFile, Viewed }` and `pub fn header_buttons(f: &PrFile) -> Vec<HeaderButton>` — every file header, **including binary and too-large files**, offers `CommentOnFile`, which emits `DiffAction::StartComment(CommentTarget::File { path: anchor_path(f) })`. For binary files this is the only way to comment (spec §7.5).
  - `pub fn render(ui: &mut egui::Ui, session: &PrSession, state: &mut DiffViewState, cache: &mut HighlightCache, actions: &mut Vec<DiffAction>)` using `egui_extras::TableBuilder` with heterogeneous row heights from `row_model.heights()`, gutter line numbers, `display_text`, highlight spans, sticky file headers rendering `header_buttons`, placeholders, and `DiffAction { ToggleViewed(u32), StartComment(CommentTarget), ScrollToFile(u32), LoadLargeDiff(u32), ExpandGap { file: u32, before_hunk: u32 } }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn split_pairs_align_changed_runs() {
    let rows = rows_from("@@ -1,3 +1,3 @@\n-a\n-b\n+a2\n+b2\n c\n");
    let pairs = split_pairs(&rows);
    assert_eq!(pairs.len(), 3);
    assert_eq!((pairs[0].0.unwrap().text.as_str(), pairs[0].1.unwrap().text.as_str()), ("a", "a2"));
    assert_eq!(pairs[2].0.unwrap().text, "c");
}

#[test]
fn uneven_runs_leave_blanks() {
    let rows = rows_from("@@ -1,1 +1,2 @@\n-a\n+a2\n+extra\n");
    let pairs = split_pairs(&rows);
    assert!(pairs[1].0.is_none() && pairs[1].1.unwrap().text == "extra");
}

#[test]
fn wrapped_rows_measure_taller() {
    let long = "x".repeat(400);
    assert!(row_height(true, &long, 8.0, 600.0) > row_height(false, &long, 8.0, 600.0));
}

#[test]
fn every_header_can_start_a_file_comment() {
    let png = PrFile { patch: None, patch_state: PatchState::Binary, changes: 0, ..file("assets/logo.png") };
    let text = file("src/a.rs");
    assert!(header_buttons(&png).contains(&HeaderButton::CommentOnFile));
    assert!(header_buttons(&text).contains(&HeaderButton::CommentOnFile));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test split_pairs_align_changed_runs uneven_runs_leave_blanks wrapped_rows_measure_taller every_header_can_start_a_file_comment`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the helpers, `render`, and the `PrDetail` screen shell (top bar, resizable left pane placeholder, center diff, collapsible right pane placeholder). When wrap is on and the pane width changes, recompute line heights via `set_height`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — open a PR with several files including a binary file; confirm smooth scrolling, correct line numbers, highlighting, sticky headers, the binary placeholder, and unified/split toggling.

- [ ] **Step 6: Commit** — `git add src/ui src/app.rs && git commit -m "feat: add virtualized diff view"`.

---

## Phase D — File tree, viewed state, filters, persistence

### Task 24: Path tree building

**Files:**
- Create: `src/model/tree.rs`
- Test: `src/model/tree.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 17.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct TreeNode { pub name: String /* display label, may be "a/b/c" */, pub path: String /* full path, "" for root */, pub is_dir: bool, pub additions: u32, pub deletions: u32, pub children: Vec<TreeNode>, pub file_index: Option<usize> }`
  - `pub fn build_tree(files: &[PrFile], include: &dyn Fn(&PrFile) -> bool) -> TreeNode` — tree over included files only; directories before files, each sorted by name; a directory whose only child is a directory is merged with it (`a` + `b` + `c` → `"a/b/c"`); additions/deletions roll up; `file_index` indexes the original slice.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn nests_sorts_rolls_up_and_collapses_dir_chains() {
    let root = tree_at(&["src/state/model.rs", "src/ui/app.rs", "README.md", "a/b/c/x.rs"]);
    let names: Vec<_> = root.children.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["a/b/c", "src", "README.md"]);   // dirs first; chain collapsed
    assert_eq!(find(&root, "a/b/c").unwrap().children[0].name, "x.rs");
    let state = find(&root, "src/state").unwrap();         // src has two children: not collapsed
    assert!(state.is_dir && state.children[0].name == "model.rs");
    assert_eq!(find(&root, "README.md").unwrap().file_index, Some(2));
    assert_eq!(find(&root, "src").unwrap().additions, 2);  // 1 per file from `file()`
}

#[test]
fn filter_excludes_files_from_tree() {
    let files = files_at(&["src/a.rs", "docs/x.md"]);
    let root = build_tree(&files, &|f| f.path.ends_with(".rs"));
    assert!(find(&root, "docs").is_none());
    assert!(find(&root, "src/a.rs").is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test nests_sorts_rolls_up_and_collapses_dir_chains filter_excludes_files_from_tree`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `build_tree`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/model/tree.rs src/model/mod.rs src/test_support.rs && git commit -m "feat: add file tree builder"`.

---

### Task 25: Viewed set keyed by blob SHA (Review Focus #7)

**Files:**
- Create: `src/state/viewed.rs`
- Test: `src/state/viewed.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 17.
- Produces:
  - `#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)] pub struct ViewedSet { marks: BTreeMap<String /* path */, String /* blob_sha when marked */> }`
  - `pub fn is_viewed(&self, f: &PrFile) -> bool` — true only when the stored SHA equals `f.blob_sha`.
  - `pub fn set(&mut self, f: &PrFile, viewed: bool)`; `pub fn set_many(&mut self, files: &[&PrFile], viewed: bool)`
  - `pub fn viewed_count(&self, files: &[PrFile]) -> usize`; `pub fn invalidated_count(&self, files: &[PrFile]) -> usize` (marked paths whose blob changed — the "N files you had viewed changed since" hint).
  - `PrSession` gains `pub viewed: ViewedSet`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn only_changed_files_lose_viewed_state() {
    let files = files_at(&["a.rs", "b.rs", "c.rs"]);
    let mut v = ViewedSet::default();
    v.set(&files[0], true);
    v.set_many(&[&files[1], &files[2]], true);
    assert_eq!(v.viewed_count(&files), 3);

    // new push: b.rs changed, a.rs and c.rs did not
    let mut after = files.clone();
    after[1].blob_sha = "new".into();
    assert!(v.is_viewed(&after[0]));
    assert!(!v.is_viewed(&after[1]));
    assert_eq!(v.viewed_count(&after), 2);
    assert_eq!(v.invalidated_count(&after), 1);

    v.set(&after[1], true);                              // re-marking stores the new SHA
    assert_eq!(v.invalidated_count(&after), 0);
    v.set(&after[0], false);
    assert!(!v.is_viewed(&after[0]));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test only_changed_files_lose_viewed_state`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `ViewedSet` and add the session field.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state && git commit -m "feat: add blob-keyed viewed set"`.

---

### Task 26: Filters

**Files:**
- Create: `src/state/filters.rs`
- Test: `src/state/filters.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 17, 25.
- Produces:
  - `#[derive(Clone, Debug, Default, PartialEq)] pub struct FilterState { pub extensions: BTreeSet<String>, pub statuses: BTreeSet<FileStatus>, pub path_query: String, pub hide_viewed: bool }` — empty sets mean "no constraint".
  - `pub fn extension(path: &str) -> String` — text after the last `.` of the file **name** (`"Makefile"` → `""`, `"a.d/b"` → `""`).
  - `pub fn status_group(s: FileStatus) -> FileStatus` — `Changed`/`Copied`/`Unchanged` group under `Modified`.
  - `pub fn matches(f: &PrFile, filters: &FilterState, viewed: &ViewedSet) -> bool` — conjunction of extension, status group, path (case-insensitive substring with `*` wildcards), hide-viewed.
  - `pub fn without_hide_viewed(filters: &FilterState) -> FilterState`
  - `pub fn apply<'a>(files: &'a [PrFile], filters: &FilterState, viewed: &ViewedSet) -> Vec<&'a PrFile>`; `pub fn available_extensions(files: &[PrFile]) -> Vec<String>`.
  - `PrSession` gains `pub filters: FilterState`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn filters_form_a_conjunction() {
    let files = files_at(&["src/a.rs", "src/b.ts", "docs/c.md", "Makefile"]);
    let none = ViewedSet::default();
    let mut f = FilterState::default();
    assert_eq!(apply(&files, &f, &none).len(), 4);       // empty filters keep everything

    f.extensions.insert("rs".into());
    f.statuses.insert(FileStatus::Modified);
    assert_eq!(names(apply(&files, &f, &none)), vec!["src/a.rs"]);

    f.path_query = "SRC/*".into();
    assert_eq!(names(apply(&files, &f, &none)), vec!["src/a.rs"]);
    f.path_query = "docs".into();
    assert!(apply(&files, &f, &none).is_empty());

    let mut v = ViewedSet::default();
    v.set(&files[0], true);
    let f = FilterState { hide_viewed: true, ..Default::default() };
    assert_eq!(apply(&files, &f, &v).len(), 3);
    assert_eq!(available_extensions(&files), vec!["", "md", "rs", "ts"]);
}

fn names(fs: Vec<&PrFile>) -> Vec<&str> { fs.into_iter().map(|f| f.path.as_str()).collect() }
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test filters_form_a_conjunction`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and the session field.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state && git commit -m "feat: add file filters"`.

---

### Task 27: Folder tri-state and snapshot-then-toggle

**Files:**
- Create: `src/state/folder_viewed.rs`
- Test: `src/state/folder_viewed.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 25, 26.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum TriState { None, Some, All }`
  - `pub fn files_under<'a>(folder: &str, files: &'a [PrFile], filters: &FilterState, viewed: &ViewedSet) -> Vec<&'a PrFile>` — files whose path starts with `folder + "/"` (all files for `""`) and that match `without_hide_viewed(filters)`.
  - `pub fn folder_state(folder: &str, files: &[PrFile], filters: &FilterState, viewed: &ViewedSet) -> TriState`
  - `#[derive(Debug, PartialEq)] pub struct ToggleResult { pub marked: bool, pub paths: Vec<String> }`
  - `pub fn toggle_folder_viewed(folder: &str, files: &[PrFile], filters: &FilterState, viewed: &mut ViewedSet) -> ToggleResult` — snapshots `files_under` **before** mutating; unmarks the snapshot if all of it is viewed, else marks it (spec §10.3).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn tri_state_ignores_hide_viewed_and_prefix_is_exact() {
    let files = files_at(&["d/a.rs", "d/b.rs", "dx/y.rs"]);
    let mut v = ViewedSet::default();
    let mut f = FilterState::default();
    assert_eq!(folder_state("d", &files, &f, &v), TriState::None);
    v.set(&files[0], true);
    assert_eq!(folder_state("d", &files, &f, &v), TriState::Some);
    f.hide_viewed = true;                                // must not hide the viewed half
    assert_eq!(folder_state("d", &files, &f, &v), TriState::Some);
    v.set(&files[1], true);
    assert_eq!(folder_state("d", &files, &f, &v), TriState::All);
    assert_eq!(folder_state("dx", &files, &f, &v), TriState::None);   // "d" did not match "dx/"
}

#[test]
fn toggle_marks_filtered_snapshot_then_unmarks() {
    let files = files_at(&["d/a.rs", "d/b.rs", "d/c.md", "e/x.rs"]);
    let mut v = ViewedSet::default();
    let f = FilterState { extensions: BTreeSet::from(["rs".to_string()]), hide_viewed: true, ..Default::default() };

    let r = toggle_folder_viewed("d", &files, &f, &mut v);
    assert_eq!(r, ToggleResult { marked: true, paths: vec!["d/a.rs".into(), "d/b.rs".into()] });
    assert!(!v.is_viewed(&files[2]) && !v.is_viewed(&files[3]));

    let r = toggle_folder_viewed("d", &files, &f, &mut v);
    assert!(!r.marked);
    assert_eq!(folder_state("d", &files, &f, &v), TriState::None);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test tri_state_ignores_hide_viewed_and_prefix_is_exact toggle_marks_filtered_snapshot_then_unmarks`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state && git commit -m "feat: add folder viewed toggling"`.

---

### Task 28: Filter bar and file tree widget

**Files:**
- Create: `src/ui/widgets/filter_bar.rs`, `src/ui/widgets/file_tree.rs`
- Modify: `src/ui/screens/pr_detail.rs`
- Test: `src/ui/widgets/file_tree.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 23, 24, 25, 26, 27.
- Produces:
  - `pub struct TreeRowVm { pub indent: u32, pub label: String, pub path: String, pub is_dir: bool, pub tri: TriState, pub status: Option<FileStatus>, pub additions: u32, pub deletions: u32, pub current: bool }`
  - `pub fn flatten_visible(tree: &TreeNode, collapsed: &BTreeSet<String>, files: &[PrFile], filters: &FilterState, viewed: &ViewedSet, current: Option<&str>) -> Vec<TreeRowVm>` — depth-first, root not rendered, top level at indent 0, children of a collapsed directory skipped. Directories are expanded unless listed in `collapsed`.
  - `pub fn render(ui: &mut egui::Ui, rows: &[TreeRowVm], filters: &mut FilterState, progress: (usize, usize, usize, usize), actions: &mut Vec<TreeAction>)` with `TreeAction { ToggleCollapsed(String), ToggleViewed(String), ToggleFolderViewed(String), SelectFile(String), MarkAllViewed }`; `progress` is `(viewed_filtered, total_filtered, viewed_all, total_all)`.
  - `PrSession` gains `pub collapsed_dirs: BTreeSet<String>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn flatten_honors_collapse_and_indent() {
    let files = files_at(&["src/a.rs", "src/state/b.rs"]);
    let root = build_tree(&files, &|_| true);
    let f = FilterState::default();
    let v = ViewedSet::default();
    let rows = flatten_visible(&root, &BTreeSet::new(), &files, &f, &v, Some("src/a.rs"));
    let shape: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.indent)).collect();
    assert_eq!(shape, vec![("src", 0), ("state", 1), ("b.rs", 2), ("a.rs", 1)]);
    assert!(rows[3].current && !rows[3].is_dir);

    let collapsed = BTreeSet::from(["src".to_string()]);
    assert_eq!(flatten_visible(&root, &collapsed, &files, &f, &v, None).len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test flatten_honors_collapse_and_indent`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `flatten_visible` and both widgets: extension chips, status filter, path box, hide-viewed toggle, progress counter, "mark all viewed" (= `toggle_folder_viewed("")`), tri-state checkboxes emitting `ToggleFolderViewed` for directories. Wire actions in `PrDetail`; selecting a file scrolls the diff via `row_model.offset_of(file_ranges[i].start)`.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Manual QA** — filter by `.rs`, enable hide-viewed, mark a folder and confirm only filtered descendants are marked and then disappear; confirm tri-states stay correct while hide-viewed is on; progress counter accurate.

- [ ] **Step 6: Commit** — `git add src/ui src/state && git commit -m "feat: add filter bar and file tree"`.

---

### Task 29: Session file persistence

**Files:**
- Create: `src/persist/session.rs`
- Modify: `src/state/pr_detail.rs`
- Test: `src/persist/session.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 6, 22, 25, 28.
- Produces:
  - `#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)] #[serde(default)] pub struct SessionFile { pub head_sha: Option<String>, pub viewed: ViewedSet, pub draft: Option<serde_json::Value> /* PendingReview, Task 40 */, pub composer_draft: Option<serde_json::Value> /* Task 40 */, pub last_opened_file: Option<String>, pub collapsed_dirs: BTreeSet<String> }`
  - `pub fn session_path(root: &Path, host: &str, repo: &RepoRef, number: u64) -> PathBuf` → `root/sessions/<host>/<owner lower>/<repo lower>/<number>.json`.
  - `pub fn load(root: &Path, host: &str, repo: &RepoRef, number: u64) -> anyhow::Result<Option<SessionFile>>` and `pub fn save(root: &Path, host: &str, repo: &RepoRef, number: u64, s: &SessionFile) -> anyhow::Result<()>` (via `write_atomic`).
  - `#[derive(Default)] pub struct DebouncedWriter` with `mark_dirty(&mut self, now: Instant)`, `should_flush(&self, now: Instant) -> bool` (250 ms after the **last** change), `flushed(&mut self)`.
  - `PrSession` loads its `SessionFile` on open (restoring viewed, collapsed dirs, last file) and saves viewed/collapse changes through a `DebouncedWriter` polled each frame; `app.rs` flushes on exit (`eframe::App::on_exit`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn session_round_trips_under_host_and_repo() {
    let dir = tempdir();
    let r = RepoRef::new("Octo", "Repo");
    let mut viewed = ViewedSet::default();
    viewed.set(&file("a.rs"), true);
    let s = SessionFile { head_sha: Some("sha2".into()), viewed, last_opened_file: Some("a.rs".into()),
                          draft: Some(serde_json::json!({"body": "hi"})), ..Default::default() };
    save(dir.path(), "github.com", &r, 7, &s).unwrap();
    assert!(session_path(dir.path(), "github.com", &r, 7).ends_with("sessions/github.com/octo/repo/7.json"));
    assert_eq!(load(dir.path(), "github.com", &r, 7).unwrap().unwrap(), s);
    assert!(load(dir.path(), "ghe.acme.io", &r, 7).unwrap().is_none());   // hosts never collide
}

#[test]
fn writer_debounces_from_the_last_change() {
    let mut w = DebouncedWriter::default();
    let t0 = Instant::now();
    assert!(!w.should_flush(t0));
    w.mark_dirty(t0);
    w.mark_dirty(t0 + Duration::from_millis(200));
    assert!(!w.should_flush(t0 + Duration::from_millis(300)));
    assert!(w.should_flush(t0 + Duration::from_millis(460)));
    w.flushed();
    assert!(!w.should_flush(t0 + Duration::from_secs(5)));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test session_round_trips_under_host_and_repo writer_debounces_from_the_last_change`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** as specified. **Do not** persist tokens here.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/persist src/state src/app.rs && git commit -m "feat: persist per-PR session state"`.

---

## Phase E — Comments and threads

### Task 30: Review threads via GraphQL (single source of inline comments)

**Files:**
- Create: `src/github/threads.rs`, `src/model/thread.rs`, `tests/fixtures/review_threads.json`, `tests/contract_threads.rs`
- Test: `src/github/threads.rs` (`#[cfg(test)]`), `tests/contract_threads.rs`

**Interfaces:**
- Consumes: Tasks 12, 15.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct ThreadComment { pub database_id: u64, pub author: String, pub avatar_url: Option<String>, pub body: String, pub created_at: String }`
  - `#[derive(Clone, Debug, PartialEq)] pub enum SubjectType { Line, File }`
  - `#[derive(Clone, Debug, PartialEq)] pub struct ReviewThread { pub id: String /* node id */, pub is_resolved: bool, pub is_outdated: bool, pub path: String, pub line: Option<u32>, pub start_line: Option<u32>, pub side: Option<Side>, pub subject_type: SubjectType, pub comments: Vec<ThreadComment> }` with `pub fn root_comment_id(&self) -> Option<u64>`.
  - `pub const THREADS_QUERY: &str` (`reviewThreads(first: 100, after: $after) { pageInfo, nodes { id isResolved isOutdated path line startLine diffSide subjectType comments(first: 100) { pageInfo nodes { databaseId author { login avatarUrl } body createdAt } } } }`) and `pub const THREAD_COMMENTS_QUERY: &str` (`node(id:) { ... on PullRequestReviewThread { comments(first: 100, after:) } }`).
  - `pub fn parse_threads_page(data: &Value) -> Result<(Vec<ReviewThread>, PageInfo), GhError>` with `pub struct PageInfo { pub has_next: bool, pub end_cursor: Option<String> }`.
  - `GithubClient::review_threads(&self, repo: &RepoRef, number: u64) -> Result<Vec<ReviewThread>, GhError>` — paginates threads and, for any thread whose comments have `hasNextPage`, its comments.
  - New variants: `Command::LoadThreads { session, repo, number }`; `Event::ThreadsLoaded { session, threads: Vec<ReviewThread> }`. `PrSession` gains `pub threads: Vec<ReviewThread>`; `open` also emits `LoadThreads`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parses_resolved_outdated_and_file_level_threads() {
    let body: Value = serde_json::from_str(fixture!("review_threads.json")).unwrap();
    let (threads, page) = parse_threads_page(&body["data"]).unwrap();
    assert!(page.has_next && page.end_cursor.is_some());
    assert_eq!(threads[0].id, "PRRT_kwDOA");
    assert!(threads[0].is_resolved);
    assert_eq!(threads[0].side, Some(Side::Right));
    assert_eq!(threads[0].comments.len(), 2);             // root + reply
    assert_eq!(threads[0].root_comment_id(), Some(1001));
    assert!(threads[1].is_outdated && threads[1].line.is_none());
    assert_eq!(threads[2].subject_type, SubjectType::File);
}
```

In `tests/contract_threads.rs`: mount two `/graphql` responses distinguished by `body_partial_json(json!({"variables": {"after": null}}))` and `body_partial_json(json!({"variables": {"after": "c1"}}))`, each with one thread, the first with `hasNextPage: true, endCursor: "c1"`; assert `review_threads` returns both threads in order.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test parses_resolved_outdated_and_file_level_threads && cargo test --test contract_threads`. Expected: FAIL.

- [ ] **Step 3: Implement** the DTOs, queries, parser, fetcher, executor arm, and session wiring. Create the fixture with: a resolved RIGHT thread with a reply, an outdated thread with `line: null`, a FILE thread, and `pageInfo.hasNextPage: true`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/threads.rs src/model tests/fixtures/review_threads.json tests/contract_threads.rs src/runtime.rs src/state && git commit -m "feat: load review threads via GraphQL"`.

---

### Task 31: Immediate line and file-level comments (contract tests)

**Files:**
- Create: `src/github/comments.rs`, `tests/contract_post_comment.rs`
- Test: `src/github/comments.rs` (`#[cfg(test)]`), `tests/contract_post_comment.rs`

**Interfaces:**
- Consumes: Tasks 7, 19, 30.
- Produces:
  - `pub struct NewComment { pub commit_id: String, pub body: String, pub target: CommentTarget }`
  - `pub fn comment_payload(c: &NewComment) -> serde_json::Value` — line target: `body`, `commit_id`, `path`, `line`, `side` (+ `start_line`/`start_side` when present); file target: `body`, `commit_id`, `path`, `subject_type: "file"` and **no** `line`/`side`.
  - `pub fn comment_from_rest(v: &Value) -> Result<ThreadComment, GhError>` (REST `id` → `database_id`).
  - `GithubClient::post_review_comment(&self, repo: &RepoRef, number: u64, c: &NewComment) -> Result<ThreadComment, GhError>` via `send_mutation` (no retry).
  - New variants: `Command::PostComment { session, repo, number, comment: NewComment, optimistic_id: u64 }`; `Event::CommentPosted { session, optimistic_id, comment: ThreadComment }`, `Event::CommentFailed { session, optimistic_id, error: GhError }`.

- [ ] **Step 1: Write the failing tests**

In `src/github/comments.rs`:

```rust
#[test]
fn payload_shapes() {
    let range = comment_payload(&NewComment { commit_id: "s".into(), body: "b".into(),
        target: CommentTarget::Line(range_anchor("p", 10, 14)) });
    assert_eq!(range, json!({"body": "b", "commit_id": "s", "path": "p",
        "start_line": 10, "start_side": "RIGHT", "line": 14, "side": "RIGHT"}));

    let file = comment_payload(&NewComment { commit_id: "s".into(), body: "b".into(),
        target: CommentTarget::File { path: "src/new name.rs".into() } });
    assert_eq!(file, json!({"body": "b", "commit_id": "s", "path": "src/new name.rs", "subject_type": "file"}));
}
```

In `tests/contract_post_comment.rs`:

```rust
#[tokio::test]
async fn posts_exact_single_line_body_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/repos/o/r/pulls/7/comments"))
        .and(body_json(json!({"body": "hi", "commit_id": "sha1", "line": 42, "path": "src/a.rs", "side": "RIGHT"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 1, "body": "hi", "path": "src/a.rs", "line": 42, "side": "RIGHT",
            "user": {"login": "me"}, "created_at": "2026-10-19T00:00:00Z"})))
        .expect(1)
        .mount(&server).await;
    let got = client(&server.uri()).post_review_comment(&RepoRef::new("o", "r"), 7, &NewComment {
        commit_id: "sha1".into(), body: "hi".into(),
        target: CommentTarget::Line(Anchor { path: "src/a.rs".into(), side: Side::Right, line: 42,
                                             start_line: None, start_side: None }) }).await.unwrap();
    assert_eq!(got.database_id, 1);
}

#[tokio::test]
async fn mutations_are_not_retried_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/repos/o/r/pulls/7/comments"))
        .respond_with(ResponseTemplate::new(502)).expect(1).mount(&server).await;
    let err = client(&server.uri()).post_review_comment(&RepoRef::new("o", "r"), 7, &NewComment {
        commit_id: "s".into(), body: "b".into(), target: CommentTarget::File { path: "p".into() } }).await;
    assert_eq!(err.unwrap_err(), GhError::Server(502));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test payload_shapes && cargo test --test contract_post_comment`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `comment_payload`, `comment_from_rest`, `post_review_comment`, and the executor arm. (wiremock's `body_json` already compares parsed JSON, so no custom matcher is needed.)

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/comments.rs src/github/mod.rs tests/contract_post_comment.rs src/runtime.rs && git commit -m "feat: post immediate line and file-level comments"`.

---

### Task 32: Thread reply (contract test)

**Files:**
- Modify: `src/github/comments.rs`, `src/runtime.rs`
- Create: `tests/contract_reply.rs`
- Test: `tests/contract_reply.rs`

**Interfaces:**
- Consumes: Tasks 30, 31.
- Produces: `GithubClient::reply_to_comment(&self, repo: &RepoRef, number: u64, root_comment_id: u64, body: &str) -> Result<ThreadComment, GhError>`; new variant `Command::ReplyToThread { session, repo, number, thread_id: String, root_comment_id: u64, body: String }` → `Event::ReplyPosted { session, thread_id, comment }` / `Event::CommentFailed`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn replies_with_body_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/repos/o/r/pulls/7/comments/99/replies"))
        .and(body_json(json!({"body": "thanks"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 2, "body": "thanks", "in_reply_to_id": 99, "path": "p",
            "user": {"login": "me"}, "created_at": "2026-10-19T00:00:00Z"})))
        .expect(1).mount(&server).await;
    let got = client(&server.uri()).reply_to_comment(&RepoRef::new("o", "r"), 7, 99, "thanks").await.unwrap();
    assert_eq!(got.database_id, 2);
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test --test contract_reply`. Expected: FAIL.

- [ ] **Step 3: Implement** `reply_to_comment` (body only, no coordinates) and the executor arm.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/comments.rs src/runtime.rs tests/contract_reply.rs && git commit -m "feat: reply to review threads"`.

---

### Task 33: Resolve/unresolve via GraphQL (contract test)

**Files:**
- Modify: `src/github/threads.rs`, `src/runtime.rs`
- Create: `tests/contract_resolve.rs`
- Test: `tests/contract_resolve.rs`

**Interfaces:**
- Consumes: Tasks 12, 30.
- Produces: `pub const RESOLVE_MUTATION: &str`, `pub const UNRESOLVE_MUTATION: &str`, `pub fn resolve_vars(thread_id: &str) -> Value` (`{"threadId": …}`), `GithubClient::set_thread_resolved(&self, thread_id: &str, resolved: bool) -> Result<bool, GhError>` (mutations are not retried); new variant `Command::SetThreadResolved { session, thread_id, resolved }` → `Event::ThreadResolved { session, thread_id, resolved }` / `Event::Failed`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn resolve_sends_thread_id_and_reads_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"threadId": "PRRT_kwDOA"}})))
        .and(body_string_contains("resolveReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            {"data": {"resolveReviewThread": {"thread": {"isResolved": true}}}})))
        .mount(&server).await;
    assert!(client(&server.uri()).set_thread_resolved("PRRT_kwDOA", true).await.unwrap());
}

#[test]
fn mutations_use_the_right_fields() {
    assert!(RESOLVE_MUTATION.contains("resolveReviewThread"));
    assert!(UNRESOLVE_MUTATION.contains("unresolveReviewThread"));
    assert_eq!(resolve_vars("t")["threadId"], "t");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test --test contract_resolve`. Expected: FAIL.

- [ ] **Step 3: Implement** the mutations and executor arm.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/threads.rs src/runtime.rs tests/contract_resolve.rs && git commit -m "feat: resolve and unresolve threads"`.

---

### Task 34: Composer state machine

**Files:**
- Create: `src/state/composer.rs`
- Test: `src/state/composer.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 19.
- Produces:
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum CommentMode { #[default] Immediate, Pending }`
  - `#[derive(Clone, Debug, Default)] pub struct ComposerState { pub target: Option<CommentTarget>, pub body: String, pub mode: CommentMode, pub preview: bool, pub error: Option<String> }`
  - `pub enum ComposerMsg { OpenFor { target: CommentTarget, has_open_review: bool }, SetSide(Side), SetBody(String), SetMode(CommentMode), TogglePreview, Cancel, Submitted }`
  - `pub fn reduce(state: &mut ComposerState, msg: ComposerMsg)` — `OpenFor` sets `mode = default_mode(..)` and forces `Immediate` for file targets; `SetSide` only applies to single-line context anchors; `SetMode(Pending)` is ignored for file targets; `Submitted`/`Cancel` reset.
  - `pub fn default_mode(has_open_review: bool) -> CommentMode` (`Pending` when a review is open).
  - `pub fn can_submit(state: &ComposerState) -> bool` (non-blank body and a target).
  - `pub fn mode_buttons(target: &CommentTarget, has_open_review: bool) -> Vec<(&'static str, CommentMode)>` — `[("Add single comment", Immediate), ("Start a review", Pending)]`; with an open review `[("Add to review", Pending), ("Add single comment", Immediate)]`; file target `[("Add single comment", Immediate)]`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn modes_and_buttons_follow_github() {
    let line = CommentTarget::Line(line_anchor("a.rs", 1));
    let file = CommentTarget::File { path: "a.rs".into() };
    assert_eq!(default_mode(false), CommentMode::Immediate);
    assert_eq!(default_mode(true), CommentMode::Pending);
    assert_eq!(mode_buttons(&line, false)[1], ("Start a review", CommentMode::Pending));
    assert_eq!(mode_buttons(&line, true)[0], ("Add to review", CommentMode::Pending));
    assert_eq!(mode_buttons(&file, true), vec![("Add single comment", CommentMode::Immediate)]);

    let mut s = ComposerState::default();
    reduce(&mut s, ComposerMsg::OpenFor { target: file, has_open_review: true });
    assert_eq!(s.mode, CommentMode::Immediate);          // file-level can never be pending
    reduce(&mut s, ComposerMsg::SetMode(CommentMode::Pending));
    assert_eq!(s.mode, CommentMode::Immediate);
}

#[test]
fn composer_requires_body_and_target() {
    let mut s = ComposerState::default();
    reduce(&mut s, ComposerMsg::SetBody("x".into()));
    assert!(!can_submit(&s));
    reduce(&mut s, ComposerMsg::OpenFor { target: CommentTarget::Line(line_anchor("a.rs", 1)), has_open_review: false });
    reduce(&mut s, ComposerMsg::SetBody("   ".into()));
    assert!(!can_submit(&s));
    reduce(&mut s, ComposerMsg::SetBody("comment".into()));
    assert!(can_submit(&s));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test modes_and_buttons_follow_github composer_requires_body_and_target`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the reducer and helpers.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state/composer.rs src/state/mod.rs && git commit -m "feat: add comment composer state"`.

---

### Task 35: Suggestion blocks

**Files:**
- Create: `src/diff/suggestion.rs`
- Test: `src/diff/suggestion.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 15.
- Produces:
  - `pub fn suggestion_block(rows: &[DiffRow]) -> Option<String>` — wraps the text of every selected row that has a new-side number (added and context; removed rows skipped) in a ```` ```suggestion ```` fence; `None` if no row has a new-side number (LEFT-side selections cannot carry suggestions).
  - `#[derive(Debug, PartialEq)] pub struct Suggestion { pub text: String }` and `pub fn parse_suggestions(md: &str) -> Vec<Suggestion>` — line-oriented fence scan; an unterminated fence yields no suggestion and never panics.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn builds_from_new_side_rows_and_parses_back() {
    let rows = rows_from("@@ -1,2 +1,3 @@\n ctx\n-old\n+let x = 1;\n+let y = 2;\n");
    let block = suggestion_block(&rows).unwrap();
    assert_eq!(block, "```suggestion\nctx\nlet x = 1;\nlet y = 2;\n```");

    let md = format!("Please change:\n{block}\nthanks");
    assert_eq!(parse_suggestions(&md), vec![Suggestion { text: "ctx\nlet x = 1;\nlet y = 2;\n".into() }]);
}

#[test]
fn left_only_selection_and_bad_input() {
    assert!(suggestion_block(&rows_from("@@ -1,1 +0,0 @@\n-gone\n")).is_none());
    assert!(parse_suggestions("just a comment").is_empty());
    assert!(parse_suggestions("```suggestion\nunterminated").is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test builds_from_new_side_rows_and_parses_back left_only_selection_and_bad_input`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** both functions.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/suggestion.rs src/diff/mod.rs && git commit -m "feat: add suggestion blocks"`.

---

### Task 36: Markdown rendering safety (Review Focus #1)

**Files:**
- Create: `src/ui/widgets/markdown.rs`
- Test: `src/ui/widgets/markdown.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `pulldown-cmark`.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub struct MarkdownDoc { pub blocks: Vec<MdBlock> }`
  - `#[derive(Clone, Debug, PartialEq)] pub enum MdBlock { Paragraph(String), Heading(u8, String), Code { lang: String, text: String, suggestion: bool }, List(Vec<String>), Quote(String), Rule }`
  - `pub fn to_doc(md: &str) -> MarkdownDoc` (pure, never panics; raw HTML dropped; images rendered as their alt text plus URL).
  - `pub fn render(ui: &mut egui::Ui, doc: &MarkdownDoc)` — code blocks get a copy button; `suggestion: true` blocks are visually distinct.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn malformed_markdown_never_panics() {
    let stars = "*".repeat(10_000);
    let nested = ">".repeat(5_000) + " deep";
    let brackets = "[".repeat(10_000);
    let big = "a".repeat(100_000);
    for bad in ["```unterminated", stars.as_str(), nested.as_str(), brackets.as_str(),
                "[](#", "# \u{0} null", big.as_str()] {
        let _ = to_doc(bad);                              // must not panic
    }
}

#[test]
fn classifies_suggestion_code_block() {
    let doc = to_doc("```suggestion\nx\n```");
    assert!(matches!(&doc.blocks[0], MdBlock::Code { suggestion: true, .. }));
}

#[test]
fn drops_raw_html() {
    let doc = to_doc("<script>alert(1)</script>\n\nhi");
    assert!(doc.blocks.iter().all(|b| !matches!(b, MdBlock::Paragraph(p) if p.contains("script"))));
    assert!(doc.blocks.iter().any(|b| matches!(b, MdBlock::Paragraph(p) if p.contains("hi"))));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test malformed_markdown_never_panics classifies_suggestion_code_block drops_raw_html`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `to_doc` over `pulldown_cmark::Parser` with `ENABLE_TABLES | ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS`, dropping `Html`/`InlineHtml` events, and `render` with `egui` `LayoutJob`s. The `suggestion` flag comes from the fenced block's info string.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/markdown.rs src/ui/widgets/mod.rs && git commit -m "feat: add safe markdown rendering"`.

---

### Task 37: Inline threads, composer rows, and their UI

**Files:**
- Create: `src/diff/annotate.rs`, `src/ui/widgets/thread_view.rs`, `src/ui/widgets/comment_composer.rs`
- Modify: `src/ui/screens/pr_detail.rs`, `src/state/pr_detail.rs`
- Test: `src/diff/annotate.rs`, `src/ui/widgets/thread_view.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 21, 30–36.
- Produces:
  - `pub struct InlineItem { pub file: u32, pub placement: Placement, pub row: RowRef /* Thread | Pending | Composer */ }` with `pub enum Placement { AfterHeader, AfterLine { side: Side, line: u32 }, EndOfFile }`.
  - `pub fn place_thread(t: &ReviewThread, idx: u32, files: &[LoadedFile]) -> Option<InlineItem>` — FILE threads → `AfterHeader`; outdated/`line: None` threads → `EndOfFile` (rendered collapsed); otherwise after the row whose coordinate on `t.side` equals `t.line`, or `EndOfFile` if that row is not in the diff.
  - `pub fn annotate(model: &RowModel, files: &[LoadedFile], items: &[InlineItem]) -> RowModel` — inserts item rows (stable order: threads, then pending, then composer) and rebuilds `file_ranges` and heights.
  - `pub struct CommentVm { pub author: String, pub avatar: Option<String>, pub body_doc: MarkdownDoc, pub created_at: String, pub is_root: bool }` and `pub fn thread_vm(t: &ReviewThread) -> Vec<CommentVm>` (root first).
  - `pub fn render_thread(ui, thread, vm, actions)` with `ThreadAction { Reply { thread_id, root_comment_id, body }, ToggleResolved { thread_id, resolved }, CopySuggestion(String) }`, and `pub fn render_composer(ui, state: &mut ComposerState, has_open_review: bool, actions)` (mode buttons, Left/Right toggle for context rows, preview, "Insert suggestion" using Task 35).
  - Immediate comments are inserted optimistically as a single-comment thread with a negative/optimistic id and removed on `CommentFailed` (composer text restored).
  - Avatars: enable `egui_extras` image loaders (`cargo add egui_extras --features all_loaders,http` and `egui_extras::install_image_loaders`) or render initials if the feature set is unavailable.

- [ ] **Step 1: Write the failing tests**

```rust
fn thread(path: &str, line: Option<u32>, side: Side, subject: SubjectType) -> ReviewThread {
    ReviewThread { id: "T".into(), is_resolved: false, is_outdated: line.is_none(), path: path.into(),
        line, start_line: None, side: Some(side), subject_type: subject, comments: vec![] }
}

#[test]
fn threads_are_placed_by_side_and_line() {
    let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-x\n+y\n z\n")];
    let right = place_thread(&thread("a.rs", Some(1), Side::Right, SubjectType::Line), 0, &files).unwrap();
    assert_eq!(right.placement, Placement::AfterLine { side: Side::Right, line: 1 });
    let file_level = place_thread(&thread("a.rs", None, Side::Right, SubjectType::File), 1, &files).unwrap();
    assert_eq!(file_level.placement, Placement::AfterHeader);
    let outdated = place_thread(&thread("a.rs", None, Side::Right, SubjectType::Line), 2, &files).unwrap();
    assert_eq!(outdated.placement, Placement::EndOfFile);
    assert!(place_thread(&thread("other.rs", Some(1), Side::Right, SubjectType::Line), 3, &files).is_none());

    let model = annotate(&build(&files), &files, &[right]);
    let pos = model.rows.iter().position(|r| *r == RowRef::Thread { thread: 0 }).unwrap();
    assert_eq!(model.rows[pos - 1], RowRef::Line { file: 0, hunk: 0, row: 1 });   // after "+y" (new line 1)
}
```

and in `thread_view.rs`:

```rust
#[test]
fn thread_vm_orders_root_first() {
    let t = ReviewThread { comments: vec![
        ThreadComment { database_id: 1, author: "a".into(), avatar_url: None, body: "root".into(), created_at: "1".into() },
        ThreadComment { database_id: 2, author: "b".into(), avatar_url: None, body: "reply".into(), created_at: "2".into() },
    ], ..thread("a.rs", Some(1), Side::Right, SubjectType::Line) };
    let vm = thread_vm(&t);
    assert!(vm[0].is_root && !vm[1].is_root);
    assert_eq!(vm.len(), 2);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test threads_are_placed_by_side_and_line thread_vm_orders_root_first`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** placement, annotation, view-models, both render functions, optimistic insert/rollback, and wiring of actions to `Command`s in `PrDetail`. Re-annotate when threads, pending comments, or the composer change.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — post an immediate line comment and a file-level comment and see both inline and in the GitHub web UI at the same place; use "Comment on file" on a binary file (e.g. a PNG) and confirm the composer offers only "Add single comment" and the comment appears on that file in the web UI; reply to a thread; resolve and unresolve and confirm the state sticks after refresh; confirm outdated threads render collapsed.

- [ ] **Step 6: Commit** — `git add src/diff/annotate.rs src/diff/mod.rs src/ui src/state Cargo.toml Cargo.lock && git commit -m "feat: add inline threads and composer UI"`.

---

### Task 38: 422 fallback to a file-level comment

**Files:**
- Modify: `src/state/composer.rs`, `src/ui/screens/pr_detail.rs`
- Test: `src/state/composer.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 31, 34, 37.
- Produces:
  - `pub enum PostOutcome { Retryable(GhError), NeedsFileLevelFallback(String), Fatal(GhError) }`
  - `pub fn interpret_post_error(target: &CommentTarget, e: GhError) -> PostOutcome` — `Validation(msg)` on a **line** target → `NeedsFileLevelFallback(msg)`; `Validation` on a file target → `Fatal`; `Server`/`Network` → `Retryable` (manual retry button only); everything else `Fatal`.
  - `pub fn fallback_comment(c: &NewComment) -> NewComment` — same body and commit, `CommentTarget::File { path }` of the original anchor.
  - In `PrDetail`, `NeedsFileLevelFallback` opens a dialog showing the API message with "Add as a file-level comment"; the composer text is kept until a post succeeds.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn validation_on_a_line_offers_file_level_fallback() {
    let line = CommentTarget::Line(line_anchor("src/a.rs", 3));
    assert!(matches!(interpret_post_error(&line, GhError::Validation("line must be part of the diff".into())),
                     PostOutcome::NeedsFileLevelFallback(m) if m.contains("part of the diff")));
    assert!(matches!(interpret_post_error(&line, GhError::Network("x".into())), PostOutcome::Retryable(_)));
    let file = CommentTarget::File { path: "src/a.rs".into() };
    assert!(matches!(interpret_post_error(&file, GhError::Validation("x".into())), PostOutcome::Fatal(_)));

    let fb = fallback_comment(&NewComment { commit_id: "s".into(), body: "b".into(), target: line });
    assert_eq!(fb.target, CommentTarget::File { path: "src/a.rs".into() });
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test validation_on_a_line_offers_file_level_fallback`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** as specified.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state/composer.rs src/ui && git commit -m "feat: fall back to file-level comments on 422"`.

---

## Phase F — Pending review and submission

### Task 39: `PendingReview` model with commit pinning

**Files:**
- Create: `src/model/review.rs`, `src/state/pending.rs`
- Test: `src/state/pending.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Task 19.
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)] pub enum ReviewEvent { #[default] Comment, Approve, RequestChanges }` with `pub fn as_api_str(&self) -> &'static str` returning exactly `"COMMENT"`, `"APPROVE"`, `"REQUEST_CHANGES"`.
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] pub struct PendingComment { pub local_id: u64, pub anchor: Anchor, pub body: String }` — line anchors only (file-level comments cannot be pending).
  - `#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)] pub struct PendingReview { pub commit_id: Option<String>, pub body: String, pub event: ReviewEvent, pub comments: Vec<PendingComment>, pub started_at: Option<SystemTime>, pub next_local_id: u64 }`
  - `#[derive(Debug, PartialEq)] pub enum PendingError { PinnedToOtherCommit { pinned: String } }`
  - `impl PendingReview { pub fn add(&mut self, anchor: Anchor, body: String, diff_head_sha: &str) -> Result<u64, PendingError>; pub fn edit(&mut self, local_id: u64, body: String) -> bool; pub fn delete(&mut self, local_id: u64) -> bool; pub fn set_body(&mut self, body: String); pub fn set_event(&mut self, e: ReviewEvent); pub fn clear(&mut self); pub fn is_empty(&self) -> bool /* no comments and blank body */ }` — the first `add` pins `commit_id`; `add` against a different SHA fails; deleting the last comment unpins.
  - `PrSession` gains `pub pending: PendingReview`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn add_edit_delete_and_unique_ids() {
    let mut p = PendingReview::default();
    let a = p.add(line_anchor("a.rs", 1), "first".into(), "sha1").unwrap();
    let b = p.add(line_anchor("a.rs", 2), "second".into(), "sha1").unwrap();
    assert_ne!(a, b);
    assert!(p.edit(a, "edited".into()));
    assert_eq!(p.comments[0].body, "edited");
    assert!(p.delete(a) && p.delete(b));
    assert!(p.is_empty());
    assert!(!p.delete(a));
}

#[test]
fn draft_is_pinned_to_one_commit() {
    let mut p = PendingReview::default();
    p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
    assert_eq!(p.commit_id.as_deref(), Some("sha1"));
    assert_eq!(p.add(line_anchor("a.rs", 2), "y".into(), "sha2"),
               Err(PendingError::PinnedToOtherCommit { pinned: "sha1".into() }));
    let id = p.comments[0].local_id;
    p.delete(id);
    assert!(p.commit_id.is_none());                       // empty draft unpins
    assert!(p.add(line_anchor("a.rs", 2), "y".into(), "sha2").is_ok());
}

#[test]
fn review_event_serializes_exactly() {
    assert_eq!(ReviewEvent::Comment.as_api_str(), "COMMENT");
    assert_eq!(ReviewEvent::Approve.as_api_str(), "APPROVE");
    assert_eq!(ReviewEvent::RequestChanges.as_api_str(), "REQUEST_CHANGES");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test add_edit_delete_and_unique_ids draft_is_pinned_to_one_commit review_event_serializes_exactly`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the model and the session field.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/model src/state && git commit -m "feat: add pending review model"`.

---

### Task 40: Draft and composer persistence

**Files:**
- Modify: `src/persist/session.rs`, `src/state/pr_detail.rs`
- Test: `src/persist/session.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 29, 34, 39.
- Produces:
  - `pub fn save_draft(root: &Path, host: &str, repo: &RepoRef, number: u64, p: &PendingReview) -> anyhow::Result<()>` — read-modify-write of the session file, **immediate and atomic** (not debounced); an empty draft is stored as `None`.
  - `pub fn load_draft(root: &Path, host: &str, repo: &RepoRef, number: u64) -> anyhow::Result<Option<PendingReview>>`
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] pub struct ComposerDraft { pub target_path: String, pub anchor: Option<Anchor>, pub body: String }` with `save_composer_draft`/`load_composer_draft` (written through the session's `DebouncedWriter`).
  - `PrSession` calls `save_draft` after every pending mutation and restores the draft and composer draft on open.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn draft_survives_a_simulated_restart_and_writes_immediately() {
    let dir = tempdir();
    let mut p = PendingReview::default();
    p.add(line_anchor("a.rs", 42), "needs a test".into(), "sha1").unwrap();
    p.set_event(ReviewEvent::RequestChanges);
    save_draft(dir.path(), "github.com", &repo(), 7, &p).unwrap();

    // written synchronously: the file already contains the comment
    let raw = std::fs::read_to_string(session_path(dir.path(), "github.com", &repo(), 7)).unwrap();
    assert!(raw.contains("needs a test"));

    let mut restored = load_draft(dir.path(), "github.com", &repo(), 7).unwrap().unwrap();
    assert_eq!(restored, p);
    let n = restored.add(line_anchor("a.rs", 43), "2".into(), "sha1").unwrap();
    assert_ne!(n, restored.comments[0].local_id);        // ids stay unique after restart

    // other session fields survive a draft write
    let mut s = load(dir.path(), "github.com", &repo(), 7).unwrap().unwrap();
    s.last_opened_file = Some("a.rs".into());
    save(dir.path(), "github.com", &repo(), 7, &s).unwrap();
    save_draft(dir.path(), "github.com", &repo(), 7, &restored).unwrap();
    assert_eq!(load(dir.path(), "github.com", &repo(), 7).unwrap().unwrap().last_opened_file.as_deref(), Some("a.rs"));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test draft_survives_a_simulated_restart_and_writes_immediately`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and session wiring.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/persist/session.rs src/state && git commit -m "feat: persist pending review and composer drafts"`.

---

### Task 41: Reviews and server-side pending reviews (Review Focus #6)

**Files:**
- Create: `src/github/reviews.rs`, `tests/fixtures/reviews_page.json`, `tests/contract_server_pending.rs`
- Test: `src/github/reviews.rs` (`#[cfg(test)]`), `tests/contract_server_pending.rs`

**Interfaces:**
- Consumes: Tasks 7, 39.
- Produces:
  - `#[derive(Clone, Debug, PartialEq)] pub enum ReviewState { Approved, ChangesRequested, Commented, Dismissed, Pending }`
  - `#[derive(Clone, Debug, PartialEq)] pub struct Review { pub id: u64, pub author: String, pub state: ReviewState, pub body: String, pub submitted_at: Option<String> }`
  - `pub fn parse_reviews(body: &str) -> Result<Vec<Review>, GhError>` and `pub fn viewer_pending_review<'a>(reviews: &'a [Review], login: &str) -> Option<&'a Review>`
  - `pub struct ServerPendingComment { pub path: String, pub line: Option<u32>, pub side: Option<Side>, pub start_line: Option<u32>, pub body: String }`
  - `GithubClient::{list_reviews(repo, number) -> Result<Vec<Review>>, pending_review_comments(repo, number, review_id) -> Result<Vec<ServerPendingComment>>, submit_server_pending(repo, number, review_id, event: ReviewEvent, body: &str) -> Result<Review> /* POST .../reviews/{id}/events */, delete_server_pending(repo, number, review_id) -> Result<()> /* DELETE .../reviews/{id} */}`
  - New variants: `Command::LoadReviews { session, repo, number }` → `Event::ReviewsLoaded { session, reviews, server_pending: Option<(Review, Vec<ServerPendingComment>)> }`; `Command::SubmitServerPending { .. }`, `Command::DeleteServerPending { .. }` → `Event::ServerPendingResolved { session }`. `PrSession` gains `pub reviews: Vec<Review>` and `pub server_pending: Option<(Review, Vec<ServerPendingComment>)>`; `open` also emits `LoadReviews`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn finds_the_viewers_pending_review() {
    let rs = parse_reviews(fixture!("reviews_page.json")).unwrap();
    assert_eq!(rs.len(), 3);
    assert_eq!(rs[0].state, ReviewState::Approved);
    let p = viewer_pending_review(&rs, "me").unwrap();
    assert_eq!((p.id, p.state.clone()), (77, ReviewState::Pending));
    assert!(viewer_pending_review(&rs, "someone-else").is_none());
}
```

In `tests/contract_server_pending.rs`:

```rust
#[tokio::test]
async fn submits_and_deletes_server_pending_reviews() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/repos/o/r/pulls/7/reviews/77/events"))
        .and(body_json(json!({"event": "COMMENT", "body": "done"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "state": "COMMENTED", "body": "done", "user": {"login": "me"},
            "submitted_at": "2026-10-19T00:00:00Z"})))
        .expect(1).mount(&server).await;
    Mock::given(method("DELETE")).and(path("/repos/o/r/pulls/7/reviews/78"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 78, "state": "PENDING", "user": {"login": "me"}})))
        .expect(1).mount(&server).await;
    let c = client(&server.uri());
    let r = c.submit_server_pending(&RepoRef::new("o", "r"), 7, 77, ReviewEvent::Comment, "done").await.unwrap();
    assert_eq!(r.state, ReviewState::Commented);
    c.delete_server_pending(&RepoRef::new("o", "r"), 7, 78).await.unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test finds_the_viewers_pending_review && cargo test --test contract_server_pending`. Expected: FAIL.

- [ ] **Step 3: Implement** the parser, client methods, executor arms, and session fields. Create `reviews_page.json` with an `APPROVED` review, a `CHANGES_REQUESTED` review, and a `PENDING` review (id 77, `submitted_at` absent) by `me`. Server-pending comments render inline in the pending style labelled "pending on GitHub".

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/reviews.rs src/github/mod.rs tests/fixtures/reviews_page.json tests/contract_server_pending.rs src/runtime.rs src/state && git commit -m "feat: load reviews and handle server-side pending reviews"`.

---

### Task 42: Review panel view-model and UI (Review Focus #6)

**Files:**
- Create: `src/ui/widgets/review_panel.rs`
- Test: `src/ui/widgets/review_panel.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 36, 39, 41.
- Produces:
  - `pub struct PanelInputs<'a> { pub pending: &'a PendingReview, pub viewer_is_author: bool, pub server_pending: bool, pub head_moved_since_pin: bool }`
  - `pub struct ReviewPanelVm { pub pending_count: usize, pub can_approve: bool, pub can_request_changes: bool, pub submit_enabled: bool, pub blocked_reason: Option<&'static str>, pub banner: Option<String> }`
  - `pub fn review_panel_vm(i: &PanelInputs) -> ReviewPanelVm` — author → no Approve/Request changes; `RequestChanges` requires a non-blank body; `Comment` requires a body or a comment; `Approve` always submittable; a server-side pending review blocks submit with `"Submit or discard your pending review from GitHub first"`; `head_moved_since_pin` sets the banner `"Your draft was written against <sha7>"`.
  - `pub struct ReviewVm { pub author: String, pub state: ReviewState, pub body_doc: MarkdownDoc, pub submitted_at: String }` and `pub fn reviews_vm(reviews: &[Review]) -> Vec<ReviewVm>` (excludes `Pending`).
  - `pub fn render(ui, vm, pending, past, actions)` with `ReviewAction { Submit, Discard, SetEvent(ReviewEvent), SetBody(String), EditComment(u64, String), DeleteComment(u64), SubmitServerPending(ReviewEvent, String), DiscardServerPending }`; `Ctrl/⌘+Enter` submits when enabled; Discard asks for confirmation.

- [ ] **Step 1: Write the failing tests**

```rust
fn inputs(p: &PendingReview) -> PanelInputs<'_> {
    PanelInputs { pending: p, viewer_is_author: false, server_pending: false, head_moved_since_pin: false }
}

#[test]
fn author_cannot_approve_or_request_changes() {
    let p = PendingReview::default();
    let vm = review_panel_vm(&PanelInputs { viewer_is_author: true, ..inputs(&p) });
    assert!(!vm.can_approve && !vm.can_request_changes);
    let vm = review_panel_vm(&inputs(&p));
    assert!(vm.can_approve && vm.can_request_changes);
}

#[test]
fn submit_rules_follow_github() {
    let mut p = PendingReview::default();
    assert!(!review_panel_vm(&inputs(&p)).submit_enabled);        // COMMENT with nothing
    p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
    assert!(review_panel_vm(&inputs(&p)).submit_enabled);         // COMMENT with a comment
    p.set_event(ReviewEvent::RequestChanges);
    assert!(!review_panel_vm(&inputs(&p)).submit_enabled);        // REQUEST_CHANGES needs a body
    p.set_body("please fix".into());
    assert!(review_panel_vm(&inputs(&p)).submit_enabled);
    p.clear();
    p.set_event(ReviewEvent::Approve);
    assert!(review_panel_vm(&inputs(&p)).submit_enabled);         // APPROVE needs nothing
}

#[test]
fn server_pending_review_blocks_local_submit() {
    let mut p = PendingReview::default();
    p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
    let vm = review_panel_vm(&PanelInputs { server_pending: true, ..inputs(&p) });
    assert!(!vm.submit_enabled && vm.blocked_reason.is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test author_cannot_approve_or_request_changes submit_rules_follow_github server_pending_review_blocks_local_submit`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the view-models and panel (pending list with edit/delete, verdict selector with disabled options and tooltips, summary `TextEdit`, Submit, Discard, server-pending banner with Submit/Discard).

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/review_panel.rs src/ui/widgets/mod.rs && git commit -m "feat: add review panel"`.

---

### Task 43: Submit-review payload (contract test)

**Files:**
- Modify: `src/github/reviews.rs`, `src/runtime.rs`
- Create: `tests/contract_submit_review.rs`
- Test: `src/github/reviews.rs` (`#[cfg(test)]`), `tests/contract_submit_review.rs`

**Interfaces:**
- Consumes: Tasks 39, 41.
- Produces:
  - `pub fn review_payload(p: &PendingReview, head_sha: &str) -> serde_json::Value` — `commit_id` is `p.commit_id` (the pin) or `head_sha` when the draft has no comments; `body`, `event`; `comments` omitted when empty; each comment has `path`, `line`, `side`, optional `start_line`/`start_side`, `body`, and **no** per-comment `commit_id`.
  - `GithubClient::submit_review(&self, repo: &RepoRef, number: u64, payload: &Value) -> Result<Review, GhError>` (no retry).
  - New variant: `Command::SubmitReview { session, repo, number, payload: Value }` → `Event::ReviewSubmitted { session, review }` / `Event::ReviewSubmitFailed { session, error }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn payload_uses_the_pinned_commit_and_ranges() {
    let mut p = PendingReview::default();
    p.add(line_anchor("src/a.rs", 42), "b1".into(), "sha1").unwrap();
    p.add(range_anchor("src/b.rs", 10, 14), "b2".into(), "sha1").unwrap();
    p.set_body("overall".into());
    p.set_event(ReviewEvent::RequestChanges);
    let v = review_payload(&p, "sha2");                   // head moved; the pin wins
    assert_eq!(v, json!({
        "commit_id": "sha1", "body": "overall", "event": "REQUEST_CHANGES",
        "comments": [
            {"path": "src/a.rs", "line": 42, "side": "RIGHT", "body": "b1"},
            {"path": "src/b.rs", "start_line": 10, "start_side": "RIGHT", "line": 14, "side": "RIGHT", "body": "b2"}
        ]}));

    let mut approve = PendingReview::default();
    approve.set_event(ReviewEvent::Approve);
    assert_eq!(review_payload(&approve, "sha2"), json!({"commit_id": "sha2", "body": "", "event": "APPROVE"}));
}
```

In `tests/contract_submit_review.rs`:

```rust
#[tokio::test]
async fn posts_review_and_returns_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/repos/o/r/pulls/7/reviews"))
        .and(body_json(json!({"commit_id": "sha1", "body": "lgtm", "event": "APPROVE"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5, "state": "APPROVED", "body": "lgtm", "user": {"login": "me"},
            "submitted_at": "2026-01-01T00:00:00Z"})))
        .expect(1).mount(&server).await;
    let mut p = PendingReview::default();
    p.set_body("lgtm".into());
    p.set_event(ReviewEvent::Approve);
    let r = client(&server.uri()).submit_review(&RepoRef::new("o", "r"), 7, &review_payload(&p, "sha1")).await.unwrap();
    assert_eq!(r.state, ReviewState::Approved);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test payload_uses_the_pinned_commit_and_ranges && cargo test --test contract_submit_review`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `review_payload`, `submit_review`, and the executor arm.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/reviews.rs src/runtime.rs tests/contract_submit_review.rs && git commit -m "feat: submit reviews"`.

---

### Task 44: Submit wiring, author rule, moved head, and 401 safety (Review Focus #5, #7)

**Files:**
- Modify: `src/state/pr_detail.rs`, `src/ui/screens/pr_detail.rs`, `src/app.rs`
- Test: `src/state/pr_detail.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 9, 22, 39–43.
- Produces:
  - `pub fn viewer_is_author(session: &PrSession, viewer: &str) -> bool`
  - `pub fn head_moved_since_pin(session: &PrSession) -> bool` — the draft is pinned and `metadata.head_sha` differs from the pin.
  - `pub fn add_pending(session: &mut PrSession, anchor: Anchor, body: String) -> Result<u64, PendingError>` — uses `diff_head_sha`; the error surfaces as "submit or discard the draft written against <sha7> first" (immediate comments remain available).
  - `pub fn start_submit(session: &PrSession) -> Option<Command>` — `None` unless `review_panel_vm` allows it.
  - `pub fn on_review_submitted(session: &mut PrSession, review: Review) -> Vec<Command>` — clears `pending` (and its persisted copy), appends the review, and returns `LoadThreads` + `LoadReviews` so new comments are anchored.
  - `pub fn on_submit_error(session: &mut PrSession, e: GhError)` — keeps `pending` intact and records the error.

- [ ] **Step 1: Write the failing tests**

```rust
fn ready_session(head: &str) -> PrSession {
    let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
    s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta_by("me", head) });
    s.on_event(&Event::PrFilesPage { session: SessionId(1), files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")], done: true });
    s
}

#[test]
fn author_detection() {
    let s = ready_session("sha1");
    assert!(viewer_is_author(&s, "me"));
    assert!(!viewer_is_author(&s, "other"));
}

#[test]
fn failure_keeps_pending_and_success_clears_it() {
    let mut s = ready_session("sha1");
    add_pending(&mut s, line_anchor("a.rs", 1), "x".into()).unwrap();
    on_submit_error(&mut s, GhError::Network("down".into()));
    assert_eq!(s.pending.comments.len(), 1);              // never lost
    let cmds = on_review_submitted(&mut s, review(ReviewState::Commented));
    assert!(s.pending.is_empty());
    assert_eq!(s.reviews.len(), 1);
    assert!(cmds.iter().any(|c| matches!(c, Command::LoadThreads { .. })));
}

#[test]
fn moved_head_keeps_the_draft_pinned() {
    let mut s = ready_session("sha1");
    add_pending(&mut s, line_anchor("a.rs", 1), "x".into()).unwrap();
    s.refresh();
    s.on_event(&Event::PrMetadataLoaded { session: SessionId(1), meta: meta_by("me", "sha2") });
    s.on_event(&Event::PrFilesPage { session: SessionId(1), files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+c\n")], done: true });
    assert!(head_moved_since_pin(&s));
    assert!(add_pending(&mut s, line_anchor("a.rs", 1), "y".into()).is_err());
    assert_eq!(s.pending.commit_id.as_deref(), Some("sha1"));
}

#[test]
fn unauthorized_never_drops_the_persisted_draft() {
    let dir = tempdir();
    let mut p = PendingReview::default();
    p.add(line_anchor("a.rs", 1), "pending body".into(), "sha1").unwrap();
    save_draft(dir.path(), "github.com", &repo(), 7, &p).unwrap();

    let store = MemoryStore::default();
    let settings = Settings { account: Some(Account { login: "me".into(), ..Default::default() }), ..Default::default() };
    store.set("github.com", "me", "tok").unwrap();
    handle_unauthorized(&store, &settings).unwrap();
    assert_eq!(store.get("github.com", "me").unwrap(), None);
    assert_eq!(load_draft(dir.path(), "github.com", &repo(), 7).unwrap().unwrap(), p);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test author_detection failure_keeps_pending_and_success_clears_it moved_head_keeps_the_draft_pinned unauthorized_never_drops_the_persisted_draft`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and wire Submit/Discard, the server-pending actions, and the pinned-draft banner in `PrDetail`; disable Approve/Request changes with a tooltip when the viewer is the author.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — on a PR you did not author, leave 2 pending comments (one multi-line) and submit Request Changes; confirm they appear in the GitHub web UI at the right lines and the buffer clears. Start a review in the web UI, open the PR in the app, and confirm the server-pending banner appears and both Submit and Discard work. Push a commit while a draft exists and confirm the draft stays pinned and only changed files lose viewed state. Revoke the token while a draft exists, confirm the app returns to Login, sign in again, and confirm the draft is restored.

- [ ] **Step 6: Commit** — `git add src/state src/ui src/app.rs && git commit -m "feat: wire review submission"`.

---

## Phase G — Rich diff

### Task 45: Context expansion via the head blob (Review Focus #4)

**Files:**
- Create: `src/github/blobs.rs`, `src/diff/expand.rs`
- Test: `src/diff/expand.rs`, `src/github/blobs.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 7, 15, 21.
- Produces:
  - `pub fn encode_path(path: &str) -> String` — percent-encodes each `/`-separated segment (spaces, `#`, `?`, `%`, non-ASCII).
  - `GithubClient::file_content(&self, repo: &RepoRef, path: &str, sha: &str) -> Result<String, GhError>` — `GET /repos/{o}/{r}/contents/{encode_path(path)}?ref={sha}` with `Accept: application/vnd.github.raw`.
  - `#[derive(Clone, Copy, Debug, PartialEq)] pub struct Gap { pub before_hunk: u32 /* == hunks.len() for the tail gap */, pub new_start: u32, pub new_end: u32 /* inclusive */, pub offset: i64 /* new − old */ }`
  - `pub fn gaps(diff: &FileDiff, head_line_count: u32) -> Vec<Gap>` — unchanged ranges before, between, and after hunks.
  - `pub enum Expand { Up(u32), Down(u32), All }`
  - `pub fn expand(diff: &FileDiff, head_lines: &[&str], gap: Gap, how: Expand) -> FileDiff` — inserts `Context` rows with `origin: Expanded`, `new_no` from the head blob and `old_no = new_no − offset`.
  - New variant: `Command::ExpandContext { session, file: u32, gap: Gap, how: Expand }` → `Event::HeadBlobLoaded { session, file: u32, text: String }` (cached per file; the expansion itself runs on the UI thread from the cache).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn gaps_follow_hunk_offsets() {
    // old: l1..l9; new: l1..l5, ins, l6..l9 (one inserted line)
    let diff = FileDiff { hunks: parse_patch("@@ -4,3 +4,4 @@\n l4\n l5\n+ins\n l6\n").unwrap() };
    let g = gaps(&diff, 10);
    assert_eq!(g, vec![
        Gap { before_hunk: 0, new_start: 1, new_end: 3, offset: 0 },
        Gap { before_hunk: 1, new_start: 8, new_end: 10, offset: 1 },
    ]);
}

#[test]
fn expanded_rows_have_exact_coordinates() {
    let head = ["l1", "l2", "l3", "l4", "l5", "ins", "l6", "l7", "l8", "l9"];
    let diff = FileDiff { hunks: parse_patch("@@ -4,3 +4,4 @@\n l4\n l5\n+ins\n l6\n").unwrap() };
    let tail = gaps(&diff, 10)[1];
    let out = expand(&diff, &head, tail, Expand::All);
    let rows: Vec<_> = out.hunks.iter().flat_map(|h| &h.rows).filter(|r| r.origin == RowOrigin::Expanded).collect();
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].old_no, rows[0].new_no, rows[0].text.as_str()), (Some(7), Some(8), "l7"));
    assert_eq!(row_anchor(rows[0]), Some((Side::Right, 8)));

    let head_gap = gaps(&diff, 10)[0];
    let up = expand(&diff, &head, head_gap, Expand::Up(2));          // last 2 lines above the hunk
    let first = &up.hunks[0].rows[0];
    assert_eq!((first.old_no, first.new_no), (Some(2), Some(2)));
}
```

and in `src/github/blobs.rs`:

```rust
#[test]
fn encodes_each_segment() {
    assert_eq!(encode_path("src/new name#1.rs"), "src/new%20name%231.rs");
    assert_eq!(encode_path("docs/ünïcode.md"), "docs/%C3%BCn%C3%AFcode.md");
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test gaps_follow_hunk_offsets expanded_rows_have_exact_coordinates encodes_each_segment`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions, executor arm, head-blob cache in `PrSession`, and `DiffAction::ExpandGap` wiring (Gap rows render "↑ 20 / ↓ 20 / all").

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github/blobs.rs src/github/mod.rs src/diff src/runtime.rs src/state src/ui && git commit -m "feat: expand diff context from the head blob"`.

---

### Task 46: Local diff for files GitHub sends without a patch

**Files:**
- Create: `src/diff/local.rs`, `tests/contract_merge_base.rs`
- Modify: `src/github/blobs.rs`, `src/runtime.rs`
- Test: `src/diff/local.rs` (`#[cfg(test)]`), `tests/contract_merge_base.rs`

**Interfaces:**
- Consumes: Tasks 17, 18, 45.
- Produces:
  - `GithubClient::merge_base(&self, repo: &RepoRef, base_sha: &str, head_sha: &str) -> Result<String, GhError>` — `GET /repos/{o}/{r}/compare/{base}...{head}` → `merge_base_commit.sha`.
  - `pub fn local_diff(old: &str, new: &str, context: usize) -> FileDiff` — `similar::TextDiff::from_lines` grouped into hunks with 1-based numbering; every row `origin: Local`.
  - New variant: `Command::LoadLargeDiff { session, file: u32 }` → executor fetches the merge base (cached per session), the base blob at the merge base (`previous_path` for renames; skipped for added files), and the head blob (skipped for removed files), then emits `Event::LocalDiffReady { session, file, diff: FileDiff }`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn local_diff_numbers_rows_and_marks_them_local() {
    let old = "a\nb\nc\nd\n";
    let new = "a\nB\nc\nd\ne\n";
    let d = local_diff(old, new, 1);
    let rows: Vec<_> = d.hunks.iter().flat_map(|h| &h.rows).collect();
    assert!(rows.iter().all(|r| r.origin == RowOrigin::Local));
    let added_b = rows.iter().find(|r| r.text == "B").unwrap();
    assert_eq!((added_b.kind.clone(), added_b.new_no), (LineKind::Added, Some(2)));
    let removed_b = rows.iter().find(|r| r.text == "b").unwrap();
    assert_eq!(removed_b.old_no, Some(2));
    assert_eq!(rows.iter().find(|r| r.text == "e").unwrap().new_no, Some(5));
}
```

In `tests/contract_merge_base.rs`: mount `GET /repos/o/r/compare/base1...head1` returning `{"merge_base_commit": {"sha": "mb"}}` and assert `merge_base` returns `"mb"`.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test local_diff_numbers_rows_and_marks_them_local && cargo test --test contract_merge_base`. Expected: FAIL.

- [ ] **Step 3: Implement** as specified, plus the "Load diff" button on `TooLarge` placeholders.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/local.rs src/diff/mod.rs src/github/blobs.rs src/runtime.rs tests/contract_merge_base.rs src/ui src/state && git commit -m "feat: diff large files locally against the merge base"`.

---

### Task 47: Anchor warnings for expanded and local rows

**Files:**
- Modify: `src/state/composer.rs`, `src/ui/widgets/comment_composer.rs`
- Test: `src/state/composer.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 34, 45, 46.
- Produces:
  - `pub fn anchor_warning(rows: &[&DiffRow]) -> Option<&'static str>` — any `Expanded` row → `"This line is outside the PR's diff; GitHub may reject the comment."`; any `Local` row → `"This diff was computed locally; GitHub may place or reject the comment differently."`; otherwise `None`.
  - The composer shows the warning banner and an "Add as file-level comment instead" button when a warning applies.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn warns_only_for_non_patch_rows() {
    let mut rows = rows_from("@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
    assert_eq!(anchor_warning(&rows.iter().collect::<Vec<_>>()), None);
    rows[0].origin = RowOrigin::Expanded;
    assert!(anchor_warning(&[&rows[0]]).unwrap().contains("outside"));
    rows[2].origin = RowOrigin::Local;
    assert!(anchor_warning(&[&rows[2]]).unwrap().contains("locally"));
}
```

- [ ] **Step 2: Run test to verify it fails** — `cargo test warns_only_for_non_patch_rows`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `anchor_warning` and the banner.

- [ ] **Step 4: Run test to verify it passes**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/state/composer.rs src/ui/widgets/comment_composer.rs && git commit -m "feat: warn before anchoring to expanded or local rows"`.

---

### Task 48: Diff options — hide whitespace-only hunks, in-diff find, word wrap

**Files:**
- Create: `src/diff/options.rs`
- Modify: `src/ui/widgets/diff_view.rs`
- Test: `src/diff/options.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 15, 21, 23.
- Produces:
  - `pub fn hide_whitespace_only(diff: &FileDiff) -> FileDiff` — drops hunks whose removed and added rows are equal after removing all whitespace (an approximation of "ignore whitespace"; the files API has no `w=1`). Row numbers of the kept hunks are unchanged.
  - `pub const MAX_FIND_MATCHES: usize = 1_000;`
  - `pub fn find_matches<'a>(rows: impl Iterator<Item = (usize, &'a str)>, needle: &str) -> Vec<usize>` — case-insensitive row indices, capped at `MAX_FIND_MATCHES`; empty needle → no matches.
  - `pub fn next_match(matches: &[usize], current: Option<usize>) -> Option<usize>` / `pub fn prev_match(matches: &[usize], current: Option<usize>) -> Option<usize>` (wrapping).
  - Toolbar toggles for wrap and hide-whitespace; a find box (Enter → next, Shift+Enter → previous) that scrolls via `row_model.offset_of`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn whitespace_only_hunks_are_hidden() {
    let ws = FileDiff { hunks: parse_patch("@@ -1,2 +1,2 @@\n fn a() {\n-\tlet x = 1;\n+\tlet x = 1;  \n").unwrap() };
    assert!(hide_whitespace_only(&ws).hunks.is_empty());
    let real = FileDiff { hunks: parse_patch("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n").unwrap() };
    assert_eq!(hide_whitespace_only(&real).hunks.len(), 1);
}

#[test]
fn find_is_case_insensitive_capped_and_wraps() {
    let texts = ["Foo", "bar", "foo"];
    let m = find_matches(texts.iter().copied().enumerate(), "foo");
    assert_eq!(m, vec![0, 2]);
    assert_eq!(next_match(&m, Some(2)), Some(0));
    assert_eq!(prev_match(&m, Some(0)), Some(2));
    let many: Vec<String> = (0..1_500).map(|_| "needle".to_string()).collect();
    assert_eq!(find_matches(many.iter().map(String::as_str).enumerate(), "NEEDLE").len(), MAX_FIND_MATCHES);
    assert!(find_matches(texts.iter().copied().enumerate(), "").is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test whitespace_only_hunks_are_hidden find_is_case_insensitive_capped_and_wraps`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the functions and toolbar wiring.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/diff/options.rs src/diff/mod.rs src/ui/widgets/diff_view.rs && git commit -m "feat: add diff options and find"`.

---

### Task 49: Image diff viewer

**Files:**
- Create: `src/diff/image.rs`, `src/ui/widgets/image_diff.rs`, `tests/contract_file_bytes.rs`
- Modify: `src/github/blobs.rs`, `src/runtime.rs`, `src/state/pr_detail.rs`, `src/ui/widgets/diff_view.rs`, `src/app.rs`, `Cargo.toml`
- Test: `src/diff/image.rs` (`#[cfg(test)]`), `tests/contract_file_bytes.rs`

**Interfaces:**
- Consumes: Tasks 17, 21, 23, 45 (`encode_path`), 46 (merge-base cache).
- Produces:
  - Dependencies: `cargo add egui_extras --features image,svg` and `cargo add image --no-default-features --features png,jpeg,gif,webp,bmp,ico` (egui_extras decodes through the `image` crate and only supports the formats enabled there). `app::run` calls `egui_extras::install_image_loaders(&cc.egui_ctx)`.
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ImageKind { Raster, Svg }` and `pub fn image_kind(path: &str) -> Option<ImageKind>` — by file-name extension, case-insensitive: `png jpg jpeg gif webp bmp ico` → `Raster`, `svg` → `Svg`.
  - `pub fn image_sources(f: &PrFile) -> (Option<String> /* base path */, Option<String> /* head path */)` — Added → `(None, Some(path))`; Removed → `(Some(path), None)`; Renamed/Copied → `(Some(previous_path), Some(path))`; otherwise `(Some(path), Some(path))`. The base blob is read **at the merge base**, the head blob at `diff_head_sha`.
  - `pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;`
  - `GithubClient::file_bytes(&self, repo: &RepoRef, path: &str, sha: &str, max: u64) -> Result<Option<Vec<u8>>, GhError>` — same contents URL and `Accept: application/vnd.github.raw` as `file_content`; returns `None` when `Content-Length` (or the streamed body) exceeds `max`, without buffering the rest.
  - `#[derive(Clone, Debug)] pub enum ImageBlob { Absent, TooLarge, Bytes(Arc<[u8]>) }`
  - New variants: `Command::LoadImages { session, file: u32 }` → `Event::ImagesLoaded { session, file: u32, base: ImageBlob, head: ImageBlob }`. Images load lazily when the file's header row first becomes visible (never for every file upfront). `PrSession` keeps loaded blobs in an LRU capped at 200 MB total; evicting one also calls `ctx.forget_image(uri)` for its texture.
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)] pub enum ImageMode { #[default] TwoUp, Swipe, Onion }`
  - Pure layout helpers: `pub fn fit_size(img: (f32, f32), avail: (f32, f32)) -> (f32, f32)` (keeps aspect ratio, never upscales), `pub fn swipe_split(t: f32) -> f32` (clamps to 0..=1), `pub fn size_delta_label(base: Option<u64>, head: Option<u64>) -> String`.
  - `pub fn render(ui: &mut egui::Ui, file: &PrFile, base: &ImageBlob, head: &ImageBlob, mode: &mut ImageMode, swipe: &mut f32, opacity: &mut f32)` — images drawn with `egui::Image::from_bytes("bytes://<session>/<file>/<base|head>", bytes)` on a checkerboard (so transparency is visible), captioned with pixel dimensions and `size_delta_label`. **2-up** shows base and head side by side; **Swipe** draws both in one rect and clips the head to the left of a draggable divider; **Onion** draws head over base with an opacity slider. Added/removed images show a single image and hide the mode switch. `TooLarge` or undecodable blobs fall back to the binary placeholder with "Open in browser".
  - Diff view: for `PatchState::Binary` files with `image_kind == Some(Raster)`, the `Placeholder` row renders the viewer instead of the placeholder text, and its row height is updated via `RowModel::set_height` once the images are laid out. SVG files have a text patch, so they keep the text diff and get a "Text / Rendered" toggle in the file header that swaps in the same viewer.
  - Comments on images use the header's "Comment on file" button (Task 23) and are always immediate (spec §7.3). GitHub cannot anchor a comment to a region of an image.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn detects_images_case_insensitively() {
    assert_eq!(image_kind("assets/Logo.PNG"), Some(ImageKind::Raster));
    assert_eq!(image_kind("icons/x.svg"), Some(ImageKind::Svg));
    assert_eq!(image_kind("data.bin"), None);
    assert_eq!(image_kind("png"), None);                  // no extension
}

#[test]
fn picks_blobs_per_status() {
    let added = PrFile { status: FileStatus::Added, ..file("a.png") };
    assert_eq!(image_sources(&added), (None, Some("a.png".to_string())));
    let removed = PrFile { status: FileStatus::Removed, ..file("a.png") };
    assert_eq!(image_sources(&removed), (Some("a.png".to_string()), None));
    let renamed = PrFile { status: FileStatus::Renamed, previous_path: Some("old.png".into()), ..file("new.png") };
    assert_eq!(image_sources(&renamed), (Some("old.png".to_string()), Some("new.png".to_string())));
    assert_eq!(image_sources(&file("a.png")), (Some("a.png".to_string()), Some("a.png".to_string())));
}

#[test]
fn layout_helpers() {
    assert_eq!(fit_size((2000.0, 1000.0), (500.0, 500.0)), (500.0, 250.0));
    assert_eq!(fit_size((100.0, 50.0), (500.0, 500.0)), (100.0, 50.0));   // never upscale
    assert_eq!((swipe_split(1.5), swipe_split(-1.0)), (1.0, 0.0));
    assert_eq!(size_delta_label(Some(1024), Some(1536)), "1.0 KB → 1.5 KB (+512 B)");
    assert_eq!(size_delta_label(None, Some(2048)), "2.0 KB (added)");
    assert_eq!(size_delta_label(Some(2048), None), "2.0 KB (removed)");
}
```

In `tests/contract_file_bytes.rs`:

```rust
#[tokio::test]
async fn fetches_raw_bytes_with_encoded_path_and_respects_the_cap() {
    let server = MockServer::start().await;
    let png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    Mock::given(method("GET")).and(path("/repos/o/r/contents/assets/my%20logo.png"))
        .and(query_param("ref", "sha1"))
        .and(header("accept", "application/vnd.github.raw"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .mount(&server).await;
    let c = client(&server.uri());
    let r = RepoRef::new("o", "r");
    assert_eq!(c.file_bytes(&r, "assets/my logo.png", "sha1", 1024).await.unwrap(), Some(png));
    assert_eq!(c.file_bytes(&r, "assets/my logo.png", "sha1", 4).await.unwrap(), None);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test detects_images_case_insensitively picks_blobs_per_status layout_helpers && cargo test --test contract_file_bytes`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the dependencies, helpers, `file_bytes`, the `LoadImages` executor arm (reusing the Task 46 merge-base cache), lazy loading and the LRU in `PrSession`, the viewer widget, and the diff-view integration.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — open a PR that adds, modifies, renames, and deletes PNG/JPEG images plus one SVG. Confirm: modified images show 2-up with dimensions and size delta; swipe and onion skin work; added and removed images show a single image; transparent PNGs show the checkerboard; the SVG toggles between text diff and rendered view; an image over 10 MB shows the placeholder with "Open in browser"; scrolling a PR with many images stays smooth and only visible images load; "Comment on file" on an image posts a file-level comment that appears on that file in the GitHub web UI.

- [ ] **Step 6: Commit** — `git add Cargo.toml Cargo.lock src/diff/image.rs src/diff/mod.rs src/ui/widgets/image_diff.rs src/ui/widgets/mod.rs src/ui/widgets/diff_view.rs src/github/blobs.rs src/runtime.rs src/state/pr_detail.rs src/app.rs tests/contract_file_bytes.rs && git commit -m "feat: add image diff viewer"`.

---

## Phase H — PR metadata surface

### Task 50: Commits list and CI checks

**Files:**
- Create: `src/github/commits.rs`, `src/github/checks.rs`, `src/ui/widgets/commit_list.rs`, `src/ui/widgets/checks.rs`, `tests/fixtures/commits_page.json`, `tests/fixtures/check_runs.json`
- Test: `src/github/checks.rs`, `src/github/commits.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 7, 12 (`CiState`).
- Produces:
  - `pub struct Commit { pub sha: String, pub message: String, pub author: String, pub committed_at: String }` and `pub fn parse_commits(body: &str) -> Result<Vec<Commit>, GhError>` (author falls back to `commit.author.name` when `author` is null).
  - `pub struct CheckRun { pub name: String, pub status: String, pub conclusion: Option<String>, pub html_url: Option<String> }`, `pub fn parse_check_runs(body: &str) -> Result<Vec<CheckRun>, GhError>` (reads `check_runs`).
  - `pub struct CombinedStatus { pub state: String, pub total_count: u32 }` and `pub fn parse_combined_status(body: &str) -> Result<CombinedStatus, GhError>`.
  - `pub fn ci_state(runs: &[CheckRun], combined: &CombinedStatus) -> Option<CiState>` — any failing conclusion (`failure`, `timed_out`, `cancelled`, `action_required`) or combined `failure`/`error` → `Failure`; any run not `completed` or combined `pending` **with `total_count > 0`** → `Pending`; any signal at all → `Success`; no runs and `total_count == 0` → `None` (GitHub reports `pending` when there are no statuses).
  - New variants: `Command::LoadCommits { session, .. }`, `Command::LoadChecks { session, sha }` → `Event::CommitsLoaded`, `Event::ChecksLoaded`; `open` emits both (checks once metadata provides the head SHA).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn ci_state_prefers_failure_then_pending_and_ignores_empty_status() {
    let runs = parse_check_runs(fixture!("check_runs.json")).unwrap();
    let empty = CombinedStatus { state: "pending".into(), total_count: 0 };
    assert_eq!(ci_state(&runs, &empty), Some(CiState::Failure));
    assert_eq!(ci_state(&[], &empty), None);                    // no CI configured
    assert_eq!(ci_state(&[], &CombinedStatus { state: "pending".into(), total_count: 2 }), Some(CiState::Pending));
    assert_eq!(ci_state(&[], &CombinedStatus { state: "success".into(), total_count: 1 }), Some(CiState::Success));
}

#[test]
fn parses_commits() {
    let cs = parse_commits(fixture!("commits_page.json")).unwrap();
    assert_eq!(cs.len(), 2);
    assert!(cs[0].message.starts_with("Fix"));
    assert_eq!(cs[1].author, "Mona Lisa");                        // null GitHub user
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test ci_state_prefers_failure_then_pending_and_ignores_empty_status parses_commits`. Expected: FAIL.

- [ ] **Step 3: Implement** the parsers, `ci_state`, executor arms, and the right-pane commits and checks lists (links open in the browser). Create both fixtures (one failing and one successful check run; two commits, the second with `author: null`).

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/github src/ui/widgets tests/fixtures/commits_page.json tests/fixtures/check_runs.json src/runtime.rs src/state && git commit -m "feat: add commits and CI checks"`.

---

### Task 51: Metadata panel, conversation, and referenced issues

**Files:**
- Create: `src/ui/widgets/metadata_panel.rs`, `src/github/conversation.rs`, `tests/fixtures/issue_comments.json`
- Test: `src/ui/widgets/metadata_panel.rs`, `src/github/conversation.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 18, 36, 41.
- Produces:
  - `#[derive(Debug, PartialEq)] pub struct IssueRef { pub repo: Option<String>, pub number: u64 }` and `pub fn referenced_issues(body: &str) -> Vec<IssueRef>` — `#N` and `owner/repo#N` tokens, de-duplicated, in order; `#` not preceded by start/whitespace/`(` (e.g. `abc#1`, URL fragments) is ignored.
  - `pub struct ConversationComment { pub author: String, pub body: String, pub created_at: String }`, `pub fn parse_issue_comments(body: &str) -> Result<Vec<ConversationComment>, GhError>`, and `Command::LoadConversation { session, .. }` → `Event::ConversationLoaded`.
  - `pub enum TimelineItem { Review(ReviewVm), Comment(ConversationComment) }` and `pub fn timeline(reviews: &[Review], comments: &[ConversationComment]) -> Vec<TimelineItem>` sorted by timestamp.
  - The right pane: description via Task 36, reviewers/labels/milestone chips, referenced-issue links, and the read-only timeline.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn extracts_referenced_issues() {
    assert_eq!(referenced_issues("Fixes #12 and closes acme/api#34; see #12 again, not abc#5"), vec![
        IssueRef { repo: None, number: 12 },
        IssueRef { repo: Some("acme/api".into()), number: 34 },
    ]);
}

#[test]
fn timeline_merges_reviews_and_comments_by_time() {
    let reviews = vec![Review { submitted_at: Some("2026-10-02T00:00:00Z".into()), ..review(ReviewState::Approved) }];
    let comments = parse_issue_comments(fixture!("issue_comments.json")).unwrap();   // 2026-10-01 and 2026-10-03
    let t = timeline(&reviews, &comments);
    assert!(matches!(t[1], TimelineItem::Review(_)));
    assert_eq!(t.len(), 3);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test extracts_referenced_issues timeline_merges_reviews_and_comments_by_time`. Expected: FAIL.

- [ ] **Step 3: Implement** the functions, executor arm, fixture, and right-pane sections.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui/widgets/metadata_panel.rs src/github/conversation.rs src/github/mod.rs tests/fixtures/issue_comments.json src/runtime.rs src/state && git commit -m "feat: add metadata panel and conversation timeline"`.

---

## Phase I — Power UX

### Task 52: Command registry, palette, keybindings, and navigation

**Files:**
- Create: `src/ui/command.rs`, `src/ui/widgets/command_palette.rs`
- Modify: `src/app.rs`
- Test: `src/ui/command.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 23, 28.
- Produces:
  - `pub struct ActionEntry { pub id: &'static str, pub label: &'static str, pub keybinding: Option<&'static str> }`
  - `pub fn registry() -> Vec<ActionEntry>` containing at least: `palette` (`Cmd+K`), `help` (`?`), `next_file` (`j`), `prev_file` (`k`), `next_hunk` (`n`), `prev_hunk` (`p`), `toggle_viewed` (`v`), `mark_viewed_and_next` (`Shift+V`), `comment` (`c`), `reply` (`r`), `toggle_side_pane` (`o`), `split` (`s`), `unified` (`u`), `submit_review` (`Cmd+Enter`), `close` (`Esc`).
  - `pub fn fuzzy_filter<'a>(entries: &'a [ActionEntry], query: &str) -> Vec<&'a ActionEntry>` — case-insensitive subsequence match on `label`, ranked by first-match position then label length.
  - `pub struct KeyPress { pub key: String /* "v", "?", "Enter", "Escape", … */, pub shift: bool, pub command: bool /* ⌘ on macOS, Ctrl elsewhere */ }`
  - `pub fn resolve_key(k: &KeyPress, text_focused: bool) -> Option<&'static str>` — unmodified single keys resolve only when `text_focused` is false; `Cmd+K`, `Cmd+Enter`, `Escape` always resolve. In `app.rs`, `text_focused = ctx.wants_keyboard_input()`, and `?` is read from `egui::Event::Text`.
  - `pub fn next_index(order: &[usize], current: Option<usize>, forward: bool) -> Option<usize>` — used for both file (`j`/`k`, over the filtered file order) and hunk (`n`/`p`) navigation; clamps at the ends.

- [ ] **Step 1: Write the failing tests**

```rust
fn key(k: &str) -> KeyPress { KeyPress { key: k.into(), shift: false, command: false } }

#[test]
fn fuzzy_matches_subsequence_and_ranks() {
    let es = registry();
    assert!(fuzzy_filter(&es, "togview").iter().any(|e| e.id == "toggle_viewed"));
    assert!(fuzzy_filter(&es, "zzz").is_empty());
}

#[test]
fn keys_resolve_only_without_text_focus() {
    assert_eq!(resolve_key(&key("v"), false), Some("toggle_viewed"));
    assert_eq!(resolve_key(&key("s"), true), None);                       // typing "s" in a comment
    assert_eq!(resolve_key(&KeyPress { shift: true, ..key("V") }, false), Some("mark_viewed_and_next"));
    assert_eq!(resolve_key(&KeyPress { command: true, ..key("K") }, true), Some("palette"));
    assert_eq!(resolve_key(&KeyPress { command: true, ..key("Enter") }, true), Some("submit_review"));
    assert_eq!(resolve_key(&key("Escape"), true), Some("close"));
    assert_eq!(resolve_key(&key("n"), false), Some("next_hunk"));
}

#[test]
fn navigation_clamps() {
    let order = [3, 5, 9];
    assert_eq!(next_index(&order, None, true), Some(3));
    assert_eq!(next_index(&order, Some(5), true), Some(9));
    assert_eq!(next_index(&order, Some(9), true), Some(9));
    assert_eq!(next_index(&order, Some(3), false), Some(3));
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test fuzzy_matches_subsequence_and_ranks keys_resolve_only_without_text_focus navigation_clamps`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** the registry, matching, key resolution, navigation, the palette widget (Enter runs the selected action; `Esc` closes), the `?` help overlay, and dispatch in `app.rs`.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Manual QA** — run each shortcut; type `s` and `v` inside a comment box and confirm no shortcut fires; open the palette with ⌘K/Ctrl+K, type a fuzzy query, and confirm the correct action runs.

- [ ] **Step 6: Commit** — `git add src/ui src/app.rs && git commit -m "feat: add command palette and keybindings"`.

---

### Task 53: Settings window, theme, and layout persistence

**Files:**
- Create: `src/ui/widgets/settings_window.rs`, `src/ui/theme.rs`
- Modify: `src/persist/settings.rs`, `src/app.rs`
- Test: `src/ui/theme.rs`, `src/persist/settings.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Tasks 6, 13, 52.
- Produces:
  - `#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)] pub enum Theme { #[default] System, Light, Dark }` with `pub fn resolve(&self, system_dark: bool) -> egui::Theme`.
  - `#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(default)] pub struct Layout { pub left_pane: f32, pub right_pane: f32, pub right_open: bool }` (tree collapse state is per PR, in the session file).
  - `Settings` gains `#[serde(default)] pub theme: Theme` and `#[serde(default)] pub layout: Layout`.
  - `pub struct SettingsVm { pub host: String, pub client_id: String, pub default_scopes: Vec<Scope>, pub theme: Theme }`, `pub fn settings_vm(s: &Settings) -> SettingsVm`, `impl SettingsVm { pub fn apply(self, s: &mut Settings) -> bool /* true if the host changed → sign-in required */ }`.
  - The settings **modal window** (not a screen) edits host, client ID, default scopes, theme, and offers Sign out. Pane sizes are saved through a `DebouncedWriter`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn theme_resolves_system_and_vm_round_trips() {
    assert_eq!(Theme::System.resolve(true), egui::Theme::Dark);
    assert_eq!(Theme::System.resolve(false), egui::Theme::Light);
    assert_eq!(Theme::Light.resolve(true), egui::Theme::Light);

    let mut s = Settings::default();
    let mut vm = settings_vm(&s);
    vm.host = "ghe.example.com".into();
    vm.default_scopes = vec![Scope::Assigned];
    assert!(vm.apply(&mut s));                            // host changed
    assert_eq!(s.host, "ghe.example.com");
    assert_eq!(s.default_scopes, vec![Scope::Assigned]);
}
```

and in `src/persist/settings.rs`:

```rust
#[test]
fn layout_survives_save_load() {
    let dir = tempdir();
    let mut s = Settings::load_or_default(dir.path());
    s.layout.left_pane = 321.0;
    s.layout.right_open = false;
    s.save(dir.path()).unwrap();
    let r = Settings::load_or_default(dir.path());
    assert_eq!(r.layout.left_pane, 321.0);
    assert!(!r.layout.right_open);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test theme_resolves_system_and_vm_round_trips layout_survives_save_load`. Expected: FAIL to compile.

- [ ] **Step 3: Implement** `Theme::resolve`, the settings fields, `settings_vm`/`apply`, the window, theme application in `app.rs`, and pane-size persistence.

- [ ] **Step 4: Run tests to verify they pass**. Expected: PASS.

- [ ] **Step 5: Commit** — `git add src/ui src/persist/settings.rs src/app.rs && git commit -m "feat: add settings window, theme, and layout persistence"`.

---

## Phase J — Performance guardrail

### Task 54: Synthetic large-PR performance test

**Files:**
- Create: `tests/perf_large_pr.rs`
- Modify: `tests/common/mod.rs`
- Test: `tests/perf_large_pr.rs`

**Interfaces:**
- Consumes: Tasks 20, 21, 26.
- Produces: no production API; `tests/common::synthetic_files(n_files: usize, rows_per_file: usize) -> Vec<LoadedFile>` (deterministic contents, every 50th line a 50,000-character minified line), a model-level guardrail, and a documented manual frame-time check.

- [ ] **Step 1: Write the failing tests**

```rust
mod common;
use common::synthetic_files;
use gh_review_desktop::{diff::{highlight::*, row_model::*}, state::{filters::*, viewed::ViewedSet}};
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(16);

#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn visible_slice_work_stays_within_frame_budget() {
    let files = synthetic_files(1_500, 34);               // ≥ 50k rows
    let model = build(&files);
    assert!(model.rows.len() >= 50_000);

    let mut cache = HighlightCache::new(4_096, Box::new(SyntectHighlighter::new()));
    cache.spans("rs", "fn warm_up() {}");                 // syntax set loads outside the timed loop

    let viewport = 40.0 * ROW_H;
    let total = model.offset_of(model.rows.len() - 1);
    let worst = (0..1_000).map(|i| {
        let y = (i as f32 * 7_919.0) % total;
        let t = Instant::now();
        for idx in model.visible_range(y, viewport) {
            if let RowRef::Line { file, hunk, row } = &model.rows[idx] {
                let r = &files[*file as usize].diff.as_ref().unwrap().hunks[*hunk as usize].rows[*row as usize];
                let shown = display_text(&r.text);
                let _ = cache.spans("rs", &shown);
            }
        }
        t.elapsed()
    }).max().unwrap();
    assert!(worst < FRAME, "worst visible-slice frame took {worst:?}");
}

#[test]
#[ignore = "wall-clock guardrail; run with `cargo test --release -- --ignored`"]
fn filtering_and_folder_state_are_fast() {
    let files: Vec<_> = synthetic_files(1_500, 34).into_iter().map(|f| f.file).collect();
    let filters = FilterState { path_query: "src/*".into(), hide_viewed: true, ..Default::default() };
    let t = Instant::now();
    let kept = apply(&files, &filters, &ViewedSet::default());
    assert!(!kept.is_empty());
    assert!(t.elapsed() < FRAME);
}
```

- [ ] **Step 2: Run tests to verify they fail** — `cargo test --release --test perf_large_pr -- --ignored`. Expected: FAIL (helper missing).

- [ ] **Step 3: Implement** `synthetic_files`. If the budget fails, fix the hot path (do not relax the budget or move highlighting out of the timed loop).

- [ ] **Step 4: Run tests to verify they pass** — `cargo test --release --test perf_large_pr -- --ignored`. Expected: PASS. Confirm `cargo test` (debug, default) skips them.

- [ ] **Step 5: Manual QA** — open a real PR with hundreds of files in a release build; enable egui's frame-time readout; scroll top to bottom and confirm frames stay under 16 ms; toggle hide-viewed and mark a large folder and confirm instant response.

- [ ] **Step 6: Commit** — `git add tests/perf_large_pr.rs tests/common/mod.rs && git commit -m "test: add large PR performance guardrail"`.

---

## Spec coverage map

| Spec section | Tasks |
|---|---|
| §4.1 hosts/URLs, §6 auth | 4, 5, 6, 7, 8, 9, 10 |
| §7.1–7.2 PR list | 11, 12, 13, 14 |
| §7.3 anchoring | 19, 31, 37, 38, 47 |
| §7.4 submit | 39, 42, 43, 44 |
| §7.5 missing patches, binary files | 17, 23, 46, 49 |
| §7.6 server-side pending review | 41, 42, 44 |
| §8 diff engine, image viewer | 15, 16, 20, 21, 23, 45, 46, 48, 49 |
| §9 UI | 10, 14, 23, 28, 37, 42, 49, 50, 51, 52, 53 |
| §10 comments, pending, viewed, filters | 25, 26, 27, 34, 39, 40 |
| §11 persistence | 6, 29, 40, 53 |
| §12 performance | 20, 21, 54 |
| §13 errors | 2, 7, 10, 22, 31, 38, 44 |
