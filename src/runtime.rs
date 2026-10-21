//! The tokio runtime and the `Command`/`Event` bridge between the UI and the
//! network.
//!
//! The UI thread never blocks on I/O: it sends [`Command`]s to a [`Worker`] and
//! drains [`Event`]s once per frame. Everything network-shaped happens on the
//! worker's own multi-thread tokio runtime.

use std::future::Future;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::task::JoinError;

#[cfg(unix)]
use secrecy::SecretString;

use crate::auth::DEFAULT_SCOPE;
use crate::auth::device_flow::{AuthFailure, DeviceCode};
use crate::auth::device_http::{HttpDeviceFlow, run_device_flow};
use crate::auth::store::TokenStore;
use crate::diff::image::{ImageBlob, MAX_IMAGE_BYTES};
use crate::diff::local::local_diff;
use crate::github::checks::{CheckRun, CombinedStatus};
use crate::github::client::{GithubClient, Me, endpoints_for};
use crate::github::comments::NewComment;
use crate::github::commits::Commit;
use crate::github::conversation::ConversationComment;
use crate::github::error::GhError;
use crate::github::files::{LoadedFile, PrFile};
use crate::github::graphql::SearchPage;
use crate::github::merge::{MergeResult, RepoSettings};
use crate::github::metadata::PrMetadata;
use crate::github::reviews::{Review, ServerPendingComment, viewer_pending_review};
use crate::model::RepoRef;
use crate::model::diff::FileDiff;
use crate::model::review::ReviewEvent;
use crate::model::thread::{ReviewThread, ThreadComment};

/// Identifies one open PR's session.
///
/// Assigned when a PR is opened and echoed by every PR-scoped [`Event`], so a
/// slow response for a PR the user has left can be dropped (the stale-event
/// rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(pub u64);

