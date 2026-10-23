//! App-run git checkout of a PR head, restore, and prepare commands (spec §7).
//!
//! The app (not the agent) performs these git operations as child processes
//! using the user's normal environment and credentials (§7.3). Every call is
//! **blocking**: callers run these functions on `spawn_blocking` (Plan 3).
//!
//! Safety guarantees (spec §7.1/§7.3):
//!
//! - The original state is recorded in SQLite **before** anything is stashed,
//!   so a crash can be undone (§7.3, Task 10's `ai_checkout` records).
//! - The stash is tracked by its **commit SHA**, never `stash@{0}`, which can
//!   shift if the user creates their own stash mid-review.
//! - Restore checks the tree, returns it to the exact original ref (branch
//!   names with slashes and detached HEAD included), re-applies our stash by
//!   SHA, and drops **only our own** stash entry by locating its index in
//!   `git stash list --format=%H`.
//! - On a stash apply conflict the stash is left in place and the record is
//!   marked `Failed`; nothing is ever forced.
//!
//! ## Path identity
//!
//! [`Store::checkout_holding`](super::store::Store::checkout_holding) matches
//! `repo_path` as exact stored text. [`plan_checkout`] canonicalises the repo
//! path once at entry and every later call path ([`execute_checkout`] via
//! [`CheckoutPlan::repo_path`], [`restore`] via [`CheckoutRecord::repo_path`])
//! uses that same canonical path, so one repo always maps to one stored path.

use std::path::{Path, PathBuf};

use thiserror::Error;

use super::store::{CheckoutRecord, CheckoutState, OriginalRef, Store};

/// Everything needed to address one pull request's head commit locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrTarget {
    /// GitHub host, e.g. `github.com`.
    pub host: String,
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub name: String,
    /// Pull request number.
    pub number: u64,
    /// Full SHA of the PR head commit.
    pub head_sha: String,
    /// Branch name the PR is from (display only; the checkout is detached).
    pub head_branch: String,
}

/// Failures of the checkout/restore machinery, with user-facing messages.
#[derive(Debug, Error)]
pub enum CheckoutError {
    /// The folder is not a git repository.
    #[error("{} is not a git repository", .0.display())]
    NotARepo(PathBuf),
    /// No remote in the repository points at the PR's `owner/name` on `host`.
    #[error("no remote in this repository points at {wanted}")]
    NoMatchingRemote {
        /// The `host/owner/name` the PR belongs to.
        wanted: String,
    },
    /// The folder's repository has no remote, so the repository it is a clone
    /// of cannot be learned.
    #[error("this folder has no git remote, so the repository it belongs to is unknown")]
    NoRemote,
    /// The folder's repository has several remotes and no `origin` to choose
    /// between them.
    #[error(
        "this folder has several remotes ({}), so the repository it belongs to is ambiguous",
        .0.join(", ")
    )]
    AmbiguousRemote(Vec<String>),
    /// A remote URL that is not a repository URL this app understands.
    #[error("the remote URL `{0}` is not a repository URL this app understands")]
    UnparseableRemote(String),
    /// A `git` invocation failed.
    #[error("`git {args}` failed: {stderr}")]
    Git {
        /// The git arguments (for display).
        args: String,
        /// Captured stderr (or the spawn error) from the failed call.
        stderr: String,
    },
    /// After checkout, `HEAD` did not match the PR head SHA (§7.1 step 7).
    #[error("HEAD is {actual} after checkout, expected the PR head {expected}")]
    HeadMismatch {
        /// The PR head SHA that was requested.
        expected: String,
        /// The SHA `HEAD` actually landed on.
        actual: String,
    },
    /// Another PR is already checked out in this repository (§4 step 2).
    #[error("PR #{pr_number} is already checked out in this repository")]
    Held {
        /// The other PR's number.
        pr_number: u64,
        /// The holding record's id, so a UI with no live run for that PR
        /// can offer to restore it (Task 4 review finding 2).
        record_id: i64,
    },
    /// The working tree has changes made during the review; the user must
    /// choose to discard them or leave the checkout in place (§7.3 step 1).
    #[error(
        "the working tree has changes made during the review; discard them or leave the checkout as is"
    )]
    NeedsDecision(Vec<String>),
    /// The repository changed while the checkout was being confirmed: the
    /// plan's snapshot of the working tree or `HEAD` no longer matches.
    /// Nothing was stashed, recorded or touched — confirm again.
    #[error("the repository changed since the checkout was confirmed; confirm again")]
    PlanStale,
    /// A checkout attempt failed (`cause`) and the working tree held changes
    /// that were not part of the attempt - almost certainly the user's own
    /// edits, made while the app was fetching. They were left untouched and
    /// the record was marked `Failed`.
    #[error(
        "{cause}; the working tree has {} uncommitted change(s) made meanwhile, which were left untouched. \
         The checkout is marked failed: restore it from the recovery dialog once those changes are dealt with",
        .changes.len()
    )]
    LeftChanges {
        /// Why the checkout attempt failed.
        cause: String,
        /// `git status --porcelain` lines of the changes that were kept.
        changes: Vec<String>,
    },
    /// The record was already restored, or replaced by a newer one, so
    /// restoring it again would reset the tree and re-apply its stash.
    #[error("this checkout was already restored or replaced; nothing was changed")]
    AlreadyFinished,
    /// The SQLite checkout record could not be written.
    #[error("could not record the checkout: {0}")]
    Store(#[from] anyhow::Error),
}

/// Runs one `git` invocation in `repo`: `git` from PATH with the user's
/// normal environment plus `GIT_TERMINAL_PROMPT=0`. Never retried.
fn git(repo: &Path, args: &[&str]) -> Result<String, CheckoutError> {
    let shown = args.join(" ");
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .map_err(|e| CheckoutError::Git {
            args: shown.clone(),
            stderr: e.to_string(),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if stderr.trim().is_empty() {
            stderr = format!("exit status {}", out.status);
        }
        Err(CheckoutError::Git {
            args: shown,
            stderr: stderr.trim().to_string(),
        })
    }
}

/// Lowercase a path slug for comparison, dropping a trailing `/` and an
/// optional `.git` suffix (in either order).
fn normalize_slug(path: &str) -> String {
    let mut p = path.trim_end_matches('/').to_ascii_lowercase();
    if let Some(stripped) = p.strip_suffix(".git") {
        p = stripped.trim_end_matches('/').to_string();
    }
    p
}

/// Split a raw remote URL into its (lowercased) host and `owner/name` slug,
/// understanding the https, `ssh://` and scp (`git@host:path`) forms. Ports
/// and userinfo are ignored; returns `None` for anything unrecognised.
fn split_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim().to_ascii_lowercase();
    let (host, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        let slash = rest.find('/')?;
        let authority = &rest[..slash];
        // Drop any userinfo; strip any `:port` from the host.
        let host = match authority.rsplit_once('@') {
            Some((_, host)) => host,
            None => authority,
        };
        let host = host.split(':').next().unwrap_or(host);
        (host, &rest[slash + 1..])
    } else {
        // scp-like: `[user@]host:path`. A `/` before the `:` means it is
        // just a (local) path, not a host.
        let colon = url.find(':')?;
        if url[..colon].contains('/') {
            return None;
        }
        let before = &url[..colon];
        let host = match before.rsplit_once('@') {
            Some((_, host)) => host,
            None => before,
        };
        (host, &url[colon + 1..])
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), normalize_slug(path)))
}

