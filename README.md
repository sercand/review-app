# GH Review

A native desktop app for reviewing GitHub pull requests. It's written in Rust
with [egui](https://github.com/emilk/egui) and built to stay fast and
keyboard-driven on PRs with hundreds of files.

Pick a PR, move through its files in a tree, read the diffs, leave comments,
and submit your review without the browser.

## Features

- **Your PRs on one screen.** Lists open PRs that request your review, are
  assigned to you, were authored by you, mention you or involve you. You can
  also list a single repository or organization. One GraphQL request fetches
  the CI status, review state and line counts for the list.
- **Diffs.** Split or unified view with syntax highlighting, word-level change
  highlighting and context expansion. Changed images open in a viewer with
  2-up, swipe and onion-skin modes.
- **Comments.** Comment on a line, a range of lines or a whole file, including
  binary files. A comment can post right away or wait in a pending review that
  you submit later. The app picks up a pending review you started in the
  GitHub web UI. Comments can include suggestion blocks.
- **Threads.** Shows existing review threads, including whether each is
  resolved or outdated. You can reply to a thread and resolve it.
- **Review progress.** Mark files, or whole folders, as viewed. Filter files by
  extension and hide the ones you've viewed. When new commits arrive, only the
  files that changed lose their viewed mark.
- **Crash-safe drafts.** Pending comments and the text in an open comment box
  are saved to disk, so they survive a quit, a crash or signing out.
- **Keyboard first.** A command palette (`⌘K`) and single-key shortcuts for
  moving around a PR.
- **AI review.** A sandboxed agent reviews the PR in your local checkout and
  drafts findings you can keep, edit or drop. Unix only; see
  [AI review](#ai-review).
- **GitHub Enterprise Server** is supported in addition to github.com.

The app doesn't merge or close PRs and doesn't edit code. See the
[design spec](docs/superpowers/specs/2026-10-19-gh-review-desktop-design.md)
for the full scope.

## Getting started

Requires a stable Rust toolchain; `rust-toolchain.toml` pins the channel.

```sh
cargo run
```

On Linux, the keychain integration needs the D-Bus development headers, e.g.
`libdbus-1-dev` on Debian/Ubuntu.

### Signing in

You can sign in two ways:

1. **Device flow.** The app shows a code. You enter it on GitHub in your
   browser. This needs the client ID of a GitHub OAuth App that has
   **Device Flow enabled**. The compiled-in default is `DEFAULT_CLIENT_ID` in
   `src/auth/mod.rs`. To use a different ID, set `client_id` in
   `settings.toml` (see below). A github.com client ID doesn't work on an
   Enterprise Server host, so on Enterprise the login screen asks for that
   host's own client ID.
2. **Personal access token.** Paste a token with the `repo` and `read:org`
   scopes.

The token is stored in the OS keychain under the service
`gh-review-desktop`. It is never written to disk in plain text.

On macOS, `cargo run` goes through `scripts/macos-run.sh`. That script
re-signs each new debug build with your Apple Development identity, so one
"Always Allow" on the keychain prompt keeps working across rebuilds.

## Keyboard shortcuts

Single-key shortcuts only work when no text field has focus. Press `?` in the
app to see the full list.

| Key | Action |
|---|---|
| `⌘K` | Command palette |
| `⌘F` | Find in diff |
| `j` / `k` | Next / previous file |
| `n` / `p` | Next / previous hunk |
| `v` | Toggle viewed |
| `⇧V` | Mark viewed and go to the next file |
| `c` | Comment on the focused line |
| `r` | Reply to the focused thread |
| `o` | Toggle the side pane |
| `s` / `u` | Split / unified diff |
| `⌘Enter` | Submit review |
| `Esc` | Close the composer, palette or dialog |

On Linux and Windows, use `Ctrl` instead of `⌘`.

## AI review

On Unix, the app can run an AI review of an open PR. It checks the PR head
out into the repo's local folder (stashing your uncommitted changes first),
runs a pipeline of agents — scout, reviewers, challengers, verifiers,
assembler — against the checkout, and drafts findings as inline comments in
the diff. Each draft is Keep / Edit / Drop; keeping one moves it into your
pending review like any other comment. Nothing is posted to GitHub unless you
submit it yourself.

### Setup

In **Settings → AI**:

1. Add a provider (name, API type, base URL, API key) and pick a model. The
   key goes to the OS keychain, like the GitHub token.
2. Per repository, set the local folder and the commands the agent may run
   without asking (e.g. `cargo test`). On your first review of a repo, the
   scout agent proposes these and you confirm them.

Other commands ask for approval in the AI tab, once or for the whole review.

### The sandbox

The guarantee is not command matching, it's that the agent can't reach
GitHub even if it tries:

- Its shell runs in an OS sandbox (Seatbelt on macOS, bubblewrap on Linux)
  that can't write to `.git/`, so it can't commit, reset or push.
- Its environment has no credentials: no `GH_TOKEN` or `SSH_AUTH_SOCK`, and
  it can't read `~/.ssh`, `~/.config/gh` or the other credential stores.
- It has no GitHub write tool at all — it reads the PR from a snapshot the
  app gathered beforehand, and can't comment, approve or merge.
- Network access is on, so builds and tests work; with a team-only repo the
  accepted risk is that a build could send the source elsewhere.

### Private dependencies

Because the agent has no credentials, fetching private dependencies from
inside the sandbox would fail. Each repo therefore has **prepare commands**
(e.g. `cargo fetch`, `npm ci`) that the app runs *outside* the sandbox, as
you, right after the checkout. The agent then builds from the filled caches.
If a build still needs a fetch, the agent can ask to re-run the configured
prepare commands; you approve, and the app runs the same fixed list.

AI review is Unix-only for now: the sandbox has no Windows backend.

## Where data is stored

Everything is stored locally in the platform config directory
(`~/Library/Application Support/gh-review-desktop/` on macOS):

```
settings.toml                                  host, client_id, theme, layout, filters
sessions/<host>/<owner>/<repo>/<number>.json   viewed files, pending review, drafts
ai.sqlite                                      AI review sessions, transcripts, findings
```

## Development

```sh
cargo test                                     # unit, contract and fixture tests
cargo test --release -- --ignored              # performance guardrails
cargo test --lib shots -- --ignored            # screenshots → target/ui-shots/ (needs a GPU)
cargo clippy --all-targets
```

- Unit tests cover the pure logic: patch parsing, comment anchoring, filters,
  viewed state and the pending-review reducer.
- `tests/contract_*.rs` run the GitHub client against a `wiremock` server and
  assert the exact request bodies.
- `tests/fixtures/` holds recorded GitHub responses, which catch changes in
  GitHub's response format.

### Layout

```
src/
├── main.rs, app.rs   eframe entry point, root app, screen routing
├── runtime.rs        tokio runtime; UI ⇄ network via Command/Event channels
├── auth/             device flow, token storage (keychain)
├── github/           REST + GraphQL client (hand-written over reqwest)
├── ai/               AI review engine (unix only): agents, tools, SQLite
├── model/            domain types
├── diff/             patch parser, context expansion, highlighting, image diff
├── state/            screen state and reducers, pending review buffer
├── persist/          settings and per-PR session files (atomic writes)
└── ui/               screens, widgets, theme, fonts, icons
crates/
├── llm/              Anthropic / OpenAI clients behind neutral types
└── sandbox/          OS-sandboxed PTY bash and file tools for the agent
```

The UI never calls the network directly. It sends a `Command` to the runtime,
and results come back as `Event`s, so the GUI thread never waits on a request.
The AI engine follows the same split: it lives in the worker and talks to the
UI only through AI commands and events.

## Releasing (macOS)

```sh
scripts/macos/archive.sh     # universal, signed app inside an .xcarchive
scripts/macos/notarize.sh    # notarize, staple, and build the .dmg and .zip
```

See [`scripts/macos/README.md`](scripts/macos/README.md) for the one-time
setup: Developer ID certificate, notarization credentials and the app icon.

## Credits

Bundled fonts: [IBM Plex Sans](https://github.com/IBM/plex) and
[JetBrains Mono](https://github.com/JetBrains/JetBrainsMono), both under the
SIL Open Font License. Icons: [Lucide](https://lucide.dev), under the ISC
License. The license files are in `assets/`.

## License

[MIT](LICENSE)