/// A unit of work handed from the UI to the worker.
///
/// `AiStart` carries the loaded snapshot inputs; later tasks match them as
/// concrete values, so the enum keeps them unboxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Command {
    /// Round-trip probe used by the bridge tests.
    Ping(u32),
    /// Ask the worker to stop.
    Shutdown,
    /// Start the OAuth device flow against `host` with `client_id`.
    StartDeviceFlow {
        /// The host to authenticate against (`github.com` or Enterprise).
        host: String,
        /// The public OAuth App client ID to use.
        client_id: String,
    },
    /// Validate a pasted personal access token and sign in with it.
    SubmitPat {
        /// The host to authenticate against.
        host: String,
        /// The token the user pasted.
        token: String,
    },
    /// Search open PRs for one query (one scope, one constraint).
    SearchPrs {
        /// The PR-list request generation that issued this search.
        request: u64,
        /// The search query to run.
        query: String,
        /// The `endCursor` to fetch the page after, when paging.
        after: Option<String>,
    },
    /// Load a PR's changed files, one page at a time.
    LoadPrFiles {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Load a PR's metadata (title, refs, head SHA, reviewers, labels).
    LoadPrMetadata {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Load a PR's review threads (spec §7.4).
    LoadThreads {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Post an immediate review comment (spec §7.3).
    PostComment {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The comment to post.
        comment: NewComment,
        /// The UI's local id for the pending comment, echoed by the result
        /// events so the pending bubble can be replaced or rolled back.
        optimistic_id: u64,
    },
    /// Add a comment to the viewer's pending review on GitHub, line or file.
    AddToServerReview {
        /// The session that opened the PR.
        session: SessionId,
        /// The pending review's GraphQL node id.
        review_id: String,
        /// The comment to add.
        comment: NewComment,
        /// The UI's local id for the optimistic bubble, echoed by the result
        /// events exactly as [`Command::PostComment`] does.
        optimistic_id: u64,
    },
    /// Reply to a review thread's root comment (spec §7.4).
    ReplyToThread {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The thread's GraphQL node id, for the result event.
        thread_id: String,
        /// The thread's root comment, the reply endpoint's anchor.
        root_comment_id: u64,
        /// The reply body.
        body: String,
    },
    /// Resolve or unresolve a review thread (spec §7.4).
    SetThreadResolved {
        /// The session that opened the PR.
        session: SessionId,
        /// The thread's GraphQL node id.
        thread_id: String,
        /// The state to move the thread to.
        resolved: bool,
    },
    /// Load a PR's reviews, including the viewer's server-side pending
    /// review (spec §7.2).
    LoadReviews {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Load a PR's commits for the right-pane list (Task 50).
    LoadCommits {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Load one commit's check runs and combined status (Task 50).
    ///
    /// The session asks once the metadata names the head SHA; both endpoints
    /// describe the same rollup, so they load as one command.
    LoadChecks {
        /// The session that asked for the checks.
        session: SessionId,
        /// The repository the commit lives in.
        repo: RepoRef,
        /// The commit SHA to check.
        sha: String,
    },
    /// Load a PR's conversation (issue) comments for the timeline (Task 51).
    LoadConversation {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
    },
    /// Submit a server-side pending review with the given verdict (spec §7.2).
    SubmitServerPending {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The pending review's REST id.
        review_id: u64,
        /// The verdict to submit it with.
        event: ReviewEvent,
        /// The review body.
        body: String,
    },
    /// Discard a server-side pending review, deleting it and its comments.
    DeleteServerPending {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The pending review's REST id.
        review_id: u64,
    },
    /// Fetch a file's head-blob text so the UI can expand an omitted-context
    /// gap from it (Task 45). The expansion itself runs on the UI thread from
    /// the session's per-file blob cache.
    ExpandContext {
        /// The session that asked for the expansion.
        session: SessionId,
        /// The repository the file lives in.
        repo: RepoRef,
        /// The index of the file in the session's file list.
        file: u32,
        /// The file's path, as the contents API needs it.
        path: String,
        /// The head SHA the diff on screen belongs to.
        sha: String,
    },
    /// Diff a file GitHub sent no patch for against its base blob (Task 46).
    ///
    /// The executor fetches the merge base unless the session already knows
    /// it (the merge base is cached per session), reads the base blob at the
    /// merge base and the head blob at the head, and sends the local diff.
    /// A side is `None` when the file has no blob on it: added files skip the
    /// base, removed files skip the head, and renames read the base at
    /// `previous_path`.
    LoadLargeDiff {
        /// The session that asked for the diff.
        session: SessionId,
        /// The index of the file in the session's file list.
        file: u32,
        /// The repository the file lives in.
        repo: RepoRef,
        /// The PR's base SHA, one end of the compare request.
        base_sha: String,
        /// The head SHA the diff on screen belongs to.
        head_sha: String,
        /// The session's cached merge base, when it has one.
        merge_base: Option<String>,
        /// The path to read the base blob at, if this file has a base side.
        base_path: Option<String>,
        /// The path to read the head blob at, if this file has a head side.
        head_path: Option<String>,
    },
    /// Fetch both sides of an image file's diff (Task 49).
    ///
    /// Like [`Command::LoadLargeDiff`], the command carries everything the
    /// stateless executor needs: the base blob is read at the merge base
    /// (fetched when the session has not cached one), the head blob at
    /// `head_sha`, each capped at [`crate::diff::image::MAX_IMAGE_BYTES`].
    /// A side is `None` when the file has no blob on it.
    LoadImages {
        /// The session that asked for the images.
        session: SessionId,
        /// The index of the file in the session's file list.
        file: u32,
        /// The repository the file lives in.
        repo: RepoRef,
        /// The PR's base SHA, one end of the compare request.
        base_sha: String,
        /// The head SHA the diff on screen belongs to.
        head_sha: String,
        /// The session's cached merge base, when it has one.
        merge_base: Option<String>,
        /// The path to read the base blob at, if this file has a base side.
        base_path: Option<String>,
        /// The path to read the head blob at, if this file has a head side.
        head_path: Option<String>,
    },
    /// Submit a review built by [`crate::github::reviews::review_payload`]
    /// (spec §7.2).
    SubmitReview {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The REST body, built once by the session so the contract is fixed
        /// at click time.
        payload: serde_json::Value,
    },
    /// Fetch a repository's merge-method allow flags.
    LoadRepoSettings {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository to ask.
        repo: RepoRef,
    },
    /// Merge a PR with a body built by
    /// [`crate::github::merge::merge_payload`].
    MergePr {
        /// The session that opened the PR.
        session: SessionId,
        /// The repository the PR lives in.
        repo: RepoRef,
        /// The PR number within the repository.
        number: u64,
        /// The REST body, built once by the session so the contract is fixed
        /// at click time.
        payload: serde_json::Value,
    },
    /// Test one configured LLM provider's connection (spec §10).
    ///
    /// Not PR-scoped: the result arrives as [`Event::AiProviderTested`],
    /// keyed by provider name.
    AiTestProvider {
        /// The provider to dial.
        provider: crate::persist::settings::ProviderSettings,
        /// The model to probe with.
        model: crate::persist::settings::ModelSettings,
    },
    /// Attach to a PR's AI review: rebinds any live run to `session` and
    /// answers with [`Event::AiSnapshot`] (spec §10.2). Unix-only with
    /// [`crate::ai`].
    #[cfg(unix)]
    AiAttach {
        /// The session that opened the PR.
        session: SessionId,
        /// The PR the session is looking at.
        key: crate::ai::store::PrKey,
    },
    /// Validate and plan the AI checkout for `pr`, answering with
    /// [`Event::AiCheckoutPlanned`] for the user to confirm (spec §4 step 2).
    #[cfg(unix)]
    AiPlanCheckout {
        /// The session that asked.
        session: SessionId,
        /// The PR the session is looking at.
        key: crate::ai::store::PrKey,
        /// The PR to check out.
        pr: crate::ai::checkout::PrTarget,
    },
    /// Start an AI review run: checkout, prepare, sandbox, snapshot,
    /// pipeline (spec §10.3).
    #[cfg(unix)]
    AiStart {
        /// The session that asked.
        session: SessionId,
        /// The PR the session is looking at.
        key: crate::ai::store::PrKey,
        /// The PR to review.
        pr: crate::ai::checkout::PrTarget,
        /// What the session already loaded for the snapshot.
        snapshot_inputs: crate::ai::hub::SnapshotInputs,
        /// Full diff or incremental re-review.
        mode: crate::ai::pipeline::RunMode,
    },
    /// Answer a parked `ask_user` question (spec §10.4).
    #[cfg(unix)]
    AiAnswer {
        /// The session that answered.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
        /// The [`Event::AiQuestion`] id.
        question_id: u64,
        /// The answer text.
        answer: String,
    },
    /// Decide a parked command approval (spec §8.1).
    #[cfg(unix)]
    AiApprove {
        /// The session that decided.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
        /// The [`Event::AiApprovalRequest`] id.
        approval_id: u64,
        /// The decision.
        decision: crate::ai::interaction::ApprovalDecision,
    },
    /// Decide a parked prepare approval (spec §8.5).
    #[cfg(unix)]
    AiApprovePrepare {
        /// The session that decided.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
        /// The [`Event::AiApprovalRequest`] id.
        approval_id: u64,
        /// Whether the prepare commands may run.
        ok: bool,
    },
    /// Queue a user note for every agent of the live run (spec §10.5).
    #[cfg(unix)]
    AiUserMessage {
        /// The session that sent it.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
        /// The note text.
        text: String,
    },
    /// Hold every running agent before its next model call.
    #[cfg(unix)]
    AiPause {
        /// The session that asked.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
    },
    /// Release an [`Self::AiPause`].
    #[cfg(unix)]
    AiResume {
        /// The session that asked.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
    },
    /// Stop the run; the checkout stays held so a re-run is cheap.
    #[cfg(unix)]
    AiCancel {
        /// The session that asked.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
    },
    /// End the run and restore the checkout (spec §7.3, §10.6).
    /// `discard = None` asks first when the tree is dirty.
    #[cfg(unix)]
    AiFinish {
        /// The session that asked.
        session: SessionId,
        /// The PR the run belongs to.
        key: crate::ai::store::PrKey,
        /// Whether to discard review changes; `None` asks first.
        discard: Option<bool>,
    },
    /// Move an AI finding to a new user state (spec §11).
    #[cfg(unix)]
    AiFindingState {
        /// The session that asked.
        session: SessionId,
        /// The PR the finding belongs to.
        key: crate::ai::store::PrKey,
        /// The finding's row id.
        id: i64,
        /// The state to move it to.
        state: crate::ai::review::FindingState,
        /// The pending-comment link to record with it.
        pending_local_id: Option<u64>,
    },
    /// Restore one unrestored checkout record from the crash-recovery dialog
    /// (spec §7.3). Not PR-scoped: the record names its own repository, and
    /// no run is live for it.
    #[cfg(unix)]
    AiRecoverRestore {
        /// The `ai_checkout` row id to restore, discarding stray changes.
        id: i64,
    },
}

/// A result handed from the worker back to the UI.
///
/// `PrMetadataLoaded` carries the largest payload (`PrMetadata`). Later tasks
/// match it as a concrete value, so the enum keeps it unboxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Event {
    /// Reply to [`Command::Ping`].
    Pong(u32),
    /// A transient message to show as a toast.
    Toast(String),
    /// A worker task panicked; carries the panic message.
    WorkerPanicked(String),
    /// A command failed. `session` is `None` for commands that are not
    /// PR-scoped.
    Failed {
        /// The session that issued the command, when the command was
        /// PR-scoped.
        session: Option<SessionId>,
        /// The classified failure.
        error: GhError,
    },
    /// The device flow returned a code for the user to enter.
    DeviceCode(DeviceCode),
    /// Sign-in succeeded and `me` was validated with `GET /user`.
    LoggedIn {
        /// The authenticated account.
        me: Me,
        /// The access token (or PAT) that produced it.
        token: String,
    },
    /// Sign-in could not complete.
    AuthFailed(AuthFailure),
    /// A PR search page arrived.
    PrSearchPage {
        /// The request generation this page belongs to.
        request: u64,
        /// The query that produced it.
        query: String,
        /// The parsed page.
        page: SearchPage,
    },
    /// A PR search failed.
    PrSearchFailed {
        /// The request generation that failed.
        request: u64,
        /// The classified failure.
        error: GhError,
    },
    /// One page of a PR's changed files arrived.
    PrFilesPage {
        /// The session that asked for the files.
        session: SessionId,
        /// The files on this page, with their patches parsed.
        files: Vec<LoadedFile>,
        /// Whether this was the last page.
        done: bool,
    },
    /// A PR's metadata arrived.
    PrMetadataLoaded {
        /// The session that asked for the metadata.
        session: SessionId,
        /// The PR's metadata.
        meta: PrMetadata,
    },
    /// A PR's review threads arrived.
    ThreadsLoaded {
        /// The session that asked for the threads.
        session: SessionId,
        /// The threads, in API order, each with all its comments.
        threads: Vec<ReviewThread>,
    },
    /// An immediate comment was posted; `optimistic_id` names the pending
    /// bubble it replaces.
    CommentPosted {
        /// The session that posted the comment.
        session: SessionId,
        /// The pending comment this result belongs to.
        optimistic_id: u64,
        /// The comment as GitHub created it.
        comment: ThreadComment,
    },
    /// An immediate comment failed; the pending bubble is rolled back.
    CommentFailed {
        /// The session that posted the comment.
        session: SessionId,
        /// The pending comment this failure belongs to.
        optimistic_id: u64,
        /// The classified failure.
        error: GhError,
    },
    /// A thread reply arrived; it belongs to the thread named by `thread_id`.
    ReplyPosted {
        /// The session that sent the reply.
        session: SessionId,
        /// The thread the reply was posted to.
        thread_id: String,
        /// The reply as GitHub created it.
        comment: ThreadComment,
    },
    /// A thread's resolved state changed, as GitHub confirmed it.
    ThreadResolved {
        /// The session that asked for the change.
        session: SessionId,
        /// The thread that changed.
        thread_id: String,
        /// The state the thread is now in.
        resolved: bool,
    },
    /// A PR's reviews arrived, with the viewer's server-side pending review
    /// and its comments split out when they have one (spec §7.2).
    ReviewsLoaded {
        /// The session that asked for the reviews.
        session: SessionId,
        /// The reviews, in API order.
        reviews: Vec<Review>,
        /// The viewer's pending review and its comments, when they have one.
        server_pending: Option<(Review, Vec<ServerPendingComment>)>,
    },
    /// A server-side pending review was submitted or discarded; the session
    /// asks for the reviews again to see the result.
    ServerPendingResolved {
        /// The session that asked for the change.
        session: SessionId,
    },
    /// A PR's commits arrived (Task 50).
    CommitsLoaded {
        /// The session that asked for the commits.
        session: SessionId,
        /// The commits, in API order.
        commits: Vec<Commit>,
    },
    /// A commit's check runs and combined status arrived (Task 50).
    ChecksLoaded {
        /// The session that asked for the checks.
        session: SessionId,
        /// The check runs, in API order.
        runs: Vec<CheckRun>,
        /// The legacy combined status beside them.
        combined: CombinedStatus,
    },
    /// A PR's conversation comments arrived (Task 51).
    ConversationLoaded {
        /// The session that asked for the conversation.
        session: SessionId,
        /// The comments, oldest first.
        comments: Vec<ConversationComment>,
    },
    /// A review was submitted; the session clears its draft and reloads the
    /// threads and reviews so the new comments are anchored (spec §7.2).
    ReviewSubmitted {
        /// The session that submitted the review.
        session: SessionId,
        /// The review as GitHub created it.
        review: Review,
    },
    /// Submitting a review failed; the draft is kept intact (spec §11).
    ReviewSubmitFailed {
        /// The session that tried to submit.
        session: SessionId,
        /// The classified failure.
        error: GhError,
    },
    /// A repository's merge-method allow flags arrived.
    RepoSettingsLoaded {
        /// The session that asked.
        session: SessionId,
        /// The allow flags.
        settings: RepoSettings,
    },
    /// The PR was merged; the session reloads metadata and reviews so the
    /// header shows the merged state.
    PrMerged {
        /// The session that merged the PR.
        session: SessionId,
        /// What GitHub reported.
        result: MergeResult,
    },
    /// Merging failed; GitHub's message is preserved for the toast.
    MergeFailed {
        /// The session that tried to merge.
        session: SessionId,
        /// The classified failure.
        error: GhError,
    },
    /// A file's head-blob text arrived; the session caches it per file and
    /// applies any expansion that was waiting for it (Task 45).
    HeadBlobLoaded {
        /// The session that asked for the blob.
        session: SessionId,
        /// The index of the file in the session's file list.
        file: u32,
        /// The blob's text.
        text: String,
    },
    /// A locally computed diff for a too-large file arrived, along with the
    /// merge base it was computed against so the session can cache it for
    /// the next such file (Task 46).
    LocalDiffReady {
        /// The session that asked for the diff.
        session: SessionId,
        /// The index of the file in the session's file list.
        file: u32,
        /// The local diff, every row `origin: Local`.
        diff: FileDiff,
        /// The merge base the base blob was read at.
        merge_base: String,
    },
    /// Both sides of an image file's diff arrived (Task 49). A side is
    /// `Absent` when the file has no blob there or its fetch failed (the
    /// failure rode along as its own [`Event::Toast`], so one bad blob
    /// never fails the session), and `TooLarge` when it is over the cap.
    ImagesLoaded {
        /// The session that asked for the images.
        session: SessionId,
        /// The index of the file in the session's file list.
        file: u32,
        /// The base side, read at the merge base.
        base: ImageBlob,
        /// The head side, read at the diff's head SHA.
        head: ImageBlob,
    },
    /// The result of a [`Command::AiTestProvider`]: `Ok` carries the model
    /// that answered, `Err` a display-safe message (the API key is never in
    /// it).
    AiProviderTested {
        /// The [`crate::persist::settings::ProviderSettings::name`] that was
        /// tested.
        provider: String,
        /// The model that served the probe, or why the probe failed.
        result: Result<String, String>,
    },
    /// A PR's full AI state, replayed on attach and after a run starts
    /// (spec §10.3).
    #[cfg(unix)]
    AiSnapshot {
        /// The session that asked.
        session: SessionId,
        /// The state to rebuild the AI tab from.
        snapshot: crate::ai::hub::AiSnapshotData,
    },
    /// A checkout plan is ready for the user to confirm (spec §4 step 2).
    #[cfg(unix)]
    AiCheckoutPlanned {
        /// The session that asked.
        session: SessionId,
        /// The plan to show.
        plan: crate::ai::checkout::CheckoutPlan,
    },
    /// A checkout, prepare, or run-start step failed; `message` is safe to
    /// show.
    #[cfg(unix)]
    AiCheckoutFailed {
        /// The session that asked.
        session: SessionId,
        /// Why it failed.
        message: String,
    },
    /// The plan found another PR still holding the repository's checkout
    /// (spec §4 step 7); the UI offers to finish that review first.
    #[cfg(unix)]
    AiCheckoutHeld {
        /// The session that asked.
        session: SessionId,
        /// The holding PR's number.
        pr_number: u64,
    },
    /// A notice for the AI tab's timeline: the state a left-as-is checkout
    /// was stopped in (spec §7.3 step 1), folded into a
    /// [`TimelineEntry::Notice`](crate::state::ai_review::TimelineEntry::Notice).
    #[cfg(unix)]
    AiNotice {
        /// The session the notice belongs to.
        session: SessionId,
        /// The text to show.
        text: String,
    },
    /// The prepare stage's command outputs (spec §8.5).
    #[cfg(unix)]
    AiPrepareOutput {
        /// The session that started the run.
        session: SessionId,
        /// One entry per prepare command, in order.
        outputs: Vec<crate::ai::checkout::PrepareOutput>,
    },
    /// A pipeline event forwarded verbatim (spec §10.3).
    #[cfg(unix)]
    AiPipeline {
        /// The run's current session, rebound by [`Command::AiAttach`].
        session: SessionId,
        /// The pipeline event.
        event: crate::ai::events::PipelineEvent,
    },
    /// An agent asked the user something; answer with [`Command::AiAnswer`]
    /// (spec §10.4).
    #[cfg(unix)]
    AiQuestion {
        /// The run's current session.
        session: SessionId,
        /// The id the answer carries.
        id: u64,
        /// The asking agent.
        agent: crate::ai::tools::AgentRef,
        /// The question text.
        text: String,
        /// Quick-reply choices; empty means free text.
        options: Vec<String>,
    },
    /// A command or the prepare stage needs approval; decide with
    /// [`Command::AiApprove`] / [`Command::AiApprovePrepare`] (spec §10.7).
    #[cfg(unix)]
    AiApprovalRequest {
        /// The run's current session.
        session: SessionId,
        /// The id the decision carries.
        id: u64,
        /// The asking agent.
        agent: crate::ai::tools::AgentRef,
        /// What is being asked.
        kind: crate::ai::ui_interaction::ApprovalKind,
    },
    /// A chat-stream event (spec §10.5, Task 7).
    #[cfg(unix)]
    AiChat {
        /// The run's current session.
        session: SessionId,
        /// The chat event.
        event: crate::ai::hub::ChatEvent,
    },
    /// A finish found changes the review made; the user must decide to keep
    /// or discard them (spec §7.3).
    #[cfg(unix)]
    AiRestoreNeedsDecision {
        /// The session that asked to finish.
        session: SessionId,
        /// `git status --porcelain` lines for the changes.
        changes: Vec<String>,
    },
    /// The checkout was restored; the run is closed (spec §7.3).
    #[cfg(unix)]
    AiRestored {
        /// The session that asked to finish.
        session: SessionId,
        /// How the restore went.
        outcome: crate::ai::checkout::RestoreOutcome,
    },
    /// Checkouts a previous session never restored, sent once at startup
    /// (spec §7.3). Not PR-scoped: the UI matches them to sessions by path.
    #[cfg(unix)]
    AiUnrestored(Vec<crate::ai::store::CheckoutRecord>),
}

/// Sends [`Event`]s from worker tasks to the UI thread.
///
/// Cloning is cheap; every clone shares the same channel and repaint callback.
#[derive(Clone)]
pub struct EventSender {
    tx: Sender<Event>,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EventSender {
    /// Send an event, then request a repaint so the UI picks it up promptly.
    ///
    /// A send failure just means the receiver is gone (the app is shutting
    /// down), so it is ignored.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(repaint) = &self.repaint {
            repaint();
        }
    }

    /// Attach the callback invoked after every [`EventSender::send`].
    ///
    /// The app passes `ctx.request_repaint()`; the default (no callback) is
    /// used by tests that poll the channel directly.
    pub fn with_repaint(mut self, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.repaint = Some(repaint);
        self
    }
}

/// Receives [`Event`]s on the UI thread.
pub struct EventReceiver {
    rx: Receiver<Event>,
}

impl EventReceiver {
    /// Take everything queued since the last call, without blocking.
    ///
    /// Called once per frame by the app.
    pub fn drain(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Wait up to `timeout` for a single event.
    ///
    /// Returns `None` on timeout or when every sender has been dropped.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// Create a connected [`EventSender`]/[`EventReceiver`] pair.
pub fn event_channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel();
    (EventSender { tx, repaint: None }, EventReceiver { rx })
}

/// A handle to the background worker.
///
/// Dropping the last `Worker` closes the command channel; the worker thread
/// then finishes and its runtime is torn down.
pub struct Worker {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl Worker {
    /// Queue a command. Silently ignored if the worker has already stopped.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Start the worker: a background thread owning a multi-thread tokio runtime.
///
/// Each command runs as its own task, so a slow command never blocks the ones
/// behind it. A task that panics is turned into [`Event::WorkerPanicked`]
/// rather than taking down the runtime.
pub fn spawn_worker<H, F>(events: EventSender, handler: H) -> Worker
where
    H: Fn(Command, EventSender) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the worker's tokio runtime");

    std::thread::Builder::new()
        .name("gh-review-worker".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                while let Some(command) = command_rx.recv().await {
                    let events = events.clone();
                    let task = tokio::spawn(handler(command, events.clone()));
                    tokio::spawn(async move {
                        if let Err(join_error) = task.await
                            && join_error.is_panic()
                        {
                            events.send(Event::WorkerPanicked(panic_message(join_error)));
                        }
                    });
                }
            });
        })
        .expect("failed to spawn the worker thread");

    Worker {
        commands: command_tx,
    }
}

/// Extract a printable message from a panicking task's payload.
fn panic_message(error: JoinError) -> String {
    let payload = error.into_panic();
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_owned(),
            Err(_) => "worker task panicked".to_owned(),
        },
    }
}

