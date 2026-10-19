//! One open PR's session: the load orchestration and everything the detail
//! screen draws from.
//!
//! Loading is a small state machine driven entirely by [`Event`]s: metadata
//! first (so the head SHA is known before any file's patch is anchored to it),
//! then the files page by page. Events carry the [`SessionId`] they belong to,
//! so a reply for a PR the user has left is dropped rather than folded into the
//! session now on screen (the stale-event rule).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::diff::annotate::{annotate, place_thread, rows_for_anchor, InlineItem, Placement};
use crate::diff::row_model::{build, RowModel, RowRef};
use crate::diff::suggestion::suggestion_block;
use crate::github::comments::NewComment;
use crate::github::error::GhError;
use crate::github::files::LoadedFile;
use crate::github::metadata::PrMetadata;
use crate::model::comment::CommentTarget;
use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
use crate::model::RepoRef;
use crate::persist::session::{self, DebouncedWriter, SessionFile};
use crate::runtime::{Command, Event, SessionId, NO_OPTIMISTIC_BUBBLE};
use crate::state::composer::{self, can_submit, CommentMode, ComposerMsg, ComposerState};
use crate::state::filters::FilterState;
use crate::state::viewed::ViewedSet;

/// How far a session has got through loading its PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadPhase {
    /// Waiting for the metadata.
    Header,
    /// Metadata is in; the files are still arriving.
    Files,
    /// Everything the detail screen needs is loaded.
    Ready,
    /// Loading stopped on an error.
    Failed,
}

/// A comment held for the pending review (Phase F submits it): local only,
/// anchored where it was written, rendered as a `Pending` row (spec §7.2).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingComment {
    /// The session-allocated id, echoed by the row and never 0.
    pub local_id: u64,
    /// What the comment is attached to.
    pub target: CommentTarget,
    /// The comment body.
    pub body: String,
}

/// An immediate comment in flight: what it takes to replace it on success
/// or roll it back on failure.
#[derive(Clone, Debug)]
struct OptimisticPost {
    /// The id echoed by the result events.
    optimistic_id: u64,
    /// Where the placeholder thread was pushed in `threads`.
    thread_index: usize,
    /// The body, restored to the composer if the post fails.
    body: String,
    /// The anchor, restored to the composer if the post fails.
    target: CommentTarget,
}

/// One open PR, its loaded data, and its loading progress.
///
/// Later phases attach their own state here (`viewed`, `filters`, `threads`,
/// `pending`, `reviews`) rather than in the app.
pub struct PrSession {
    /// The id echoed by every PR-scoped event this session owns.
    pub id: SessionId,
    /// The GitHub host the PR lives on.
    pub host: String,
    /// The repository the PR lives in.
    pub repo: RepoRef,
    /// The PR number within the repository.
    pub number: u64,
    /// The PR's metadata, once loaded.
    pub metadata: Option<PrMetadata>,
    /// The head SHA the displayed diff came from (spec §7.3).
    pub diff_head_sha: Option<String>,
    /// The PR's files, in API order, with their patches parsed.
    pub files: Vec<LoadedFile>,
    /// The flat row list the diff view virtualizes.
    pub row_model: RowModel,
    /// How far loading has got.
    pub phase: LoadPhase,
    /// The failure that stopped loading, if any.
    pub error: Option<GhError>,
    /// Whether the head moved since the diff on screen was loaded.
    pub head_moved: bool,
    /// Which files the reviewer has marked viewed, keyed by blob SHA.
    pub viewed: ViewedSet,
    /// What the file tree is filtering the file list by.
    pub filters: FilterState,
    /// The PR's review threads, once loaded (spec §7.4).
    pub threads: Vec<ReviewThread>,
    /// The inline comment composer's state machine (spec §7.1).
    pub composer: ComposerState,
    /// Comments held for the pending review, in the order they were added.
    pub pending: Vec<PendingComment>,
    /// The next locally allocated comment id; starts at 1 because 0 is the
    /// reserved "no optimistic bubble" sentinel.
    next_optimistic_id: u64,
    /// The immediate comment currently in flight, if any.
    optimistic: Option<OptimisticPost>,
    /// The file-tree directories the reviewer has collapsed, by path.
    pub collapsed_dirs: BTreeSet<String>,
    /// The last file selected in the tree, restored from the session file.
    pub last_opened_file: Option<String>,
    /// The saved pending-review draft, loaded from the session file and
    /// written back untouched so a session write never clobbers it.
    /// Task 40 owns editing it.
    pub draft: Option<serde_json::Value>,
    /// The saved composer draft, loaded from the session file and written
    /// back untouched so a session write never clobbers it.
    /// Task 40 owns editing it.
    pub composer_draft: Option<serde_json::Value>,
    /// The config root the session file lives under.
    root: PathBuf,
    /// Coalesces viewed/collapse changes into debounced writes.
    writer: DebouncedWriter,
}

