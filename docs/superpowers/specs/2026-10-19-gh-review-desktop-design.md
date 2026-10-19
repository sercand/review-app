# Design Spec — `gh-review-desktop`: a native GitHub PR review client

- **Status:** Draft, revised after review (see §19)
- **Date:** 2026-10-19
- **Topic:** Rust + egui desktop client for reviewing GitHub pull requests

---

## 1. Purpose

Reviewing large pull requests in the GitHub web UI is slow and awkward: heavy
pages, scroll-heavy navigation, weak keyboard support, and a "viewed" model that
is easy to lose track of. This project builds a **native desktop application**
that makes reviewing a pull request a fast, keyboard-friendly, local-first
operation: choose a PR, navigate a file tree, read diffs, leave comments, and
submit a review.

## 2. Goals

1. **See the right PRs immediately.** After logging in, show open PRs that are
   assigned to me, authored by me, requesting my review, mentioning/involving
   me, or simply open in a chosen repository or organization.
2. **Open a PR and review it like GitHub, but faster.** File tree on the left,
   diff on the right, file-level and line-level interaction. Changed images are
   shown visually (2-up, swipe, onion skin), and every file — including binary
   files — can be commented on.
3. **Filter and track progress.** Filter files by extension and other
   properties, hide viewed files, and mark individual files *or whole folders*
   as viewed.
4. **Comment the two GitHub ways.** Post an **immediate single comment**, or add
   a **pending review comment** that is only posted when the review is
   submitted. View existing comments, threads, and the current un-submitted
   review — including one started earlier in the GitHub web UI (§7.6).
5. **Stay responsive on huge diffs.** A PR with hundreds of files and tens of
   thousands of changed lines must scroll and filter as smoothly as a tiny one.
6. **Submit a review** with a verdict (Comment / Approve / Request Changes) and
   an optional summary body.

### Non-goals (v1)

- Merging, closing, or otherwise mutating PR state.
- Web/headless/CI usage. This is a desktop GUI app.
- Code editing. This is a review client, not an editor.
- Non-PR commit review, per-commit diffs, arbitrary ref diffs, or stacked-PR
  awareness.
- Posting top-level (non-inline) PR conversation comments. They are displayed
  read-only in the timeline.

## 3. Users and success criteria

**Primary user:** a developer who reviews pull requests in their own
repositories and in their organization's repositories, on a machine where they
are already logged into GitHub in a browser.

| # | Success criterion | How we know |
|---|---|---|
| S1 | Log in once and stay logged in | Token survives app restarts via OS keychain |
| S2 | Find the PR you want in under 5 seconds | Scope switcher + search over the PR list |
| S3 | Navigate a 500-file PR without stutter | Frame time stays within budget while scrolling (see §12) |
| S4 | Never lose a written comment | Pending comments **and the open composer's text** persist across crash/quit/sign-out |
| S5 | Comment exactly where GitHub would put it | Posted comments appear at the expected line/side in the web UI |
| S6 | Track review progress across a big PR | Viewed state + filters + progress counter |

## 4. Constraints and decisions

| Decision | Choice | Rationale |
|---|---|---|
| Language / GUI | Rust (edition 2021), `egui` via `eframe` | Immediate-mode GUI, single binary, good performance story, straightforward async bridging |
| Auth | **OAuth Device Flow** with a bundled public client ID, plus **PAT paste** fallback | No backend, no client secret in the binary, works for private repos; PAT covers Enterprise/edge cases |
| HTTP client | A thin hand-written client over `reqwest` (rustls). **No `octocrab`.** | We need exact control of request bodies (comment anchors), headers (`Accept`, rate-limit, SSO), and raw bodies for fixture tests; a second typed client would duplicate DTOs and hide those details |
| Diff source | GitHub's precomputed `patch` from the PR files API; context expansion uses the **head blob** plus hunk offsets; a local `similar` diff only for files GitHub sends without a patch | Comment anchors match GitHub; cheap for huge PRs |
| GraphQL | Used where REST can't do the job well: the PR list (one round trip with CI/review/+- data), review-thread listing with resolved/outdated state, and thread resolution | Avoids N+1 REST calls for list badges and avoids reconstructing threads from flat comments |
| Platform | macOS, Linux, Windows | `eframe` targets all three |
| Hosts | `github.com` + configurable GitHub Enterprise Server host | Enterprise users exist |
| Workspace | One PR open at a time | Simplicity; chosen explicitly over tabs/three-pane |
| Persistence | Local config dir + OS keychain | Local-first, no server |

### 4.1 Host → URL derivation

| Host | Web / OAuth base | REST base | GraphQL endpoint |
|---|---|---|---|
| `github.com` | `https://github.com` | `https://api.github.com` | `https://api.github.com/graphql` |
| Enterprise `{h}` | `https://{h}` | `https://{h}/api/v3` | `https://{h}/api/graphql` |

All endpoint paths below are relative to the REST base unless noted. Device
flow endpoints are relative to the web/OAuth base.

### Prerequisite (external)

An OAuth App must be registered on GitHub with **Device Flow enabled** (Settings
→ Developer settings → OAuth Apps). Its *public* client ID is compiled in as a
default constant. GitHub Device Flow requires no client secret. The client ID is
overridable in settings so forks/enterprises can use their own. **A github.com
client ID does not work on an Enterprise Server host**: for Enterprise, the user
must configure their own OAuth App's client ID or use a PAT; the login screen
says so when an Enterprise host is selected with no client ID override.

