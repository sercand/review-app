# AI settings tab, per-PR repo folder, and the Local repos list

Date: 2026-10-22. Branch: `ai-review/ui`. Builds on the AI review work in
`docs/superpowers/plans/2026-10-20-ai-review-{1,2,3}-*.md` and the spec §10 (settings).

## Why

The AI review feature works, but two parts of its setup are hard to use:

1. Providers and models are edited in one long scroll area inside the global Settings window.
   The model is a bare text field, sizes are drag-values, and Test connection only works after
   Save, which closes the window.
2. The local checkout folder for a repo is set in a free-text "Repositories" list in Settings
   (and once, in a dialog, when the first review starts). The user wants the folder tied to the
   repo and managed where they use it, not as global configuration.

## Goals

- A separate **AI tab** in Settings with header-row tables for providers and models. Clicking a
  row (or "Add …") opens a small dialog to edit it.
- The repo ↔ local folder link is managed **from the PR** (AI tab) and from a new **Local repos**
  tab on the PR list. It is no longer a Settings list.
- The PR list gets an **All PRs | Local repos** switch. Local repos lists every linked repo as a
  row; clicking a row expands it (accordion) to show that repo's PRs. All rows start collapsed.
- Repos can be linked two ways: by choosing a folder from a PR's AI tab, and by "+ Add local
  repo…" on the Local repos tab. Both read the folder's git remote to learn which repo it is.

## Non-goals

- No change to the review pipeline, sandbox, approval system (the user is replacing that
  separately), or the findings flow.
- No new provider APIs. The two existing protocols (Anthropic, OpenAI) stay.
- Not moving secrets out of the keychain. Keys stay keychain-only.
- No migration tool: the stored data shape does not change (see Data).

## Anti-goals

- A key must never end up in `settings.toml` or a session file.
- A rename or a failed Save must never silently delete a stored key.
- The GUI thread must not do blocking work (directory listing, git, keychain) in a frame.

## Data (unchanged shape)

The link stays in `settings.ai.repos` (`RepoAiSettings`: `repo` = `host/owner/name`, `path`,
`commands`, `prepare`, extra paths). Existing files keep working. What changes is only who edits
it. A repo row with an empty `path` is treated as not linked and is not listed.

## Settings → AI tab

Layout as in the approved mockup.

- Tabs in the Settings window: General, Appearance, AI (unix only, as today).
- **Providers table**, header columns: Name, API, Base URL, Key, Status. Row click or
  "+ Add provider" opens the **provider dialog**: name, API, base URL (optional), API key
  (password field, "stored in keychain" when present), a connection-test result line, and
  Delete / Test / Cancel / Save.
- **Models table**, header columns: Role, Provider, Model, Context, Max out, Reasoning. Rows are
  the two roles that exist today: "Review & chat" (`models.default`) and "Lookup helper"
  (`models.lookup`, optional, may be empty). Row click opens the **model dialog**: provider
  (dropdown of configured providers), model id, context window, max output, reasoning level.
- Test works with the key typed in the dialog, before saving. Save in a dialog commits that
  row only; the Settings window itself no longer closes because of an AI edit.