impl PrSession {
    /// Open a PR: build the session and the commands that start its load.
    ///
    /// The metadata and the review threads are requested here; the files are
    /// requested from [`PrSession::on_event`] when the metadata arrives, so
    /// the diff on screen and `diff_head_sha` are always the same commit.
    pub fn open(
        id: SessionId,
        host: String,
        repo: RepoRef,
        number: u64,
    ) -> (PrSession, Vec<Command>) {
        Self::open_at(&crate::persist::config_root(), id, host, repo, number)
    }

    /// Open a PR against a specific config root: the real one in
    /// [`PrSession::open`], a temporary one in tests.
    pub fn open_at(
        root: &Path,
        id: SessionId,
        host: String,
        repo: RepoRef,
        number: u64,
    ) -> (PrSession, Vec<Command>) {
        let (viewed, collapsed_dirs, last_opened_file, draft, composer_draft) =
            match session::load(root, &host, &repo, number) {
                Ok(Some(file)) => (
                    file.viewed,
                    file.collapsed_dirs,
                    file.last_opened_file,
                    file.draft,
                    file.composer_draft,
                ),
                Ok(None) => (ViewedSet::default(), BTreeSet::new(), None, None, None),
                Err(error) => {
                    tracing::warn!(%error, "could not read the PR session file");
                    (ViewedSet::default(), BTreeSet::new(), None, None, None)
                }
            };

        let session = PrSession {
            id,
            host,
            repo: repo.clone(),
            number,
            metadata: None,
            diff_head_sha: None,
            files: Vec::new(),
            row_model: build(&[]),
            phase: LoadPhase::Header,
            error: None,
            head_moved: false,
            viewed,
            filters: FilterState::default(),
            threads: Vec::new(),
            composer: ComposerState::default(),
            pending: Vec::new(),
            next_optimistic_id: 1,
            optimistic: None,
            collapsed_dirs,
            last_opened_file,
            draft,
            composer_draft,
            root: root.to_path_buf(),
            writer: DebouncedWriter::default(),
        };

        let commands = vec![
            Command::LoadPrMetadata {
                session: id,
                repo: repo.clone(),
                number,
            },
            // Threads are independent of the head SHA, so they load alongside
            // the metadata rather than waiting for the files.
            Command::LoadThreads {
                session: id,
                repo,
                number,
            },
        ];
        (session, commands)
    }