## 5. Architecture

```
gh-review-desktop (one Cargo package: lib target + thin bin target)
└── src/
    ├── main.rs          eframe bootstrap, tracing, panic hook (calls into lib)
    ├── lib.rs           pub module tree (public so integration tests can reach it)
    ├── app.rs           ReviewApp root, Screen enum, global keyboard dispatch, theme
    ├── runtime.rs       tokio runtime, Command/Event channels, command executor
    ├── auth/            device flow, PAT, keyring, token store
    ├── github/          REST + GraphQL client, DTOs, pagination, rate limit, GhError
    ├── model/           domain types (RepoRef, PrFile, DiffRow, ReviewThread, ...)
    ├── diff/            patch parser, line/side coordinates, context expansion, highlight cache
    ├── state/           PrListState, PrSession, reducers over Event, pending review buffer
    ├── persist/         settings, session/viewed state, draft persistence (atomic writes)
    └── ui/
        ├── screens/     login.rs, pr_list.rs, pr_detail.rs
        └── widgets/     file_tree.rs, diff_view.rs, comment_composer.rs, thread_view.rs,
                         review_panel.rs, filter_bar.rs, commit_list.rs, checks.rs,
                         command_palette.rs, settings_window.rs, markdown.rs, toast.rs
```

**Layering rule:** `ui` and `state` never talk to the network directly. They emit
`Command`s and consume `Event`s. `github` never depends on `ui`. `model` and
`diff` are pure and dependency-light, which keeps them unit-testable without a
network or a GUI.

**Stale-event rule:** every PR-scoped `Command` carries a `SessionId` (a
monotonically increasing counter assigned when a PR is opened), and every
resulting `Event` echoes it. Events whose `SessionId` does not match the open
session are dropped, so a slow response for a PR the user has left can never
land in the next PR's state.

### Dependencies (indicative)

| Crate | Use |
|---|---|
| `eframe` / `egui_extras` (`image`, `svg` features) | App shell, widgets, virtualized tables, image loaders (avatars and the image diff viewer). `egui` is used via `eframe::egui` so versions cannot drift |
| `image` (`png`, `jpeg`, `gif`, `webp`, `bmp`, `ico` only) | Decoders used by `egui_extras`' image loader; only the enabled formats can be displayed |
| `tokio` | Async runtime for all I/O |
| `reqwest` (rustls TLS feature) | HTTP transport for REST, GraphQL, and device flow |
| `serde`, `serde_json`, `toml` | Serialization |
| `similar` | Local diffing for files GitHub sends without a patch |
| `syntect` | Syntax highlighting |
| `pulldown-cmark` | Markdown rendering (descriptions, comments) |
| `keyring` | OS keychain token storage — **platform backend features must be enabled explicitly** (`apple-native`, `windows-native`, `sync-secret-service`); without them `keyring` 3.x silently uses a non-persistent mock store |
| `directories` | Platform config/cache paths |
| `thiserror`, `anyhow` | Error types |
| `tracing`, `tracing-subscriber` | Structured logging (never logs secrets) |
| dev: `wiremock`, `tempfile` | Mock HTTP server for contract tests; temp dirs for persistence tests |

## 6. Auth component

**Device Flow sequence**

1. `POST {web}/login/device/code` with `client_id` and `scope=repo read:org`
   (scope set is configurable; `repo` is required to read private repos and to
   post comments). Send `Accept: application/json` — without it GitHub answers
   form-encoded.
2. Show the returned `user_code`, `verification_uri`, and expiry. Offer "Copy
   code" and "Open in browser" (opens the URI).