/// Find the remote whose **raw** configured URL points at
/// `host/owner/name` (spec §7.1 step 1). Matches the https, ssh and scp URL
/// forms, case-insensitively, ignoring a trailing `/`, an optional `:port`
/// and an optional `.git` suffix. Reads raw URLs via
/// `git config --get-regexp '^remote\..*\.url$'` because `git remote -v`
/// shows URLs after `insteadOf` rewriting.
pub fn find_remote(
    repo: &Path,
    host: &str,
    owner: &str,
    name: &str,
) -> Result<String, CheckoutError> {
    let wanted = format!("{host}/{owner}/{name}");
    let (want_host, want_slug) = (
        host.to_ascii_lowercase(),
        normalize_slug(&format!("{owner}/{name}")),
    );
    let raw = match git(repo, &["config", "--get-regexp", "^remote\\..*\\.url$"]) {
        Ok(out) => out,
        // No remotes configured at all: `--get-regexp` exits non-zero.
        Err(CheckoutError::Git { .. }) => {
            return Err(CheckoutError::NoMatchingRemote { wanted });
        }
        Err(e) => return Err(e),
    };
    for line in raw.lines() {
        let Some((key, url)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(remote) = key
            .strip_prefix("remote.")
            .and_then(|k| k.strip_suffix(".url"))
        else {
            continue;
        };
        if let Some((u_host, u_slug)) = split_remote_url(url.trim())
            && u_host == want_host
            && u_slug == want_slug
        {
            return Ok(remote.to_string());
        }
    }
    Err(CheckoutError::NoMatchingRemote { wanted })
}

/// The repository a local folder is a clone of, as [`read_origin_repo`] names
/// it: lowercase `host`, `owner` and `name`, the shape of a settings
/// `host/owner/name` key (spec §4 step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRepo {
    /// The host, e.g. `github.com`.
    pub host: String,
    /// The repository's owner or organization.
    pub owner: String,
    /// The repository name.
    pub name: String,
}