    /// Fold one worker event in and return the commands it triggers.
    ///
    /// An event whose session is not this one is ignored entirely — that is
    /// what keeps a slow reply for a closed PR from corrupting this one.
    pub fn on_event(&mut self, event: &Event) -> Vec<Command> {
        match event {
            Event::PrMetadataLoaded { session, meta } if *session == self.id => {
                // The head is only "moved" once we have a diff to compare it
                // against; the first load has nothing to invalidate.
                if let Some(shown) = &self.diff_head_sha {
                    if shown != &meta.head_sha {
                        self.head_moved = true;
                    }
                }
                self.metadata = Some(meta.clone());
                self.phase = LoadPhase::Files;

                vec![Command::LoadPrFiles {
                    session: self.id,
                    repo: self.repo.clone(),
                    number: self.number,
                }]
            }
            Event::PrFilesPage {
                session,
                files,
                done,
            } if *session == self.id => {
                self.files.extend(files.iter().cloned());
                self.rebuild_rows();

                if *done {
                    // The diff on screen now corresponds to the head we loaded
                    // it for, so a later metadata reply can detect a move.
                    self.diff_head_sha = self
                        .metadata
                        .as_ref()
                        .map(|metadata| metadata.head_sha.clone());
                    self.head_moved = false;
                    self.phase = LoadPhase::Ready;
                } else {
                    self.phase = LoadPhase::Files;
                }

                Vec::new()
            }
            Event::ThreadsLoaded { session, threads } if *session == self.id => {
                self.threads = threads.clone();
                // A full thread list replaces everything it should contain,
                // including any optimistic placeholder an in-flight post
                // pushed; that post's result event then finds nothing to
                // touch and is ignored.
                self.optimistic = None;
                self.rebuild_rows();
                Vec::new()
            }
            Event::CommentPosted {
                session,
                optimistic_id,
                comment,
            } if *session == self.id => {
                if let Some(post) = self.optimistic.take() {
                    if post.optimistic_id == *optimistic_id {
                        // Replace the placeholder's body with what GitHub
                        // created; the anchor and slot stay put.
                        if let Some(thread) = self.threads.get_mut(post.thread_index) {
                            thread.comments = vec![comment.clone()];
                        }
                    } else {
                        self.optimistic = Some(post);
                    }
                }
                self.rebuild_rows();
                Vec::new()
            }
            Event::CommentFailed {
                session,
                optimistic_id,
                error,
            } if *session == self.id => {
                // A reply failure (the reserved id) has no bubble to roll
                // back; its composer reports the failure itself.
                if *optimistic_id != NO_OPTIMISTIC_BUBBLE {
                    if let Some(post) = self.optimistic.take() {
                        if post.optimistic_id == *optimistic_id {
                            if post.thread_index < self.threads.len() {
                                self.threads.remove(post.thread_index);
                            }
                            // The reviewer's text comes back so a retry
                            // starts from what they wrote (spec §7.1).
                            self.composer.target = Some(post.target.clone());
                            self.composer.body = post.body.clone();
                            self.composer.mode = CommentMode::Immediate;
                            self.composer.error = Some(error.to_string());
                        } else {
                            self.optimistic = Some(post);
                        }
                    }
                    self.rebuild_rows();
                }
                Vec::new()
            }
            Event::ReplyPosted {
                session,
                thread_id,
                comment,
            } if *session == self.id => {
                if let Some(thread) = self.threads.iter_mut().find(|t| t.id == *thread_id) {
                    thread.comments.push(comment.clone());
                }
                self.rebuild_rows();
                Vec::new()
            }
            Event::ThreadResolved {
                session,
                thread_id,
                resolved,
            } if *session == self.id => {
                if let Some(thread) = self.threads.iter_mut().find(|t| t.id == *thread_id) {
                    thread.is_resolved = *resolved;
                }
                self.rebuild_rows();
                Vec::new()
            }
            Event::Failed {
                session: Some(session),
                error,
            } if *session == self.id => {
                self.error = Some(error.clone());
                self.phase = LoadPhase::Failed;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Re-fetch the PR from scratch, which re-fetches the files too.
    ///
    /// The previous diff is dropped immediately so stale rows are never shown
    /// against a new head; `diff_head_sha` is kept so the metadata reply can
    /// detect that the head moved. Threads are dropped and re-requested for
    /// the same reason: a rendered thread must never stay anchored to the old
    /// head. The composer keeps its text — a post in flight or being retried
    /// is the reviewer's, not the cache's.
    pub fn refresh(&mut self) -> Vec<Command> {
        self.head_moved = false;
        self.files.clear();
        self.row_model = build(&[]);
        self.threads.clear();
        self.optimistic = None;
        self.phase = LoadPhase::Header;
        self.error = None;

        vec![
            Command::LoadPrMetadata {
                session: self.id,
                repo: self.repo.clone(),
                number: self.number,
            },
            Command::LoadThreads {
                session: self.id,
                repo: self.repo.clone(),
                number: self.number,
            },
        ]
    }

    /// Re-derive the row model from the files plus the current threads,
    /// pending comments, and composer (spec §8.4).
    ///
    /// Called wherever those change. The patch rows are rebuilt, but their
    /// measured heights are carried over in order — the line rows come out
    /// of `build` in exactly the order they went in — so a comment event
    /// never collapses wrapped lines back to [`ROW_H`](crate::diff::row_model::ROW_H).
    pub fn rebuild_rows(&mut self) {
        let items = self.inline_items();
        let measured: Vec<f32> = self
            .row_model
            .rows
            .iter()
            .zip(self.row_model.heights())
            .filter(|(row, _)| matches!(row, RowRef::Line { .. }))
            .map(|(_, height)| *height)
            .collect();

        let mut model = annotate(&build(&self.files), &self.files, &items);
        let mut heights = measured.into_iter();
        for index in 0..model.rows.len() {
            if matches!(model.rows[index], RowRef::Line { .. }) {
                if let Some(height) = heights.next() {
                    model.set_height(index, height);
                }
            }
        }
        self.row_model = model;
    }

    /// The rows to splice in: every thread that has a file, every pending
    /// comment, and the composer while it is open — in that order, so
    /// [`annotate`] can keep threads ahead of pending ahead of the composer
    /// at any one point.
    fn inline_items(&self) -> Vec<InlineItem> {
        let mut items = Vec::new();
        for (index, thread) in self.threads.iter().enumerate() {
            if let Some(item) = place_thread(thread, index as u32, &self.files) {
                items.push(item);
            }
        }
        for pending in &self.pending {
            if let Some(file) = self.file_of(&pending.target) {
                items.push(InlineItem {
                    file,
                    placement: target_placement(&pending.target),
                    row: RowRef::Pending {
                        local_id: pending.local_id,
                    },
                });
            }
        }
        if let Some(target) = &self.composer.target {
            if let Some(file) = self.file_of(target) {
                items.push(InlineItem {
                    file,
                    placement: target_placement(target),
                    row: RowRef::Composer,
                });
            }
        }
        items
    }

    /// The index of the file a target is anchored to.
    fn file_of(&self, target: &CommentTarget) -> Option<u32> {
        let path = match target {
            CommentTarget::Line(anchor) => anchor.path.as_str(),
            CommentTarget::File { path } => path,
        };
        self.files
            .iter()
            .position(|l| l.file.path == path)
            .map(|index| index as u32)
    }

    /// Open the composer for `target`, picking the mode GitHub would.
    pub fn open_comment(&mut self, target: CommentTarget) {
        composer::reduce(
            &mut self.composer,
            ComposerMsg::OpenFor {
                target,
                has_open_review: !self.pending.is_empty(),
            },
        );
        self.rebuild_rows();
    }

    /// Post the composer in `mode`: immediately as a single comment, or as
    /// a pending comment for the review (Phase F submits that).
    ///
    /// An immediate post inserts an optimistic single-comment thread right
    /// away and sends the mutation exactly once; the result events replace
    /// or roll it back (spec §7.1).
    pub fn submit_comment(
        &mut self,
        mode: CommentMode,
        author: &str,
        now: Instant,
    ) -> Vec<Command> {
        if !can_submit(&self.composer) {
            return Vec::new();
        }
        let Some(target) = self.composer.target.clone() else {
            return Vec::new();
        };
        // A file-level comment is never pending, whatever button was hit.
        let mode = match target {
            CommentTarget::File { .. } => CommentMode::Immediate,
            CommentTarget::Line(_) => mode,
        };
        let body = self.composer.body.clone();

        if mode == CommentMode::Pending {
            let local_id = self.allocate_id();
            self.pending.push(PendingComment {
                local_id,
                target,
                body,
            });
            composer::reduce(&mut self.composer, ComposerMsg::Cancel);
            self.rebuild_rows();
            self.mark_session_dirty(now);
            return Vec::new();
        }

        let Some(commit_id) = self.diff_head_sha.clone() else {
            self.composer.error = Some("the diff is still loading".to_owned());
            return Vec::new();
        };

        let optimistic_id = self.allocate_id();
        let thread_index = self.threads.len();
        self.threads.push(optimistic_thread(
            &target,
            optimistic_id,
            &body,
            if author.is_empty() { "me" } else { author },
        ));
        self.optimistic = Some(OptimisticPost {
            optimistic_id,
            thread_index,
            body: body.clone(),
            target: target.clone(),
        });
        composer::reduce(&mut self.composer, ComposerMsg::Submitted);
        self.rebuild_rows();

        vec![Command::PostComment {
            session: self.id,
            repo: self.repo.clone(),
            number: self.number,
            comment: NewComment {
                commit_id,
                body,
                target,
            },
            optimistic_id,
        }]
    }

    /// Append a suggestion block for the composer's anchored rows (spec §7.4).
    pub fn insert_suggestion(&mut self) {
        let Some(CommentTarget::Line(anchor)) = self.composer.target.clone() else {
            return;
        };
        let Some(block) = suggestion_block(&rows_for_anchor(&self.files, &anchor)) else {
            return;
        };
        if !self.composer.body.ends_with('\n') {
            self.composer.body.push('\n');
        }
        self.composer.body.push_str(&block);
        self.composer.body.push('\n');
    }

    /// Drop a pending comment from the review.
    pub fn delete_pending(&mut self, local_id: u64, now: Instant) {
        self.pending.retain(|p| p.local_id != local_id);
        self.rebuild_rows();
        self.mark_session_dirty(now);
    }

    /// Take the next local comment id. Never 0: that id is the reserved
    /// "no optimistic bubble" sentinel.
    fn allocate_id(&mut self) -> u64 {
        let id = self.next_optimistic_id;
        self.next_optimistic_id += 1;
        id
    }

    pub fn mark_session_dirty(&mut self, now: Instant) {
        self.writer.mark_dirty(now);
    }

    /// Write the session file once the debounce window has elapsed.
    ///
    /// Called once per frame; the writer coalesces a burst of toggles into one
    /// write 250 ms after the last change.
    pub fn tick(&mut self, now: Instant) {
        if self.writer.should_flush(now) {
            self.write_session();
        }
    }

    /// Write any pending change now — used on exit, regardless of the debounce.
    pub fn flush_session(&mut self) {
        if self.writer.is_dirty() {
            self.write_session();
        }
    }

    /// The [`SessionFile`] this session would persist.
    fn session_file(&self) -> SessionFile {
        SessionFile {
            head_sha: self.diff_head_sha.clone(),
            viewed: self.viewed.clone(),
            // Round-tripped verbatim from the loaded file: Task 40 owns
            // these values, so a debounced viewed/collapse write here must
            // never clobber a saved draft.
            draft: self.draft.clone(),
            composer_draft: self.composer_draft.clone(),
            last_opened_file: self.last_opened_file.clone(),
            collapsed_dirs: self.collapsed_dirs.clone(),
        }
    }

    /// Write the session file atomically, reporting but not retrying a failure.
    fn write_session(&mut self) {
        let file = self.session_file();
        if let Err(error) = session::save(&self.root, &self.host, &self.repo, self.number, &file) {
            tracing::warn!(%error, "could not save the PR session");
        }
        self.writer.flushed();
    }
}

/// The placement a comment target implies, mirroring how
/// [`place_thread`](crate::diff::annotate::place_thread) reads a thread.
fn target_placement(target: &CommentTarget) -> Placement {
    match target {
        CommentTarget::File { .. } => Placement::AfterHeader,
        CommentTarget::Line(anchor) => Placement::AfterLine {
            side: anchor.side,
            line: anchor.line,
        },
    }
}

/// The placeholder thread an optimistic immediate comment shows as: one
/// comment, anchored exactly where the real one will be, with a local id
/// the result events can find it by.
fn optimistic_thread(target: &CommentTarget, id: u64, body: &str, author: &str) -> ReviewThread {
    let (path, line, side, subject_type) = match target {
        CommentTarget::File { path } => (path.clone(), None, None, SubjectType::File),
        CommentTarget::Line(anchor) => (
            anchor.path.clone(),
            Some(anchor.line),
            Some(anchor.side),
            SubjectType::Line,
        ),
    };
    ReviewThread {
        id: format!("optimistic-{id}"),
        is_resolved: false,
        is_outdated: false,
        path,
        line,
        start_line: None,
        side,
        subject_type,
        comments: vec![ThreadComment {
            database_id: id,
            author: author.to_owned(),
            avatar_url: None,
            body: body.to_owned(),
            created_at: String::new(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::error::GhError;
    use crate::runtime::{Command, Event, SessionId};
    use crate::test_support::{line_anchor, loaded, meta, repo};

    #[test]
    fn metadata_then_files_then_ready() {
        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(matches!(
            &cmds[..],
            [Command::LoadPrMetadata { .. }, Command::LoadThreads { .. }]
        ));
        let cmds = s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::LoadPrFiles { .. })));
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: false,
        });
        assert!(matches!(s.phase, LoadPhase::Files));
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("b.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        assert!(matches!(s.phase, LoadPhase::Ready));
        assert_eq!(s.row_model.file_ranges.len(), 2);
        assert_eq!(s.diff_head_sha.as_deref(), Some("sha1"));
    }

    #[test]
    fn threads_load_into_the_session() {
        use crate::model::thread::{ReviewThread, SubjectType};

        let thread = || ReviewThread {
            id: "PRRT_1".into(),
            is_resolved: false,
            is_outdated: false,
            path: "a.rs".into(),
            line: Some(1),
            start_line: None,
            side: None,
            subject_type: SubjectType::Line,
            comments: Vec::new(),
        };

        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::LoadThreads { .. })));

        s.on_event(&Event::ThreadsLoaded {
            session: SessionId(2),
            threads: vec![thread()],
        });
        assert!(
            s.threads.is_empty(),
            "a reply for another session is dropped"
        );

        s.on_event(&Event::ThreadsLoaded {
            session: SessionId(1),
            threads: vec![thread()],
        });
        assert_eq!(s.threads.len(), 1);
    }

