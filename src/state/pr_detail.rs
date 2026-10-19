//! One open PR's session: the load orchestration and everything the detail
//! screen draws from.
//!
//! Loading is a small state machine driven entirely by [`Event`]s: metadata
//! first (so the head SHA is known before any file's patch is anchored to it),
//! then the files page by page. Events carry the [`SessionId`] they belong to,
//! so a reply for a PR the user has left is dropped rather than folded into the
//! session now on screen (the stale-event rule).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::diff::annotate::{annotate, place_thread, rows_for_anchor, InlineItem, Placement};
use crate::diff::expand::{expand, gaps, Expand};
use crate::diff::image::{image_kind, image_sources, image_uri, ImageBlob, ImageKind};
use crate::diff::options::hide_whitespace_only;
use crate::diff::row_model::{build_one, RowModel, RowRef};
use crate::diff::suggestion::suggestion_block;
use crate::github::checks::{CheckRun, CombinedStatus};
use crate::github::comments::NewComment;
use crate::github::commits::Commit;
use crate::github::conversation::ConversationComment;
use crate::github::error::GhError;
use crate::github::files::{FileStatus, LoadedFile, PatchState};
use crate::github::metadata::PrMetadata;
use crate::github::reviews::{review_payload, Review, ServerPendingComment};
use crate::model::comment::{Anchor, CommentTarget};
use crate::model::diff::{DiffMode, FileDiff};
use crate::model::review::{PendingError, PendingReview, ReviewEvent};
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
use crate::ui::widgets::review_panel::{review_panel_vm, sha7, PanelInputs};

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

/// The most all of one session's image blobs may hold at once; the oldest
/// loaded file is evicted past it (Task 49).
const MAX_IMAGE_CACHE_BYTES: u64 = 200 * 1024 * 1024;

/// The most files GitHub's files API ever returns for one PR (spec §7.5):
/// a load that reaches the cap is silently missing the rest of the diff,
/// so the session flags it and the diff view banners it.
pub const GITHUB_FILE_CAP: usize = 3000;

/// One file's loaded image sides and the bytes they cost the cache.
#[derive(Debug)]
pub struct ImageEntry {
    /// The base side, read at the merge base.
    pub base: ImageBlob,
    /// The head side, read at the diff's head SHA.
    pub head: ImageBlob,
    /// The bytes both sides buffer.
    bytes: u64,
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
    /// Whether the file load reached [`GITHUB_FILE_CAP`]: GitHub's files
    /// API returns at most that many files for one PR, so the diff on
    /// screen is silently incomplete (spec §7.5).
    pub files_truncated: bool,
    /// Which files the reviewer has marked viewed, keyed by blob SHA.
    pub viewed: ViewedSet,
    /// What the file tree is filtering the file list by.
    pub filters: FilterState,
    /// The PR's review threads, once loaded (spec §7.4).
    pub threads: Vec<ReviewThread>,
    /// The PR's reviews, once loaded, in API order (spec §7.2).
    pub reviews: Vec<Review>,
    /// The viewer's server-side pending review and its comments, when they
    /// have one; it blocks a local submit (Phase F).
    pub server_pending: Option<(Review, Vec<ServerPendingComment>)>,
    /// The PR's commits, once loaded, in API order (Task 50).
    pub commits: Vec<Commit>,
    /// The head commit's check runs, once loaded (Task 50).
    pub check_runs: Vec<CheckRun>,
    /// The head commit's legacy combined status, once loaded (Task 50).
    pub combined_status: Option<CombinedStatus>,
    /// The PR's conversation (issue) comments, once loaded, oldest first
    /// (Task 51).
    pub conversation: Vec<ConversationComment>,
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
    /// Head-blob text by file index, cached for context expansion (Task 45);
    /// the expansion itself runs on the UI thread, so the cache lives here.
    pub head_blobs: HashMap<u32, String>,
    /// Expansion requests waiting on their file's head blob, as
    /// `(file, before_hunk, how)`.
    pending_expands: Vec<(u32, u32, Expand)>,
    /// The PR's merge base, fetched once per session (Task 46): every
    /// too-large file's base blob is read at this commit.
    pub merge_base: Option<String>,
    /// Whether whitespace-only hunks are hidden (Task 48). The row model
    /// resolves line rows by hunk index, so hiding filters each file's diff
    /// in place rather than only the rows built from it.
    pub hide_whitespace: bool,
    /// The layout the diff shows: unified rows or paired split rows.
    pub mode: DiffMode,
    /// The unfiltered diffs stashed while whitespace is hidden, by file
    /// index, so turning the option off restores exactly what was on screen.
    original_diffs: HashMap<u32, FileDiff>,
    /// Files already run through [`hide_whitespace_only`] since the option
    /// last changed, so a rebuild never re-filters (or re-stashes) them.
    filtered_files: std::collections::HashSet<u32>,
    /// Loaded image diffs by file index, evicted oldest-first once the
    /// total exceeds [`MAX_IMAGE_CACHE_BYTES`] (Task 49).
    images: HashMap<u32, ImageEntry>,
    /// File indices in `images`, oldest first: the eviction order.
    image_lru: std::collections::VecDeque<u32>,
    /// The bytes `images` currently holds.
    image_bytes: u64,
    /// Texture URIs whose blobs were evicted (or dropped with a refresh).
    /// The session must not know about egui, so the app drains this each
    /// frame and calls `ctx.forget_image` on every entry.
    pub evicted_images: Vec<String>,
    /// Files whose image blobs are in flight or loaded: the lazy trigger
    /// fires once per file (until eviction allows a re-request).
    images_requested: std::collections::HashSet<u32>,
    /// SVG files shown rendered instead of as a text diff (Task 49).
    pub rendered_svg: std::collections::HashSet<u32>,
    /// The text diffs stashed while an SVG shows rendered, by file index:
    /// the row model resolves a file's rows from its diff, so "no diff" is
    /// what puts the viewer where the lines were.
    svg_diffs: HashMap<u32, FileDiff>,
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