3. Poll `POST {web}/login/oauth/access_token` (again `Accept: application/json`)
   with `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
   `device_code`, `client_id`, waiting the server-provided `interval` between
   polls and stopping at `expires_in`.
4. Handle responses: `authorization_pending` → keep polling; `slow_down` →
   **add 5 seconds** to the interval (or use the `interval` in the response if
   present), per RFC 8628; `expired_token` → stop and offer "restart sign-in";
   `access_denied` → abort with a message; `incorrect_client_credentials`,
   `device_flow_disabled`, `unsupported_grant_type`, `incorrect_device_code` →
   abort with a configuration-error message; success → `access_token`.
5. Validate with `GET /user`, then store the token in the OS keychain under
   service `gh-review-desktop`, account `{host}:{login}`. Cache `login`, `name`,
   `avatar_url`, and granted scopes (from the `X-OAuth-Scopes` response header)
   in settings. At startup the cached `{host, login}` pair is what locates the
   keychain entry.

**PAT fallback:** a "paste a personal access token" path on the login screen,
storing into the same keychain entry after validating with `GET /user`.

**Token lifecycle:** on any `401`, delete the keychain entry, drop to the
`Login` screen, and show "your session expired, please sign in again".
**Pending review drafts and composer text are never deleted by a 401 or by
sign-out** — they are keyed by host/repo/PR, not by token, and are restored
after the user signs in again. "Sign out" removes only the keychain entry and
the cached user in settings.

## 7. GitHub API contract

This is the part most likely to cause real-world bugs, so it is specified
precisely.

### 7.1 Endpoints used

| Purpose | Call |
|---|---|
| Current user (+ granted scopes header) | `GET /user` |
| PR list per scope | GraphQL `search(query:, type: ISSUE, first: 50, after:)` selecting `... on PullRequest { number title isDraft updatedAt createdAt additions deletions reviewDecision totalCommentsCount url author { login } repository { nameWithOwner } commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } }` and `issueCount` |
| PR metadata (incl. `node_id`, `head.sha`, `base.sha`) | `GET /repos/{o}/{r}/pulls/{n}` |
| Merge base (only for files without a patch) | `GET /repos/{o}/{r}/compare/{base_sha}...{head_sha}` → `merge_base_commit.sha` |
| PR files | `GET /repos/{o}/{r}/pulls/{n}/files?per_page=100&page=N` (GitHub caps this at 3000 files) |
| PR commits | `GET /repos/{o}/{r}/pulls/{n}/commits?per_page=100&page=N` |
| Check runs | `GET /repos/{o}/{r}/commits/{sha}/check-runs?per_page=100` |
| Combined status | `GET /repos/{o}/{r}/commits/{sha}/status` |
| Reviews (incl. the viewer's own `PENDING` review) | `GET /repos/{o}/{r}/pulls/{n}/reviews?per_page=100&page=N` |
| Comments of a server-side pending review | `GET /repos/{o}/{r}/pulls/{n}/reviews/{review_id}/comments` |
| Submit a server-side pending review | `POST /repos/{o}/{r}/pulls/{n}/reviews/{review_id}/events` |
| Delete a server-side pending review | `DELETE /repos/{o}/{r}/pulls/{n}/reviews/{review_id}` |
| Conversation (top-level PR comments, read-only) | `GET /repos/{o}/{r}/issues/{n}/comments?per_page=100&page=N` |
| Review threads + resolved/outdated state (the single source of inline comments) | GraphQL `repository.pullRequest.reviewThreads(first: 100, after:)` with `comments(first: 100)` (paginate both levels) |
| Post immediate line comment | `POST /repos/{o}/{r}/pulls/{n}/comments` |
| Post immediate file-level comment | `POST /repos/{o}/{r}/pulls/{n}/comments` with `subject_type: "file"` |
| Reply to a thread | `POST /repos/{o}/{r}/pulls/{n}/comments/{comment_id}/replies` (`comment_id` = the root comment's `databaseId`) |
| Submit a new review | `POST /repos/{o}/{r}/pulls/{n}/reviews` |
| Resolve / unresolve a thread | GraphQL mutations `resolveReviewThread` / `unresolveReviewThread` |
| File content at a ref (context expansion, no-patch files, images) | `GET /repos/{o}/{r}/contents/{path}?ref={sha}` with `Accept: application/vnd.github.raw`; **each path segment percent-encoded** (spaces, `#`, non-ASCII). Text is decoded as UTF-8; images are kept as raw bytes, capped at 10 MB |

Every list endpoint is paginated until exhausted (REST: `Link: rel="next"`;
GraphQL: `pageInfo { hasNextPage endCursor }`).

### 7.2 Scope queries

GitHub search ANDs qualifiers, so **scopes cannot be combined into one query**.
Each selected scope is its own search; results are merged and de-duplicated by
`(repo, number)`.