    #[test]
    fn stale_session_events_are_dropped() {
        let (mut s, _) = PrSession::open(SessionId(2), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("old"),
        });
        assert!(s.metadata.is_none());
        s.on_event(&Event::Failed {
            session: Some(SessionId(1)),
            error: GhError::NotFound,
        });
        assert!(s.error.is_none());
    }

    #[test]
    fn refresh_detects_a_moved_head() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![],
            done: true,
        });
        s.refresh();
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha2"),
        });
        assert!(s.head_moved);
    }

    #[test]
    fn refresh_clears_a_moved_head() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![],
            done: true,
        });
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha2"),
        });
        assert!(s.head_moved);

        s.refresh();
        assert!(!s.head_moved);
    }

    #[test]
    fn viewed_and_collapsed_changes_are_debounced_then_persisted() {
        use crate::test_support::{file, tempdir};
        use std::time::Duration;

        let dir = tempdir();
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        // Point the session at a temp config root so the test never touches the
        // real one (the field is private, but this module owns it).
        s.root = dir.path().to_path_buf();

        let a = file("src/a.rs");
        s.viewed.set(&a, true);
        s.collapsed_dirs.insert("src".into());

        let t0 = Instant::now();
        s.mark_session_dirty(t0);
        s.tick(t0);
        assert!(
            crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
                .unwrap()
                .is_none(),
            "nothing is written before the debounce elapses"
        );

        s.tick(t0 + Duration::from_millis(300));
        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert!(saved.viewed.is_viewed(&a));
        assert!(saved.collapsed_dirs.contains("src"));
    }

    #[test]
    fn session_writes_preserve_saved_drafts() {
        use crate::test_support::{file, tempdir};
        use std::time::Duration;

        let dir = tempdir();
        let draft = serde_json::json!({ "body": "review text" });
        let composer_draft = serde_json::json!({ "target_path": "a.rs", "body": "wip" });
        session::save(
            dir.path(),
            "github.com",
            &repo(),
            7,
            &SessionFile {
                draft: Some(draft.clone()),
                composer_draft: Some(composer_draft.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let (mut s, _) =
            PrSession::open_at(dir.path(), SessionId(1), "github.com".into(), repo(), 7);
        assert_eq!(s.draft.as_ref(), Some(&draft));
        assert_eq!(s.composer_draft.as_ref(), Some(&composer_draft));

        let a = file("src/a.rs");
        s.viewed.set(&a, true);
        let t0 = Instant::now();
        s.mark_session_dirty(t0);
        s.tick(t0 + Duration::from_millis(300));

        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert!(saved.viewed.is_viewed(&a), "the toggle was written");
        assert_eq!(
            saved.draft.as_ref(),
            Some(&draft),
            "a viewed write must not clobber the saved draft"
        );
        assert_eq!(
            saved.composer_draft.as_ref(),
            Some(&composer_draft),
            "a viewed write must not clobber the saved composer draft"
        );
    }

    /// A session with one loaded file and a known head SHA.
    fn loaded_session() -> PrSession {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-x\n+y\n z\n")],
            done: true,
        });
        s
    }

    fn comment(database_id: u64, body: &str) -> ThreadComment {
        ThreadComment {
            database_id,
            author: "me".into(),
            avatar_url: None,
            body: body.into(),
            created_at: "2026-10-01T00:00:00Z".into(),
        }
    }

    fn anchored_thread(id: &str) -> ReviewThread {
        ReviewThread {
            id: id.into(),
            is_resolved: false,
            is_outdated: false,
            path: "a.rs".into(),
            line: Some(1),
            start_line: None,
            side: Some(crate::model::diff::Side::Right),
            subject_type: SubjectType::Line,
            comments: vec![comment(5, "root")],
        }
    }

    fn open_composer(s: &mut PrSession, body: &str) {
        s.open_comment(CommentTarget::Line(line_anchor("a.rs", 1)));
        composer::reduce(&mut s.composer, ComposerMsg::SetBody(body.into()));
    }

    #[test]
    fn refresh_clears_threads_and_asks_for_them_again() {
        let mut s = loaded_session();
        s.on_event(&Event::ThreadsLoaded {
            session: SessionId(1),
            threads: vec![anchored_thread("PRRT_1")],
        });
        assert_eq!(s.threads.len(), 1);

        let cmds = s.refresh();
        assert!(
            s.threads.is_empty(),
            "threads anchored to the old head must not survive a refresh"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadThreads { .. })),
            "a refresh re-requests the threads"
        );
    }

    #[test]
    fn an_immediate_comment_shows_optimistically_then_confirms() {
        let mut s = loaded_session();
        open_composer(&mut s, "hi");
        let cmds = s.submit_comment(CommentMode::Immediate, "me", Instant::now());
        assert!(
            matches!(
                &cmds[..],
                [Command::PostComment {
                    optimistic_id: 1,
                    ..
                }]
            ),
            "the first optimistic id is 1, never 0"
        );
        assert_eq!(s.threads.len(), 1, "the bubble shows before the reply");
        assert_eq!(s.threads[0].comments[0].body, "hi");
        assert!(s.row_model.rows.contains(&RowRef::Thread { thread: 0 }));
        assert!(s.composer.target.is_none(), "the composer closes");

        s.on_event(&Event::CommentPosted {
            session: SessionId(1),
            optimistic_id: 1,
            comment: comment(99, "hi"),
        });
        assert_eq!(s.threads[0].comments[0].database_id, 99);
        assert_eq!(s.threads[0].comments[0].created_at, "2026-10-01T00:00:00Z");
    }

    #[test]
    fn a_failed_comment_rolls_back_and_restores_the_text() {
        let mut s = loaded_session();
        open_composer(&mut s, "my lost words");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());
        assert_eq!(s.threads.len(), 1);

        s.on_event(&Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Server(500),
        });
        assert!(s.threads.is_empty(), "the optimistic bubble is removed");
        assert_eq!(s.composer.body, "my lost words", "the text comes back");
        assert_eq!(
            s.composer.target,
            Some(CommentTarget::Line(line_anchor("a.rs", 1)))
        );
        assert!(s.composer.error.is_some(), "the failure is shown");
    }

    #[test]
    fn a_pending_comment_gets_a_row_and_can_be_removed() {
        let mut s = loaded_session();
        open_composer(&mut s, "for the review");
        let cmds = s.submit_comment(CommentMode::Pending, "me", Instant::now());
        assert!(cmds.is_empty(), "pending comments post nothing yet");
        assert_eq!(s.pending.len(), 1);
        assert!(s.row_model.rows.contains(&RowRef::Pending { local_id: 1 }));

        s.delete_pending(1, Instant::now());
        assert!(s.pending.is_empty());
        assert!(!s.row_model.rows.contains(&RowRef::Pending { local_id: 1 }));
    }

    #[test]
    fn reply_and_resolve_events_fold_into_their_threads() {
        let mut s = loaded_session();
        s.on_event(&Event::ThreadsLoaded {
            session: SessionId(1),
            threads: vec![anchored_thread("PRRT_1")],
        });
        assert!(s.row_model.rows.contains(&RowRef::Thread { thread: 0 }));

        s.on_event(&Event::ReplyPosted {
            session: SessionId(1),
            thread_id: "PRRT_1".into(),
            comment: comment(6, "reply"),
        });
        assert_eq!(s.threads[0].comments.len(), 2);

        s.on_event(&Event::ThreadResolved {
            session: SessionId(1),
            thread_id: "PRRT_1".into(),
            resolved: true,
        });
        assert!(s.threads[0].is_resolved);

        // Another session's events change nothing.
        s.on_event(&Event::ThreadResolved {
            session: SessionId(2),
            thread_id: "PRRT_1".into(),
            resolved: false,
        });
        assert!(s.threads[0].is_resolved);
    }
}