        let mut session = PrSession {
            id,
            host,
            repo: repo.clone(),
            number,
            metadata: None,
            diff_head_sha: None,
            files: Vec::new(),
            row_model: build_one(&[], None, DiffMode::Unified),
            phase: LoadPhase::Header,
            error: None,
            head_moved: false,
            files_truncated: false,
            viewed,
            filters: FilterState::default(),
            threads: Vec::new(),
            reviews: Vec::new(),
            server_pending: None,
            commits: Vec::new(),
            check_runs: Vec::new(),
            combined_status: None,
            conversation: Vec::new(),
            composer: ComposerState::default(),
            pending,
            fallback: None,
            head_blobs: HashMap::new(),
            pending_expands: Vec::new(),
            merge_base: None,
            hide_whitespace: false,
            mode: DiffMode::Unified,
            original_diffs: HashMap::new(),
            filtered_files: std::collections::HashSet::new(),
            images: HashMap::new(),
            image_lru: std::collections::VecDeque::new(),
            image_bytes: 0,
            evicted_images: Vec::new(),
            images_requested: std::collections::HashSet::new(),
            rendered_svg: std::collections::HashSet::new(),
            svg_diffs: HashMap::new(),
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

        // A saved composer draft reopens the composer where the reviewer left
        // it (spec §11). The empty→`None` rule means a stored draft always
        // carries text, so restoring it can never reopen an empty composer —
        // and the live composer matching its own draft is what keeps the
        // per-frame [`PrSession::sync_composer_draft`] from clobbering it.
        if let Some(draft) = &session.composer_draft {
            session.composer.target = Some(match draft.anchor.clone() {
                Some(anchor) => CommentTarget::Line(anchor),
                None => CommentTarget::File {
                    path: draft.target_path.clone(),
                },
            });
            session.composer.body = draft.body.clone();
        }

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
                repo: repo.clone(),
                number,
            },
            // Reviews too: the review panel and the server-pending banner
            // should be there as soon as the PR opens (spec §7.2).
            Command::LoadReviews {
                session: id,
                repo: repo.clone(),
                number,
            },
            // And the commits: the right-pane list does not wait on the head
            // SHA the way the checks do (Task 50).
            Command::LoadCommits {
                session: id,
                repo: repo.clone(),
                number,
            },
            // And the conversation: the timeline merges it with the reviews
            // the command above brings back (Task 51).
            Command::LoadConversation {
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

                vec![
                    Command::LoadPrFiles {
                        session: self.id,
                        repo: self.repo.clone(),
                        number: self.number,
                    },
                    // Checks are the one load that needs the head SHA, so
                    // they ride out of this fold rather than `open_at`
                    // (Task 50).
                    Command::LoadChecks {
                        session: self.id,
                        repo: self.repo.clone(),
                        sha: meta.head_sha.clone(),
                    },
                ]
            }
            Event::PrFilesPage {
                session,
                files,
                done,
            } if *session == self.id => {
                self.files.extend(files.iter().cloned());
                // GitHub's files API returns at most 3000 files for one PR
                // and says nothing when it truncates; reaching the cap is
                // the only signal the diff on screen is incomplete
                // (spec §7.5).
                if self.files.len() >= GITHUB_FILE_CAP {
                    self.files_truncated = true;
                }
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
            Event::ReviewsLoaded {
                session,
                reviews,
                server_pending,
            } if *session == self.id => {
                self.reviews = reviews.clone();
                self.server_pending = server_pending.clone();
                Vec::new()
            }
            Event::CommitsLoaded { session, commits } if *session == self.id => {
                self.commits = commits.clone();
                Vec::new()
            }
            Event::ChecksLoaded {
                session,
                runs,
                combined,
            } if *session == self.id => {
                self.check_runs = runs.clone();
                self.combined_status = Some(combined.clone());
                Vec::new()
            }
            Event::ConversationLoaded { session, comments } if *session == self.id => {
                self.conversation = comments.clone();
                Vec::new()
            }
            Event::ServerPendingResolved { session } if *session == self.id => {
                // The submit or discard changed GitHub's state; reload the
                // reviews so the panel and the banner show the result.
                vec![Command::LoadReviews {
                    session: self.id,
                    repo: self.repo.clone(),
                    number: self.number,
                }]
            }
            Event::ReviewSubmitted { session, review } if *session == self.id => {
                on_review_submitted(self, review.clone())
            }
            Event::ReviewSubmitFailed { session, error } if *session == self.id => {
                on_submit_error(self, error.clone());
                Vec::new()
            }
            Event::HeadBlobLoaded {
                session,
                file,
                text,
            } if *session == self.id => {
                self.head_blobs.insert(*file, text.clone());
                // Apply every expansion this blob unblocks, in click order;
                // each recomputes its gap, so repeated clicks reveal the
                // next chunk rather than re-inserting the same rows.
                let mut applied = false;
                for (pending_file, before_hunk, how) in std::mem::take(&mut self.pending_expands) {
                    if pending_file == *file {
                        self.apply_expand(pending_file, before_hunk, how);
                        applied = true;
                    } else {
                        self.pending_expands.push((pending_file, before_hunk, how));
                    }
                }
                if applied {
                    self.rebuild_rows();
                }
                Vec::new()
            }
            Event::LocalDiffReady {
                session,
                file,
                diff,
                merge_base,
            } if *session == self.id => {
                // Cache the merge base for the next too-large file, and put
                // the local diff where the placeholder row will pick it up.
                self.merge_base = Some(merge_base.clone());
                if let Some(loaded) = self.files.get_mut(*file as usize) {
                    loaded.diff = Some(diff.clone());
                }
                // The fresh diff has never seen the whitespace filter, so
                // any filtered mark on the file is a lie now; drop it as
                // `apply_expand` does, and the rebuild below re-filters
                // the new diff (stashing its original) while the option
                // is on.
                self.filtered_files.remove(file);
                self.rebuild_rows();
                Vec::new()
            }
            Event::ImagesLoaded {
                session,
                file,
                base,
                head,
            } if *session == self.id => {
                // The placeholder row already exists, so only the cache
                // changes; the row's height follows once the viewer lays
                // out (Task 49).
                self.put_images(*file, base.clone(), head.clone());
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
        // Image blobs belong to the diff being dropped: forget them and
        // queue their textures for the app to release (Task 49). The paths
        // are still loaded here, so the URIs are exact.
        for file in 0..self.files.len() {
            let file = file as u32;
            if self.images.contains_key(&file) {
                let path = self.files[file as usize].file.path.as_str();
                self.evicted_images
                    .push(image_uri(self.id.0, file, "base", path));
                self.evicted_images
                    .push(image_uri(self.id.0, file, "head", path));
            }
        }
        self.images.clear();
        self.image_lru.clear();
        self.image_bytes = 0;
        self.images_requested.clear();
        self.rendered_svg.clear();
        self.svg_diffs.clear();
        self.files.clear();
        // The truncation verdict belongs to the load being dropped; the
        // reload sets it again only if it hits the cap too (spec §7.5).
        self.files_truncated = false;
        self.row_model = build_one(&[], None, DiffMode::Unified);
        self.threads.clear();
        self.restore_optimistic_to_composer();
        self.fallback = None;
        // Blob caches are keyed by file index, which the reload invalidates;
        // the merge base is re-derived against whatever head the reload finds.
        self.head_blobs.clear();
        self.pending_expands.clear();
        self.merge_base = None;
        self.original_diffs.clear();
        self.filtered_files.clear();
        // The commits and checks belong to the head being re-fetched; the
        // checks ride back in on the metadata reply, which names the new
        // head SHA (Task 50).
        self.commits.clear();
        self.check_runs.clear();
        self.combined_status = None;
        // The conversation is part of the timeline the reload rebuilds (Task 51).
        self.conversation.clear();
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
            Command::LoadReviews {
                session: self.id,
                repo: self.repo.clone(),
                number: self.number,
            },
            Command::LoadCommits {
                session: self.id,
                repo: self.repo.clone(),
                number: self.number,
            },
            Command::LoadConversation {
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
    /// The visible files' indices in display order (see
    /// [`filters::file_order`](crate::state::filters::file_order)).
    pub fn file_order(&self) -> Vec<usize> {
        let prs: Vec<crate::github::files::PrFile> = self
            .files
            .iter()
            .map(|loaded| loaded.file.clone())
            .collect();
        crate::state::filters::file_order(&prs, &self.filters, &self.viewed)
    }

    /// The file the diff shows: `last_opened_file` while it is visible,
    /// else the first file in display order. `None` only when no file is
    /// visible at all (empty PR, or the filters hide everything).
    pub fn current_file(&self) -> Option<(usize, &LoadedFile)> {
        let order = self.file_order();
        let index = self
            .last_opened_file
            .as_deref()
            .and_then(|path| {
                order
                    .iter()
                    .copied()
                    .find(|i| self.files[*i].file.path == path)
            })
            .or_else(|| order.first().copied())?;
        Some((index, &self.files[index]))
    }

    /// Show file `index`: remember it as the last opened file and rebuild
    /// the rows. The composer is left alone — a written comment is never
    /// lost to a file switch (spec S4).
    pub fn select_file(&mut self, index: usize) {
        let Some(loaded) = self.files.get(index) else {
            return;
        };
        self.last_opened_file = Some(loaded.file.path.clone());
        self.rebuild_rows();
    }

    /// Switch between the unified and split layouts and rebuild the rows;
    /// measured heights belong to the old layout and are dropped with it.
    pub fn set_mode(&mut self, mode: DiffMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.rebuild_rows();
    }

    pub fn rebuild_rows(&mut self) {
        if self.hide_whitespace {
            self.filter_whitespace();
        }
        let items = self.inline_items();
        let selected = self.current_file().map(|(index, _)| index);
        // Measured heights belong to the file and layout they were measured
        // on: carry them over only while neither changes.
        let same_layout = self
            .row_model
            .rows
            .first()
            .map(|row| {
                matches!(row, RowRef::FileHeader { file } if *file as usize == selected.unwrap_or(usize::MAX))
            })
            .unwrap_or(true)
            && self
                .row_model
                .rows
                .iter()
                .any(|row| matches!(row, RowRef::Pair { .. }))
                == (self.mode == DiffMode::Split);
        let measured: Vec<f32> = if same_layout {
            self.row_model
                .rows
                .iter()
                .zip(self.row_model.heights())
                .filter(|(row, _)| matches!(row, RowRef::Line { .. } | RowRef::Pair { .. }))
                .map(|(_, height)| *height)
                .collect()
        } else {
            Vec::new()
        };

        let mut model = annotate(
            &build_one(&self.files, selected, self.mode),
            &self.files,
            &items,
        );
        let mut heights = measured.into_iter();
        for index in 0..model.rows.len() {
            if matches!(model.rows[index], RowRef::Line { .. } | RowRef::Pair { .. }) {
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

    /// Reveal omitted context in one gap (Task 45, Review Focus #4).
    ///
    /// With the file's head blob already cached the expansion applies right
    /// away; otherwise the request is queued and the blob fetched. A repeat
    /// click on the same gap while its blob is in flight still asks once, but
    /// the queued request widens to cover the later click — "all" after
    /// "↑ 20" expands everything, not just the first request.
    pub fn expand_gap(&mut self, file: u32, before_hunk: u32, how: Expand) -> Vec<Command> {
        if !self
            .files
            .get(file as usize)
            .is_some_and(|loaded| loaded.diff.is_some())
        {
            return Vec::new();
        }
        if self.head_blobs.contains_key(&file) {
            self.apply_expand(file, before_hunk, how);
            self.rebuild_rows();
            return Vec::new();
        }
        if let Some(entry) = self
            .pending_expands
            .iter_mut()
            .find(|(f, b, _)| *f == file && *b == before_hunk)
        {
            // Keep the dedupe — the blob is still fetched once — but let the
            // wider request win so a later click is never swallowed.
            entry.2 = wider_expand(entry.2, how);
            return Vec::new();
        }
        self.pending_expands.push((file, before_hunk, how));
        vec![Command::ExpandContext {
            session: self.id,
            repo: self.repo.clone(),
            file,
            path: self.files[file as usize].file.path.clone(),
            sha: self.diff_head_sha.clone().unwrap_or_default(),
        }]
    }

    /// Insert the chosen part of the gap before `before_hunk` from the
    /// cached head blob.
    ///
    /// The gap is recomputed from the diff as it stands, so successive
    /// expansions of one gap each reveal the next chunk, and a gap already
    /// fully revealed expands to nothing.
    fn apply_expand(&mut self, file: u32, before_hunk: u32, how: Expand) {
        let Some(blob) = self.head_blobs.get(&file).cloned() else {
            return;
        };
        let Some(diff) = self
            .files
            .get(file as usize)
            .and_then(|loaded| loaded.diff.clone())
        else {
            return;
        };
        // The blob bounds the gap; a caller without a line count yet leaves
        // the tail open-ended and `expand` clamps it to the blob.
        let Some(gap) = gaps(&diff, u32::MAX)
            .into_iter()
            .find(|gap| gap.before_hunk == before_hunk)
        else {
            return;
        };
        let lines: Vec<&str> = blob.lines().collect();
        let expanded = expand(&diff, &lines, gap, how);
        if let Some(loaded) = self.files.get_mut(file as usize) {
            loaded.diff = Some(expanded);
        }
        // The expanded rows may hide a whitespace-only change, so the file
        // has to be filtered again on the next rebuild.
        self.filtered_files.remove(&file);
    }

    /// Hide or show whitespace-only hunks (Task 48).
    ///
    /// Hiding filters every file's diff in place — line rows resolve hunks
    /// by index, so the filtered diff must be the one the rows point at —
    /// and stashes each original that changes; showing restores them. Row
    /// indices change either way, so whatever the caller keys by them (the
    /// diff view's find matches) must be cleared.
    pub fn set_hide_whitespace(&mut self, hide: bool) {
        if self.hide_whitespace == hide {
            return;
        }
        self.hide_whitespace = hide;
        if !hide {
            for (index, diff) in std::mem::take(&mut self.original_diffs) {
                if self.rendered_svg.contains(&index) {
                    // The file shows the rendered viewer, not text rows:
                    // the original belongs to the stash "Text" restores
                    // from, so the filtered copy in it is replaced rather
                    // than a diff installed under the viewer.
                    self.svg_diffs.insert(index, diff);
                } else if let Some(loaded) = self.files.get_mut(index as usize) {
                    loaded.diff = Some(diff);
                }
            }
            self.filtered_files.clear();
        }
        self.rebuild_rows();
    }

    /// Flip the hide-whitespace option (Task 48).
    pub fn toggle_hide_whitespace(&mut self) {
        let hide = !self.hide_whitespace;
        self.set_hide_whitespace(hide);
    }

    /// Replace each not-yet-filtered file's diff with its
    /// [`hide_whitespace_only`] form, stashing the original of every diff
    /// that changes. Files pages that arrive while the option is on are
    /// picked up here too, since [`PrSession::rebuild_rows`] calls this.
    fn filter_whitespace(&mut self) {
        for index in 0..self.files.len() {
            let file = index as u32;
            if self.filtered_files.contains(&file) {
                continue;
            }
            // Only files that actually have a diff are marked: a diff-less
            // file (too-large, binary) has nothing to filter, and marking
            // it would hide it from this pass once its diff does arrive.
            let Some(diff) = self.files[index].diff.clone() else {
                continue;
            };
            self.filtered_files.insert(file);
            let filtered = hide_whitespace_only(&diff);
            if filtered != diff {
                self.original_diffs.entry(file).or_insert(diff);
                self.files[index].diff = Some(filtered);
            }
        }
    }

    /// Whether `file` is an image whose viewer is on screen and whose blobs
    /// have not been asked for yet (Task 49): a binary raster at its
    /// placeholder, or an SVG the reviewer switched to rendered.
    pub fn wants_images(&self, file: u32) -> bool {
        if self.images_requested.contains(&file) {
            return false;
        }
        let Some(loaded) = self.files.get(file as usize) else {
            return false;
        };
        match image_kind(&loaded.file.path) {
            Some(ImageKind::Raster) => loaded.file.patch_state == PatchState::Binary,
            Some(ImageKind::Svg) => self.rendered_svg.contains(&file),
            None => false,
        }
    }

    /// Ask for both sides of an image file's diff (Task 49), once per file.
    ///
    /// The command carries everything the stateless executor needs: the
    /// base blob is read at the merge base — the session's cached one when
    /// it has one — and the head blob at the diff's head SHA.
    pub fn load_images(&mut self, file: u32) -> Vec<Command> {
        if !self.wants_images(file) {
            return Vec::new();
        }
        let Some(loaded) = self.files.get(file as usize) else {
            return Vec::new();
        };
        let Some(metadata) = self.metadata.as_ref() else {
            return Vec::new();
        };
        self.images_requested.insert(file);
        let (base_path, head_path) = image_sources(&loaded.file);
        vec![Command::LoadImages {
            session: self.id,
            file,
            repo: self.repo.clone(),
            base_sha: metadata.base_sha.clone(),
            head_sha: self
                .diff_head_sha
                .clone()
                .unwrap_or_else(|| metadata.head_sha.clone()),
            merge_base: self.merge_base.clone(),
            base_path,
            head_path,
        }]
    }

    /// The loaded image sides of one file, when the cache holds them.
    pub fn image_for(&self, file: u32) -> Option<&ImageEntry> {
        self.images.get(&file)
    }

    /// Show an SVG file rendered instead of as a text diff, or back again
    /// (Task 49). The text diff is stashed while rendered so un-rendering
    /// restores exactly what was there.
    pub fn toggle_rendered_svg(&mut self, file: u32) {
        if self.rendered_svg.remove(&file) {
            // Prefer the stashed original while the option is off: the
            // svg stash may hold a diff filtered while it was on, and a
            // filtered diff must never install with the option unchecked.
            let stash = self.svg_diffs.remove(&file);
            let restored = if self.hide_whitespace {
                stash
            } else {
                self.original_diffs.remove(&file).or(stash)
            };
            if let Some(diff) = restored {
                if let Some(loaded) = self.files.get_mut(file as usize) {
                    loaded.diff = Some(diff);
                }
                // The restored diff may predate a filter pass the file is
                // still marked for; let the next rebuild re-filter it.
                self.filtered_files.remove(&file);
            }
        } else {
            if let Some(loaded) = self.files.get_mut(file as usize) {
                if let Some(diff) = loaded.diff.take() {
                    self.svg_diffs.insert(file, diff);
                }
            }
            self.rendered_svg.insert(file);
        }
        self.rebuild_rows();
    }

    /// Store one file's image sides, evicting the oldest-loaded files until
    /// the cache is back under [`MAX_IMAGE_CACHE_BYTES`]. An evicted file's
    /// egui texture URIs are recorded in [`Self::evicted_images`] for the
    /// app to forget, and it may be requested again if it comes back on
    /// screen (Task 49).
    fn put_images(&mut self, file: u32, base: ImageBlob, head: ImageBlob) {
        let bytes = base.size().unwrap_or(0) + head.size().unwrap_or(0);
        if let Some(old) = self.images.remove(&file) {
            self.image_bytes -= old.bytes;
            self.image_lru.retain(|f| *f != file);
        }
        self.image_bytes += bytes;
        self.images.insert(file, ImageEntry { base, head, bytes });
        self.image_lru.push_back(file);
        while self.image_bytes > MAX_IMAGE_CACHE_BYTES {
            let Some(oldest) = self.image_lru.pop_front() else {
                break;
            };
            let Some(entry) = self.images.remove(&oldest) else {
                continue;
            };
            self.image_bytes -= entry.bytes;
            self.images_requested.remove(&oldest);
            let path = self
                .files
                .get(oldest as usize)
                .map(|loaded| loaded.file.path.as_str())
                .unwrap_or_default();
            self.evicted_images
                .push(image_uri(self.id.0, oldest, "base", path));
            self.evicted_images
                .push(image_uri(self.id.0, oldest, "head", path));
        }
    }

    /// Ask for a local diff of a file GitHub sent no patch for (Task 46).
    ///
    /// The base blob is read at the merge base — at `previous_path` for a
    /// rename, and not at all for an added file; the head blob is skipped
    /// for a removed file. The session's cached merge base rides along when
    /// it has one, so only the first too-large file pays for the compare.
    pub fn load_large_diff(&mut self, file: u32) -> Vec<Command> {
        let Some(loaded) = self.files.get(file as usize) else {
            return Vec::new();
        };
        let Some(metadata) = self.metadata.as_ref() else {
            return Vec::new();
        };
        let f = &loaded.file;
        let (base_path, head_path) = match f.status {
            FileStatus::Added => (None, Some(f.path.clone())),
            FileStatus::Removed => (Some(f.path.clone()), None),
            FileStatus::Renamed | FileStatus::Copied => {
                (f.previous_path.clone(), Some(f.path.clone()))
            }
            _ => (Some(f.path.clone()), Some(f.path.clone())),
        };
        vec![Command::LoadLargeDiff {
            session: self.id,
            file,
            repo: self.repo.clone(),
            base_sha: metadata.base_sha.clone(),
            head_sha: self
                .diff_head_sha
                .clone()
                .unwrap_or_else(|| metadata.head_sha.clone()),
            merge_base: self.merge_base.clone(),
            base_path,
            head_path,
        }]
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
            if self.diff_head_sha.is_none() {
                self.composer.error = Some("the diff is still loading".to_owned());
                return Vec::new();
            }
            match add_pending(self, anchor, body.clone()) {
                Ok(_) => {
                    composer::reduce(&mut self.composer, ComposerMsg::Cancel);
                    self.mark_session_dirty(now);
                }
                // The draft is pinned to another commit: keep the composer
                // text and say so — submit or discard the pinned draft first,
                // and immediate comments stay available (spec §7.2).
                Err(PendingError::PinnedToOtherCommit { pinned }) => {
                    self.composer.error = Some(format!(
                        "submit or discard the draft written against {} first",
                        sha7(&pinned)
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

    /// Replace the draft's summary body, persisting it immediately.
    ///
    /// The panel's text edit writes through to `pending` as the reviewer
    /// types; this is the matching persist the reported `SetBody` action
    /// triggers.
    pub fn set_pending_body(&mut self, body: String) {
        self.pending.set_body(body);
        self.save_pending_draft();
    }

    /// Pick the verdict the draft submits with, persisting it immediately.
    pub fn set_pending_event(&mut self, event: ReviewEvent) {
        self.pending.set_event(event);
        self.save_pending_draft();
    }

    /// Replace one pending comment's body, persisting the draft immediately.
    pub fn edit_pending(&mut self, local_id: u64, body: String) {
        self.pending.edit(local_id, body);
        self.save_pending_draft();
    }

    /// Drop the whole draft — in memory and in the session file (the panel
    /// has already asked the reviewer to confirm).
    pub fn discard_pending_review(&mut self, now: Instant) {
        self.pending.clear();
        self.rebuild_rows();
        self.mark_session_dirty(now);
        self.save_pending_draft();
    }

    /// Snapshot the live composer into the persisted draft field, marking the
    /// session dirty when it changed (spec §11).
    ///
    /// `write_session` owns the session file's `composer_draft` field, and
    /// this is the only writer of the in-memory value; the app calls it once
    /// per frame before [`PrSession::tick`], so a debounced whole-file write
    /// can never carry a stale composer draft. An empty composer stores
    /// `None`, never `Some(empty)`.
    pub fn sync_composer_draft(&mut self, now: Instant) {
        let draft = composer_draft_of(&self.composer);
        if self.composer_draft != draft {
            self.composer_draft = draft;
            self.mark_session_dirty(now);
        }
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

/// Whether `viewer` wrote this PR; the author may not approve or request
/// changes on it (spec §7.2). GitHub logins are case-insensitive.
pub fn viewer_is_author(session: &PrSession, viewer: &str) -> bool {
    session
        .metadata
        .as_ref()
        .is_some_and(|meta| meta.author.eq_ignore_ascii_case(viewer))
}

/// Whether the head moved out from under the draft's pinned commit (Review
/// Focus #7). The draft stays pinned either way — this only drives the
/// panel's banner and the submit payload's `commit_id`.
pub fn head_moved_since_pin(session: &PrSession) -> bool {
    let Some(pinned) = &session.pending.commit_id else {
        return false;
    };
    session
        .metadata
        .as_ref()
        .is_some_and(|meta| &meta.head_sha != pinned)
}

/// The request that reveals at least as much of one gap as both `a` and `b`:
/// `All` subsumes everything, a larger `n` subsumes a smaller one pointing
/// the same way, and opposite directions fall back to `All` — the one single
/// request covering both — so a queued click is widened, never dropped.
fn wider_expand(a: Expand, b: Expand) -> Expand {
    match (a, b) {
        (Expand::All, _) | (_, Expand::All) => Expand::All,
        (Expand::Up(x), Expand::Up(y)) => Expand::Up(x.max(y)),
        (Expand::Down(x), Expand::Down(y)) => Expand::Down(x.max(y)),
        _ => Expand::All,
    }
}

/// Add a pending comment anchored to the diff on screen, persisting the
/// draft immediately.
///
/// The pin is `diff_head_sha` — the commit the reviewer is actually looking
/// at. When the draft is pinned elsewhere the error reaches the reviewer as
/// "submit or discard the draft written against <sha7> first", and immediate
/// comments stay available (Review Focus #7).
pub fn add_pending(
    session: &mut PrSession,
    anchor: Anchor,
    body: String,
) -> Result<u64, PendingError> {
    let Some(head) = session.diff_head_sha.clone() else {
        // Nothing to pin to before the diff loads; report the pin the draft
        // already has, if any, so the caller's message stays truthful.
        return Err(PendingError::PinnedToOtherCommit {
            pinned: session.pending.commit_id.clone().unwrap_or_default(),
        });
    };
    let result = session.pending.add(anchor, body, &head);
    if result.is_ok() {
        session.rebuild_rows();
        session.mark_session_dirty(Instant::now());
        session.save_pending_draft();
    }
    result
}

/// The submit command for the draft, when the panel's view-model allows it;
/// `None` otherwise (the panel disables Submit in exactly that case).
///
/// The payload is built here, at click time, so the contract GitHub sees is
/// the one the reviewer confirmed.
pub fn start_submit(session: &PrSession) -> Option<Command> {
    let head_sha = session.diff_head_sha.clone()?;
    let inputs = PanelInputs {
        pending: &session.pending,
        // The caller gates the verdict buttons on the author rule; GitHub
        // rejects a verdict the author may not pick. The app gates writes
        // on the rate-limit window before calling this (spec §13), so the
        // draft's own rules are all that is left to check here.
        viewer_is_author: false,
        server_pending: session.server_pending.is_some(),
        head_moved_since_pin: head_moved_since_pin(session),
        write_blocked: None,
    };
    if !review_panel_vm(&inputs).submit_enabled {
        return None;
    }
    Some(Command::SubmitReview {
        session: session.id,
        repo: session.repo.clone(),
        number: session.number,
        payload: review_payload(&session.pending, &head_sha),
    })
}

/// Fold a successful submit in: the draft GitHub just posted is dropped —
/// in memory and on disk — the review is appended, and the threads and
/// reviews are reloaded so the new comments render anchored (spec §7.2).
pub fn on_review_submitted(session: &mut PrSession, review: Review) -> Vec<Command> {
    session.pending.clear();
    session.rebuild_rows();
    session.save_pending_draft();
    session.reviews.push(review);
    vec![
        Command::LoadThreads {
            session: session.id,
            repo: session.repo.clone(),
            number: session.number,
        },
        Command::LoadReviews {
            session: session.id,
            repo: session.repo.clone(),
            number: session.number,
        },
    ]
}

/// Fold a failed submit in: the draft is never lost (spec §11), only the
/// error is recorded.
pub fn on_submit_error(session: &mut PrSession, error: GhError) {
    session.error = Some(error);
}

/// The persisted composer draft for the live composer; `None` when there is
/// nothing to keep — no target, or a blank body (the empty→`None` rule).
fn composer_draft_of(state: &ComposerState) -> Option<ComposerDraft> {
    let target = state.target.as_ref()?;
    if state.body.trim().is_empty() {
        return None;
    }
    let (target_path, anchor) = match target {
        CommentTarget::Line(anchor) => (anchor.path.clone(), Some(anchor.clone())),
        CommentTarget::File { path } => (path.clone(), None),
    };
    Some(ComposerDraft {
        target_path,
        anchor,
        body: state.body.clone(),
    })
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
    use crate::github::reviews::ReviewState;
    use crate::runtime::{Command, Event, SessionId};
    use crate::test_support::{line_anchor, loaded, meta, meta_by, repo, review, tempdir};

    #[test]
    fn metadata_then_files_then_ready() {
        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(matches!(
            &cmds[..],
            [
                Command::LoadPrMetadata { .. },
                Command::LoadThreads { .. },
                Command::LoadReviews { .. },
                Command::LoadCommits { .. },
                Command::LoadConversation { .. }
            ]
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

    /// A ready session over `(path, patch)` pairs, in the given order.
    fn session_with(files: &[(&str, &str)]) -> PrSession {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        let loaded_files = files
            .iter()
            .map(|(path, patch)| loaded(path, patch))
            .collect();
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: loaded_files,
            done: true,
        });
        s
    }

    const PATCH: &str = "@@ -1 +1 @@\n-a\n+b\n";

    #[test]
    fn initial_selection_is_the_first_file_in_display_order() {
        let s = session_with(&[("src/b.rs", PATCH), ("src/a.rs", PATCH)]);
        assert_eq!(s.current_file().map(|(i, _)| i), Some(1));
    }

    #[test]
    fn selecting_a_file_rebuilds_rows_and_keeps_the_composer() {
        let mut s = session_with(&[("a.rs", PATCH), ("b.rs", PATCH)]);
        s.composer.body = "wip".into();
        s.composer.target = Some(CommentTarget::File {
            path: "a.rs".into(),
        });
        s.select_file(1);
        assert_eq!(s.current_file().map(|(i, _)| i), Some(1));
        assert_eq!(s.last_opened_file.as_deref(), Some("b.rs"));
        assert_eq!(
            s.row_model
                .rows
                .iter()
                .filter(|r| matches!(r, RowRef::FileHeader { .. }))
                .count(),
            1
        );
        assert!(matches!(
            &s.row_model.rows[s.row_model.file_ranges[1].start],
            RowRef::FileHeader { file: 1 }
        ));
        // The written comment survives the switch untouched (spec S4).
        assert_eq!(s.composer.body, "wip");
        assert!(matches!(
            &s.composer.target,
            Some(CommentTarget::File { path }) if path == "a.rs"
        ));
    }

    #[test]
    fn restores_the_last_opened_file_and_survives_empty_prs() {
        let mut s = session_with(&[("a.rs", PATCH), ("b.rs", PATCH)]);
        s.last_opened_file = Some("b.rs".into());
        assert_eq!(s.current_file().map(|(i, _)| i), Some(1));
        s.rebuild_rows();
        assert!(matches!(
            &s.row_model.rows[s.row_model.file_ranges[1].start],
            RowRef::FileHeader { file: 1 }
        ));

        // A file hidden by the filters falls back to the first visible one.
        s.filters.path_query = "a.rs".into();
        s.rebuild_rows();
        assert_eq!(s.current_file().map(|(i, _)| i), Some(0));

        // An empty PR has no selection and no rows, and stays that way.
        let mut empty = session_with(&[]);
        assert!(empty.current_file().is_none());
        assert!(empty.row_model.rows.is_empty());
        empty.rebuild_rows();
        assert!(empty.row_model.rows.is_empty());
    }

    #[test]
    fn reaching_the_file_cap_flags_the_diff_as_truncated() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        assert!(!s.files_truncated, "a fresh session has loaded nothing");

        // One short of the cap: GitHub returned everything there was.
        let almost: Vec<_> = (0..GITHUB_FILE_CAP - 1)
            .map(|i| loaded(&format!("f{i}.rs"), "@@ -1 +1 @@\n-a\n+b\n"))
            .collect();
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: almost,
            done: false,
        });
        assert!(!s.files_truncated, "one short of the cap is a full diff");

        // Reaching it means the rest of the PR was silently withheld.
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("last.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        assert!(s.files_truncated, "the cap means the diff is incomplete");

        // A refresh re-loads from scratch; the flag waits for the reload.
        s.refresh();
        assert!(!s.files_truncated, "a refresh clears the flag");
    }

    #[test]
    fn commits_load_on_open_checks_on_metadata_and_fold_into_the_session() {
        use crate::github::checks::{CheckRun, CombinedStatus};
        use crate::github::commits::Commit;

        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadCommits { .. })),
            "open asks for the commits"
        );

        let cmds = s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        assert!(
            matches!(
                cmds.iter().find(|c| matches!(c, Command::LoadChecks { .. })),
                Some(Command::LoadChecks { sha, .. }) if sha == "sha1"
            ),
            "the checks ride out of the metadata fold with the head SHA"
        );

        let commit = Commit {
            sha: "c1".into(),
            message: "Fix it".into(),
            author: "me".into(),
            committed_at: "2026-10-01T00:00:00Z".into(),
        };
        s.on_event(&Event::CommitsLoaded {
            session: SessionId(2),
            commits: vec![commit.clone()],
        });
        assert!(s.commits.is_empty(), "another session's reply is dropped");
        s.on_event(&Event::CommitsLoaded {
            session: SessionId(1),
            commits: vec![commit.clone()],
        });
        assert_eq!(s.commits, vec![commit]);

        let run = CheckRun {
            name: "build".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            html_url: None,
        };
        let combined = CombinedStatus {
            state: "success".into(),
            total_count: 1,
        };
        s.on_event(&Event::ChecksLoaded {
            session: SessionId(2),
            runs: vec![run.clone()],
            combined: combined.clone(),
        });
        assert!(
            s.check_runs.is_empty() && s.combined_status.is_none(),
            "another session's reply is dropped"
        );
        s.on_event(&Event::ChecksLoaded {
            session: SessionId(1),
            runs: vec![run.clone()],
            combined: combined.clone(),
        });
        assert_eq!(s.check_runs, vec![run]);
        assert_eq!(s.combined_status.as_ref(), Some(&combined));

        let cmds = s.refresh();
        assert!(
            s.commits.is_empty() && s.check_runs.is_empty() && s.combined_status.is_none(),
            "a refresh drops the commits and checks with the rest"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadCommits { .. })),
            "a refresh re-requests the commits; the checks ride back in on \
             the metadata reply that names the new head"
        );
    }

    #[test]
    fn the_conversation_loads_with_the_session_and_folds_in() {
        use crate::github::conversation::ConversationComment;

        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadConversation { .. })),
            "open asks for the conversation"
        );

        let comment = ConversationComment {
            author: "alice".into(),
            body: "looks good".into(),
            created_at: "2026-10-01T00:00:00Z".into(),
        };
        s.on_event(&Event::ConversationLoaded {
            session: SessionId(2),
            comments: vec![comment.clone()],
        });
        assert!(
            s.conversation.is_empty(),
            "another session's reply is dropped"
        );
        s.on_event(&Event::ConversationLoaded {
            session: SessionId(1),
            comments: vec![comment.clone()],
        });
        assert_eq!(s.conversation, vec![comment]);

        let cmds = s.refresh();
        assert!(s.conversation.is_empty(), "a refresh drops it");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::LoadConversation { .. })),
            "and asks again"
        );
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

    /// A session with metadata and one loaded file, backed by a temporary
    /// config root: `add_pending` saves the draft immediately, and a test
    /// must never write to the real one. (The brief's helper used
    /// `PrSession::open`; `open_at` with a temp root is the same session
    /// minus the real-config-root write, per the rule the `loaded_session`
    /// helper above already follows.)
    fn ready_session(head: &str) -> (PrSession, tempfile::TempDir) {
        let dir = tempdir();
        let (mut s, _) =
            PrSession::open_at(dir.path(), SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta_by("me", head),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        (s, dir)
    }

    #[test]
    fn author_detection() {
        let (s, _dir) = ready_session("sha1");
        assert!(viewer_is_author(&s, "me"));
        assert!(!viewer_is_author(&s, "other"));
    }

    #[test]
    fn failure_keeps_pending_and_success_clears_it() {
        let (mut s, dir) = ready_session("sha1");
        add_pending(&mut s, line_anchor("a.rs", 1), "x".into()).unwrap();
        on_submit_error(&mut s, GhError::Network("down".into()));
        assert_eq!(s.pending.comments.len(), 1); // never lost
        let cmds = on_review_submitted(&mut s, review(ReviewState::Commented));
        assert!(s.pending.is_empty());
        assert_eq!(s.reviews.len(), 1);
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::LoadThreads { .. })));
        assert!(
            session::load_draft(dir.path(), "github.com", &repo(), 7)
                .unwrap()
                .is_none(),
            "the persisted copy goes with the draft"
        );
    }