/// Shared state every executor arm needs: the HTTP client (once signed in),
/// the token store, and the AI hub.
pub struct ExecCtx {
    /// The installed GitHub client, or `None` before sign-in.
    pub client: RwLock<Option<GithubClient>>,
    /// The OS keychain, shared with the UI thread.
    pub store: Arc<dyn TokenStore>,
    /// The AI review hub (spec §10.2). Unix-only with [`crate::ai`].
    #[cfg(unix)]
    pub ai: crate::ai::hub::AiHub,
}

impl ExecCtx {
    /// A context with no client installed yet and a throwaway in-memory AI
    /// store — right for tests; the app uses [`Self::with_ai`] so runs
    /// persist.
    pub fn new(store: Arc<dyn TokenStore>) -> ExecCtx {
        ExecCtx {
            client: RwLock::new(None),
            store: store.clone(),
            #[cfg(unix)]
            ai: crate::ai::hub::AiHub::new(
                Arc::new(
                    crate::ai::store::Store::open_in_memory()
                        .expect("an in-memory AI store always opens"),
                ),
                store,
            ),
        }
    }

    /// A context whose AI hub is `ai` (the app opens the real database).
    #[cfg(unix)]
    pub fn with_ai(store: Arc<dyn TokenStore>, ai: crate::ai::hub::AiHub) -> ExecCtx {
        ExecCtx {
            client: RwLock::new(None),
            store,
            ai,
        }
    }