- Renaming a provider moves its keychain entry (`llm:<old>` → `llm:<new>`) and updates any model
  that referenced it. Deleting a provider that a model uses is blocked with a message naming the
  model (or clears that model after confirmation — builder's choice, but never silently).
- Validation stays as today: names non-empty, no leading/trailing spaces, unique.
- Saving general settings is no longer blocked by an invalid AI row, because AI rows are saved
  by their own dialogs.

## PR → AI tab: Local checkout row

Shown before a review starts and kept visible afterwards.

- Row: "Local checkout for `owner/name`", the folder, a check that it is a clone of that repo, and
  **Change…** / **Unlink**. Below it, the repo's allowed commands and prepare commands with an
  edit affordance (these move here from Settings).
- No folder: the row shows "Choose folder…" and Start is disabled with the reason stated.
- A saved folder that no longer exists or is no longer a clone of the repo shows an error **in
  this row**, not a toast, and offers Change….
- Folder choice uses the native picker only (the hand-rolled Browse popup in Settings is
  removed). A picker answer is tied to the repo that asked for it, so a late answer cannot land
  on another repo's dialog.
- Start also checks, before any git checkout, that a default model and its provider key exist,
  and says what is missing.

## PR list → Local repos tab

Layout as in the approved mockup.

- Switch: **All PRs | Local repos**. The choice is remembered across launches.
- Local repos is a table, header columns: (chevron), Repository, Folder, Open PRs, (⋯ menu).
  Rows come from `settings.ai.repos` entries that have a folder and whose host is the signed-in
  host.
- Row click toggles an accordion that shows that repo's open PRs with the same PR row widget the
  main list uses; clicking a PR opens it as usual. All rows start collapsed.
- "Open PRs" count and the expanded list come from the existing GitHub search with a
  `repo:owner/name` constraint. They are fetched when needed (see Builder's calls).
- ⋯ menu: Change folder, Unlink. A repo whose folder is missing shows "folder missing" in red in
  the Folder column; its PRs still load.
- "+ Add local repo…": native folder picker, read the git remote, derive `host/owner/name`,
  validate it is a clone, save the link. If the remote cannot be read or is not on the signed-in
  host, say so and add nothing. Adding an already-linked repo changes nothing and says so.
- Empty state: explain how to link a repo (button plus "or open a PR and use its AI tab").

## Constraints

- AI code is unix-only (`#[cfg(unix)]`); the AI Settings tab and the Local repos tab follow suit
  and are absent on Windows. The existing Windows build and clippy must keep passing.
- Follow AGENTS.md: UI never does I/O; commands go to the worker, events come back; stale-event
  rule (SessionId) for PR-scoped events; widgets return action enums that `app.rs` applies;
  decision logic in pure, unit-tested functions. Keychain access and folder/git probing happen
  off the GUI thread.
- CI must stay green: `cargo fmt --check`, clippy with `-D warnings`, `cargo test --workspace
  --features ai-testing`.
- Screens get `egui_kittest` shots in `src/ui/shots.rs` like the other screens.

## Builder's calls (the author's choices, not the user's)

- Model presets: a short built-in list per API (e.g. Anthropic and OpenAI model ids with their
  context/output sizes) that fills the dialog; "Custom" keeps free text. The exact list is the
  builder's call.
- Open-PR counts: one search per linked repo when the Local repos tab opens, results cached for
  the session and refreshed on pull-to-refresh/the existing refresh action. Expanding a row uses
  the same cached result when present.
- Local repos on a different host than the signed-in one are not listed.
- Local repos tab and AI Settings tab are unix-only because the data lives in AI settings.
- Per-repo extra write/deny-read paths stay editable but live in the "edit" affordance under the
  Local checkout row.

- Mockups the user approved: `docs/design/mockups/ai-settings/ai-settings-tab.html` (Settings
  AI tab + dialog), `local-repos-accordion.html` (Local repos tab; this is the approved
  layout), `repo-path-and-list-tab.html` (the Local checkout row at the top; ignore the A/B
  options below it, the accordion file supersedes them).
- **Unlink** clears only `path`. Commands, prepare commands and extra paths stay in the entry,
  so relinking restores them. The Local checkout row can edit commands for a repo that is not
  linked yet; such an entry (empty `path`) is simply not listed in Local repos.
- **Rename and key move** run as one worker command: copy the key to `llm:<new>`, write
  `settings.toml`, then delete `llm:<old>`. If the copy fails, nothing changes and the dialog
  shows the error. If the settings write fails, the new entry is removed again and the old key
  is untouched. A key typed in the dialog but not yet stored is the key that gets written.
- **Dialog Save patches only its own row** on the live settings (providers or models) and
  persists. The AI part of the Settings window's draft is removed; General and Appearance keep
  their existing draft-and-Save flow and are not written by an AI dialog.
- **"Is a clone" check:** read `origin`; when there is no `origin` and exactly one remote, use
  it; otherwise it is ambiguous and the user is told. Accept SSH (`git@host:o/r`), `ssh://` and
  HTTPS forms, with or without `.git`. The remote decides the repo (a fork's clone links to the
  fork). The host must equal the signed-in host. This is pure parsing, unit-tested, and the git
  call runs off the GUI thread. "+ Add local repo…" uses the same rule.
- Deleting a provider that a model uses is blocked with a message naming the model. With no
  default model, Start fails its pre-check and says so.
- **Unlink / Change folder is blocked** while an AI review or a checkout for that repo is
  active or unrestored (the restore logic depends on the path), with the reason shown.
- **Open-PR searches** carry a generation counter (not a `SessionId`, since they are not
  PR-scoped); replies from an older generation are dropped. At most 4 run at once.

## Out of scope but noted

The remaining review findings (run completion semantics, verifier failure handling, prompt
delimiters, token budget, Windows clippy for `session_is`, `crates/llm` redirects) are separate
work and not part of this design.