impl RemoteRepo {
    /// The `host/owner/name` key the settings store entry under.
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// Parse one remote URL into a [`RemoteRepo`]. The raw `url` is kept in
/// [`CheckoutError::UnparseableRemote`], because that is what the user has to
/// look at and fix.
fn parse_remote(url: &str) -> Result<RemoteRepo, CheckoutError> {
    let unparseable = || CheckoutError::UnparseableRemote(url.to_string());
    let (host, slug) = split_remote_url(url).ok_or_else(unparseable)?;
    // A repository is exactly `owner/name`: a deeper path is another kind of
    // URL (a browse link, a GitLab subgroup), not a GitHub remote.
    let (owner, name) = slug.split_once('/').ok_or_else(unparseable)?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(unparseable());
    }
    Ok(RemoteRepo {
        host,
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// The repository a local folder is a clone of (spec §4 step 2): its `origin`
/// remote, or its only remote when it has no `origin`.
///
/// Reads the **raw** configured URLs for the same reason as [`find_remote`]:
/// `insteadOf` rewrites would hide the host this app has to match. Whether a
/// remote is a fork or a mirror has no say here — the remote decides the
/// repository, so a fork's clone names the fork.
///
/// Fails with [`CheckoutError::NotARepo`] when `folder` is not a git
/// repository, [`CheckoutError::NoRemote`] when it has no remote at all,
/// [`CheckoutError::AmbiguousRemote`] when it has several remotes and none is
/// named `origin` (the names are sorted for display), and
/// [`CheckoutError::UnparseableRemote`] when the chosen remote's URL is not a
/// repository URL.
pub fn read_origin_repo(folder: &Path) -> Result<RemoteRepo, CheckoutError> {
    git(folder, &["rev-parse", "--git-dir"])
        .map_err(|_| CheckoutError::NotARepo(folder.to_path_buf()))?;
    let raw = match git(folder, &["config", "--get-regexp", "^remote\\..*\\.url$"]) {
        Ok(raw) => raw,
        // A repository with no remotes makes `--get-regexp` exit non-zero.
        Err(CheckoutError::Git { .. }) => return Err(CheckoutError::NoRemote),
        Err(e) => return Err(e),
    };

    // The first URL of each remote is its fetch URL (git ignores any later
    // ones for fetching), so a duplicate key keeps the first.
    let mut remotes: Vec<(String, String)> = Vec::new();
    for line in raw.lines() {
        let Some((key, url)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some(name) = key
            .strip_prefix("remote.")
            .and_then(|k| k.strip_suffix(".url"))
        else {
            continue;
        };
        if !remotes.iter().any(|(known, _)| known == name) {
            remotes.push((name.to_string(), url.trim().to_string()));
        }
    }

    let url = if let Some((_, url)) = remotes.iter().find(|(name, _)| name == "origin") {
        url.clone()
    } else if remotes.is_empty() {
        return Err(CheckoutError::NoRemote);
    } else if let [(_, only)] = remotes.as_slice() {
        only.clone()
    } else {
        let mut names: Vec<String> = remotes.into_iter().map(|(name, _)| name).collect();
        names.sort();
        return Err(CheckoutError::AmbiguousRemote(names));
    };
    parse_remote(&url)
}

/// The confirmed plan for checking out one PR (spec §4 step 2 dialog).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutPlan {
    /// Canonical absolute path of the local clone (see the module docs on
    /// path identity).
    pub repo_path: PathBuf,
    /// The remote name to fetch the PR ref from.
    pub remote: String,
    /// What the working tree is on right now, to be restored later.
    pub original: OriginalRef,
    /// Whether the working tree is dirty (untracked files included).
    pub dirty: bool,
    /// The repository is still held by this PR's own unrestored checkout
    /// and the tree sits on that checkout's head (a re-review after Keep
    /// or a cancel): the tree's changes are the review's own output, so
    /// the plan runs no stash step and [`execute_checkout`] carries the
    /// held record's stash and `original` ref into the new row (Task 8).
    pub skip_stash: bool,
    /// Exact display strings for the commands the checkout will run.
    pub commands: Vec<String>,
}

/// The ref the working tree is on right now: the branch short name, or the
/// HEAD SHA when detached.
fn current_original_ref(repo: &Path) -> Result<OriginalRef, CheckoutError> {
    match git(repo, &["symbolic-ref", "--short", "HEAD"]) {
        Ok(name) => Ok(OriginalRef::Branch(name.trim().to_string())),
        Err(_) => {
            let sha = git(repo, &["rev-parse", "HEAD"])?;
            Ok(OriginalRef::Detached(sha.trim().to_string()))
        }
    }
}

/// The stash message used for PR `number` (spec §7.1 step 4).
fn stash_message(number: u64) -> String {
    format!("gh-review AI #{number}")
}

/// Validate a checkout of `pr` in `repo` and build the display plan.
///
/// Fails with [`CheckoutError::Held`] when the store shows a different PR
/// still checked out in this repo; a record for the *same* PR is allowed (a
/// retry after a failed attempt, or a re-review while the previous checkout
/// is still held — the latter sets [`CheckoutPlan::skip_stash`] and drops
/// the stash line from `commands` when the tree sits on that checkout's
/// head).
pub fn plan_checkout(
    repo: &Path,
    pr: &PrTarget,
    store: &Store,
) -> Result<CheckoutPlan, CheckoutError> {
    // Canonicalise once at entry: the record, the holding check and every
    // later git call use this exact path (see the module docs).
    let repo = repo
        .canonicalize()
        .map_err(|_| CheckoutError::NotARepo(repo.to_path_buf()))?;
    git(&repo, &["rev-parse", "--git-dir"]).map_err(|_| CheckoutError::NotARepo(repo.clone()))?;

    let holding = store
        .checkout_holding(&repo)
        .map_err(CheckoutError::Store)?;
    if let Some(rec) = &holding
        && rec.pr_number != pr.number
    {
        return Err(CheckoutError::Held {
            pr_number: rec.pr_number,
            record_id: rec.id,
        });
    }

    let remote = find_remote(&repo, &pr.host, &pr.owner, &pr.name)?;
    let original = current_original_ref(&repo)?;
    let dirty = !git(&repo, &["status", "--porcelain"])?.trim().is_empty();

    // Held by this PR's own unrestored checkout with the tree still on
    // that checkout's head: this is a re-review (Continue), not a fresh
    // checkout. The tree's changes are the review's own output — stashing
    // them would bury build output under a new stash — so the plan runs
    // no stash step and `execute_checkout` carries the held record's
    // stash and original ref into the new row (Task 8).
    let skip_stash = holding.as_ref().is_some_and(|rec| {
        rec.pr_number == pr.number && original == OriginalRef::Detached(rec.head_sha.clone())
    });

    let mut commands = Vec::new();
    if dirty && !skip_stash {
        commands.push(format!(
            "git stash push --include-untracked -m \"{}\"",
            stash_message(pr.number)
        ));
    }
    commands.push(format!("git fetch {remote} pull/{}/head", pr.number));
    commands.push(format!("git checkout --detach {}", pr.head_sha));

    Ok(CheckoutPlan {
        repo_path: repo,
        remote,
        original,
        dirty,
        skip_stash,
        commands,
    })
}

/// Whether `plan`'s snapshot still describes the repository: the dirty flag
/// (untracked files included) and the original ref must be unchanged since
/// [`plan_checkout`] built it. A dirty-flag mismatch is
/// [`CheckoutError::PlanStale`], and so is a failed original-ref probe (e.g.
/// `HEAD` went missing); a failed `git status` propagates as
/// [`CheckoutError::Git`] instead.
///
/// The dirty comparison is skipped for a [`CheckoutPlan::skip_stash`] plan:
/// the flag exists to protect the stash step, and this plan has none — the
/// tree's changes are the review's own output and may come and go while the
/// confirmation dialog is open.
fn check_plan_current(plan: &CheckoutPlan) -> Result<(), CheckoutError> {
    let dirty = !git(&plan.repo_path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    if dirty != plan.dirty && !plan.skip_stash {
        return Err(CheckoutError::PlanStale);
    }
    let unchanged = match &plan.original {
        OriginalRef::Branch(name) => git(
            &plan.repo_path,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .ok()
        .is_some_and(|current| current.trim() == name.as_str()),
        OriginalRef::Detached(sha) => git(&plan.repo_path, &["rev-parse", "HEAD"])
            .ok()
            .is_some_and(|current| current.trim() == sha.as_str()),
    };
    if unchanged {
        Ok(())
    } else {
        Err(CheckoutError::PlanStale)
    }
}

/// Run the plan (spec §7.1): record `Started` → stash (if dirty) and note
/// its SHA → fetch → detached checkout → verify `HEAD` → `Active`.
///
/// Any failure after the stash was created restores the repository before
/// the error is returned; a `HEAD` mismatch runs a full restore and then
/// fails with [`CheckoutError::HeadMismatch`].
///
/// Two deliberate exceptions to that restore rule, both at a point where no
/// usable stash exists yet (none was created, or it could not be named):
///
/// - A failed `git stash push` leaves the tree **untouched** (a restore
///   would `reset --hard` the very changes the stash was meant to protect)
///   and marks the record `Failed`.
/// - If `rev-parse stash@{0}` fails right after a push that reported
///   success, the tree is likewise left untouched and the record marked
///   `Failed`; the returned error names the situation so the user can find
///   their work in `git stash list`. Restoring here would `reset --hard`
///   with `stash_sha == None` and orphan the work inside the stash.
///
/// The plan is re-checked against the repository before anything happens: a
/// confirmation dialog may have been open while the user edited a clean tree
/// (which [`abort_failed_checkout`] would then `reset --hard` away) or
/// cleaned a dirty one (making `git stash push` a no-op and `stash@{0}` an
/// unrelated entry). On any mismatch this returns
/// [`CheckoutError::PlanStale`] without stashing, inserting a row or touching
/// the tree.
///
/// Same-PR retry: when the store still holds a `Started`/`Active`/`Failed`
/// record for the *same* PR (a retry after a failed attempt, or a
/// re-review of a PR whose checkout was never restored), that row is
/// marked `Superseded` and both its stash SHA and its `original` ref are
/// carried into the new record, so the repo lock, the user's stash and the
/// real pre-checkout state move to the new row instead of leaking. A
/// superseded row is neither a lock holder nor a recovery target. The
/// `original` is only taken over when this plan's `original` is
/// `Detached(pr.head_sha)` or `Detached(superseded.head_sha)`, i.e. the
/// retry starts from our own previous checkout; otherwise the plan's value
/// describes a genuine user state. A [`CheckoutPlan::skip_stash`] plan —
/// the same-PR re-review — runs no stash step, so the held stash carries
/// over even though the tree is dirty with the review's own output.
/// (A retry that *does* stash new work supersedes the old record but leaves
/// the old stash entry in `git stash list` unreferenced; nothing is dropped.)
pub fn execute_checkout(
    plan: &CheckoutPlan,
    pr: &PrTarget,
    store: &Store,
) -> Result<CheckoutRecord, CheckoutError> {
    // The dialog may have been open a while: verify the snapshot it showed
    // still describes the repository BEFORE anything is recorded or changed.
    check_plan_current(plan)?;
    let repo = plan.repo_path.clone();
    // A same-PR record still held means this is a retry: supersede it below.
    let superseded = store
        .checkout_holding(&plan.repo_path)
        .map_err(CheckoutError::Store)?
        .filter(|old| old.pr_number == pr.number);
    let stash_now = plan.dirty && !plan.skip_stash;
    let carried_stash = if stash_now {
        None
    } else {
        superseded.as_ref().and_then(|old| old.stash_sha.clone())
    };
    // A retry runs on top of our own previous checkout, so the plan says
    // `Detached(pr.head_sha)` — or, for a re-review of a moved head, the
    // superseded record's own head — neither is a user state worth
    // returning to. Keep the superseded row's original (e.g.
    // `Branch("main")`) so restore still returns the user to where they
    // really were (§7.3).
    let original = match (&superseded, &plan.original) {
        (Some(old), OriginalRef::Detached(sha)) if *sha == pr.head_sha || *sha == old.head_sha => {
            old.original.clone()
        }
        _ => plan.original.clone(),
    };
    let mut rec = CheckoutRecord {
        id: 0,
        repo_path: repo.clone(),
        repo: format!("{}/{}", pr.owner, pr.name),
        pr_number: pr.number,
        original,
        // A carried-over stash from a superseded same-PR attempt is part of
        // the new row from the moment it exists, so the lock and the stash
        // move together.
        stash_sha: carried_stash,
        head_sha: pr.head_sha.clone(),
        state: CheckoutState::Started,
    };
    // Record BEFORE anything is stashed so a crash can be undone (§7.3).
    rec.id = store.checkout_begin(&rec).map_err(CheckoutError::Store)?;
    // The tree is clean once we stashed (or it was clean), so at an abort
    // any change is one the user made during the network-bound fetch. Only
    // a re-review (`skip_stash`) legitimately holds the review's own output,
    // which an abort may throw away as it always did.
    let discard_ok = plan.skip_stash;
    if let Some(old) = &superseded {
        // Move the lock over only after the new row exists; if the
        // supersede write fails, back out the new row and report.
        if let Err(e) = store
            .checkout_set_state(old.id, CheckoutState::Superseded)
            .map_err(CheckoutError::Store)
        {
            let _ = store.checkout_set_state(rec.id, CheckoutState::Failed);
            return Err(e);
        }
    }

    if stash_now {
        let msg = stash_message(pr.number);
        if let Err(e) = git(
            repo.as_path(),
            &["stash", "push", "--include-untracked", "-m", &msg],
        ) {
            // The push failed, so no stash protects the user's work: a
            // restore here would `reset --hard` + `clean -fd` with
            // `stash_sha == None` and destroy it. Leave the tree exactly as
            // it is. Nothing was changed, so there is nothing to recover:
            // close the record (best-effort) rather than leave a `Failed`
            // row whose recovery would discard the user's own work (§7.3).
            let _ = store.checkout_set_state(rec.id, CheckoutState::Restored);
            return Err(e);
        }
        // Track the stash by commit SHA, never `stash@{0}` (§7.1 step 4).
        let sha = match git(repo.as_path(), &["rev-parse", "stash@{0}"]) {
            Ok(sha) => sha.trim().to_string(),
            Err(e) => {
                // The push succeeded but we cannot name the stash: do NOT
                // restore (that would reset --hard with no stash SHA and
                // orphan the work). Mark Failed and describe the situation.
                let _ = store.checkout_set_state(rec.id, CheckoutState::Failed);
                return Err(CheckoutError::Git {
                    args: format!("rev-parse stash@{{0}} [{}]", stash_message(pr.number)),
                    stderr: format!(
                        "the stash was created but its SHA could not be resolved; \
                         the working tree was left untouched and the checkout \
                         record marked failed - recover the changes from \
                         `git stash list` ({e})"
                    ),
                });
            }
        };
        if let Err(e) = store
            .checkout_set_stash(rec.id, &sha)
            .map_err(CheckoutError::Store)
        {
            return Err(abort_failed_checkout(&rec, store, discard_ok, e));
        }
        rec.stash_sha = Some(sha);
    }

    let fetch_ref = format!("pull/{}/head", pr.number);
    if let Err(e) = git(repo.as_path(), &["fetch", &plan.remote, &fetch_ref]) {
        return Err(abort_failed_checkout(&rec, store, discard_ok, e));
    }
    if let Err(e) = git(repo.as_path(), &["checkout", "--detach", &pr.head_sha]) {
        return Err(abort_failed_checkout(&rec, store, discard_ok, e));
    }

    // §7.1 step 7: verify HEAD, aborting (with restore) on mismatch.
    let head = match git(repo.as_path(), &["rev-parse", "HEAD"]) {
        Ok(head) => head.trim().to_string(),
        Err(e) => return Err(abort_failed_checkout(&rec, store, discard_ok, e)),
    };
    if head != pr.head_sha {
        let mismatch = CheckoutError::HeadMismatch {
            expected: pr.head_sha.clone(),
            actual: head,
        };
        return Err(abort_failed_checkout(&rec, store, discard_ok, mismatch));
    }

    store
        .checkout_set_state(rec.id, CheckoutState::Active)
        .map_err(CheckoutError::Store)?;
    rec.state = CheckoutState::Active;
    Ok(rec)
}

/// Undo a failed checkout attempt and return the error to report. A restore
/// that needs no discard is always safe; `discard_ok` (a re-review whose
/// tree holds only the review's own output) allows throwing changes away.
/// Otherwise changes found now were made by the user while we were fetching:
/// they are left untouched, the record is marked `Failed` and the error
/// becomes [`CheckoutError::LeftChanges`]. If the restore fails for another
/// reason the record is likewise marked `Failed` for crash recovery (§7.3).
fn abort_failed_checkout(
    rec: &CheckoutRecord,
    store: &Store,
    discard_ok: bool,
    cause: CheckoutError,
) -> CheckoutError {
    match restore(rec, discard_ok, store) {
        Ok(_) => cause,
        Err(CheckoutError::NeedsDecision(changes)) => {
            let _ = store.checkout_set_state(rec.id, CheckoutState::Failed);
            CheckoutError::LeftChanges {
                cause: cause.to_string(),
                changes,
            }
        }
        Err(_) => {
            let _ = store.checkout_set_state(rec.id, CheckoutState::Failed);
            cause
        }
    }
}

/// `git status --porcelain` lines for the checked-out repo of `rec`
/// (spec §7.3 step 1).
pub fn restore_preview(rec: &CheckoutRecord) -> Result<Vec<String>, CheckoutError> {
    let out = git(&rec.repo_path, &["status", "--porcelain"])?;
    Ok(out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect())
}

/// Result of a restore attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The original ref and (if any) the stashed changes were restored.
    Restored,
    /// `git stash apply --index` conflicted; the stash is still in place and
    /// the record is marked `Failed`.
    StashConflict {
        /// SHA of the stash entry holding the user's changes.
        stash_sha: String,
    },
}

/// Drop our stash entry by locating our SHA's index in `git stash list`.
/// Never touches other entries (e.g. one the user pushed mid-review).
fn drop_our_stash(repo: &Path, sha: &str) -> Result<(), CheckoutError> {
    let list = git(repo, &["stash", "list", "--format=%H"])?;
    if let Some(i) = list.lines().position(|l| l.trim() == sha) {
        git(repo, &["stash", "drop", &format!("stash@{{{i}}}")])?;
    }
    Ok(())
}

/// Return the repository to exactly the state [`execute_checkout`] found
/// (spec §7.3). See [`RestoreOutcome`] and [`CheckoutError::NeedsDecision`].
pub fn restore(
    rec: &CheckoutRecord,
    discard: bool,
    store: &Store,
) -> Result<RestoreOutcome, CheckoutError> {
    let repo = rec.repo_path.as_path();

    // The caller's copy may be stale (a double-clicked dialog, a second
    // recovery of the same row): a finished row must never reset the tree
    // or re-apply its stash again.
    if let Some(current) = store.checkout_get(rec.id).map_err(CheckoutError::Store)?
        && matches!(
            current.state,
            CheckoutState::Restored | CheckoutState::Superseded
        )
    {
        return Err(CheckoutError::AlreadyFinished);
    }

    // §7.3 step 1: changes made during the review need a decision.
    let preview = restore_preview(rec)?;
    if !preview.is_empty() && !discard {
        return Err(CheckoutError::NeedsDecision(preview));
    }

    let result = (|| -> Result<RestoreOutcome, CheckoutError> {
        if discard {
            git(repo, &["reset", "--hard"])?;
            // Never `-x`: keep ignored build artifacts, they are not the
            // user's work and may be expensive to regenerate.
            git(repo, &["clean", "-fd"])?;
        }
        // Back to the exact original ref; branch names may contain slashes.
        match &rec.original {
            OriginalRef::Branch(name) => {
                // `switch` has no file/branch ambiguity and `--end-of-options`
                // keeps an odd branch name from being read as a flag.
                git(repo, &["switch", "--end-of-options", name])?;
            }
            OriginalRef::Detached(sha) => {
                git(repo, &["checkout", "--detach", sha])?;
            }
        }
        if let Some(sha) = &rec.stash_sha {
            // Apply by SHA (not `stash@{0}`, which may have shifted) and
            // drop only our own entry afterwards. Never force anything.
            if git(repo, &["stash", "apply", "--index", sha]).is_err() {
                return Ok(RestoreOutcome::StashConflict {
                    stash_sha: sha.clone(),
                });
            }
            // The stash is applied, so the work is back: a failed drop only
            // leaves a stray entry. Failing here would mark the row `Failed`
            // and a retry would apply the stash a second time.
            if let Err(error) = drop_our_stash(repo, sha) {
                tracing::warn!(%error, stash = %sha, "could not drop the applied stash");
            }
        }
        Ok(RestoreOutcome::Restored)
    })();

    match result {
        Ok(outcome @ RestoreOutcome::Restored) => {
            store
                .checkout_set_state(rec.id, CheckoutState::Restored)
                .map_err(CheckoutError::Store)?;
            Ok(outcome)
        }
        Ok(conflict @ RestoreOutcome::StashConflict { .. }) => {
            // The stash stays put; the user decides what to do (§7.3).
            store
                .checkout_set_state(rec.id, CheckoutState::Failed)
                .map_err(CheckoutError::Store)?;
            Ok(conflict)
        }
        Err(e) => {
            let _ = store.checkout_set_state(rec.id, CheckoutState::Failed);
            Err(e)
        }
    }
}

/// One prepare command's result (spec §8.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareOutput {
    /// The command that was run.
    pub command: String,
    /// Its exit status (−1 if the shell could not run or was signalled).
    pub status: i32,
    /// Captured stdout followed by stderr.
    pub output: String,
}

/// Run the repo's prepare commands with `sh -c` in `repo` under the user's
/// normal environment, in order; the first failure is returned as `Err`.
pub fn run_prepare(repo: &Path, commands: &[String]) -> Result<Vec<PrepareOutput>, PrepareOutput> {
    let mut outputs = Vec::with_capacity(commands.len());
    for command in commands {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(repo)
            .output()
            .map_err(|e| PrepareOutput {
                command: command.clone(),
                status: -1,
                output: e.to_string(),
            })?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let output = PrepareOutput {
            command: command.clone(),
            status: out.status.code().unwrap_or(-1),
            output: text,
        };
        if output.status != 0 {
            return Err(output);
        }
        outputs.push(output);
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// Run git with a hermetic identity, panicking on failure; returns
    /// trimmed stdout.
    fn gitx(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .arg("-c")
            .arg("user.name=t")
            .arg("-c")
            .arg("user.email=t@t")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgSign=false")
            .args(args)
            .output()
            .expect("git on PATH");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write");
    }

    /// A bare `origin.git`, a clone on `main` with one commit, and a commit
    /// on `feature/x` pushed as `refs/pull/7/head`. The clone's origin URL
    /// is rewritten to the canonical GitHub https URL *after* the push, with
    /// an `insteadOf` back to the bare repo so fetches still land locally.
    fn fixture() -> (TempDir, PathBuf /*clone*/, PrTarget) {
        let dir = tempdir();
        gitx(dir.path(), &["init", "--bare", "-b", "main", "origin.git"]);
        let clone = dir.path().join("clone");
        gitx(
            dir.path(),
            &[
                "clone",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "clone.defaultRemoteName=origin",
                "origin.git",
                "clone",
            ],
        );
        // Persisted identity so the app-run `git stash` can commit, plus
        // repo-local overrides so the user's global git config cannot change
        // what these tests observe: a global core.excludesFile, a shared
        // core.hooksPath, or `status.showUntrackedFiles=no` hiding untracked
        // files would all perturb the dirty/preview logic.
        gitx(&clone, &["config", "user.name", "t"]);
        gitx(&clone, &["config", "user.email", "t@t"]);
        gitx(&clone, &["config", "commit.gpgsign", "false"]);
        gitx(&clone, &["config", "status.showUntrackedFiles", "all"]);
        let no_gitignore = dir.path().join("no-gitignore");
        gitx(
            &clone,
            &[
                "config",
                "core.excludesFile",
                &no_gitignore.display().to_string(),
            ],
        );
        gitx(
            &clone,
            &[
                "config",
                "core.hooksPath",
                &dir.path().join("hooks").display().to_string(),
            ],
        );
        write_file(&clone.join("a.txt"), "a\n");
        gitx(&clone, &["add", "a.txt"]);
        gitx(&clone, &["commit", "-m", "initial"]);
        gitx(&clone, &["push", "origin", "main"]);
        gitx(&clone, &["checkout", "-b", "feature/x"]);
        write_file(&clone.join("b.txt"), "b\n");
        gitx(&clone, &["add", "b.txt"]);
        gitx(&clone, &["commit", "-m", "feature"]);
        gitx(&clone, &["push", "origin", "feature/x:refs/pull/7/head"]);
        gitx(&clone, &["checkout", "main"]);
        let head_sha = gitx(&clone, &["rev-parse", "feature/x"]);
        gitx(
            &clone,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/app.git",
            ],
        );
        // Fetches of the canonical GitHub URL must still hit the bare repo.
        let bare = dir.path().join("origin.git");
        let key = format!("url.{}.insteadOf", bare.display());
        gitx(
            &clone,
            &["config", key.as_str(), "https://github.com/acme/app.git"],
        );
        let pr = PrTarget {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            name: "app".to_string(),
            number: 7,
            head_sha,
            head_branch: "feature/x".to_string(),
        };
        (dir, clone, pr)
    }

    #[test]
    fn find_remote_matches_https_ssh_and_scp_forms() {
        let (_dir, clone, _pr) = fixture();
        // `git remote -v` shows the insteadOf-rewritten URL, but the raw
        // config still holds the GitHub URL, and that is what must match.
        let shown = gitx(&clone, &["remote", "-v"]);
        assert!(
            shown.contains("origin.git"),
            "insteadOf should rewrite `git remote -v`: {shown}"
        );
        assert_eq!(
            find_remote(&clone, "github.com", "acme", "app").unwrap(),
            "origin"
        );

        gitx(&clone, &["remote", "remove", "origin"]);
        for url in [
            "git@github.com:acme/app.git",            // scp form
            "git@github.com:acme/app",                // scp form, no .git
            "ssh://git@github.com/acme/app.git",      // ssh scheme
            "ssh://git@github.com:7999/acme/app.git", // ssh scheme with port
            "https://GitHub.com/Acme/App.GIT",        // case-insensitive
            "https://github.com/acme/app/",           // trailing slash
            "https://github.com/acme/app.git/",       // .git and slash
        ] {
            gitx(&clone, &["remote", "add", "up", url]);
            assert_eq!(
                find_remote(&clone, "github.com", "acme", "app").unwrap(),
                "up",
                "should match {url}"
            );
            gitx(&clone, &["remote", "remove", "up"]);
        }

        // A fork of the same-named repo under a different owner is no match.
        gitx(
            &clone,
            &["remote", "add", "fork", "https://github.com/other/app.git"],
        );
        assert!(
            matches!(
                find_remote(&clone, "github.com", "acme", "app"),
                Err(CheckoutError::NoMatchingRemote { .. })
            ),
            "a fork URL must not match"
        );
    }

    /// A fresh git repository with no remotes, plus its temp dir.
    fn empty_repo() -> (TempDir, PathBuf) {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create the repo dir");
        gitx(&repo, &["init", "-b", "main"]);
        (dir, repo)
    }

    /// The https, ssh, scp and `.git`/no-`.git` forms all name the same
    /// repository (spec §4 step 2).
    #[test]
    fn origin_https_ssh_scp_and_dot_git_forms_parse() {
        let (_dir, clone) = empty_repo();
        for url in [
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
        ] {
            gitx(&clone, &["config", "remote.origin.url", url]);
            let remote = read_origin_repo(&clone).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(
                remote,
                RemoteRepo {
                    host: "github.com".to_string(),
                    owner: "acme".to_string(),
                    name: "widgets".to_string(),
                },
                "should parse {url}"
            );
            assert_eq!(remote.key(), "github.com/acme/widgets", "{url}");
        }
    }

    /// Without an `origin`, the only remote is the answer.
    #[test]
    fn no_origin_single_remote_is_used() {
        let (_dir, clone) = empty_repo();
        gitx(
            &clone,
            &[
                "config",
                "remote.upstream.url",
                "git@gitlab.example.com:Team/App.git",
            ],
        );
        assert_eq!(
            read_origin_repo(&clone).unwrap().key(),
            "gitlab.example.com/team/app"
        );
    }

    /// Without an `origin`, several remotes leave the repository unknown and
    /// the error names them.
    #[test]
    fn no_origin_two_remotes_is_ambiguous() {
        let (_dir, clone) = empty_repo();
        gitx(
            &clone,
            &[
                "config",
                "remote.upstream.url",
                "https://github.com/acme/app.git",
            ],
        );
        gitx(
            &clone,
            &["config", "remote.fork.url", "https://github.com/me/app.git"],
        );
        match read_origin_repo(&clone) {
            Err(CheckoutError::AmbiguousRemote(names)) => {
                assert_eq!(names, vec!["fork".to_string(), "upstream".to_string()])
            }
            other => panic!("expected AmbiguousRemote, got {other:?}"),
        }
    }

    /// A repository with no remotes at all has no repository to name.
    #[test]
    fn no_remote_errors() {
        let (_dir, clone) = empty_repo();
        assert!(matches!(
            read_origin_repo(&clone),
            Err(CheckoutError::NoRemote)
        ));
    }

    /// A remote that is not a repository URL keeps its raw URL in the error.
    #[test]
    fn garbage_url_is_unparseable() {
        let (_dir, clone) = empty_repo();
        gitx(
            &clone,
            &["config", "remote.origin.url", "./some/local/path"],
        );
        match read_origin_repo(&clone) {
            Err(CheckoutError::UnparseableRemote(url)) => assert_eq!(url, "./some/local/path"),
            other => panic!("expected UnparseableRemote, got {other:?}"),
        }
    }

    #[test]
    fn clean_tree_checkout_and_restore_round_trip() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();

        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(!plan.dirty);
        assert_eq!(plan.remote, "origin");
        assert_eq!(plan.original, OriginalRef::Branch("main".to_string()));
        assert_eq!(
            plan.commands,
            vec![
                "git fetch origin pull/7/head".to_string(),
                format!("git checkout --detach {}", pr.head_sha),
            ]
        );

        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        assert_eq!(rec.state, CheckoutState::Active);
        assert_eq!(rec.pr_number, 7);
        assert_eq!(rec.stash_sha, None);
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), pr.head_sha);
        assert!(store.checkout_holding(&rec.repo_path).unwrap().is_some());

        let outcome = restore(&rec, false, &store).unwrap();
        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(
            gitx(&clone, &["rev-parse", "HEAD"]),
            gitx(&clone, &["rev-parse", "main"])
        );
        assert!(store.checkout_holding(&rec.repo_path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    #[test]
    fn dirty_tree_is_stashed_and_restored_with_untracked() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        write_file(&clone.join("a.txt"), "my work\n");
        write_file(&clone.join("scratch.txt"), "untracked\n");

        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(plan.dirty);
        assert_eq!(
            plan.commands,
            vec![
                "git stash push --include-untracked -m \"gh-review AI #7\"".to_string(),
                "git fetch origin pull/7/head".to_string(),
                format!("git checkout --detach {}", pr.head_sha),
            ]
        );

        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        let stash_sha = rec.stash_sha.clone().expect("stash recorded");
        assert!(stash_sha.len() >= 40, "stash recorded by SHA: {stash_sha}");
        // Both the tracked modification and the untracked file are gone.
        assert!(gitx(&clone, &["status", "--porcelain"]).is_empty());
        assert!(!clone.join("scratch.txt").exists());
        assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "a\n");

        let outcome = restore(&rec, false, &store).unwrap();
        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "my work\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("scratch.txt")).unwrap(),
            "untracked\n"
        );
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        // Our stash entry was dropped; nothing is left behind.
        assert!(gitx(&clone, &["stash", "list"]).is_empty());
        assert!(store.checkout_holding(&rec.repo_path).unwrap().is_none());
    }