    /// Install the client every later command uses.
    pub fn install_client(&self, client: GithubClient) {
        *self.write_client() = Some(client);
    }

    /// Forget the installed client (sign-out, and after a 401).
    pub fn clear_client(&self) {
        *self.write_client() = None;
    }

    /// A clone of the installed client, or `None` before sign-in.
    ///
    /// Cloning is cheap: the underlying connection pool is refcounted.
    pub fn client(&self) -> Option<GithubClient> {
        self.client
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The client lock for writing, recovered even if a holder panicked.
    fn write_client(&self) -> std::sync::RwLockWriteGuard<'_, Option<GithubClient>> {
        self.client
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// The single place where [`Command`]s become network calls.
///
/// Every arm reports progress and failure through `events`; nothing here
/// touches the UI, the settings file, or drafts.
pub async fn execute(ctx: Arc<ExecCtx>, cmd: Command, events: EventSender) {
    // The startup checkout-recovery event, at most once (spec §7.3).
    #[cfg(unix)]
    ctx.ai.drain_startup(&events);
    match cmd {
        Command::Ping(n) => events.send(Event::Pong(n)),
        Command::Shutdown => {}
        Command::StartDeviceFlow { host, client_id } => {
            let flow = HttpDeviceFlow::new(&endpoints_for(&host).web);
            let code_events = events.clone();
            let result = run_device_flow(&flow, &client_id, DEFAULT_SCOPE, |code| {
                code_events.send(Event::DeviceCode(code.clone()));
            })
            .await;

            match result {
                Ok(token) => validate_and_sign_in(&ctx, &host, token, &events).await,
                Err(failure) => events.send(Event::AuthFailed(failure)),
            }
        }
        Command::SubmitPat { host, token } => {
            validate_and_sign_in(&ctx, &host, token, &events).await;
        }
        Command::SearchPrs {
            request,
            query,
            after,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::PrSearchFailed {
                    request,
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.search_prs(&query, after.as_deref()).await {
                Ok(page) => events.send(Event::PrSearchPage {
                    request,
                    query,
                    page,
                }),
                Err(error) => events.send(Event::PrSearchFailed { request, error }),
            }
        }
        Command::LoadPrFiles {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // One request per page, following `Link: rel="next"`; each page is
            // parsed and sent as it arrives, so the UI can render while the
            // remaining pages are still loading.
            let mut next = Some(format!(
                "/repos/{}/{}/pulls/{number}/files?per_page=100",
                repo.owner, repo.name
            ));
            while let Some(path) = next {
                match client.get_json::<Vec<PrFile>>(&path).await {
                    Ok((page, link)) => {
                        let files = page.into_iter().map(LoadedFile::parse).collect();
                        events.send(Event::PrFilesPage {
                            session,
                            files,
                            done: link.is_none(),
                        });
                        next = link;
                    }
                    Err(error) => {
                        events.send(Event::Failed {
                            session: Some(session),
                            error,
                        });
                        return;
                    }
                }
            }
        }
        Command::LoadPrMetadata {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            let path = format!("/repos/{}/{}/pulls/{number}", repo.owner, repo.name);
            match client.get_json::<PrMetadata>(&path).await {
                Ok((meta, _link)) => events.send(Event::PrMetadataLoaded { session, meta }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::LoadThreads {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.review_threads(&repo, number).await {
                Ok(threads) => events.send(Event::ThreadsLoaded { session, threads }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::PostComment {
            session,
            repo,
            number,
            comment,
            optimistic_id,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::CommentFailed {
                    session,
                    optimistic_id,
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A mutation: sent once, never retried; the optimistic bubble in
            // the UI is replaced or rolled back by these two events.
            match client.post_review_comment(&repo, number, &comment).await {
                Ok(comment) => events.send(Event::CommentPosted {
                    session,
                    optimistic_id,
                    comment,
                }),
                Err(error) => events.send(Event::CommentFailed {
                    session,
                    optimistic_id,
                    error,
                }),
            }
        }
        Command::AddToServerReview {
            session,
            review_id,
            comment,
            optimistic_id,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::CommentFailed {
                    session,
                    optimistic_id,
                    error: GhError::Unauthorized,
                });
                return;
            };
            // A mutation, sent once; the same two events as an immediate
            // post replace or roll back the optimistic bubble.
            match client.add_review_thread(&review_id, &comment).await {
                Ok(comment) => events.send(Event::CommentPosted {
                    session,
                    optimistic_id,
                    comment,
                }),
                Err(error) => events.send(Event::CommentFailed {
                    session,
                    optimistic_id,
                    error,
                }),
            }
        }
        Command::ReplyToThread {
            session,
            repo,
            number,
            thread_id,
            root_comment_id,
            body,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::CommentFailed {
                    session,
                    optimistic_id: NO_OPTIMISTIC_BUBBLE,
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client
                .reply_to_comment(&repo, number, root_comment_id, &body)
                .await
            {
                Ok(comment) => events.send(Event::ReplyPosted {
                    session,
                    thread_id,
                    comment,
                }),
                Err(error) => events.send(Event::CommentFailed {
                    session,
                    optimistic_id: NO_OPTIMISTIC_BUBBLE,
                    error,
                }),
            }
        }
        Command::SetThreadResolved {
            session,
            thread_id,
            resolved,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.set_thread_resolved(&thread_id, resolved).await {
                Ok(is_resolved) => events.send(Event::ThreadResolved {
                    session,
                    thread_id,
                    resolved: is_resolved,
                }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::LoadReviews {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // The viewer's login is what names their own pending review, so
            // the list is not usable without it; `me()` is a cheap read.
            let me = match client.me().await {
                Ok(me) => me,
                Err(error) => {
                    events.send(Event::Failed {
                        session: Some(session),
                        error,
                    });
                    return;
                }
            };
            let reviews = match client.list_reviews(&repo, number).await {
                Ok(reviews) => reviews,
                Err(error) => {
                    events.send(Event::Failed {
                        session: Some(session),
                        error,
                    });
                    return;
                }
            };
            // A pending review is useless without its comments, so a failure
            // to fetch them fails the load like any other part of it.
            let server_pending = match viewer_pending_review(&reviews, &me.user.login) {
                Some(pending) => {
                    match client
                        .pending_review_comments(&repo, number, pending.id)
                        .await
                    {
                        Ok(comments) => Some((pending.clone(), comments)),
                        Err(error) => {
                            events.send(Event::Failed {
                                session: Some(session),
                                error,
                            });
                            return;
                        }
                    }
                }
                None => None,
            };
            events.send(Event::ReviewsLoaded {
                session,
                reviews,
                server_pending,
            });
        }
        Command::LoadCommits {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.list_commits(&repo, number).await {
                Ok(commits) => events.send(Event::CommitsLoaded { session, commits }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::LoadChecks { session, repo, sha } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // Both endpoints describe the same rollup, so one failing means
            // the rollup would be a lie; report it like any other load.
            let runs = match client.list_check_runs(&repo, &sha).await {
                Ok(runs) => runs,
                Err(error) => {
                    events.send(Event::Failed {
                        session: Some(session),
                        error,
                    });
                    return;
                }
            };
            let combined = match client.combined_status(&repo, &sha).await {
                Ok(combined) => combined,
                Err(error) => {
                    events.send(Event::Failed {
                        session: Some(session),
                        error,
                    });
                    return;
                }
            };
            events.send(Event::ChecksLoaded {
                session,
                runs,
                combined,
            });
        }
        Command::LoadConversation {
            session,
            repo,
            number,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.list_issue_comments(&repo, number).await {
                Ok(comments) => events.send(Event::ConversationLoaded { session, comments }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::SubmitServerPending {
            session,
            repo,
            number,
            review_id,
            event,
            body,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A mutation: sent once, never retried.
            match client
                .submit_server_pending(&repo, number, review_id, event, &body)
                .await
            {
                Ok(_) => events.send(Event::ServerPendingResolved { session }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::DeleteServerPending {
            session,
            repo,
            number,
            review_id,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A mutation: sent once, never retried.
            match client.delete_server_pending(&repo, number, review_id).await {
                Ok(()) => events.send(Event::ServerPendingResolved { session }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::SubmitReview {
            session,
            repo,
            number,
            payload,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::ReviewSubmitFailed {
                    session,
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A mutation: sent once, never retried; the draft survives until
            // GitHub confirms (spec §11).
            match client.submit_review(&repo, number, &payload).await {
                Ok(review) => events.send(Event::ReviewSubmitted { session, review }),
                Err(error) => events.send(Event::ReviewSubmitFailed { session, error }),
            }
        }
        Command::LoadRepoSettings { session, repo } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            match client.repo_settings(&repo).await {
                Ok(settings) => events.send(Event::RepoSettingsLoaded { session, settings }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::MergePr {
            session,
            repo,
            number,
            payload,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::MergeFailed {
                    session,
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A mutation: sent once, never retried.
            match client.merge_pr(&repo, number, &payload).await {
                Ok(result) => events.send(Event::PrMerged { session, result }),
                Err(error) => events.send(Event::MergeFailed { session, error }),
            }
        }
        Command::AiTestProvider { provider, model } => {
            // Not PR-scoped, so no SessionId: the event names the provider.
            // Reads only, and the client applies its own retry policy.
            let name = provider.name.clone();
            #[cfg(unix)]
            {
                let result = match crate::auth::store::llm_key(&*ctx.store, &name) {
                    Ok(Some(key)) => {
                        match crate::ai::llm::build_client(&provider, SecretString::from(key)) {
                            Ok(client) => crate::ai::llm::test_connection(&client, &model).await,
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    Ok(None) => Err(format!("no API key stored for {name}")),
                    Err(error) => Err(error.to_string()),
                };
                events.send(Event::AiProviderTested {
                    provider: name,
                    result,
                });
            }
            // The LLM stack is Unix-only for now (see `crate::ai`); the
            // command still answers so the Settings UI never hangs.
            #[cfg(not(unix))]
            {
                let _ = (provider, model);
                events.send(Event::AiProviderTested {
                    provider: name,
                    result: Err("AI review is not available on this platform".to_owned()),
                });
            }
        }
        #[cfg(unix)]
        Command::AiAttach { session, key } => {
            ctx.ai.attach(session, key, &events);
        }
        #[cfg(unix)]
        Command::AiPlanCheckout { session, key, pr } => {
            ctx.ai.plan_checkout(session, key, pr, &events).await;
        }
        #[cfg(unix)]
        Command::AiStart {
            session,
            key,
            pr,
            snapshot_inputs,
            mode,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };
            ctx.ai
                .start(client, session, key, pr, snapshot_inputs, mode, &events)
                .await;
        }
        #[cfg(unix)]
        Command::AiAnswer {
            session: _,
            key,
            question_id,
            answer,
        } => ctx.ai.answer(&key, question_id, answer),
        #[cfg(unix)]
        Command::AiApprove {
            session: _,
            key,
            approval_id,
            decision,
        } => ctx.ai.approve(&key, approval_id, decision),
        #[cfg(unix)]
        Command::AiApprovePrepare {
            session: _,
            key,
            approval_id,
            ok,
        } => ctx.ai.approve_prepare(&key, approval_id, ok),
        #[cfg(unix)]
        Command::AiUserMessage { session, key, text } => {
            ctx.ai.user_message(session, &key, text, &events);
        }
        #[cfg(unix)]
        Command::AiPause { session: _, key } => ctx.ai.pause(&key, &events),
        #[cfg(unix)]
        Command::AiResume { session: _, key } => ctx.ai.resume(&key, &events),
        #[cfg(unix)]
        Command::AiCancel { session: _, key } => ctx.ai.cancel(&key, &events),
        #[cfg(unix)]
        Command::AiFinish {
            session,
            key,
            discard,
        } => {
            ctx.ai.finish(session, &key, discard, &events).await;
        }
        #[cfg(unix)]
        Command::AiFindingState {
            session,
            key: _,
            id,
            state,
            pending_local_id,
        } => ctx
            .ai
            .finding_state(session, id, state, pending_local_id, &events),
        #[cfg(unix)]
        Command::AiRecoverRestore { id } => {
            ctx.ai.recover_restore(id, &events).await;
        }
        Command::ExpandContext {
            session,
            repo,
            file,
            path,
            sha,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // A read of the head blob; the gap itself is expanded on the UI
            // thread from the session's per-file cache (Task 45).
            match client.file_content(&repo, &path, &sha).await {
                Ok(text) => events.send(Event::HeadBlobLoaded {
                    session,
                    file,
                    text,
                }),
                Err(error) => events.send(Event::Failed {
                    session: Some(session),
                    error,
                }),
            }
        }
        Command::LoadLargeDiff {
            session,
            file,
            repo,
            base_sha,
            head_sha,
            merge_base,
            base_path,
            head_path,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // The merge base is cached per session: the command carries it
            // when the session has one, and the event hands it back for the
            // next too-large file.
            let merge_base = match merge_base {
                Some(sha) => sha,
                None => match client.merge_base(&repo, &base_sha, &head_sha).await {
                    Ok(sha) => sha,
                    Err(error) => {
                        events.send(Event::Failed {
                            session: Some(session),
                            error,
                        });
                        return;
                    }
                },
            };
            // A missing side is not a failure: an added file has no base
            // blob and a removed file no head blob, and diffing against the
            // empty string is exactly what GitHub shows.
            let base = match &base_path {
                Some(path) => match client.file_content(&repo, path, &merge_base).await {
                    Ok(text) => text,
                    Err(error) => {
                        events.send(Event::Failed {
                            session: Some(session),
                            error,
                        });
                        return;
                    }
                },
                None => String::new(),
            };
            let head = match &head_path {
                Some(path) => match client.file_content(&repo, path, &head_sha).await {
                    Ok(text) => text,
                    Err(error) => {
                        events.send(Event::Failed {
                            session: Some(session),
                            error,
                        });
                        return;
                    }
                },
                None => String::new(),
            };

            let diff = local_diff(&base, &head, LOCAL_DIFF_CONTEXT);
            events.send(Event::LocalDiffReady {
                session,
                file,
                diff,
                merge_base,
            });
        }
        Command::LoadImages {
            session,
            file,
            repo,
            base_sha,
            head_sha,
            merge_base,
            base_path,
            head_path,
        } => {
            let Some(client) = ctx.client() else {
                events.send(Event::Failed {
                    session: Some(session),
                    error: GhError::Unauthorized,
                });
                return;
            };

            // Reads only, so the shared read-retry applies. A side that
            // cannot be fetched must not fail the session the way a files
            // page would: the file falls back to its placeholder and the
            // reviewer sees a toast instead (Task 49).
            let merge_base = match merge_base {
                Some(sha) => Some(sha),
                None => match client.merge_base(&repo, &base_sha, &head_sha).await {
                    Ok(sha) => Some(sha),
                    Err(error) => {
                        events.send(Event::Toast(format!(
                            "could not find the merge base for the image diff: {error}"
                        )));
                        None
                    }
                },
            };
            let base = match (&base_path, &merge_base) {
                (Some(path), Some(sha)) => {
                    fetch_image_side(&client, &repo, path, sha, &events).await
                }
                _ => ImageBlob::Absent,
            };
            let head = match &head_path {
                Some(path) => fetch_image_side(&client, &repo, path, &head_sha, &events).await,
                None => ImageBlob::Absent,
            };
            events.send(Event::ImagesLoaded {
                session,
                file,
                base,
                head,
            });
        }
    }
}

/// Unchanged lines kept around each change in a locally computed diff —
/// GitHub's own context width for the patches it sends.
const LOCAL_DIFF_CONTEXT: usize = 3;

/// Fetch one side of an image diff, capped at [`MAX_IMAGE_BYTES`].
///
/// Over the cap the blob was never buffered and becomes `TooLarge`; a
/// failed fetch becomes `Absent` with a toast, so one bad side still leaves
/// the other side (and the session) intact (Task 49).
async fn fetch_image_side(
    client: &GithubClient,
    repo: &RepoRef,
    path: &str,
    sha: &str,
    events: &EventSender,
) -> ImageBlob {
    match client.file_bytes(repo, path, sha, MAX_IMAGE_BYTES).await {
        Ok(Some(bytes)) => ImageBlob::Bytes(bytes.into()),
        Ok(None) => ImageBlob::TooLarge,
        Err(error) => {
            events.send(Event::Toast(format!("could not load {path}: {error}")));
            ImageBlob::Absent
        }
    }
}

/// The [`Event::CommentFailed`] id for a failure that has no pending bubble
/// to roll back — a thread reply, whose composer reports the failure itself.
pub(crate) const NO_OPTIMISTIC_BUBBLE: u64 = 0;

/// Prove a fresh credential with `GET /user`, then install it and announce it.
///
/// The token is only stored and the client only installed after `me()` succeeds,
/// so a bad credential leaves the app in its previous state. The UI thread still
/// owns the settings bookkeeping ([`crate::auth::session::complete_login`]),
/// which it performs when it receives [`Event::LoggedIn`].
async fn validate_and_sign_in(ctx: &ExecCtx, host: &str, token: String, events: &EventSender) {
    let client = GithubClient::new(host, &token);
    let me = match client.me().await {
        Ok(me) => me,
        Err(error) => {
            events.send(Event::AuthFailed(AuthFailure::Http(error.to_string())));
            return;
        }
    };

    if let Err(error) = ctx.store.set(host, &me.user.login, &token) {
        tracing::warn!(%error, "could not store the token in the keychain");
        events.send(Event::Toast(format!("could not save the token: {error}")));
    }

    ctx.install_client(client);
    events.send(Event::LoggedIn { me, token });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn command_round_trips_through_worker() {
        let (tx, rx) = event_channel();
        let worker = spawn_worker(tx, |cmd, events| async move {
            if let Command::Ping(n) = cmd {
                events.send(Event::Pong(n));
            }
        });
        worker.send(Command::Ping(7));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Some(Event::Pong(7))
        ));
    }

    #[test]
    fn handler_panic_becomes_an_event() {
        let (tx, rx) = event_channel();
        let worker = spawn_worker(tx, |_cmd, _events| async move { panic!("boom") });
        worker.send(Command::Ping(1));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Some(Event::WorkerPanicked(_))
        ));
    }

    #[test]
    fn sending_requests_a_repaint() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let (tx, _rx) = event_channel();
        let tx = tx.with_repaint(Arc::new(move || {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        tx.send(Event::Pong(1));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
