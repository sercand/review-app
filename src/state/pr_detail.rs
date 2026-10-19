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
use crate::model::review::{PendingError, PendingReview};
use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
use crate::model::RepoRef;
use crate::persist::session::{self, ComposerDraft, DebouncedWriter, SessionFile};
use crate::runtime::{Command, Event, SessionId, NO_OPTIMISTIC_BUBBLE};
use crate::state::composer::{
    self, can_submit, fallback_comment, interpret_post_error, CommentMode, ComposerMsg,
    ComposerState, PostOutcome,
};
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

/// A 422 on a line anchor: GitHub's message and the same comment re-anchored
/// to the file, offered to the reviewer as a dialog (Task 38, spec §7.1).
#[derive(Clone, Debug, PartialEq)]
pub struct FallbackPrompt {
    /// GitHub's validation message, shown verbatim.
    pub message: String,
    /// The comment as it would post if the reviewer accepts.
    pub comment: NewComment,
}

/// An immediate comment in flight: what it takes to replace it on success
/// or roll it back on failure.
#[derive(Clone, Debug)]
struct OptimisticPost {
    /// The id echoed by the result events.
    optimistic_id: u64,
    /// Where the placeholder thread was pushed in `threads`.
    thread_index: usize,
    /// The commit the post was anchored to, so a fallback re-posts against
    /// the same commit the reviewer saw.
    commit_id: String,
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
    /// The live pending-review draft (Phase F submits it), pinned to one
    /// commit while it has comments (spec §7.2). Restored from the session
    /// file on open and saved immediately after every mutation; while it is
    /// non-empty it owns the session file's `draft` field.
    pub pending: PendingReview,
    /// A line post rejected with a 422, awaiting the reviewer's decision
    /// on the file-level fallback.
    pub fallback: Option<FallbackPrompt>,
    /// The next locally allocated comment id; starts at 1 because 0 is the
    /// reserved "no optimistic bubble" sentinel.
    next_optimistic_id: u64,
    /// The immediate comment currently in flight, if any.
    optimistic: Option<OptimisticPost>,
    /// The file-tree directories the reviewer has collapsed, by path.
    pub collapsed_dirs: BTreeSet<String>,
    /// The last file selected in the tree, restored from the session file.
    pub last_opened_file: Option<String>,
    /// The saved composer draft, restored on open; while `Some` it owns the
    /// session file's `composer_draft` field. The composer UI (Phase F)
    /// writes it through the debounced session write.
    pub composer_draft: Option<ComposerDraft>,
    /// A `draft` value on disk this build cannot parse as a `PendingReview`.
    /// Written back verbatim so a session write never destroys a draft it
    /// does not own; while the typed `pending` owns the field this is `None`.
    raw_draft: Option<serde_json::Value>,
    /// A `composer_draft` value on disk this build cannot parse as a
    /// `ComposerDraft`; kept verbatim for the same reason as
    /// [`Self::raw_draft`].
    raw_composer_draft: Option<serde_json::Value>,
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
        let empty = (
            ViewedSet::default(),
            BTreeSet::new(),
            None,
            PendingReview::default(),
            None,
            None,
            None,
        );
        let (
            viewed,
            collapsed_dirs,
            last_opened_file,
            pending,
            composer_draft,
            raw_draft,
            raw_composer_draft,
        ) = match session::load(root, &host, &repo, number) {
            Ok(Some(file)) => {
                // A draft this build understands becomes live typed state;
                // one it does not is preserved verbatim so a session write
                // never destroys data it does not own.
                let (pending, raw_draft) = restore_draft(file.draft);
                let (composer_draft, raw_composer_draft) = restore_draft(file.composer_draft);
                (
                    file.viewed,
                    file.collapsed_dirs,
                    file.last_opened_file,
                    pending,
                    composer_draft,
                    raw_draft,
                    raw_composer_draft,
                )
            }
            Ok(None) => empty,
            Err(error) => {
                tracing::warn!(%error, "could not read the PR session file");
                empty
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
            pending,
            fallback: None,
            next_optimistic_id: 1,
            optimistic: None,
            collapsed_dirs,
            last_opened_file,
            composer_draft,
            raw_draft,
            raw_composer_draft,
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
                // touch and is ignored, so its text goes back to the
                // composer here rather than being lost with the placeholder.
                self.restore_optimistic_to_composer();
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
                            // A 422 on a line anchor offers the file-level
                            // fallback; everything else just shows the error.
                            if let PostOutcome::NeedsFileLevelFallback(message) =
                                interpret_post_error(&post.target, error.clone())
                            {
                                self.fallback = Some(FallbackPrompt {
                                    message,
                                    comment: fallback_comment(&NewComment {
                                        commit_id: post.commit_id.clone(),
                                        body: post.body.clone(),
                                        target: post.target.clone(),
                                    }),
                                });
                            }
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
    /// head. An immediate post still in flight loses its placeholder, but
    /// [`Self::restore_optimistic_to_composer`] returns its text to the
    /// composer first — a post in flight or being retried is the reviewer's,
    /// not the cache's (spec §7.1).
    pub fn refresh(&mut self) -> Vec<Command> {
        self.head_moved = false;
        self.files.clear();
        self.row_model = build(&[]);
        self.threads.clear();
        self.restore_optimistic_to_composer();
        self.fallback = None;
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

    /// Return an in-flight immediate post's text and anchor to the composer
    /// and release the parked post, so a refresh or a full thread reload can
    /// never lose what the reviewer wrote (spec §7.1: the text is kept until
    /// a post succeeds). A composer the reviewer has typed into again is
    /// left alone; the parked post is released either way, since its
    /// placeholder thread is gone and its result event has nothing to roll
    /// back.
    fn restore_optimistic_to_composer(&mut self) {
        if let Some(post) = self.optimistic.take() {
            if self.composer.body.is_empty() {
                self.composer.target = Some(post.target);
                self.composer.body = post.body;
                self.composer.mode = CommentMode::Immediate;
            }
        }
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
        for pending in &self.pending.comments {
            if let Some(file) = self.file_at(&pending.anchor.path) {
                items.push(InlineItem {
                    file,
                    placement: Placement::AfterLine {
                        side: pending.anchor.side,
                        line: pending.anchor.line,
                    },
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
        self.file_at(path)
    }

    /// The index of the file at `path`, when the loaded diff has one.
    fn file_at(&self, path: &str) -> Option<u32> {
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
            // A file-level target was forced to Immediate above, so a pending
            // comment always has a line anchor (spec §7.3).
            let CommentTarget::Line(anchor) = target else {
                return Vec::new();
            };
            let Some(commit_id) = self.diff_head_sha.clone() else {
                self.composer.error = Some("the diff is still loading".to_owned());
                return Vec::new();
            };
            match self.pending.add(anchor, body.clone(), &commit_id) {
                Ok(_) => {
                    composer::reduce(&mut self.composer, ComposerMsg::Cancel);
                    self.rebuild_rows();
                    self.mark_session_dirty(now);
                    self.save_pending_draft();
                }
                // The draft is pinned to another commit: keep the composer
                // text and say so — submit or discard the pinned draft first
                // (spec §7.2).
                Err(PendingError::PinnedToOtherCommit { pinned }) => {
                    self.composer.error = Some(format!(
                        "the pending review is pinned to {pinned}; submit or discard it first"
                    ));
                }
            }
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
            commit_id: commit_id.clone(),
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
        self.pending.delete(local_id);
        self.rebuild_rows();
        self.mark_session_dirty(now);
        self.save_pending_draft();
    }

    /// Post the 422 fallback: the same text as a file-level comment, as a
    /// fresh optimistic immediate post. The composer text stays until this
    /// one succeeds.
    pub fn post_fallback(&mut self, author: &str, now: Instant) -> Vec<Command> {
        let Some(prompt) = self.fallback.take() else {
            return Vec::new();
        };
        self.composer.target = Some(prompt.comment.target.clone());
        self.composer.body = prompt.comment.body.clone();
        self.composer.mode = CommentMode::Immediate;
        self.composer.error = None;
        self.submit_comment(CommentMode::Immediate, author, now)
    }

    /// Close the fallback dialog; the composer keeps its text for editing.
    pub fn dismiss_fallback(&mut self) {
        self.fallback = None;
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

    /// Persist the pending-review draft now, atomically: a draft the reviewer
    /// just wrote must never be waiting on the debounce window when a crash
    /// or quit comes (spec §11), so every pending mutation calls this.
    fn save_pending_draft(&self) {
        if let Err(error) = session::save_draft(
            &self.root,
            &self.host,
            &self.repo,
            self.number,
            &self.pending,
        ) {
            tracing::warn!(%error, "could not save the pending review draft");
        }
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
            // The typed draft owns this field while it has content; a value
            // this build could not parse is written back untouched, so a
            // debounced viewed/collapse write never clobbers a saved draft.
            draft: draft_field(&self.pending, self.raw_draft.as_ref()),
            composer_draft: match &self.composer_draft {
                Some(draft) => serde_json::to_value(draft).ok(),
                None => self.raw_composer_draft.clone(),
            },
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

/// Split a draft stored in the session file into the typed value this build
/// understands and — when it does not understand it — the raw value kept
/// verbatim, so a session write never destroys data it does not own.
fn restore_draft<T: serde::de::DeserializeOwned + Default>(
    raw: Option<serde_json::Value>,
) -> (T, Option<serde_json::Value>) {
    let Some(value) = raw else {
        return (T::default(), None);
    };
    match serde_json::from_value::<T>(value.clone()) {
        Ok(typed) => (typed, None),
        Err(_) => (T::default(), Some(value)),
    }
}

/// The `draft` field for a session write: the typed draft while it has
/// content (an empty draft is stored as `None`), otherwise the raw value
/// this build does not own, verbatim.
fn draft_field(
    pending: &PendingReview,
    raw: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    if pending.is_empty() {
        raw.cloned()
    } else {
        serde_json::to_value(pending).ok()
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
    use crate::test_support::{line_anchor, loaded, meta, repo, tempdir};

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
        // Two saved drafts, written before the typed model existed. The
        // review draft cannot be parsed as a `PendingReview`, so the session
        // must preserve it verbatim; the composer draft *is* a valid
        // `ComposerDraft` (its `anchor` reads as `None`), so the typed model
        // adopts it and must keep its content through a session write.
        let draft = serde_json::json!({ "body": "review text" });
        let composer_draft = serde_json::json!({ "target_path": "a.rs", "body": "wip" });
        let adopted = ComposerDraft {
            target_path: "a.rs".into(),
            anchor: None,
            body: "wip".into(),
        };
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
        assert_eq!(s.raw_draft.as_ref(), Some(&draft));
        assert!(
            s.pending.is_empty(),
            "a draft this build cannot parse is not adopted into the typed draft"
        );
        assert_eq!(s.composer_draft, Some(adopted.clone()));
        assert!(s.raw_composer_draft.is_none());

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
        let written: ComposerDraft =
            serde_json::from_value(saved.composer_draft.clone().expect("a composer draft"))
                .expect("the written composer draft parses");
        assert_eq!(
            written, adopted,
            "a viewed write must not clobber the saved composer draft"
        );
    }

    #[test]
    fn the_session_restores_the_draft_and_saves_mutations_immediately() {
        use crate::test_support::tempdir;

        let dir = tempdir();
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "keep me".into(), "sha1")
            .unwrap();
        session::save_draft(dir.path(), "github.com", &repo(), 7, &p).unwrap();

        let (mut s, _) =
            PrSession::open_at(dir.path(), SessionId(1), "github.com".into(), repo(), 7);
        assert_eq!(s.pending, p, "the typed draft is restored on open");

        // Deleting the last comment is persisted immediately — the empty
        // draft is stored as `None` without waiting for the debounce.
        let id = s.pending.comments[0].local_id;
        s.delete_pending(id, Instant::now());
        assert!(s.pending.commit_id.is_none());
        assert!(session::load_draft(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .is_none());
    }

    /// A session with one loaded file and a known head SHA, backed by a
    /// temporary config root: pending mutations save the draft immediately,
    /// and a test must never write to the real one.
    fn loaded_session() -> (PrSession, tempfile::TempDir) {
        let dir = tempdir();
        let (mut s, _) =
            PrSession::open_at(dir.path(), SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-x\n+y\n z\n")],
            done: true,
        });
        (s, dir)
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
        let (mut s, _dir) = loaded_session();
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
        let (mut s, _dir) = loaded_session();
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
        let (mut s, _dir) = loaded_session();
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
    fn a_refresh_during_an_immediate_post_returns_the_text_to_the_composer() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "words at risk");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());
        assert!(
            s.composer.body.is_empty(),
            "submitting clears the composer and parks the text"
        );

        s.refresh();
        assert_eq!(
            s.composer.body, "words at risk",
            "a refresh must not swallow the in-flight text"
        );

        // The late failure for the parked post finds the text already back
        // in the composer and must not lose it (spec §7.1).
        s.on_event(&Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Server(500),
        });
        assert_eq!(s.composer.body, "words at risk");
        assert_eq!(
            s.composer.target,
            Some(CommentTarget::Line(line_anchor("a.rs", 1))),
            "the anchor comes back with the text"
        );
    }

    #[test]
    fn threads_loaded_during_an_immediate_post_returns_the_text_to_the_composer() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "still mine");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());

        s.on_event(&Event::ThreadsLoaded {
            session: SessionId(1),
            threads: vec![anchored_thread("PRRT_1")],
        });
        assert_eq!(
            s.composer.body, "still mine",
            "a full thread reload must not swallow the in-flight text"
        );
        assert!(s.optimistic.is_none(), "the parked post is released");
    }

    #[test]
    fn a_pending_comment_gets_a_row_and_can_be_removed() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "for the review");
        let cmds = s.submit_comment(CommentMode::Pending, "me", Instant::now());
        assert!(cmds.is_empty(), "pending comments post nothing yet");
        assert_eq!(s.pending.comments.len(), 1);
        assert!(s.row_model.rows.contains(&RowRef::Pending { local_id: 1 }));

        s.delete_pending(1, Instant::now());
        assert!(s.pending.is_empty());
        assert!(!s.row_model.rows.contains(&RowRef::Pending { local_id: 1 }));
    }

    #[test]
    fn a_422_on_a_line_offers_the_file_level_fallback() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "this line moved");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());

        s.on_event(&Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Validation("Validation Failed: line must be part of the diff".into()),
        });
        let prompt = s.fallback.as_ref().expect("a line 422 offers the fallback");
        assert!(prompt.message.contains("part of the diff"));
        assert_eq!(
            prompt.comment.target,
            CommentTarget::File {
                path: "a.rs".into()
            },
            "the fallback keeps the body and commit and drops the coordinates"
        );
        assert_eq!(prompt.comment.commit_id, "sha1");
        assert_eq!(prompt.comment.body, "this line moved");
        assert_eq!(
            s.composer.body, "this line moved",
            "the composer text is kept until a post succeeds"
        );

        // Accepting posts the file-level comment as a new optimistic post.
        let cmds = s.post_fallback("me", Instant::now());
        assert!(
            matches!(
                &cmds[..],
                [Command::PostComment {
                    optimistic_id: 2,
                    ..
                }]
            ),
            "the fallback reuses the immediate-post path with a fresh id"
        );
        assert!(s.fallback.is_none());
        assert_eq!(s.threads.len(), 1);
        assert_eq!(
            s.threads[0].subject_type,
            SubjectType::File,
            "the placeholder is a file-level thread"
        );

        // A success clears the composer and leaves no prompt behind.
        s.on_event(&Event::CommentPosted {
            session: SessionId(1),
            optimistic_id: 2,
            comment: comment(77, "this line moved"),
        });
        assert!(s.fallback.is_none());
        assert!(s.composer.target.is_none());
    }

    #[test]
    fn a_server_error_keeps_the_text_but_offers_no_fallback() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "transient");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());
        s.on_event(&Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Server(502),
        });
        assert!(s.fallback.is_none(), "5xx is retryable, not a fallback");
        assert_eq!(s.composer.body, "transient");
    }

    #[test]
    fn dismissing_the_fallback_keeps_the_composer_text() {
        let (mut s, _dir) = loaded_session();
        open_composer(&mut s, "keep me");
        s.submit_comment(CommentMode::Immediate, "me", Instant::now());
        s.on_event(&Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Validation("nope".into()),
        });
        assert!(s.fallback.is_some());
        s.dismiss_fallback();
        assert!(s.fallback.is_none());
        assert_eq!(s.composer.body, "keep me");
    }

    #[test]
    fn reply_and_resolve_events_fold_into_their_threads() {
        let (mut s, _dir) = loaded_session();
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
