//! Per-PR session state: what is worth restoring when a PR is opened again
//! (spec §11).
//!
//! **Never put a token here.** The session file holds only local review state:
//! the head SHA the state was written against, the blob-keyed viewed marks, the
//! pending-review and composer drafts, the last file opened, and the collapsed
//! tree directories. Tokens live in the OS keychain ([`crate::auth::store`]).
//!
//! A file lives at `sessions/<host>/<owner>/<repo>/<number>.json`, keyed by host
//! so two GitHub hosts never collide.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::comment::Anchor;
use crate::model::review::PendingReview;
use crate::model::RepoRef;
use crate::state::viewed::ViewedSet;

/// How long a [`DebouncedWriter`] waits after the last change before flushing.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Everything persisted for one `(host, repo, PR)`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionFile {
    /// The head SHA the state was last written against.
    pub head_sha: Option<String>,
    /// The viewed marks, keyed by blob SHA.
    pub viewed: ViewedSet,
    /// The pending review draft: a serialized [`PendingReview`], or `None`
    /// when there is no draft.
    pub draft: Option<serde_json::Value>,
    /// The in-progress composer draft: a serialized [`ComposerDraft`].
    pub composer_draft: Option<serde_json::Value>,
    /// The last file opened, restored as the tree's current file.
    pub last_opened_file: Option<String>,
    /// The collapsed file-tree directories, by path.
    pub collapsed_dirs: BTreeSet<String>,
}

/// The path of a PR's session file.
pub fn session_path(root: &Path, host: &str, repo: &RepoRef, number: u64) -> PathBuf {
    root.join("sessions")
        .join(host)
        .join(repo.owner.to_lowercase())
        .join(repo.name.to_lowercase())
        .join(format!("{number}.json"))
}

/// Load a PR's session file, or `None` when it has never been saved.
pub fn load(
    root: &Path,
    host: &str,
    repo: &RepoRef,
    number: u64,
) -> anyhow::Result<Option<SessionFile>> {
    let path = session_path(root, host, repo, number);
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Write a PR's session file atomically.
pub fn save(
    root: &Path,
    host: &str,
    repo: &RepoRef,
    number: u64,
    session: &SessionFile,
) -> anyhow::Result<()> {
    let path = session_path(root, host, repo, number);
    let raw = serde_json::to_vec_pretty(session)?;
    super::write_atomic(&path, &raw)?;
    Ok(())
}

/// The in-progress composer draft: the anchor and text of a comment the
/// reviewer is writing but has not posted (spec §11).
///
/// The session file's `composer_draft` field is owned by the PR session's
/// whole-file [`write_session`][crate::state::pr_detail::PrSession::write_session]
/// path: the session snapshots its live composer into its in-memory
/// `composer_draft` every frame (`sync_composer_draft`) and the debounced
/// write persists it, with an empty composer stored as `None`. There is
/// deliberately **no** standalone `save_composer_draft` here — a partial
/// write of this field would race the whole-file write and clobber the
/// fresher value with a stale in-memory one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComposerDraft {
    /// The file the composer is anchored in.
    pub target_path: String,
    /// The line anchor, when the composer is open on a line.
    pub anchor: Option<Anchor>,
    /// The text typed so far.
    pub body: String,
}

/// Save the pending-review draft, **immediately and atomically**: a draft is
/// never left waiting on the debounce window, because a crash inside it must
/// not lose what the reviewer wrote (spec §11).
///
/// A read-modify-write of the whole session file, so the other fields
/// survive; an empty draft is stored as `None`.
pub fn save_draft(
    root: &Path,
    host: &str,
    repo: &RepoRef,
    number: u64,
    pending: &PendingReview,
) -> anyhow::Result<()> {
    let mut file = load(root, host, repo, number)?.unwrap_or_default();
    file.draft = if pending.is_empty() {
        None
    } else {
        Some(serde_json::to_value(pending)?)
    };
    save(root, host, repo, number, &file)
}

/// Load the pending-review draft, or `None` when no draft is saved.
pub fn load_draft(
    root: &Path,
    host: &str,
    repo: &RepoRef,
    number: u64,
) -> anyhow::Result<Option<PendingReview>> {
    let Some(draft) = load(root, host, repo, number)?.and_then(|file| file.draft) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(draft)?))
}

/// Coalesces rapid changes into one write, 250 ms after the **last** change.
///
/// Each call to [`DebouncedWriter::mark_dirty`] restarts the timer, so a burst
/// of toggles costs one write; [`DebouncedWriter::flushed`] clears it once the
/// caller has written.
#[derive(Debug, Default)]
pub struct DebouncedWriter {
    dirty_since: Option<Instant>,
}

impl DebouncedWriter {
    /// Note a change at `now`, restarting the debounce window.
    pub fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since = Some(now);
    }

    /// Whether `now` is at least [`DEBOUNCE`] past the last change.
    pub fn should_flush(&self, now: Instant) -> bool {
        self.dirty_since
            .is_some_and(|since| now.duration_since(since) >= DEBOUNCE)
    }

    /// Whether a change is waiting to be written.
    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Record that the pending change has been written.
    pub fn flushed(&mut self) {
        self.dirty_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::review::{PendingReview, ReviewEvent};
    use crate::model::RepoRef;
    use crate::state::viewed::ViewedSet;
    use crate::test_support::{file, line_anchor, repo, tempdir};
    use std::time::{Duration, Instant};

    #[test]
    fn session_round_trips_under_host_and_repo() {
        let dir = tempdir();
        let r = RepoRef::new("Octo", "Repo");
        let mut viewed = ViewedSet::default();
        viewed.set(&file("a.rs"), true);
        let s = SessionFile {
            head_sha: Some("sha2".into()),
            viewed,
            last_opened_file: Some("a.rs".into()),
            draft: Some(serde_json::json!({"body": "hi"})),
            ..Default::default()
        };
        save(dir.path(), "github.com", &r, 7, &s).unwrap();
        assert!(session_path(dir.path(), "github.com", &r, 7)
            .ends_with("sessions/github.com/octo/repo/7.json"));
        assert_eq!(load(dir.path(), "github.com", &r, 7).unwrap().unwrap(), s);
        assert!(load(dir.path(), "ghe.acme.io", &r, 7).unwrap().is_none()); // hosts never collide
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

    #[test]
    fn draft_survives_a_simulated_restart_and_writes_immediately() {
        let dir = tempdir();
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 42), "needs a test".into(), "sha1")
            .unwrap();
        p.set_event(ReviewEvent::RequestChanges);
        save_draft(dir.path(), "github.com", &repo(), 7, &p).unwrap();

        // written synchronously: the file already contains the comment
        let raw =
            std::fs::read_to_string(session_path(dir.path(), "github.com", &repo(), 7)).unwrap();
        assert!(raw.contains("needs a test"));

        let mut restored = load_draft(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert_eq!(restored, p);
        let n = restored
            .add(line_anchor("a.rs", 43), "2".into(), "sha1")
            .unwrap();
        assert_ne!(n, restored.comments[0].local_id); // ids stay unique after restart

        // other session fields survive a draft write
        let mut s = load(dir.path(), "github.com", &repo(), 7).unwrap().unwrap();
        s.last_opened_file = Some("a.rs".into());
        save(dir.path(), "github.com", &repo(), 7, &s).unwrap();
        save_draft(dir.path(), "github.com", &repo(), 7, &restored).unwrap();
        assert_eq!(
            load(dir.path(), "github.com", &repo(), 7)
                .unwrap()
                .unwrap()
                .last_opened_file
                .as_deref(),
            Some("a.rs")
        );
    }
}