    #[test]
    fn restore_applies_our_stash_even_if_another_was_pushed() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        assert!(rec.stash_sha.is_some());

        // Mid-review the user stashes something of their own, pushing our
        // entry off `stash@{0}`.
        write_file(&clone.join("a.txt"), "theirs\n");
        gitx(&clone, &["stash", "push", "-m", "theirs"]);
        let list = gitx(&clone, &["stash", "list", "--format=%s"]);
        // Ours is now stash@{1} (git prefixes `-m` with the branch name).
        assert_eq!(list, "On (no branch): theirs\nOn main: gh-review AI #7");

        let outcome = restore(&rec, false, &store).unwrap();
        assert_eq!(outcome, RestoreOutcome::Restored);
        // Our changes came back (not the user's newer stash at stash@{0})…
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "mine\n"
        );
        // …and only our entry was dropped; theirs is still in the list.
        assert_eq!(
            gitx(&clone, &["stash", "list", "--format=%s"]),
            "On (no branch): theirs"
        );
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
    }

    #[test]
    fn restores_branch_with_slashes_and_detached_original() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();

        // A branch name containing slashes.
        gitx(&clone, &["checkout", "-b", "team/feature"]);
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert_eq!(
            plan.original,
            OriginalRef::Branch("team/feature".to_string())
        );
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), pr.head_sha);
        assert_eq!(
            restore(&rec, false, &store).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(
            gitx(&clone, &["symbolic-ref", "--short", "HEAD"]),
            "team/feature"
        );

        // A detached HEAD at a specific SHA.
        let base = gitx(&clone, &["rev-parse", "HEAD"]);
        gitx(&clone, &["checkout", "--detach", &base]);
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert_eq!(plan.original, OriginalRef::Detached(base.clone()));
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), pr.head_sha);
        assert_eq!(
            restore(&rec, false, &store).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), base);
        // Still detached: `--abbrev-ref` reports literal "HEAD".
        assert_eq!(gitx(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");
    }

    #[test]
    fn changes_made_during_review_need_a_decision() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();

        // The build/agent produced a new untracked file.
        write_file(&clone.join("during.txt"), "built\n");

        let err = restore(&rec, false, &store).unwrap_err();
        match &err {
            CheckoutError::NeedsDecision(lines) => {
                assert!(
                    lines.iter().any(|l| l.contains("during.txt")),
                    "preview should list the new file: {lines:?}"
                );
            }
            other => panic!("expected NeedsDecision, got {other:?}"),
        }
        // Nothing moved: still on the PR head, still holding.
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), pr.head_sha);
        assert!(store.checkout_holding(&rec.repo_path).unwrap().is_some());

        // Discarding puts the tree back exactly and removes the new file.
        let outcome = restore(&rec, true, &store).unwrap();
        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(!clone.join("during.txt").exists());
        assert!(store.checkout_holding(&rec.repo_path).unwrap().is_none());
    }

    #[test]
    fn stash_conflict_leaves_the_stash_and_marks_failed() {
        let dir = tempdir();
        let db = dir.path().join("ai.sqlite");
        let store = Store::open(&db).unwrap();
        let (_origin_dir, clone, pr) = fixture();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        let stash_sha = rec.stash_sha.clone().unwrap();

        // Mid-review a conflicting change to the stashed file lands on main.
        gitx(&clone, &["checkout", "main"]);
        write_file(&clone.join("a.txt"), "theirs\n");
        gitx(&clone, &["commit", "-am", "theirs"]);

        let outcome = restore(&rec, false, &store).unwrap();
        assert_eq!(
            outcome,
            RestoreOutcome::StashConflict {
                stash_sha: stash_sha.clone()
            }
        );
        // The stash is left in place so the user's work is not lost.
        assert!(gitx(&clone, &["stash", "list", "--format=%H"]).contains(&stash_sha));
        // The record is marked Failed (not Restored).
        let conn = rusqlite::Connection::open(&db).unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM ai_checkout WHERE id = ?1",
                [rec.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "failed");
        // A Failed row still holds the tree and is a recovery target.
        let held = store
            .checkout_holding(&rec.repo_path)
            .unwrap()
            .expect("a failed row still holds");
        assert_eq!(held.state, CheckoutState::Failed);
        let unrestored = store.checkouts_unrestored().unwrap();
        assert_eq!(unrestored.len(), 1);
        assert_eq!(unrestored[0].id, rec.id);
    }

    #[test]
    fn second_pr_in_same_repo_is_held() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        execute_checkout(&plan, &pr, &store).unwrap();

        let other = PrTarget {
            number: 8,
            ..pr.clone()
        };
        let err = plan_checkout(&clone, &other, &store).unwrap_err();
        assert!(
            matches!(
                err,
                CheckoutError::Held {
                    pr_number: 7,
                    record_id: 1
                }
            ),
            "expected Held {{ pr_number: 7, record_id: 1 }}, got {err:?}"
        );

        // The same PR may be re-planned (retry after a failed attempt).
        assert!(plan_checkout(&clone, &pr, &store).is_ok());
    }

    #[test]
    fn stash_push_failure_leaves_the_tree_untouched_and_marks_failed() {
        let dir = tempdir();
        let db = dir.path().join("ai.sqlite");
        let store = Store::open(&db).unwrap();
        let (_git_dir, clone, pr) = fixture();
        write_file(&clone.join("a.txt"), "precious\n");
        write_file(&clone.join("scratch.txt"), "keep me\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(plan.dirty);

        // Another process is updating the stash ref: `refs/stash.lock`
        // makes `git stash push` fail ("Cannot save the current status")
        // while `git reset --hard` + `git clean -fd` still work — the exact
        // destructive path this test forbids. (An `index.lock` would also
        // block the reset, hiding the bug instead of exposing it.)
        let lock = clone.join(".git").join("refs").join("stash.lock");
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::File::create(&lock).unwrap();
        let err = execute_checkout(&plan, &pr, &store).expect_err("stash push must fail");
        match &err {
            CheckoutError::Git { args, .. } => assert!(
                args.starts_with("stash push"),
                "the failure must come from the stash push: {err:?}"
            ),
            other => panic!("expected a git error from stash push, got {other:?}"),
        }
        std::fs::remove_file(&lock).unwrap();

        // The tree is untouched: the work the stash was meant to protect
        // is still there, uncommitted, and we are still on main.
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "precious\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("scratch.txt")).unwrap(),
            "keep me\n"
        );
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(
            gitx(&clone, &["stash", "list"]).is_empty(),
            "no stash was created"
        );

        // Nothing was changed, so the record is closed: a recoverable
        // `Failed` row with no stash would offer to discard the user's own
        // work, and the repo is not held by anything.
        let conn = rusqlite::Connection::open(&db).unwrap();
        let states: Vec<String> = conn
            .prepare("SELECT state FROM ai_checkout")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(states, vec!["restored".to_string()]);
        assert!(store.checkout_holding(&rec_path(&clone)).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    /// Bug (a): the user edits a clean tree while `git fetch` is running and
    /// the fetch then fails; the abort must not `reset --hard` those edits.
    #[test]
    fn edits_made_during_a_failing_fetch_survive_the_abort() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let db = dir.path().join("ai.sqlite");
        let store = Store::open(&db).unwrap();
        let (_origin_dir, clone, pr) = fixture();
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(!plan.dirty);

        // A local upload-pack that edits the work tree (the user typing
        // while the network call runs) and then fails the fetch.
        let script = dir.path().join("upload-pack.sh");
        write_file(
            &script,
            &format!(
                "#!/bin/sh\necho 'typed mid-fetch' > '{}'\necho 'new' > '{}'\nexit 1\n",
                clone.join("a.txt").display(),
                clone.join("new.txt").display()
            ),
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        gitx(
            &clone,
            &[
                "config",
                "remote.origin.uploadpack",
                &script.display().to_string(),
            ],
        );

        let err = execute_checkout(&plan, &pr, &store).expect_err("the fetch must fail");
        match &err {
            CheckoutError::LeftChanges { changes, .. } => assert_eq!(changes.len(), 2, "{err:?}"),
            other => panic!("expected LeftChanges, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "typed mid-fetch\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("new.txt")).unwrap(),
            "new\n"
        );
        // Still a recovery target, so the lock is not silently dropped.
        let held = store.checkout_holding(&rec_path(&clone)).unwrap().unwrap();
        assert_eq!(held.state, CheckoutState::Failed);
    }

    /// Bug (b): a stale copy of a finished record must not reset the tree.
    #[test]
    fn restoring_a_finished_record_again_changes_nothing() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        restore(&rec, false, &store).unwrap();

        write_file(&clone.join("a.txt"), "typed after the restore\n");
        write_file(&clone.join("mine.txt"), "mine\n");
        // `rec` is still the stale `Active` copy a double-clicked dialog has.
        let err = restore(&rec, true, &store).expect_err("a restored row is final");
        assert!(matches!(err, CheckoutError::AlreadyFinished), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "typed after the restore\n"
        );
        assert!(clone.join("mine.txt").exists());

        // A superseded row is final too.
        let id = store.checkout_begin(&rec).unwrap();
        store
            .checkout_set_state(id, CheckoutState::Superseded)
            .unwrap();
        let stale = CheckoutRecord { id, ..rec };
        assert!(matches!(
            restore(&stale, true, &store),
            Err(CheckoutError::AlreadyFinished)
        ));
        assert!(clone.join("mine.txt").exists());
    }

    /// Bug (c): a failed `stash drop` after a successful apply is a warning,
    /// not a failure that would make a retry apply the stash twice.
    #[test]
    fn failed_stash_drop_still_marks_the_record_restored() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();

        // `stash apply` does not touch refs/stash; `stash drop` must.
        let lock = clone.join(".git").join("refs").join("stash.lock");
        std::fs::File::create(&lock).unwrap();
        let outcome = restore(&rec, false, &store).expect("a failed drop is only a warning");
        std::fs::remove_file(&lock).unwrap();

        assert_eq!(outcome, RestoreOutcome::Restored);
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(store.checkouts_unrestored().unwrap().is_empty());
        assert!(matches!(
            restore(&rec, false, &store),
            Err(CheckoutError::AlreadyFinished)
        ));
    }

    /// A branch named like a flag must not be parsed as one on restore.
    #[test]
    fn restore_switches_to_a_branch_that_looks_like_a_path() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        // `a.txt` is both a file and (now) a branch: plain `git checkout
        // a.txt` would restore the file instead of switching.
        gitx(&clone, &["branch", "a.txt"]);
        gitx(&clone, &["checkout", "a.txt"]);
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let rec = execute_checkout(&plan, &pr, &store).unwrap();
        restore(&rec, false, &store).unwrap();
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "a.txt");
    }

    #[test]
    fn a_clean_plan_with_a_new_untracked_file_is_stale_and_records_nothing() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(!plan.dirty);

        // The user edits the clean tree while the confirmation dialog is
        // open. `abort_failed_checkout` would `reset --hard` these away.
        write_file(&clone.join("stray.txt"), "typed during the dialog\n");
        let err = execute_checkout(&plan, &pr, &store).expect_err("plan is stale");
        assert!(
            matches!(err, CheckoutError::PlanStale),
            "expected PlanStale, got {err:?}"
        );

        // Nothing was recorded, stashed or touched.
        assert!(store.checkout_holding(&plan.repo_path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(clone.join("stray.txt")).unwrap(),
            "typed during the dialog\n"
        );
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(gitx(&clone, &["stash", "list"]).is_empty());
    }

    #[test]
    fn a_dirty_plan_cleaned_before_execution_is_stale_and_records_nothing() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(plan.dirty);

        // The user reverts their edits while the dialog is open. Proceeding
        // would make `git stash push` a no-op and `stash@{0}` unrelated.
        gitx(&clone, &["checkout", "--", "a.txt"]);
        let err = execute_checkout(&plan, &pr, &store).expect_err("plan is stale");
        assert!(
            matches!(err, CheckoutError::PlanStale),
            "expected PlanStale, got {err:?}"
        );

        assert!(store.checkout_holding(&plan.repo_path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
        assert!(
            gitx(&clone, &["stash", "list"]).is_empty(),
            "a stale plan must not stash"
        );
        assert_eq!(std::fs::read_to_string(clone.join("a.txt")).unwrap(), "a\n");
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
    }

    #[test]
    fn same_pr_retry_supersedes_the_old_record_and_carries_the_stash() {
        let dir = tempdir();
        let db = dir.path().join("ai.sqlite");
        let store = Store::open(&db).unwrap();
        let (_git_dir, clone, pr) = fixture();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let first = execute_checkout(&plan, &pr, &store).unwrap();
        let stash_sha = first.stash_sha.clone().expect("first attempt stashed");

        // Retry the same PR: the tree is clean now, the work lives in the
        // first attempt's stash.
        let plan2 = plan_checkout(&clone, &pr, &store).unwrap();
        assert!(!plan2.dirty);
        let second = execute_checkout(&plan2, &pr, &store).unwrap();
        assert_eq!(second.stash_sha.as_deref(), Some(stash_sha.as_str()));
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), pr.head_sha);

        // The old row was superseded; the new row holds the lock and is the
        // only recovery target, since it inherited the stash.
        let conn = rusqlite::Connection::open(&db).unwrap();
        let old_state: String = conn
            .query_row(
                "SELECT state FROM ai_checkout WHERE id = ?1",
                [first.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_state, "superseded");
        let held = store
            .checkout_holding(&second.repo_path)
            .unwrap()
            .expect("new row holds");
        assert_eq!(held.id, second.id);
        let unrestored = store.checkouts_unrestored().unwrap();
        assert_eq!(unrestored.len(), 1, "only the replacement is recoverable");
        assert_eq!(unrestored[0].id, second.id);

        // Restoring the new record brings the user's work back, drops the
        // carried stash, and leaves nothing unrestored.
        assert_eq!(
            restore(&second, false, &store).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(gitx(&clone, &["stash", "list"]).is_empty());
        assert!(store.checkout_holding(&second.repo_path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    #[test]
    fn same_pr_retry_carries_the_original_ref_and_restores_the_branch() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        assert_eq!(plan.original, OriginalRef::Branch("main".to_string()));
        let first = execute_checkout(&plan, &pr, &store).unwrap();

        // A second start while the first checkout is still Active: the tree
        // is already detached at the PR head, so the new plan's "original"
        // is our own checkout, not the user's state.
        let plan2 = plan_checkout(&clone, &pr, &store).unwrap();
        assert_eq!(
            plan2.original,
            OriginalRef::Detached(pr.head_sha.clone()),
            "the retry starts on top of the previous checkout"
        );
        let second = execute_checkout(&plan2, &pr, &store).unwrap();
        // …so the record must still know where the user really came from.
        assert_eq!(second.original, OriginalRef::Branch("main".to_string()));
        assert_eq!(second.stash_sha, first.stash_sha);

        assert_eq!(
            restore(&second, false, &store).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(
            gitx(&clone, &["symbolic-ref", "--short", "HEAD"]),
            "main",
            "restore must return to the user's branch, not the PR head"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(gitx(&clone, &["stash", "list"]).is_empty());
        assert!(store.checkout_holding(&second.repo_path).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    /// A re-review while the same PR still holds the checkout: the plan
    /// skips the stash step (the tree's changes are the review's own
    /// output), the new head is fetched and checked out, and the held
    /// record's stash and original ref carry into the new row (Task 8).
    #[test]
    fn continue_while_held_skips_the_stash_and_carries_the_record() {
        let dir = tempdir();
        let db = dir.path().join("ai.sqlite");
        let store = Store::open(&db).unwrap();
        let (_git_dir, clone, pr) = fixture();
        write_file(&clone.join("a.txt"), "mine\n");
        let plan = plan_checkout(&clone, &pr, &store).unwrap();
        let first = execute_checkout(&plan, &pr, &store).unwrap();
        let stash_sha = first.stash_sha.clone().expect("first attempt stashed");

        // The review's build output lands in the tree, and the PR head
        // moves: a new commit reaches `refs/pull/7/head`.
        write_file(&clone.join("build.txt"), "output\n");
        write_file(&clone.join("ship.txt"), "ship\n");
        gitx(&clone, &["add", "ship.txt"]);
        gitx(&clone, &["commit", "-m", "second"]);
        gitx(&clone, &["push", "origin", "HEAD:refs/pull/7/head"]);
        let new_head = gitx(&clone, &["rev-parse", "HEAD"]);
        gitx(&clone, &["checkout", "--detach", &pr.head_sha]);
        let moved = PrTarget {
            head_sha: new_head.clone(),
            ..pr.clone()
        };

        // The continue plan: dirty, but no stash line.
        let plan2 = plan_checkout(&clone, &moved, &store).unwrap();
        assert!(plan2.dirty, "the build output is still in the tree");
        assert!(plan2.skip_stash, "the held checkout skips the stash step");
        assert_eq!(
            plan2.commands,
            vec![
                "git fetch origin pull/7/head".to_string(),
                format!("git checkout --detach {new_head}"),
            ]
        );

        // The continue checkout: the new head is checked out, no new stash
        // is created, and the original stash and ref ride along.
        let second = execute_checkout(&plan2, &moved, &store).unwrap();
        assert_eq!(second.stash_sha.as_deref(), Some(stash_sha.as_str()));
        assert_eq!(second.original, OriginalRef::Branch("main".to_string()));
        assert_eq!(gitx(&clone, &["rev-parse", "HEAD"]), new_head);
        assert_eq!(gitx(&clone, &["stash", "list", "--format=%H"]), stash_sha);

        // The restore still returns the user to main with their work back.
        assert_eq!(
            restore(&second, true, &store).unwrap(),
            RestoreOutcome::Restored
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "mine\n"
        );
        assert!(gitx(&clone, &["stash", "list"]).is_empty());
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
    }

    #[test]
    fn head_mismatch_restores_and_errors() {
        let (_dir, clone, pr) = fixture();
        let store = Store::open_in_memory().unwrap();

        // An annotated tag object's SHA: `git checkout --detach` accepts it,
        // but `HEAD` resolves to the tagged commit, not the tag object.
        gitx(&clone, &["tag", "-a", "v1", "-m", "t", &pr.head_sha]);
        let tag_sha = gitx(&clone, &["rev-parse", "v1"]);
        assert_ne!(tag_sha, pr.head_sha, "annotated tags have their own SHA");
        let bad = PrTarget {
            head_sha: tag_sha.clone(),
            ..pr.clone()
        };

        let plan = plan_checkout(&clone, &bad, &store).unwrap();
        let err = execute_checkout(&plan, &bad, &store).unwrap_err();
        match &err {
            CheckoutError::HeadMismatch { expected, actual } => {
                assert_eq!(expected, &tag_sha);
                assert_eq!(actual, &pr.head_sha);
            }
            other => panic!("expected HeadMismatch, got {other:?}"),
        }
        // The failed attempt restored everything.
        assert_eq!(gitx(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert!(store.checkout_holding(&rec_path(&clone)).unwrap().is_none());
        assert!(store.checkouts_unrestored().unwrap().is_empty());
    }

    #[test]
    fn prepare_runs_in_order_and_stops_at_first_failure() {
        let (_dir, clone, _pr) = fixture();

        let outputs = run_prepare(
            &clone,
            &[
                "echo a".to_string(),
                "echo b > prepared.txt".to_string(),
                "pwd".to_string(),
            ],
        )
        .expect("all succeed");
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].status, 0);
        assert_eq!(outputs[0].output.trim(), "a");
        assert!(clone.join("prepared.txt").exists(), "runs inside the repo");
        assert!(outputs[2].output.contains("clone"));

        let err = run_prepare(
            &clone,
            &[
                "echo a".to_string(),
                "false".to_string(),
                "echo c".to_string(),
            ],
        )
        .expect_err("first failure is returned");
        assert_eq!(err.command, "false");
        assert_eq!(err.status, 1);
        assert!(!err.output.contains('c'), "echo c must never run");
    }

    /// The canonical path [`plan_checkout`] stores records under.
    fn rec_path(p: &Path) -> PathBuf {
        p.canonicalize().expect("fixture path exists")
    }
}