    #[test]
    fn moved_head_keeps_the_draft_pinned() {
        let (mut s, _dir) = ready_session("sha1");
        add_pending(&mut s, line_anchor("a.rs", 1), "x".into()).unwrap();
        s.refresh();
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta_by("me", "sha2"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+c\n")],
            done: true,
        });
        assert!(head_moved_since_pin(&s));
        assert!(add_pending(&mut s, line_anchor("a.rs", 1), "y".into()).is_err());
        assert_eq!(s.pending.commit_id.as_deref(), Some("sha1"));
    }

    #[test]
    fn unauthorized_never_drops_the_persisted_draft() {
        use crate::auth::session::handle_unauthorized;
        use crate::auth::store::{MemoryStore, TokenStore};
        use crate::persist::settings::{Account, Settings};

        let dir = tempdir();
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "pending body".into(), "sha1")
            .unwrap();
        session::save_draft(dir.path(), "github.com", &repo(), 7, &p).unwrap();

        let store = MemoryStore::default();
        let settings = Settings {
            account: Some(Account {
                login: "me".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        store.set("github.com", "me", "tok").unwrap();
        handle_unauthorized(&store, &settings).unwrap();
        assert_eq!(store.get("github.com", "me").unwrap(), None);
        assert_eq!(
            session::load_draft(dir.path(), "github.com", &repo(), 7)
                .unwrap()
                .unwrap(),
            p
        );
    }

    /// The composer draft round trip: a typed composer survives the debounced
    /// write, a later viewed-toggle write must not clobber it with a stale
    /// in-memory value, and an empty composer stores `None`.
    #[test]
    fn the_composer_draft_survives_a_viewed_toggle_write() {
        use std::time::Duration;

        let (mut s, dir) = loaded_session();
        open_composer(&mut s, "half-written");
        let t0 = Instant::now();
        s.sync_composer_draft(t0);
        s.tick(t0 + Duration::from_millis(300));
        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        let written: ComposerDraft =
            serde_json::from_value(saved.composer_draft.expect("a composer draft"))
                .expect("the written composer draft parses");
        assert_eq!(
            written,
            ComposerDraft {
                target_path: "a.rs".into(),
                anchor: Some(line_anchor("a.rs", 1)),
                body: "half-written".into(),
            },
            "the composer draft round-trips"
        );

        // A viewed toggle's whole-file write must carry the same fresh value.
        let a = crate::test_support::file("a.rs");
        s.viewed.set(&a, true);
        let t1 = Instant::now();
        s.mark_session_dirty(t1);
        s.tick(t1 + Duration::from_millis(300));
        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert!(saved.viewed.is_viewed(&a), "the toggle was written");
        let written: ComposerDraft =
            serde_json::from_value(saved.composer_draft.expect("a composer draft"))
                .expect("the written composer draft parses");
        assert_eq!(
            written.body, "half-written",
            "a viewed write must not clobber the composer draft"
        );

        // An empty composer stores `None`, never `Some(empty)`.
        composer::reduce(&mut s.composer, ComposerMsg::Cancel);
        let t2 = Instant::now();
        s.sync_composer_draft(t2);
        s.tick(t2 + Duration::from_millis(300));
        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert!(
            saved.composer_draft.is_none(),
            "an empty composer stores None"
        );
    }

    #[test]
    fn expanding_a_gap_waits_for_the_head_blob_then_inserts_rows() {
        use crate::diff::expand::Expand;
        use crate::model::diff::RowOrigin;

        let (mut s, _dir) = loaded_session();
        // `loaded_session`'s file is "@@ -1,2 +1,2 @@\n-x\n+y\n z\n", which
        // starts at line 1 and has no gap; replace it with one that does.
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("b.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n")],
            done: true,
        });
        s.select_file(1);
        assert!(s.row_model.rows.contains(&RowRef::Gap {
            file: 1,
            before_hunk: 0
        }));

        let cmds = s.expand_gap(1, 0, Expand::All);
        assert!(
            matches!(&cmds[..], [Command::ExpandContext { file: 1, sha, .. }] if sha == "sha1"),
            "the first click asks the worker for the head blob at the shown head"
        );
        assert!(
            s.expand_gap(1, 0, Expand::All).is_empty(),
            "a second click while the blob is in flight asks once"
        );

        s.on_event(&Event::HeadBlobLoaded {
            session: SessionId(1),
            file: 1,
            text: "l1\nl2\nl3\nb\n c\n".into(),
        });
        let rows: Vec<_> = s.files[1]
            .diff
            .as_ref()
            .expect("the file has a diff")
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .collect();
        let expanded: Vec<_> = rows
            .iter()
            .filter(|r| r.origin == RowOrigin::Expanded)
            .collect();
        assert_eq!(expanded.len(), 3, "the whole gap became real rows");
        assert_eq!(
            (
                expanded[0].old_no,
                expanded[0].new_no,
                expanded[0].text.as_str()
            ),
            (Some(1), Some(1), "l1"),
            "expanded rows carry exact coordinates"
        );
        assert!(
            !s.row_model.rows.contains(&RowRef::Gap {
                file: 1,
                before_hunk: 0
            }),
            "a fully expanded gap is gone"
        );

        // The blob is cached: a later expansion applies without a command.
        assert!(s.expand_gap(1, 0, Expand::All).is_empty());
    }

    #[test]
    fn a_pending_all_upgrades_an_earlier_partial() {
        use crate::diff::expand::Expand;
        use crate::model::diff::RowOrigin;

        let (mut s, _dir) = loaded_session();
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("b.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n")],
            done: true,
        });

        assert!(
            matches!(
                &s.expand_gap(1, 0, Expand::Up(1))[..],
                [Command::ExpandContext { .. }]
            ),
            "the ↑ click asks for the head blob"
        );
        assert!(
            s.expand_gap(1, 0, Expand::All).is_empty(),
            "the later 'all' still asks for the blob only once"
        );

        s.on_event(&Event::HeadBlobLoaded {
            session: SessionId(1),
            file: 1,
            text: "l1\nl2\nl3\nb\n c\n".into(),
        });
        let expanded: Vec<_> = s.files[1]
            .diff
            .as_ref()
            .expect("the file has a diff")
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .filter(|r| r.origin == RowOrigin::Expanded)
            .collect();
        assert_eq!(
            expanded.len(),
            3,
            "the queued ↑ 1 is upgraded to the later 'all', not dropped"
        );
    }

    #[test]
    fn a_repeat_click_widens_the_queued_request() {
        let (mut s, _dir) = loaded_session();
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("b.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n")],
            done: true,
        });

        s.expand_gap(1, 0, Expand::Up(1));
        assert!(s.expand_gap(1, 0, Expand::Up(2)).is_empty());
        assert!(s.expand_gap(1, 0, Expand::All).is_empty());
        assert!(
            s.expand_gap(1, 0, Expand::Up(3)).is_empty(),
            "an 'all' already queued is not narrowed back to ↑ 3"
        );

        s.on_event(&Event::HeadBlobLoaded {
            session: SessionId(1),
            file: 1,
            text: "l1\nl2\nl3\nb\n c\n".into(),
        });
        assert!(
            !s.row_model.rows.contains(&RowRef::Gap {
                file: 1,
                before_hunk: 0
            }),
            "the queued request stayed 'all': the whole gap is revealed"
        );
    }

    #[test]
    fn loading_a_large_diff_caches_the_merge_base_and_installs_the_diff() {
        use crate::github::files::PatchState;
        use crate::model::diff::FileDiff;

        let (mut s, _dir) = loaded_session();
        let mut huge = loaded("huge.json", "@@ -1 +1 @@\n-a\n+b\n");
        huge.file.patch = None;
        huge.file.patch_state = PatchState::TooLarge;
        huge.diff = None;
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![huge],
            done: true,
        });

        let cmds = s.load_large_diff(1);
        assert!(
            matches!(
                &cmds[..],
                [Command::LoadLargeDiff {
                    file: 1,
                    merge_base: None,
                    ..
                }]
            ),
            "the first large file asks with no merge base known yet"
        );

        s.on_event(&Event::LocalDiffReady {
            session: SessionId(1),
            file: 1,
            diff: FileDiff { hunks: Vec::new() },
            merge_base: "mb".into(),
        });
        assert_eq!(s.merge_base.as_deref(), Some("mb"));
        assert!(
            s.files[1].diff.is_some(),
            "the local diff replaces the placeholder"
        );

        let cmds = s.load_large_diff(1);
        assert!(
            matches!(&cmds[..], [Command::LoadLargeDiff { merge_base: Some(sha), .. }] if sha == "mb"),
            "the cached merge base rides along on the next large file"
        );
    }

    #[test]
    fn hide_whitespace_toggle_filters_rows_and_restores_them() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![
                loaded(
                    "a.rs",
                    "@@ -1,2 +1,2 @@\n fn a() {\n-\tlet x = 1;\n+\tlet x = 1;  \n",
                ),
                loaded("b.rs", "@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n"),
            ],
            done: true,
        });
        let before = s.row_model.rows.len();

        s.toggle_hide_whitespace();
        assert!(s.hide_whitespace);
        assert!(
            s.files[0].diff.as_ref().unwrap().hunks.is_empty(),
            "the whitespace-only file keeps no hunks"
        );
        assert_eq!(s.files[1].diff.as_ref().unwrap().hunks.len(), 1);
        assert!(s.row_model.rows.len() < before);

        // A page that arrives while the option is on is filtered too.
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("c.rs", "@@ -1,2 +1,2 @@\n fn c() {\n-\tx;\n+\tx;\n")],
            done: true,
        });
        assert!(s.files[2].diff.as_ref().unwrap().hunks.is_empty());

        s.toggle_hide_whitespace();
        assert!(!s.hide_whitespace);
        assert_eq!(s.files[0].diff.as_ref().unwrap().hunks.len(), 1);
        assert_eq!(s.files[2].diff.as_ref().unwrap().hunks.len(), 1);
        // One file at a time: the rows cover the current file (`a.rs`) only,
        // and it comes back exactly as it was.
        assert_eq!(s.row_model.rows.len(), before);
    }

    /// A binary raster file, as the files API describes a PNG.
    fn png(path: &str) -> crate::github::files::LoadedFile {
        let mut f = loaded(path, "@@ -1 +1 @@\n-a\n+b\n");
        f.file.patch = None;
        f.file.patch_state = crate::github::files::PatchState::Binary;
        f.diff = None;
        f
    }

    #[test]
    fn images_load_once_and_evict_oldest_past_the_cap() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        let pngs: Vec<_> = (0..201u32).map(|i| png(&format!("img{i}.png"))).collect();
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: pngs,
            done: true,
        });

        let cmds = s.load_images(0);
        assert!(
            matches!(&cmds[..], [Command::LoadImages { merge_base: None, head_sha, .. }]
                if head_sha == "sha1"),
            "the command carries the session's (missing) merge base and head"
        );
        assert!(s.load_images(0).is_empty(), "one request per file");
        assert!(s.wants_images(1));
        assert!(!s.wants_images(999));

        let blob = || ImageBlob::Bytes(vec![7u8; 1024 * 1024].into());
        s.on_event(&Event::ImagesLoaded {
            session: SessionId(2),
            file: 0,
            base: ImageBlob::Absent,
            head: blob(),
        });
        assert!(
            s.image_for(0).is_none(),
            "an event for another session is dropped"
        );
        s.on_event(&Event::ImagesLoaded {
            session: SessionId(1),
            file: 0,
            base: ImageBlob::Absent,
            head: blob(),
        });
        assert!(s.image_for(0).is_some());

        // 201 MB of head blobs over the 200 MB cap: the oldest files are
        // gone, their textures are queued for the app to forget, and they
        // may be requested again if they come back on screen.
        for file in 1..201u32 {
            s.on_event(&Event::ImagesLoaded {
                session: SessionId(1),
                file,
                base: ImageBlob::Absent,
                head: blob(),
            });
        }
        assert!(s.image_for(0).is_none());
        assert!(s.image_for(200).is_some());
        assert!(s
            .evicted_images
            .iter()
            .any(|uri| uri.ends_with("/0/head.png")));
        assert!(
            !s.load_images(0).is_empty(),
            "an evicted file can be requested again"
        );
    }

    #[test]
    fn rendered_svg_swaps_the_text_diff_for_the_viewer() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("x.svg", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        assert!(!s.wants_images(0), "an SVG in text mode asks for nothing");

        s.toggle_rendered_svg(0);
        assert!(s.files[0].diff.is_none(), "the text diff is stashed");
        assert!(s.row_model.rows.contains(&RowRef::Placeholder { file: 0 }));
        assert!(s.wants_images(0));
        assert!(matches!(
            &s.load_images(0)[..],
            [Command::LoadImages { .. }]
        ));

        s.toggle_rendered_svg(0);
        assert!(
            s.files[0].diff.is_some(),
            "the stashed text diff comes back"
        );
        assert!(!s.wants_images(0));
        assert!(s
            .row_model
            .rows
            .iter()
            .any(|r| matches!(r, RowRef::Line { .. })));
    }

    /// A patch with one whitespace-only hunk and one real hunk, so the
    /// filtered and original forms differ and either can be told apart.
    const WS_AND_REAL: &str =
        "@@ -1,2 +1,2 @@\n fn a() {\n-\tlet x = 1;\n+\tlet x = 1;  \n@@ -4 +4 @@\n-let y = 1;\n+let y = 2;\n";

    #[test]
    fn hide_whitespace_off_under_a_rendered_svg_restores_the_original_to_text() {
        use crate::diff::parse::parse_patch;

        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        let original = FileDiff {
            hunks: parse_patch(WS_AND_REAL).unwrap(),
        };
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("x.svg", WS_AND_REAL)],
            done: true,
        });

        // Hide on: the whitespace-only hunk is filtered out and the
        // original stashed.
        s.toggle_hide_whitespace();
        assert_eq!(s.files[0].diff.as_ref().unwrap().hunks.len(), 1);

        // Rendered: the filtered diff moves to the svg stash.
        s.toggle_rendered_svg(0);
        assert!(s.files[0].diff.is_none());

        // Hide off while rendered: the original belongs to the stash the
        // "Text" switch restores from, never to the loaded diff — text
        // rows must not reappear under a selected "Rendered" label.
        s.toggle_hide_whitespace();
        assert!(!s.hide_whitespace);
        assert!(
            s.files[0].diff.is_none(),
            "the rendered view keeps the viewer: no diff is installed under it"
        );
        assert!(s.row_model.rows.contains(&RowRef::Placeholder { file: 0 }));
        assert!(
            !s.row_model
                .rows
                .iter()
                .any(|r| matches!(r, RowRef::Line { .. })),
            "no text rows while 'Rendered' is selected"
        );

        // Back to text: the unfiltered original is installed, not the
        // stale filtered copy.
        s.toggle_rendered_svg(0);
        assert_eq!(
            s.files[0].diff.as_ref().unwrap(),
            &original,
            "switching back to 'Text' installs the unfiltered original"
        );
    }

    #[test]
    fn a_local_diff_that_arrives_while_hide_whitespace_is_on_is_filtered() {
        use crate::diff::parse::parse_patch;
        use crate::github::files::PatchState;

        let (mut s, _dir) = loaded_session();
        let mut huge = loaded("huge.json", "@@ -1 +1 @@\n-a\n+b\n");
        huge.file.patch = None;
        huge.file.patch_state = PatchState::TooLarge;
        huge.diff = None;
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![huge],
            done: true,
        });

        s.set_hide_whitespace(true);

        s.on_event(&Event::LocalDiffReady {
            session: SessionId(1),
            file: 1,
            diff: FileDiff {
                hunks: parse_patch(WS_AND_REAL).unwrap(),
            },
            merge_base: "mb".into(),
        });
        assert_eq!(
            s.files[1].diff.as_ref().unwrap().hunks.len(),
            1,
            "the fresh diff is filtered on arrival: its whitespace-only hunk is hidden"
        );

        s.set_hide_whitespace(false);
        assert_eq!(
            s.files[1].diff.as_ref().unwrap().hunks.len(),
            2,
            "turning the option off restores the fresh diff's whitespace hunk too"
        );
    }
}