| Scope | Search query (`{login}` = the signed-in user's login) |
|---|---|
| Assigned to me | `is:pr is:open assignee:{login}` |
| Authored by me | `is:pr is:open author:{login}` |
| Review requested | `is:pr is:open review-requested:{login}` |
| Involves me | `is:pr is:open involves:{login}` |
| Mentions me | `is:pr is:open mentions:{login}` |

A **constraint** is appended to every scope query: `repo:{o}/{r}` or
`org:{org}`. With a constraint and **no** personal scope selected, a single
query `is:pr is:open repo:{o}/{r}` (or `org:{org}`) lists every open PR there.
Each query also carries the chosen sort as `sort:updated-desc`,
`sort:created-desc`, or `sort:comments-desc`; merged results are re-sorted
locally by the same key.

The UI default is "Review requested + Assigned + Authored" (three queries).
Search results are capped at 1000 per query and search is rate-limited; the
"N of M" counter shows loaded vs. the sum of `issueCount`s and is labelled
"≈" when more than one scope is merged (duplicates make the sum an upper
bound). Both limits are surfaced in the UI rather than silently truncating.

### 7.3 Comment anchoring — the contract

Every inline comment must carry a coordinate that GitHub accepts. Our internal
diff model produces both sides of each line, so we map exactly:

- `path` — the file path **as it appears in the diff**: the file's `filename`
  from the files API (the new path for renames; for deleted files `filename` is
  the old path). Never `previous_filename`.
- `commit_id` — the head SHA **the displayed diff was loaded from**. If the PR
  head has moved since, we still send the SHA the user was looking at; GitHub
  then marks the comment outdated where appropriate instead of misplacing it.
- Single line on the new side: `line = new_line_number`, `side = "RIGHT"`.
- Single line on the old side: `line = old_line_number`, `side = "LEFT"`.
- Context lines: default to `RIGHT`; the composer exposes a side toggle.
- Multi-line range: send `start_line`/`start_side` plus `line`/`side`.
  Line numbers increase downward on **both** sides, so the constraint is
  `start_line < line` regardless of side. A one-line "range" is sent as a
  single-line comment (no `start_*`). The UI restricts a range to rows of one
  side **within one hunk** (GitHub rejects ranges that cross hunks).
- Reply to an existing thread: `POST .../comments/{id}/replies` with `body`
  only (no coordinates).
- File-level comment: REST `POST .../pulls/{n}/comments` with
  `{ "body", "commit_id", "path", "subject_type": "file" }` (no `line`/`side`).
  This publishes immediately. GitHub's review-creation endpoint cannot carry
  file-level comments, so **file-level comments are always immediate**; the
  composer hides "Add to review" for them.

If GitHub rejects a line comment with `422`, the app shows the API message and
offers "add as a file-level comment instead" rather than failing silently.

### 7.4 Submitting a review

`POST /repos/{o}/{r}/pulls/{n}/reviews` with:

```json
{
  "commit_id": "<the commit the draft was written against (§10.2)>",
  "body": "<optional summary>",
  "event": "APPROVE | REQUEST_CHANGES | COMMENT",
  "comments": [
    { "path": "src/a.rs", "line": 42, "side": "RIGHT", "body": "..." },
    { "path": "src/b.rs", "start_line": 10, "start_side": "RIGHT",
      "line": 14, "side": "RIGHT", "body": "..." }
  ]
}
```

Rules enforced before sending:

- `REQUEST_CHANGES` requires a non-empty `body`.
- `COMMENT` requires a non-empty `body` or at least one comment.
- `APPROVE` needs neither.
- `comments` is omitted when empty.
- `APPROVE` and `REQUEST_CHANGES` are rejected by GitHub when the authenticated
  user is the PR author; the UI disables those options in that case with an
  explanation.

### 7.5 Missing patches

GitHub omits `patch` for binary files and for very large text diffs. The files
API does not say which, so:

- `patch` absent and `changes == 0` → **binary**. Image files are rendered by
  the image diff viewer (§8.5); other binary files show a "binary file not
  shown" placeholder with the status and "Open in browser". Every binary file
  can be commented on through the file header's "Comment on file" button;
  those comments are file-level and therefore immediate (§7.3).
- `patch` absent and `changes > 0` → **too large**: show a placeholder with a
  "Load diff" button. Loading fetches the head blob and the base blob **at the
  merge base** (not `base.sha`, which is the base branch tip and would show
  unrelated changes), and computes a local diff with `similar`. These rows are
  marked `local`; comment anchoring is best-effort and falls back to
  file-level on `422`.
- PRs with more than 3000 changed files show a banner: GitHub's files API
  returns at most 3000.

### 7.6 Server-side pending reviews

GitHub allows one pending review per user per PR. If the viewer started a
review in the web UI, `GET .../reviews` returns it with `state: "PENDING"`, and
`POST .../reviews` will fail with `422` until it is submitted or deleted.

- On PR load, if a `PENDING` review by the viewer exists, its comments are
  fetched and shown inline in the pending style, labelled "pending on GitHub",
  and a banner offers **Submit it** (`POST .../reviews/{id}/events` with the
  chosen `event` and `body`) or **Discard it** (`DELETE .../reviews/{id}`,
  after confirmation).
- While a server-side pending review exists, submitting the local draft is
  disabled with that explanation. Immediate comments still work.

## 8. Diff engine

### 8.1 Model

```rust
pub enum FileStatus { Added, Removed, Modified, Renamed, Copied, Changed, Unchanged }

pub enum PatchState { Present, Binary, TooLarge }

pub struct PrFile {
    pub path: String,                 // API `filename` (the comment anchor path)
    pub previous_path: Option<String>,// API `previous_filename` (renames)
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub changes: u32,
    pub patch: Option<String>,        // raw unified diff from the API
    pub patch_state: PatchState,
    pub blob_sha: String,             // API `sha`; drives viewed-state invalidation (§10.3)
}

pub struct FileDiff {
    pub hunks: Vec<Hunk>,
}

pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    pub rows: Vec<DiffRow>,
}

pub enum LineKind { Context, Added, Removed }

pub enum RowOrigin { Patch, Expanded, Local } // see §8.3 / §7.5

pub struct DiffRow {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    pub no_newline_at_eof: bool,      // set from a following `\ No newline at end of file`
    pub origin: RowOrigin,
}
```

Syntax highlighting is not stored on rows; it lives in a bounded cache (§8.4).

### 8.2 Parser requirements

- The API `patch` starts at the first `@@` header; tolerate and skip any
  `diff --git`/`---`/`+++`/`index` lines if present.
- Parse `@@ -old_start,old_len +new_start,new_len @@ optional section` headers,
  including the abbreviated form with omitted counts (count defaults to 1).
- Assign `old_no`/`new_no` correctly for context, added, and removed rows.
- `\ No newline at end of file` sets `no_newline_at_eof` on the preceding row
  and consumes no line number.
- Accept CRLF input (strip one trailing `\r` per line) and arbitrary UTF-8.
- Treat a completely empty line inside a hunk as an empty context line.
- Round-trip guarantee: for any row, the coordinate we would send as a comment
  matches what GitHub's own UI offers for that row.

### 8.3 Context expansion

Default context is whatever the API patch contains (typically 3 lines). Lines
between and around hunks are unchanged, so they are identical on both sides and
their numbers follow from the hunk headers: for a gap, `old_no = new_no -
offset`, where `offset` is the running `new − old` delta at that point. Expansion
therefore needs **only the head blob** (`contents?ref={head_sha}` of `path`) —
no local diff and no possibility of disagreeing with GitHub's patch. Expansion
can be per-gap (up/down by 20 lines, or the whole gap) or "expand all".

Expanded rows have `origin = Expanded`. They lie outside GitHub's diff hunks,
where GitHub may reject a line comment; the composer warns before anchoring a
comment to an expanded row and offers the file-level fallback directly.

### 8.4 Rendering and virtualization

- The PR is one virtualized scroll surface built from a flat row list: a header
  row per file, then that file's diff rows, interleaved with existing-thread,
  pending-comment, and composer rows. Rows reference data by index
  (`file_idx`, `hunk_idx`, `row_idx`) rather than cloning paths or text.
- Rows have **variable height** (threads, composer, word wrap). The surface uses
  `egui_extras::TableBuilder` with heterogeneous row heights, from a
  precomputed height per row plus a prefix-sum array, so scroll-to-file and
  "which rows are visible" are O(log n). Thread/composer heights are measured
  after their first render and cached; heights are recomputed on width change
  only when wrap is on.
- All patches are parsed on the worker thread as file pages arrive (parsing
  50k lines takes milliseconds); the UI thread only receives finished rows.
- Syntax highlighting is computed **only for visible rows**, on demand, and
  cached in a bounded LRU keyed by `(syntax_name, hash(text))`. Highlighting is
  per line in isolation (a known limitation for multi-line strings/comments).
- **Long-line guard:** a row's displayed text is truncated at 10,000 characters
  (on a `char` boundary) with a "line truncated" marker, and rows longer than
  2,000 characters are not highlighted. This keeps a minified bundle from
  forcing a huge galley layout.
- Unified and split modes share the row model; split mode pairs removed/added
  runs side by side from the same coordinates.

### 8.5 Image diff viewer

- **Which files:** binary files whose extension is `png`, `jpg`, `jpeg`, `gif`,
  `webp`, `bmp`, or `ico` (case-insensitive) render the viewer in place of the
  binary placeholder. SVG files have a text patch, so they keep the text diff
  and get a "Text / Rendered" toggle in the file header that swaps in the same
  viewer.
- **Which blobs:** base at the **merge base** (the `previous_filename` for
  renames/copies), head at the diff's head SHA. Added files have only a head
  image, removed files only a base image.
- **Loading:** lazily, when the file's header first scrolls into view — never
  for every file upfront. Blobs over 10 MB are not downloaded (placeholder with
  "Open in browser"). Loaded blobs live in a per-session LRU capped at 200 MB;
  evicting one also frees its GPU texture.
- **Modes (mirroring GitHub):** **2-up** — base and head side by side, each
  captioned with pixel dimensions, plus the file-size change; **Swipe** — both
  drawn in one frame with a draggable divider revealing the head on one side;
  **Onion skin** — head drawn over base with an opacity slider. Added/removed
  images show a single image and hide the mode switch.
- **Rendering:** images are fitted to the pane width keeping the aspect ratio
  and never upscaled; a checkerboard background makes transparency visible.
  Undecodable data falls back to the placeholder. The viewer occupies one
  variable-height row in the virtualized surface (§8.4).
- **Comments:** file-level only, via "Comment on file". GitHub has no way to
  anchor a comment to a region of an image.

## 9. UI design

### 9.1 Screens

- **Login** — device-flow panel (code, copy, open browser, progress/spinner) or
  PAT entry; host selector (github.com / Enterprise) with client-ID override.
- **PR list** — scope switcher, repo/org constraint, text search (local filter
  over loaded rows), sort (updated / created / comments), and rows showing:
  repo, number, title, author, draft badge, CI badge, review decision, +/-
  counts, comment count, last updated. Paginated with an "N of M" counter.
- **PR detail** — the working surface:
  - **Top bar:** back, `repo#number`, title, state/draft badge, CI badge,
    refresh, theme toggle, settings, command palette, and a review button
    showing the pending-comment count.
  - **Left pane (resizable):** filter bar (extension chips, hide-viewed toggle,
    path search, status filter), file list summary (`N files, +X −Y`),
    "mark all viewed", and the file tree.
  - **Center pane:** the virtualized diff, with a sticky per-file header
    showing status, +/-, copy-path, open-in-browser, **comment on file**
    (present on every file, including binary and too-large files), and a
    **viewed** checkbox; split/unified toggle, ignore-whitespace, word wrap,
    and in-diff find. Image files render the image diff viewer (§8.5).
  - **Right pane (collapsible):** PR description (markdown), conversation
    timeline (reviews + top-level comments, read-only), commits, checks, and
    reviewers.
  - **Pending review bar (floating):** "3 pending comments — Submit review /
    Discard".

Settings (host, client ID, default scopes, theme, sign out) is a **modal
window**, not a fourth screen.

### 9.2 File tree

- Built from the filtered file list's paths; folders nest and show rolled-up
  +/- and a tri-state viewed indicator. Single-child directory chains are
  collapsed into one row (`a/b/c`). Directories are expanded by default; the
  collapsed set is stored per PR.
- Sorted so that directories precede files and both are ordered by path, with
  the option to order by "largest change first".
- Selecting a file scrolls the center pane to that file and expands it; the tree
  highlights the current file. `j`/`k` move to the next/previous file in the
  filtered list.
- Clicking a folder's viewed checkbox **toggles** it: if every filtered
  descendant is viewed it unmarks them, otherwise it marks them (§10.3).

### 9.3 Comment composer

Appears inline beneath the target row(s). Contents:

- A multiline markdown text box with a preview toggle. Its text is persisted
  (debounced) to the session file so a crash does not lose it.
- A segmented control that mirrors GitHub:
  - No open review → `[ Add single comment ] [ Start a review ]`.
  - Open review → `[ Add to review ] [ Add single comment ]`.
  - File-level target → `[ Add single comment ]` only (§7.3).
- For context lines, a `Left / Right` side toggle.
- "Insert suggestion" (RIGHT side only), which wraps the text of the selected
  rows in a GitHub ` ```suggestion ` fence. A suggestion replaces the
  commented lines, so it uses all selected new-side rows (added and context).
- Cancel.

Existing comments render inline in the diff as threads with author, avatar,
timestamp, markdown body, replies, a reply box, and a resolve/unresolve control.
Outdated threads (GraphQL `isOutdated`) are shown collapsed with a note.
Suggestion blocks are rendered normally and additionally offer a copy action.

### 9.4 Keyboard shortcuts

Single-key shortcuts fire only when no text field has keyboard focus
(`egui::Context::wants_keyboard_input()` is false).

| Key | Action |
|---|---|
| `⌘K` / `Ctrl+K` | Command palette |
| `?` | Shortcut help |
| `j` / `k` | Next / previous file |
| `n` / `p` | Next / previous hunk |
| `v` | Toggle viewed on the current file |
| `⇧V` | Mark current file viewed and jump to next |
| `c` | Comment on the focused line |
| `r` | Reply to the focused thread |
| `o` | Toggle the right pane |
| `s` / `u` | Split / unified diff |
| `⌘Enter` / `Ctrl+Enter` | Submit review (from the review panel) |
| `Esc` | Close composer/palette/modal |

## 10. Review, comments, and progress semantics

### 10.1 Two comment modes

Each composed comment carries a `CommentMode`:

- **Immediate** — posted right away via
  `POST /repos/{o}/{r}/pulls/{n}/comments`. Optimistically inserted into the
  diff; rolled back with a toast on failure (the composer text is kept). This
  is GitHub's "Add single comment".
- **Pending** — appended to the local `PendingReview` buffer and only sent when
  the review is submitted. This is GitHub's "Add to review"/"Start a review".

### 10.2 Pending review buffer

```rust
pub struct PendingReview {
    pub commit_id: Option<String>,      // pinned by the first comment
    pub body: String,                   // summary text
    pub event: ReviewEvent,             // Comment | Approve | RequestChanges
    pub comments: Vec<PendingComment>,  // local_id, anchor (path, line/side/range), body
    pub started_at: Option<SystemTime>,
    pub next_local_id: u64,
}
```

- The draft is **pinned to one commit**: the first pending comment sets
  `commit_id` to the head SHA of the displayed diff. If the PR head moves while
  the draft is non-empty, the draft and its pin are kept, a banner says "your
  draft was written against `abc1234`", new *pending* comments on the new diff
  are blocked until the draft is submitted (against its pinned commit) or
  discarded, and immediate comments keep working.
- Every buffer mutation (add/edit/delete/body/verdict) writes the session file
  **immediately and atomically** (write temp file, fsync, rename). Only
  viewed-state and in-progress composer text are debounced.
- Pending comments are listed in the review panel, each editable and deletable.
- Submit sends one request per §7.4; on success the buffer clears, the new
  review appears in the timeline, and threads reload so the new comments are
  anchored. On failure the buffer is untouched.
- Discard clears the buffer after a confirmation.
- Pending comments render inline in the diff as their own rows, with a distinct
  "pending" style so they are unmistakable next to already-posted comments.

### 10.3 Viewed state and folder marking

- Viewed state is stored per `(host, repo, pr_number)` as a map
  `path → blob_sha at the time it was marked`. A file counts as viewed only if
  its stored blob SHA equals its current `blob_sha`. This mirrors GitHub, which
  on a new push un-views **only the files that changed** and keeps the rest
  viewed. Files whose mark was invalidated this way are counted for the hint
  "N files you had viewed changed since".
- Viewed state is local only; it is not synced with GitHub's own per-file
  viewed flags in v1 (§16).
- Folder tri-state is computed over the folder's descendants that pass every
  filter **except hide-viewed** (otherwise hide-viewed would make every visible
  folder look unviewed): *All* when all are viewed, *Some* when some are,
  *None* otherwise.
- **Folder marking order is explicit and non-ambiguous:** (1) snapshot the
  folder's descendants that pass the filters except hide-viewed, (2) if all of
  them are viewed unmark the snapshot, otherwise mark it, (3) re-evaluate
  filters. If "hide viewed" is on, newly marked files disappear from the tree
  after step 3 — the snapshot is what makes this correct, rather than marking
  while the list shrinks.
- A progress counter in the left pane shows `viewed / total` (filtered and
  unfiltered).

### 10.4 Filters

Filters form a conjunction (a file is shown if it passes all of them); the
result does not depend on evaluation order. An empty selection in a
multi-select filter means "no constraint".

- **Extension** — multi-select chips derived from the file list (text after the
  last `.` of the file name; files with no extension are grouped as "(none)").
- **Status** — added / modified / removed / renamed (others group under
  modified).
- **Path** — case-insensitive substring; `*` matches any run of characters.
- **Hide viewed** — toggle.

Filter state is local and never triggers a network call.

## 11. Persistence

Platform config dir via `directories`:

```
<config>/gh-review-desktop/
├── settings.toml        # host, client_id override, cached login/name/avatar/scopes,
│                        # theme, default scopes, keybinding overrides, pane sizes
└── sessions/
    └── <host>/<owner>/<repo>/<pr-number>.json
          # head_sha, viewed {path → blob_sha}, pending review draft (incl. pinned
          # commit_id), open composer draft, last opened file, collapsed tree dirs
```

`<owner>` and `<repo>` are lowercased; the nested layout avoids name collisions
between hosts and between repos. All writes are atomic (temp file + rename).

Token: OS keychain (service `gh-review-desktop`, account `{host}:{login}`).
Secrets are never written to `settings.toml`, session files, or logs.

A later milestone adds `<cache>/gh-review-desktop/` for diffs and ETags.

## 12. Performance strategy

Target: **no visible stutter (frame time under ~16 ms) on a 1,500-file PR with
50,000+ changed lines**, while scrolling, filtering, and toggling viewed state.

Mechanisms:

1. Row virtualization with prefix-sum heights for the entire diff surface (§8.4).
2. Lazy, LRU-cached syntax highlighting for visible rows only; long-line guard.
3. Paginated file/metadata loading with progressive rendering (header and file
   tree first, diff rows as pages are parsed on the worker).
4. Filtering and viewed-marking operate on path sets, not on rendered widgets.
5. Index-based row model; no per-row string clones.
6. Debounced disk writes for viewed state and composer text.

This is verified, not assumed: a synthetic large-PR fixture drives a
**model-level budget test** (building/visiting the visible slice, highlighting
it with a warm syntax set, filtering) in release mode, and a manual frame-time
check using egui's frame-time readout is part of QA for the relevant
milestones. A unit test cannot measure real GPU frame time.

## 13. Error handling

A single `GhError` maps to specific UI behavior:

| Condition | Behavior |
|---|---|
| 401 / invalid token | Delete token, go to Login, explain session expiry; drafts kept |
| 403/429 with `x-ratelimit-remaining: 0` or a `retry-after` header (primary or secondary rate limit) | Banner with reset countdown; disable write actions until reset |
| 403 with `X-GitHub-SSO` header | Banner "authorize this token for the organization's SSO" with the URL from the header |
| Other 403 | Permission error with the API message (e.g. OAuth App access restricted by the org) |
| 404 | PR/repo gone or inaccessible; close the session with a toast |
| 422 validation | Show the API message; for line comments offer the file-level fallback |
| 5xx / network / timeout | Reads: automatic retry with backoff (max 3), then inline retry. **Mutations are never retried automatically** (a retry could double-post); the UI offers a manual retry and keeps the text |
| GraphQL `errors[]` | Surfaced per-field where identifiable |
| Worker task panic | Caught (`JoinError`), logged, degraded to a toast; the app stays alive |

Note: `x-ratelimit-reset` is present on every response, so its presence alone
never implies rate limiting.

All mutations are optimistic with rollback. The pending review buffer is written
to disk on every change, so no failure mode loses user writing.

## 14. Testing strategy

Development follows TDD: failing test first, then implementation.

**Unit tests (pure, no network, no GUI):**

- Patch parser: hunk headers, omitted counts, no-newline markers, CRLF, empty
  context lines, non-ASCII text, and correct old/new numbering per row.
- Comment coordinate mapping: single line both sides, context-side toggle,
  multi-line range constraints (`start_line < line`, same side, same hunk),
  file-level.
- Context expansion numbering from hunk offsets.
- Filter conjunction and the hide-viewed interaction with folder marking.
- Folder viewed tri-state derivation and the snapshot-then-toggle ordering.
- Viewed invalidation by blob SHA.
- Pending review reducer: add/edit/delete/submit/discard, commit pinning, and
  draft serialization round-trip.
- Path tree building, including renames, deep nesting, and chain collapsing.
- Suggestion block build/parse.
- Markdown rendering of comment bodies (no panics on malformed input).
- Image viewer helpers: image detection by extension, base/head blob choice per
  file status, fit-to-pane sizing, size-delta labels; every file header
  (binary included) offers "Comment on file".

**Contract tests** against a local mock HTTP server (`wiremock`):

- Page (`Link` header) and cursor (GraphQL `pageInfo`) pagination for files,
  threads, reviews, and commits.
- Rate-limit and SSO header classification.
- Device-flow request bodies and `Accept` header.
- Raw blob download with a percent-encoded path and the 10 MB cap.
- **Exact request bodies** for: immediate line comment, immediate file-level
  comment, thread reply, review submission (including multi-line ranges), and
  thread resolve. Wrong `side`/`line`/`commit_id`/`path` is the most likely
  real bug, so these assertions are mandatory.

**Fixture tests:** recorded GitHub JSON for PR search, file lists (including a
no-patch large file and a binary file), threads, reviews (including a
`PENDING` one), and metadata, replayed through the parsers to catch schema
drift.

**Performance guardrail:** a synthetic 1,500-file / 50,000-line PR driving the
model-level budget test (§12). It is wall-clock sensitive, so it is
`#[ignore]`d in the default test run and executed in CI as a separate
`cargo test --release -- --ignored` step.

**Manual QA checklist per milestone:** fresh install login; a PR with pending
comments surviving a quit/restart; a PR that receives a new commit while open
(only changed files lose viewed state, draft is preserved and pinned); a PR you
authored (approve disabled); a PR with a pending review started on the web; a
PR that adds, modifies, renames, and deletes images (viewer modes, file-level
comment on an image); a GitHub Enterprise host.

## 15. Milestones

| # | Milestone | Definition of done |
|---|---|---|
| **M0** | Scaffold | Cargo package (lib + bin), `eframe` + `tokio` wiring, `Command`/`Event` plumbing, `fmt`/`clippy`/`test` in CI |
| **M1** | Auth | Device flow + PAT, keyring storage, `GithubClient`, `GhError`, login screen, sign-out, app shell routing |
| **M2** | PR list | Scope switcher, repo/org constraint, search, sort, badges, pagination |
| **M3** | PR detail shell + diff engine | Metadata + files load, patch parser, virtualized unified diff, syntax highlighting, binary/too-large placeholders, "Comment on file" header button |
| **M4** | File tree, viewed, filters | Tree with rollups, viewed state + persistence, extension/status/path filters, hide-viewed, folder marking, progress counter |
| **M5** | Comments | Existing threads with replies, resolve/unresolve, immediate comments, multi-line ranges, suggestion rendering, file-level comments |
| **M6** | Pending review | Buffer with draft persistence, review panel, submit with verdict, discard, server-side pending review handling, timeline of past reviews |
| **M7** | Rich diff polish | Split view, ignore whitespace, context expansion via head blob, local diff for too-large files, in-diff find, word wrap, image diff viewer (2-up / swipe / onion skin) |
| **M8** | Metadata surface | Description markdown, commits list, CI checks, reviewers, labels, referenced issues, conversation comments |
| **M9** | Power UX | Command palette, full keybindings, themes, settings window, pane/layout persistence |
| **M10** | Later | Diff/ETag cache, background refresh and new-comment polling, notifications, offline browsing |

## 16. Deferred / explicitly out of scope

Offline cache, ETags, and background polling (M10); syncing viewed state with
GitHub's per-file viewed flags; simultaneous multi-account; PR merge/close;
posting top-level conversation comments; stacked-PR awareness; CODEOWNERS
hints; emoji reactions; subscribing to notification threads; per-commit or
arbitrary-ref diffs; editing files.

## 17. Known limitations

- Search-based PR lists inherit GitHub's 1000-result-per-query cap and the
  search rate limit; multi-scope totals are approximate.
- The files API returns at most 3000 files per PR.
- Files where GitHub omits the patch cannot always be line-commented; they fall
  back to file-level comments.
- Comments on expanded context rows (outside GitHub's hunks) may be rejected by
  GitHub; the app warns and offers the file-level fallback.
- File-level comments cannot be part of a pending review (API limitation); they
  are always posted immediately. This includes every comment on a binary or
  image file.
- Comments on images cannot point at a region of the image; they are
  file-level.
- Only the image formats enabled in the `image` crate are rendered (PNG, JPEG,
  GIF, WebP, BMP, ICO, plus SVG); others (e.g. PSD, TIFF, AVIF) and images over
  10 MB show the binary placeholder.
- Syntax highlighting is per line, so multi-line constructs can be mis-colored.
- Organizations with OAuth App access restrictions hide their repos from the
  device-flow token until an org owner approves the app; a PAT works around it.
- `Approve` / `Request changes` are unavailable on PRs you authored (a GitHub
  rule, surfaced in the UI).

## 18. Open questions

None outstanding.

## 19. Revision notes (2026-10-19 review)

Corrections made to the first draft:

- Multi-line ranges: `start_line < line` on both sides (the draft's reversed
  rule for `LEFT` was wrong); ranges must stay within one hunk.
- Scope queries: search qualifiers AND together, so each scope is a separate
  query merged locally (the draft combined them into one query).
- File-level comments: REST `subject_type: "file"` instead of the GraphQL
  `addPullRequestReviewThread` route, so they publish immediately and need no
  PR node id; they cannot be pending.
- Server-side pending reviews (§7.6) were unhandled and would make every
  submit fail with `422`.
- Viewed state: GitHub un-views only changed files on a new push; keyed by blob
  SHA instead of resetting everything.
- Context expansion: uses the head blob plus hunk offsets (exact numbering);
  the local `similar` diff is only for no-patch files and must use the merge
  base, not the base branch tip.
- `FileStatus` no longer has a fake `Binary` variant; binary vs. too-large is
  `PatchState`.
- Sign-out and 401 no longer delete drafts (contradicted S4).
- Draft writes are immediate and atomic (the draft said both "immediately" and
  "debounced"); the draft is pinned to one commit.
- 403 is only a rate limit when the rate-limit headers say so; mutations are
  never auto-retried; stale events are dropped via `SessionId`.
- Dropped `octocrab` (unused next to hand-parsed bodies), the per-row
  `OnceCell` highlight (duplicated the LRU), the "comment on a specific commit"
  clause (no per-commit diff view exists), and ETag tests from v1.
- Added later the same day: the image diff viewer (§8.5) and a "Comment on
  file" button on every file header, so binary files can be commented on.
- Added: host/URL derivation, Enterprise client-ID caveat, `keyring` backend
  features, PR list via GraphQL (badges in one round trip), long-line guard,
  variable-height virtualization, settings as a modal, keyboard focus rule,
  3000-file cap, `REQUEST_CHANGES` body rule.
