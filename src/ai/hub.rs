//! The worker-side AI hub: one place that owns live runs and turns AI
//! [`Command`](crate::runtime::Command)s into [`Event`](crate::runtime::Event)s
//! (spec §10.2, §10.3).
//!
//! The UI never touches the pipeline directly. It attaches to a PR
//! ([`AiHub::attach`], which rebinds the run's [`SessionId`] and replays
//! [`Event::AiSnapshot`]), plans and confirms a checkout, starts a run, and
//! answers the questions and approvals the agents raise. Everything the hub
//! emits for a live run carries the run's *current* session, read when the
//! event is sent, so the stale-event rule survives tab switches (spec §10.2).
//!
//! A run holds its PR's checkout until [`AiHub::finish`] restores it — with a
//! decision prompt when the tree picked up changes (spec §7.3). Cancelling
//! stops the run but keeps the checkout, so the user can re-run without
//! touching the working tree again.
//!
//! Everything here is Unix-only with the rest of `crate::ai`: the sandboxed
//! agent shell has no Windows backend.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ai::chat::{self, ChatHandle, ChatSetup};

use crate::ai::checkout::{
    CheckoutError, CheckoutPlan, PrTarget, PrepareOutput, RestoreOutcome, execute_checkout,
    restore, restore_preview, run_prepare,
};
use crate::ai::events::PipelineEvent;
use crate::ai::interaction::{ApprovalDecision, Interaction};
use crate::ai::llm;
use crate::ai::model::Llm;
use crate::ai::pipeline::{PipelineControl, PipelineInputs, RunMode, RunStatus, start_pipeline};
use crate::ai::review::{CoverageEntry, Finding, FindingState};
use crate::ai::snapshot::gather_snapshot;
use crate::ai::stages::assemble::ReviewDraft;
use crate::ai::store::{
    CheckoutRecord, CheckoutState, OriginalRef, PrKey, RunRecord, Store, ai_db_path,
};
use crate::ai::ui_interaction::UiInteraction;
use crate::auth::store::{TokenStore, llm_key};
use crate::github::client::GithubClient;
use crate::github::files::LoadedFile;
use crate::github::metadata::PrMetadata;
use crate::model::RepoRef;
use crate::persist::config_root;
use crate::persist::settings::{AiSettings, ModelSettings, RepoAiSettings};
use crate::runtime::{Event, EventSender, SessionId};

use review_sandbox::{
    EnvInput, Launch, LocalHost, SandboxOps, SandboxPolicy, SandboxUnavailable, agent_env,
    default_deny_read, default_write_paths, sandboxed_launch,
};

/// How the hub starts the sandboxed shell; a test seam (spec §8.2).
pub type SandboxLauncher = Arc<
    dyn Fn(&SandboxPolicy, BTreeMap<OsString, OsString>) -> Result<Launch, SandboxUnavailable>
        + Send
        + Sync,
>;

/// How the hub builds the run's models from settings and the keychain; a
/// test seam (spec §10.1). `Err` is the display-safe reason.
pub type LlmFactory = Arc<
    dyn Fn(&AiSettings, &dyn TokenStore) -> Result<(Arc<dyn Llm>, Option<Arc<dyn Llm>>), String>
        + Send
        + Sync,
>;

/// What the UI already loaded for [`AiHub::start`]: the hub fetches the rest
/// of the snapshot itself (spec §10.3).
#[derive(Clone, Debug)]
pub struct SnapshotInputs {
    /// The PR's files, as the session loaded them.
    pub files: Vec<LoadedFile>,
    /// The PR's metadata, as the session loaded it.
    pub metadata: PrMetadata,
}

/// One transcript row for the UI timeline (spec §10.3). `content_json` is
/// the `ai_messages` payload verbatim.
#[derive(Clone, Debug, PartialEq)]
pub struct AiTimelineEntry {
    /// The agent that produced it.
    pub agent_id: String,
    /// `"user"`, `"assistant"`, or `"tool"`.
    pub role: String,
    /// The message content, as stored.
    pub content_json: String,
}

/// Everything the AI tab needs to rebuild itself on attach (spec §10.3).
#[derive(Clone, Debug)]
pub struct AiSnapshotData {
    /// The session's findings, newest run included.
    pub findings: Vec<Finding>,
    /// Coverage of the latest run, empty when there is none.
    pub coverage: Vec<CoverageEntry>,
    /// The latest run's transcript, empty when there is none.
    pub timeline: Vec<AiTimelineEntry>,
    /// The scout's brief, when a run has filed one.
    pub brief: Option<String>,
    /// The latest run row, when the session has one.
    pub run: Option<RunRecord>,
    /// The checkout this PR holds, when any does.
    pub checkout: Option<CheckoutRecord>,
    /// Why the agent shell is unsandboxed (spec §8.2), for the live run.
    pub bash_disabled: Option<String>,
    /// The head the last completed review ran against (spec §5.8).
    pub last_reviewed_sha: Option<String>,
    /// The latest run's assembler draft, when it filed one (spec §5.6):
    /// replayed so a run that finished while the user was on another PR
    /// still pre-fills the review on re-attach.
    pub draft: Option<ReviewDraft>,
    /// The latest run's token totals, `(input, output)`; `(0, 0)` while the
    /// newest run is still open.
    pub usage: (u64, u64),
}

/// Chat-stream events (spec §10.5). The chat agent arrives with Task 7;
/// the shape exists now so the event surface is stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatEvent {
    /// The user's message, echoed back into the stream.
    User {
        /// The message text.
        text: String,
    },
    /// A streamed assistant text delta.
    Text {
        /// The delta.
        delta: String,
    },
    /// A tool the chat agent called.
    ToolCall {
        /// The tool name.
        name: String,
        /// A one-line summary of its input.
        summary: String,
    },
    /// The assistant's turn ended.
    TurnDone,
}

/// One PR's live (or finished-but-unrestored) AI state.
pub struct RunHandle {
    /// The pipeline handle, taken once the run is cancelled or finishing.
    /// `None` on the placeholder [`AiHub::start`] installs before its first
    /// await; the real handle always has one.
    pub control: Option<PipelineControl>,
    /// The run's current session, rebound by [`AiHub::attach`] (spec §10.2).
    pub session: Arc<Mutex<SessionId>>,
    /// The parked questions and approvals, shared with the answer commands.
    pub interaction: Arc<UiInteraction>,
    /// The checkout this run holds until [`AiHub::finish`].
    pub checkout: CheckoutRecord,
    /// The chat's message sender (Task 7); `None` before the run starts or
    /// after it closes.
    pub chat: Option<ChatHandle>,
    /// Why the agent shell is unsandboxed (spec §8.2).
    pub bash_disabled: Option<String>,
    /// Whether the pipeline is still running; the forwarder clears it.
    live: Arc<AtomicBool>,
    /// The chat task, spawned on the first user message.
    chat_task: Option<JoinHandle<()>>,
    /// What the first message spawns the chat task with.
    chat_setup: Option<ChatSetup>,
    /// The chat's receiver, held until the first message spawns the task.
    chat_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// True while `AiStart` is still working through checkout, prepare, and
    /// sandbox: the placeholder handle holds the PR's slot so a concurrent
    /// Start is rejected before it can touch the checkout (spec §10.3).
    starting: bool,
    /// True while a [`Self::finish`] is restoring this run's checkout: a
    /// second finish — a quit re-check landing mid-restore, or a double
    /// clicked dialog answer — stands down instead of moving the tree
    /// twice (Task 8). Cleared when the run is put back awaiting a
    /// decision, so a stalled decision can be re-raised.
    finishing: bool,
    /// The orchestrator's handle, awaited by [`AiHub::finish`] so the agents
    /// are gone before the working tree is restored.
    pipeline: Option<JoinHandle<RunStatus>>,
    /// The agents' sandbox (their PTY shell dies with its last owner), so a
    /// finish can wait for the shell to be gone before moving the tree.
    sandbox: Option<Weak<dyn SandboxOps>>,
    /// The run's agent-owned temp dir, removed when the run closes.
    tmp_dir: Option<PathBuf>,
}

impl RunHandle {
    /// The placeholder [`AiHub::start`] installs before its first await, so
    /// a concurrent Start for the same PR sees a live entry and stands down.
    /// `session` is the shared cell the real handle and the forwarder will
    /// keep using, so an attach that lands mid-start still rebinds.
    fn starting(session: Arc<Mutex<SessionId>>, events: &EventSender) -> RunHandle {
        RunHandle {
            control: None,
            session: session.clone(),
            interaction: UiInteraction::new(events.clone(), session, Vec::new()),
            checkout: CheckoutRecord {
                id: 0,
                repo_path: PathBuf::new(),
                repo: String::new(),
                pr_number: 0,
                original: OriginalRef::Detached(String::new()),
                stash_sha: None,
                head_sha: String::new(),
                state: CheckoutState::Started,
            },
            chat: None,
            bash_disabled: None,
            live: Arc::new(AtomicBool::new(true)),
            chat_task: None,
            chat_setup: None,
            chat_rx: None,
            starting: true,
            finishing: false,
            pipeline: None,
            sandbox: None,
            tmp_dir: None,
        }
    }
}

/// Releases [`AiHub::start`]'s placeholder when the start ends by any route
/// (return, panic, cancelled future). A no-op once the real handle replaced
/// the placeholder.
struct StartSlot<'a> {
    hub: &'a AiHub,
    key: &'a PrKey,
}

impl Drop for StartSlot<'_> {
    fn drop(&mut self) {
        self.hub.release_starting(self.key);
    }
}

/// How long a finish waits for the pipeline to wind down after cancelling.
const PIPELINE_GRACE: Duration = Duration::from_secs(30);

/// How long a finish waits for the agent shell to die once the pipeline task
/// is gone.
const SANDBOX_RELEASE: Duration = Duration::from_secs(10);

/// Stop the pipeline task: wait up to `grace` for it to end by itself, then
/// abort it and wait for the abort to land, so no agent is still running
/// when the caller moves the working tree. Returns whether it had to abort.
async fn halt_pipeline<T>(mut pipeline: JoinHandle<T>, grace: Duration) -> bool {
    if tokio::time::timeout(grace, &mut pipeline).await.is_ok() {
        return false;
    }
    pipeline.abort();
    let _ = pipeline.await;
    true
}

/// Wait up to `max` for every owner of the sandbox to drop it - the agent
/// shell is killed when its last owner goes. True when it is gone.
async fn sandbox_released(sandbox: &Weak<dyn SandboxOps>, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    while sandbox.strong_count() > 0 {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// One run's part of the window-close decision (spec §7.3, Task 8): what
/// the GUI thread needs in order to run the Finish flow for it, copied out
/// of the hub so the decision itself is a pure function over
/// [`HubCloseState`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunCloseState {
    /// The PR the run belongs to; `AiFinish { discard: None }` goes to it.
    pub key: PrKey,
    /// The run's current session, so the finish's events route like the
    /// run's other ones (spec §10.2).
    pub session: SessionId,
    /// Whether the run holds its repository's checkout. Every real run
    /// does until `finish` gives it back; a `starting` placeholder has no
    /// checkout yet and does not hold the window open — closing over a
    /// start leaves any checkout it makes to the startup recovery, the
    /// same as a force-quit.
    pub holds_checkout: bool,
}

/// The hub state the quit decision reads (spec §7.3, Task 8): one entry
/// per run handle, with no locks, tasks, or I/O in sight.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HubCloseState {
    /// One entry per run the hub holds.
    pub runs: Vec<RunCloseState>,
}

/// Whether closing the app must run the Finish flow first (spec §7.3):
/// true when any run still holds a checkout. Closing over one would leave
/// the repository stashed and detached without asking — the startup
/// recovery is the fallback for a crash, not the plan for a normal quit.
pub fn close_requires_finish(hub_state: &HubCloseState) -> bool {
    hub_state.runs.iter().any(|run| run.holds_checkout)
}

/// The toast for a control aimed at a run whose `AiStart` is still working
/// through checkout, prepare, and sandbox; shared by `finish`, `cancel`,
/// `pause`, and `resume` (Task 8).
const STILL_STARTING: &str = "the AI review is still starting";

/// The toast for a control aimed at a PR with no run at all.
const NO_RUN: &str = "no AI review is running for this PR";

/// The worker's AI surface: live runs, confirmed checkout plans, and the
/// settings the runs read.
pub struct AiHub {
    store: Arc<Store>,
    runs: Mutex<HashMap<PrKey, RunHandle>>,
    /// Plans the user confirmed through `AiPlanCheckout`, consumed by
    /// [`AiHub::start`] (which re-plans when the entry is gone).
    plans: Mutex<HashMap<PrKey, CheckoutPlan>>,
    settings: RwLock<AiSettings>,
    tokens: Arc<dyn TokenStore>,
    launcher: SandboxLauncher,
    llms: LlmFactory,
    /// Checkouts the store shows unrestored at startup, sent once as
    /// [`Event::AiUnrestored`] (spec §7.3 recovery).
    unrestored: Mutex<Option<Vec<CheckoutRecord>>>,
    /// Sequence for agent temp dir names.
    tmp_seq: AtomicU64,
}

impl AiHub {
    /// A hub over `store` with the real launcher and LLM factory.
    pub fn new(store: Arc<Store>, tokens: Arc<dyn TokenStore>) -> AiHub {
        AiHub::build(
            store,
            tokens,
            Arc::new(sandboxed_launch),
            Arc::new(default_llm_factory),
        )
    }

    /// A hub whose AI database lives under `config_root`, falling back to an
    /// in-memory store when it cannot be opened (a broken database must not
    /// take the whole app down; the failure is logged).
    pub fn open(config_root: &Path, tokens: Arc<dyn TokenStore>) -> AiHub {
        let path = ai_db_path(config_root);
        let store = match Store::open(&path) {
            Ok(store) => Arc::new(store),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ai hub: opening the AI database failed");
                Arc::new(Store::open_in_memory().expect("an in-memory AI store always opens"))
            }
        };
        AiHub::new(store, tokens)
    }

    /// A hub with injected seams, for tests.
    pub fn with_seams(
        store: Arc<Store>,
        tokens: Arc<dyn TokenStore>,
        launcher: SandboxLauncher,
        llms: LlmFactory,
    ) -> AiHub {
        AiHub::build(store, tokens, launcher, llms)
    }

    fn build(
        store: Arc<Store>,
        tokens: Arc<dyn TokenStore>,
        launcher: SandboxLauncher,
        llms: LlmFactory,
    ) -> AiHub {
        let unrestored = store.checkouts_unrestored().unwrap_or_else(|error| {
            tracing::warn!(%error, "ai hub: could not read unrestored checkouts");
            Vec::new()
        });
        AiHub {
            store,
            runs: Mutex::new(HashMap::new()),
            plans: Mutex::new(HashMap::new()),
            settings: RwLock::new(AiSettings::default()),
            tokens,
            launcher,
            llms,
            unrestored: Mutex::new(Some(unrestored)),
            tmp_seq: AtomicU64::new(0),
        }
    }

    /// Replace the AI settings every later run reads (the app calls this at
    /// startup and whenever the user saves them).
    pub fn set_settings(&self, settings: AiSettings) {
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = settings;
    }

    /// Send the startup [`Event::AiUnrestored`] once, when the store shows
    /// checkouts a previous session never restored (spec §7.3). Called from
    /// the command executor before it dispatches.
    pub fn drain_startup(&self, events: &EventSender) {
        let pending = self
            .unrestored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(records) = pending
            && !records.is_empty()
        {
            events.send(Event::AiUnrestored(records));
        }
    }

    fn settings(&self) -> AiSettings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot the run state for the window-close decision (spec §7.3,
    /// Task 8): every run handle, saying whether it holds a checkout.
    /// Locks the run map only — no I/O — so the GUI thread may call it
    /// directly, the way it calls [`Self::set_settings`].
    pub fn close_state(&self) -> HubCloseState {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        HubCloseState {
            runs: runs
                .iter()
                .map(|(key, run)| RunCloseState {
                    key: key.clone(),
                    session: *run.session.lock().unwrap_or_else(|e| e.into_inner()),
                    holds_checkout: !run.starting,
                })
                .collect(),
        }
    }

    fn repo_settings(&self, key: &PrKey) -> Option<RepoAiSettings> {
        self.settings()
            .repo(&key.host, &key.owner, &key.repo)
            .cloned()
    }

    /// Rebind the run for `key` to `session` and replay its full state
    /// (spec §10.2). Works with no live run too: the stored findings and
    /// checkout still describe the PR.
    pub fn attach(&self, session: SessionId, key: PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = runs.get(&key) {
            *run.session.lock().unwrap_or_else(|e| e.into_inner()) = session;
        }
        match self.snapshot_data(&key, runs.get(&key)) {
            Ok(snapshot) => events.send(Event::AiSnapshot { session, snapshot }),
            Err(error) => events.send(Event::Toast(format!(
                "could not load the AI review state: {error}"
            ))),
        }
    }

    /// The stored state for `key`, overlaid with the live run's checkout and
    /// bash reason when there is one. A start placeholder (`starting`) has no
    /// real checkout yet, so it is treated like no live run.
    fn snapshot_data(
        &self,
        key: &PrKey,
        live: Option<&RunHandle>,
    ) -> anyhow::Result<AiSnapshotData> {
        let session_id = self.store.session_id(key)?;
        let findings = self.store.finding_list(session_id)?;
        let run = self.store.latest_run(session_id)?;
        let coverage = match &run {
            Some(run) => self.store.coverage_list(run.id)?,
            None => Vec::new(),
        };
        let timeline = match &run {
            Some(run) => self
                .store
                .run_timeline(run.id)?
                .into_iter()
                .map(|(agent_id, role, content_json)| AiTimelineEntry {
                    agent_id,
                    role,
                    content_json,
                })
                .collect(),
            None => Vec::new(),
        };
        let brief = self.store.session_brief(session_id)?;
        let last_reviewed_sha = self.store.session_last_reviewed(session_id)?;
        // The draft and usage ride the snapshot so a run that finished while
        // the user was elsewhere still lands its pre-filled review and token
        // line on re-attach; a row that cannot be read costs only the replay.
        let draft = self
            .store
            .run_latest_draft(session_id)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "ai hub: could not read the run draft");
                None
            });
        let usage = self
            .store
            .run_latest_usage(session_id)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "ai hub: could not read the run usage");
                (0, 0)
            });
        let (checkout, bash_disabled) = match live.filter(|run| !run.starting) {
            Some(run) => (Some(run.checkout.clone()), run.bash_disabled.clone()),
            None => {
                // No live run: the checkout the store still holds for this
                // repo's configured path, if any.
                let rec = self
                    .repo_settings(key)
                    .and_then(|repo| std::fs::canonicalize(&repo.path).ok())
                    .and_then(|path| self.store.checkout_holding(&path).ok().flatten());
                (rec, None)
            }
        };
        Ok(AiSnapshotData {
            findings,
            coverage,
            timeline,
            brief,
            run,
            checkout,
            bash_disabled,
            last_reviewed_sha,
            draft,
            usage,
        })
    }

    /// Validate and plan the checkout for `pr`, answering with
    /// [`Event::AiCheckoutPlanned`] for the user to confirm (spec §4 step 2).
    /// The confirmed plan is kept for the next [`AiHub::start`].
    pub async fn plan_checkout(
        &self,
        session: SessionId,
        key: PrKey,
        pr: PrTarget,
        events: &EventSender,
    ) {
        let Some(repo) = self.repo_settings(&key) else {
            events.send(Event::Toast(format!(
                "no AI checkout folder is configured for {}/{}/{} — set it in Settings",
                key.host, key.owner, key.repo
            )));
            return;
        };
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::ai::checkout::plan_checkout(&repo.path, &pr, &store)
        })
        .await;
        match result {
            Ok(Ok(plan)) => {
                self.plans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, plan.clone());
                events.send(Event::AiCheckoutPlanned { session, plan });
            }
            // The held case gets its own event so the UI can name the
            // holding PR (spec §4 step 7); every other failure toasts.
            Ok(Err(CheckoutError::Held {
                pr_number,
                record_id,
            })) => {
                // A holding record with no live run cannot be finished —
                // Finish would toast "no AI review is running" — so offer
                // the restore instead (Task 4 review finding 2).
                let live = self
                    .runs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .any(|run| {
                        run.number == pr_number
                            && run.host.eq_ignore_ascii_case(&key.host)
                            && run.owner.eq_ignore_ascii_case(&key.owner)
                            && run.repo.eq_ignore_ascii_case(&key.repo)
                    });
                events.send(Event::AiCheckoutHeld {
                    session,
                    pr_number,
                    restore: (!live).then_some(record_id),
                });
            }
            Ok(Err(error)) => events.send(Event::AiCheckoutFailed {
                session,
                message: error.to_string(),
            }),
            Err(_) => events.send(Event::AiCheckoutFailed {
                session,
                message: "the checkout plan was cancelled".to_owned(),
            }),
        }
    }

    /// Run the whole start sequence (spec §10.3): checkout, prepare, sandbox,
    /// snapshot, pipeline — then forward the pipeline's events under the
    /// run's current session.
    ///
    /// Failures before the pipeline starts answer with
    /// [`Event::AiCheckoutFailed`] (or [`Event::Failed`] for a GitHub error)
    /// and undo the checkout, so a half-started run never holds the working
    /// tree.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        client: GithubClient,
        session: SessionId,
        key: PrKey,
        pr: PrTarget,
        snapshot_inputs: SnapshotInputs,
        mode: RunMode,
        events: &EventSender,
    ) {
        // The run's session cell exists before anything else: the
        // placeholder and the real handle share it, so an `AiAttach` that
        // lands mid-start rebinds the cell the pipeline will use (§10.2).
        let session_cell = Arc::new(Mutex::new(session));

        let Some(repo_settings) = self.repo_settings(&key) else {
            events.send(Event::Toast(format!(
                "no AI checkout folder is configured for {}/{}/{} — set it in Settings",
                key.host, key.owner, key.repo
            )));
            return;
        };
        let settings = self.settings();

        // A live run (or a start in progress) for this PR owns the checkout;
        // a finished one that was never finished() is replaced. The
        // placeholder goes in under the same lock as the check, before the
        // first await: the worker runs commands concurrently, so a
        // double-clicked Start must not get two pipelines through the
        // checkout (spec §10.3).
        let replacing = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            match runs.get(&key) {
                // A finish restoring this PR's checkout owns the slot until
                // it is done: replacing the entry would start a pipeline on
                // a tree that is being reset under it.
                Some(run) if run.finishing => {
                    events.send(Event::Toast(
                        "the previous AI review is still being restored".to_owned(),
                    ));
                    return;
                }
                Some(run) if run.live.load(Ordering::SeqCst) || run.starting => {
                    events.send(Event::Toast(
                        "an AI review is already running for this PR".to_owned(),
                    ));
                    return;
                }
                _ => {
                    let old = runs.remove(&key);
                    runs.insert(
                        key.clone(),
                        RunHandle::starting(session_cell.clone(), events),
                    );
                    old
                }
            }
        };
        // From here every exit - an early return, a panic, a dropped
        // future - releases the placeholder; the real handle, once
        // inserted, stays.
        let _slot = StartSlot {
            hub: self,
            key: &key,
        };
        if let Some(old) = replacing {
            close_run_handle(old);
        }

        // An attach can rebind the run's session at any await point, so
        // every event reads the cell at the moment it is sent.
        let current = || *session_cell.lock().unwrap_or_else(|e| e.into_inner());

        // Undo a checkout this start made when a later step fails. The
        // placeholder is held until the restore is done (the guard above
        // releases it on return), so a Start in between cannot race it.
        let abort = |rec: CheckoutRecord, events: EventSender, message: String| {
            let store = self.store.clone();
            async move {
                events.send(Event::AiCheckoutFailed {
                    session: current(),
                    message,
                });
                let outcome = tokio::task::spawn_blocking(move || restore(&rec, true, &store))
                    .await
                    .map_err(|_| "the undo was cancelled".to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                if let Err(error) = outcome {
                    events.send(Event::Toast(format!(
                        "the AI run did not start, and restoring the checkout failed: {error}"
                    )));
                }
            }
        };

        // 1. The checkout: the confirmed plan when we still have one, a
        //    fresh one otherwise.
        let planned = self
            .plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        let plan = match planned {
            Some(plan) => plan,
            None => {
                let store = self.store.clone();
                let path = repo_settings.path.clone();
                let pr_for_plan = pr.clone();
                let planned = tokio::task::spawn_blocking(move || {
                    crate::ai::checkout::plan_checkout(&path, &pr_for_plan, &store)
                })
                .await
                .map_err(|_| CheckoutError::NotARepo(repo_settings.path.clone()))
                .and_then(|r| r);
                match planned {
                    Ok(plan) => plan,
                    Err(error) => {
                        events.send(Event::AiCheckoutFailed {
                            session: current(),
                            message: error.to_string(),
                        });
                        return;
                    }
                }
            }
        };
        let rec = {
            let store = self.store.clone();
            let plan = plan.clone();
            let pr = pr.clone();
            match tokio::task::spawn_blocking(move || execute_checkout(&plan, &pr, &store)).await {
                Ok(Ok(rec)) => rec,
                Ok(Err(error)) => {
                    events.send(Event::AiCheckoutFailed {
                        session: current(),
                        message: error.to_string(),
                    });
                    return;
                }
                Err(_) => {
                    events.send(Event::AiCheckoutFailed {
                        session: current(),
                        message: "the checkout was cancelled".to_owned(),
                    });
                    return;
                }
            }
        };

        // 2. Prepare commands, outside the sandbox, before the run (spec §8.5).
        if !repo_settings.prepare.is_empty() {
            let mut outputs: Vec<PrepareOutput> = Vec::new();
            for command in &repo_settings.prepare {
                let repo_path = plan.repo_path.clone();
                let cmd = command.clone();
                let step = tokio::task::spawn_blocking(move || {
                    run_prepare(&repo_path, std::slice::from_ref(&cmd))
                })
                .await;
                match step {
                    Ok(Ok(done)) => outputs.extend(done),
                    Ok(Err(failed)) => {
                        outputs.push(failed);
                        break;
                    }
                    Err(_) => {
                        outputs.push(PrepareOutput {
                            command: command.clone(),
                            status: -1,
                            output: "the prepare step was cancelled".to_owned(),
                        });
                        break;
                    }
                }
            }
            events.send(Event::AiPrepareOutput {
                session: current(),
                outputs: outputs.clone(),
            });
            if let Some(failed) = outputs.iter().find(|o| o.status != 0) {
                let message = format!("prepare command failed: {}", failed.command);
                abort(rec.clone(), events.clone(), message).await;
                return;
            }
        }

        // 3. The sandbox: agent env, policy, and the OS launch (spec §8.2).
        //    `SandboxUnavailable` is not fatal: the run goes on with an
        //    unsandboxed shell and the UI says why.
        let tmp_dir = std::env::temp_dir().join(format!(
            "gh-review-ai-{}-{}",
            std::process::id(),
            self.tmp_seq.fetch_add(1, Ordering::SeqCst)
        ));
        if let Err(error) = std::fs::create_dir_all(&tmp_dir) {
            abort(
                rec.clone(),
                events.clone(),
                format!("could not create the agent temp dir: {error}"),
            )
            .await;
            return;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let launcher = self.launcher.clone();
        let extra_env_allow = settings.extra_env_allow.clone();
        let repo_root = plan.repo_path.clone();
        let extra_write = repo_settings.extra_write_paths.clone();
        let extra_deny = repo_settings.extra_deny_read.clone();
        let app_config = config_root();
        let tmp_for_sandbox = tmp_dir.clone();
        let sandbox_build = tokio::task::spawn_blocking(move || {
            let home = home.ok_or_else(|| "HOME is not set".to_string())?;
            let env = agent_env(&EnvInput {
                host_env: std::env::vars_os().collect(),
                extra_allow: extra_env_allow,
                tmp_dir: tmp_for_sandbox.clone(),
                git_user: git_identity(&repo_root),
            })
            .map_err(|e| e.to_string())?;
            let mut write_paths = default_write_paths(&home);
            write_paths.extend(extra_write);
            let mut deny_read = default_deny_read(&home, &app_config);
            deny_read.extend(extra_deny);
            let policy = SandboxPolicy {
                repo: repo_root.clone(),
                tmp: tmp_for_sandbox,
                write_paths,
                deny_read,
            };
            match launcher(&policy, env) {
                Ok(launch) => LocalHost::with_launch(&repo_root, launch)
                    .map(|host| (host, None))
                    .map_err(|e| e.to_string()),
                Err(why) => LocalHost::new(&repo_root)
                    .map(|host| (host, Some(why.to_string())))
                    .map_err(|e| e.to_string()),
            }
        })
        .await;
        let (sandbox, bash_disabled) = match sandbox_build {
            Ok(Ok(pair)) => pair,
            Ok(Err(message)) => {
                abort(
                    rec.clone(),
                    events.clone(),
                    format!("could not start the agent shell: {message}"),
                )
                .await;
                return;
            }
            Err(_) => {
                abort(
                    rec.clone(),
                    events.clone(),
                    "the sandbox build was cancelled".to_owned(),
                )
                .await;
                return;
            }
        };
        let sandbox: Arc<dyn SandboxOps> = Arc::new(sandbox);
        let sandbox_weak = Arc::downgrade(&sandbox);

        // 4. The frozen PR (spec §4 step 2).
        let repo = RepoRef::new(&pr.owner, &pr.name);
        let snapshot = match gather_snapshot(
            &client,
            &repo,
            pr.number,
            snapshot_inputs.files,
            snapshot_inputs.metadata,
        )
        .await
        {
            Ok(snapshot) => Arc::new(snapshot),
            Err(error) => {
                events.send(Event::Failed {
                    session: Some(current()),
                    error,
                });
                abort(
                    rec.clone(),
                    events.clone(),
                    "could not gather the PR snapshot; the run did not start".to_owned(),
                )
                .await;
                return;
            }
        };

        // 5. The models.
        let (llm, lookup_llm) = match (self.llms)(&settings, &*self.tokens) {
            Ok(pair) => pair,
            Err(message) => {
                abort(rec.clone(), events.clone(), message).await;
                return;
            }
        };

        // 6. The pipeline, and the task that forwards its events.
        let ai_session_id = match self.store.session_id(&key) {
            Ok(id) => id,
            Err(error) => {
                abort(
                    rec.clone(),
                    events.clone(),
                    format!("could not open the AI session row: {error}"),
                )
                .await;
                return;
            }
        };
        let interaction = UiInteraction::new(
            events.clone(),
            session_cell.clone(),
            repo_settings.prepare.clone(),
        );
        let elide_threshold = f64::from(settings.context_threshold);
        let (control, mut rx, pipeline) = start_pipeline(PipelineInputs {
            store: self.store.clone(),
            session_id: ai_session_id,
            snapshot,
            repo_root: plan.repo_path.clone(),
            sandbox,
            settings,
            repo_settings,
            llm: llm.clone(),
            lookup_llm,
            interaction: interaction.clone() as Arc<dyn Interaction>,
            mode,
            bash_disabled: bash_disabled.clone(),
        });
        if let Err(error) = self.store.checkout_set_state(rec.id, CheckoutState::Active) {
            tracing::warn!(%error, "ai hub: marking the checkout active failed");
        }

        let live = Arc::new(AtomicBool::new(true));
        // The chat (spec §6.5) shares the run's model, findings, snapshot and
        // cancellation; it spawns on the first user message.
        let (chat_send, chat_rx) = ChatHandle::channel();
        let chat_setup = match (
            control.run_state(),
            control.chat_tool_cx(chat::chat_agent()),
        ) {
            (Some(run), Some(cx)) => {
                let run_id = run.run_id();
                Some(ChatSetup {
                    llm,
                    run,
                    store: self.store.clone(),
                    session_id: ai_session_id,
                    run_id,
                    cx,
                    control: Some(control.clone()),
                    interaction: interaction.clone(),
                    live: live.clone(),
                    session: session_cell.clone(),
                    events: events.clone(),
                    bash_disabled: bash_disabled.clone(),
                    elide_threshold,
                    cancel: control.cancel_token(),
                })
            }
            _ => {
                tracing::warn!("ai hub: the pipeline has no chat setup");
                None
            }
        };
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key.clone(),
            RunHandle {
                control: Some(control),
                session: session_cell.clone(),
                interaction: interaction.clone(),
                checkout: rec,
                chat: Some(chat_send),
                bash_disabled: bash_disabled.clone(),
                live: live.clone(),
                chat_task: None,
                chat_setup,
                chat_rx: Some(chat_rx),
                starting: false,
                finishing: false,
                pipeline: Some(pipeline),
                sandbox: Some(sandbox_weak),
                tmp_dir: Some(tmp_dir),
            },
        );

        let forward_events = events.clone();
        let forward_session = session_cell.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                // The run is over once the orchestrator reports its end:
                // the chat's steering tools answer "the review has finished"
                // from here on, even though the handle (and its checkout)
                // stays until `finish`.
                if matches!(event, PipelineEvent::RunEnded { .. }) {
                    live.store(false, Ordering::SeqCst);
                }
                let session = *forward_session.lock().unwrap_or_else(|e| e.into_inner());
                forward_events.send(Event::AiPipeline { session, event });
            }
            live.store(false, Ordering::SeqCst);
            // Anything still parked (a question the user never answered)
            // resolves the way `NoInteraction` would.
            interaction.cancel_all();
        });

        // The UI needs the checkout record and the bash reason right away.
        {
            let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            match self.snapshot_data(&key, runs.get(&key)) {
                Ok(snapshot) => events.send(Event::AiSnapshot {
                    session: current(),
                    snapshot,
                }),
                Err(error) => events.send(Event::Toast(format!(
                    "could not load the AI review state: {error}"
                ))),
            }
        }
    }

    /// Answer a parked question. Unknown or already-resolved ids (the run
    /// moved on, was cancelled) are ignored (spec §10.4).
    pub fn answer(&self, key: &PrKey, question_id: u64, answer: String) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = runs.get(key)
            && !run.interaction.answer(question_id, answer)
        {
            tracing::debug!(question_id, "ai hub: answer for an unknown question");
        }
    }

    /// Decide a parked command approval.
    pub fn approve(&self, key: &PrKey, approval_id: u64, decision: ApprovalDecision) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = runs.get(key)
            && !run.interaction.approve(approval_id, decision)
        {
            tracing::debug!(approval_id, "ai hub: decision for an unknown approval");
        }
    }

    /// Decide a parked prepare approval (spec §8.5).
    pub fn approve_prepare(&self, key: &PrKey, approval_id: u64, ok: bool) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = runs.get(key)
            && !run.interaction.decide_prepare(approval_id, ok)
        {
            tracing::debug!(
                approval_id,
                "ai hub: decision for an unknown prepare request"
            );
        }
    }

    /// Hand `text` to the run's chat agent (spec §10.5). The first message
    /// of a run spawns the long-lived chat task; later ones reuse it
    /// (Task 7). The message goes to the chat only — typing never pauses
    /// or notes the pipeline itself (spec §6.5).
    pub fn user_message(
        &self,
        _session: SessionId,
        key: &PrKey,
        text: String,
        events: &EventSender,
    ) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(run) = runs.get_mut(key) else {
            events.send(Event::Toast(NO_RUN.to_owned()));
            return;
        };
        let Some(chat) = run.chat.as_ref() else {
            events.send(Event::Toast(NO_RUN.to_owned()));
            return;
        };
        // The first message spawns the chat task; the receiver and its setup
        // live in the handle until then.
        if run.chat_task.is_none()
            && let (Some(messages), Some(setup)) = (run.chat_rx.take(), run.chat_setup.take())
        {
            run.chat_task = Some(tokio::spawn(chat::run_chat(messages, setup)));
        }
        if !chat.send(text) {
            events.send(Event::Toast(
                "the chat agent has stopped; start a new review to chat again".to_owned(),
            ));
        }
        // The chat task echoes the message back into the stream itself, so
        // the UI's optimistic entry and the echo reconcile in the reducer.
    }

    /// Pause every running agent before its next model call. A start still
    /// in progress gets the "still starting" refusal, mirroring
    /// [`Self::finish`] (Task 8).
    pub fn pause(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key) {
            Some(run) if run.starting => {
                events.send(Event::Toast(STILL_STARTING.to_owned()));
            }
            Some(run) => match run.control.as_ref() {
                Some(control) => control.pause(),
                None => events.send(Event::Toast(NO_RUN.to_owned())),
            },
            None => events.send(Event::Toast(NO_RUN.to_owned())),
        }
    }

    /// Release a [`Self::pause`].
    pub fn resume(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key) {
            Some(run) if run.starting => {
                events.send(Event::Toast(STILL_STARTING.to_owned()));
            }
            Some(run) => match run.control.as_ref() {
                Some(control) => control.resume(),
                None => events.send(Event::Toast(NO_RUN.to_owned())),
            },
            None => events.send(Event::Toast(NO_RUN.to_owned())),
        }
    }

    /// Stop the run; the checkout stays held so a re-run is cheap (spec §10.3).
    pub fn cancel(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key) {
            Some(run) if run.starting => {
                events.send(Event::Toast(STILL_STARTING.to_owned()));
            }
            Some(run) => match run.control.as_ref() {
                Some(control) => control.cancel(),
                None => events.send(Event::Toast(NO_RUN.to_owned())),
            },
            None => events.send(Event::Toast(NO_RUN.to_owned())),
        }
    }

    /// End the run and give the checkout back (spec §7.3, §10.6).
    ///
    /// `discard = None` asks first: changes made during the review come back
    /// as [`Event::AiRestoreNeedsDecision`] and the run stays so the user
    /// can decide. `discard = Some(true)` throws those changes away and
    /// closes the run with [`Event::AiRestored`]. `discard = Some(false)`
    /// on a dirty tree is that decision's **Keep** answer: the run closes,
    /// the record is marked `Failed` (still holding the repo, still a
    /// recovery target), and an [`Event::AiNotice`] describes the state the
    /// checkout was left in.
    pub async fn finish(
        &self,
        session: SessionId,
        key: &PrKey,
        discard: Option<bool>,
        events: &EventSender,
    ) {
        // `None`: no run at all. `Some(None)`: a finish is already
        // restoring it — the quit flow re-checks while a restore is in
        // flight — and its events are already on the way (Task 8).
        let taken = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            match runs.get_mut(key) {
                Some(run) if run.finishing => Some(None),
                Some(run) => {
                    run.finishing = true;
                    Some(Some(RunHandle {
                        control: run.control.take(),
                        session: run.session.clone(),
                        interaction: run.interaction.clone(),
                        checkout: run.checkout.clone(),
                        chat: run.chat.take(),
                        bash_disabled: run.bash_disabled.clone(),
                        live: run.live.clone(),
                        chat_task: run.chat_task.take(),
                        chat_setup: run.chat_setup.take(),
                        chat_rx: run.chat_rx.take(),
                        starting: run.starting,
                        // The clone is what `put_back` re-installs; a run
                        // put back awaits a decision, not a restore.
                        finishing: false,
                        pipeline: run.pipeline.take(),
                        sandbox: run.sandbox.clone(),
                        tmp_dir: run.tmp_dir.clone(),
                    }))
                }
                None => None,
            }
        };
        let Some(Some(mut run)) = taken else {
            if taken.is_none() {
                events.send(Event::Toast(NO_RUN.to_owned()));
            }
            return;
        };
        if run.starting {
            events.send(Event::Toast(STILL_STARTING.to_owned()));
            self.put_back(key, run);
            return;
        }

        // The agents must be gone before the tree moves under them.
        if let Some(control) = run.control.take() {
            control.cancel();
        }
        run.interaction.cancel_all();
        if let Some(pipeline) = run.pipeline.take()
            && halt_pipeline(pipeline, PIPELINE_GRACE).await
        {
            // The pipeline ignored the cancel and was aborted. Its tools
            // and the chat may still own the agent shell, which keeps
            // running until its last owner drops: stop the chat too and wait
            // for the shell to die, or refuse to touch the tree.
            if let Some(chat) = run.chat_task.take() {
                chat.abort();
            }
            run.chat_setup = None;
            run.chat = None;
            if let Some(sandbox) = run.sandbox.as_ref()
                && !sandbox_released(sandbox, SANDBOX_RELEASE).await
            {
                events.send(Event::Toast(
                    "the AI agents did not stop; the checkout was left as it is - \
                     try finishing again in a moment"
                        .to_owned(),
                ));
                self.put_back(key, run);
                return;
            }
        }

        let store = self.store.clone();
        let rec = run.checkout.clone();
        match discard {
            Some(discard) => {
                // The Keep path needs the record after the blocking call
                // moved it, so keep a copy for the notice.
                let rec_for_notice = rec.clone();
                let outcome =
                    tokio::task::spawn_blocking(move || restore(&rec, discard, &store)).await;
                match outcome {
                    Ok(Ok(outcome)) => {
                        self.close_run(key, run);
                        events.send(Event::AiRestored { session, outcome });
                    }
                    // `discard = Some(false)` on a dirty tree is the user's
                    // "leave as is" answer (spec §7.3 step 1): stop without
                    // touching the tree. The record keeps the repo lock and
                    // stays a recovery target — mark it `Failed`, close the
                    // run, and tell the tab exactly what state it left.
                    Ok(Err(CheckoutError::NeedsDecision(changes))) if !discard => {
                        let _ = self
                            .store
                            .checkout_set_state(rec_for_notice.id, CheckoutState::Failed);
                        self.close_run(key, run);
                        events.send(Event::AiNotice {
                            session,
                            text: left_as_is_text(&rec_for_notice, changes.len()),
                        });
                    }
                    Ok(Err(CheckoutError::NeedsDecision(changes))) => {
                        events.send(Event::AiRestoreNeedsDecision { session, changes });
                        self.put_back(key, run);
                    }
                    Ok(Err(error)) => {
                        events.send(Event::Toast(format!(
                            "restoring the checkout failed: {error}"
                        )));
                        self.put_back(key, run);
                    }
                    Err(_) => {
                        events.send(Event::Toast("the restore was cancelled".to_owned()));
                        self.put_back(key, run);
                    }
                }
            }
            None => {
                let preview = {
                    let rec = rec.clone();
                    tokio::task::spawn_blocking(move || restore_preview(&rec)).await
                };
                match preview {
                    Ok(Ok(changes)) if !changes.is_empty() => {
                        events.send(Event::AiRestoreNeedsDecision { session, changes });
                        self.put_back(key, run);
                    }
                    Ok(Ok(_)) => {
                        let outcome =
                            tokio::task::spawn_blocking(move || restore(&rec, false, &store)).await;
                        match outcome {
                            Ok(Ok(outcome)) => {
                                self.close_run(key, run);
                                events.send(Event::AiRestored { session, outcome });
                            }
                            Ok(Err(CheckoutError::NeedsDecision(changes))) => {
                                events.send(Event::AiRestoreNeedsDecision { session, changes });
                                self.put_back(key, run);
                            }
                            Ok(Err(error)) => {
                                // The run goes back so a later finish — the
                                // quit flow's re-check, or the user's retry
                                // — can try again instead of hitting the
                                // `finishing` guard forever (Task 8 review).
                                events.send(Event::Toast(format!(
                                    "restoring the checkout failed: {error}"
                                )));
                                self.put_back(key, run);
                            }
                            Err(_) => {
                                events.send(Event::Toast("the restore was cancelled".to_owned()));
                                self.put_back(key, run);
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        events.send(Event::Toast(format!(
                            "could not inspect the checkout: {error}"
                        )));
                        self.put_back(key, run);
                    }
                    Err(_) => {
                        events.send(Event::Toast("the restore was cancelled".to_owned()));
                        self.put_back(key, run);
                    }
                }
            }
        }
    }

    /// Drop the start placeholder if it is still the entry for `key`; the
    /// real handle, once inserted, stays.
    fn release_starting(&self, key: &PrKey) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if runs.get(key).is_some_and(|run| run.starting) {
            runs.remove(key);
        }
    }

    /// Put a run back after a restore that needs a decision: the control and
    /// pipeline are already gone, but the checkout and parked prompts stay.
    ///
    /// Only the entry `run` was taken from is replaced: a slot another run
    /// has since claimed is never overwritten (its checkout and agents are
    /// not ours to evict).
    fn put_back(&self, key: &PrKey, run: RunHandle) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if runs.get(key).is_none_or(|current| owns(&run, current)) {
            runs.insert(key.clone(), run);
        } else {
            drop(runs);
            close_run_handle(run);
        }
    }

    /// Remove `run`'s own entry (never one a newer run installed) and tear
    /// the run down.
    fn close_run(&self, key: &PrKey, run: RunHandle) {
        {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            if runs.get(key).is_some_and(|current| owns(&run, current)) {
                runs.remove(key);
            }
        }
        self.plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        close_run_handle(run);
    }

    /// Move a finding to a new user state (spec §11). The UI optimistically
    /// updates and this confirms through [`PipelineEvent::FindingChanged`].
    pub fn finding_state(
        &self,
        session: SessionId,
        id: i64,
        state: FindingState,
        pending_local_id: Option<u64>,
        events: &EventSender,
    ) {
        match self
            .store
            .finding_set_user_state(id, state, pending_local_id)
        {
            Ok(()) => events.send(Event::AiPipeline {
                session,
                event: PipelineEvent::FindingChanged { id },
            }),
            Err(error) => events.send(Event::Toast(format!(
                "could not update the finding: {error}"
            ))),
        }
    }

    /// Rewrite finding `id`'s body after the inline editor saved it (spec
    /// §12, AI Task 5). The UI already shows the new text; the store is the
    /// durable copy.
    pub fn finding_edit(&self, session: SessionId, id: i64, body: String, events: &EventSender) {
        match self.store.finding_set_body(id, &body) {
            Ok(()) => events.send(Event::AiPipeline {
                session,
                event: PipelineEvent::FindingChanged { id },
            }),
            Err(error) => events.send(Event::Toast(format!("could not save the finding: {error}"))),
        }
    }

    /// Restore one unrestored checkout record from the crash-recovery dialog
    /// (spec §7.3): discard whatever the abandoned review left, return to
    /// the recorded original ref, and re-apply the stash. Not PR-scoped —
    /// no run is live for a record a previous session left behind — so the
    /// outcome arrives as a [`Event::Toast`].
    pub async fn recover_restore(&self, id: i64, events: &EventSender) {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            let rec = store
                .checkout_get(id)?
                .ok_or_else(|| anyhow::anyhow!("no checkout record #{id}"))?;
            // A failed record with no stash is not review output alone: it
            // can come from a failed `git stash push`, where the tree holds
            // the user's own work. Never discard there without the user
            // looking at it (`restore` itself refuses finished records).
            let discard = !(rec.state == CheckoutState::Failed && rec.stash_sha.is_none());
            restore(&rec, discard, &store).map(|outcome| (rec, outcome))
        })
        .await;
        match result {
            Ok(Err(CheckoutError::NeedsDecision(changes))) => events.send(Event::Toast(format!(
                "the checkout has {} uncommitted change(s) and no stash was recorded, so they \
                 may be your own work: nothing was discarded. Commit, stash or discard them \
                 yourself, then restore again",
                changes.len()
            ))),
            Ok(Ok((rec, RestoreOutcome::Restored))) => events.send(Event::Toast(format!(
                "restored {} to {}",
                rec.repo_path.display(),
                original_ref_text(&rec.original)
            ))),
            Ok(Ok((rec, RestoreOutcome::StashConflict { stash_sha }))) => {
                events.send(Event::Toast(format!(
                    "{} went back to its original ref, but the stash did not apply cleanly; \
                 your changes are still in stash {stash_sha}",
                    rec.repo_path.display()
                )))
            }
            Ok(Err(error)) => events.send(Event::Toast(format!(
                "could not restore the checkout: {error}"
            ))),
            Err(_) => events.send(Event::Toast("the restore was cancelled".to_owned())),
        }
    }
}

/// Whether `taken` is the handle that `slot` was split from: the run's `live`
/// flag is created once per run and shared by the clones `finish` makes.
fn owns(taken: &RunHandle, slot: &RunHandle) -> bool {
    Arc::ptr_eq(&taken.live, &slot.live)
}

/// Tear down a run's leftovers: resolve anything parked, drop the chat agent,
/// remove the agent temp dir.
fn close_run_handle(run: RunHandle) {
    run.interaction.cancel_all();
    if let Some(chat) = run.chat_task {
        chat.abort();
    }
    // Dropping the sender ends the chat loop even if the task is between
    // turns.
    drop(run.chat);
    if let Some(dir) = &run.tmp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The display text for a recorded original ref.
fn original_ref_text(original: &OriginalRef) -> String {
    match original {
        OriginalRef::Branch(name) => format!("branch {name}"),
        OriginalRef::Detached(sha) => format!("detached {}", short(sha)),
    }
}

/// The first seven characters of a SHA.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// The Notice text for a checkout the user chose to leave as is (spec §7.3
/// step 1): where the tree stands, what holds their original work, and how
/// the checkout gets restored later — the Held dialog offers the restore on
/// the next review in this repo, and the launch recovery list names it.
fn left_as_is_text(rec: &CheckoutRecord, changes: usize) -> String {
    let stash = match &rec.stash_sha {
        Some(sha) => format!(
            "your pre-checkout changes are in stash {} (`git stash apply` takes them back)",
            short(sha)
        ),
        None => "no stash was made".to_string(),
    };
    format!(
        "left the checkout as is: {} is still on PR #{}'s head with {changes} change(s) from \
         the review; {} is recorded and {stash}. Starting a review on another pull request in \
         this repository offers to restore it, and so does the next launch.",
        rec.repo_path.display(),
        rec.pr_number,
        original_ref_text(&rec.original),
    )
}

/// The repo's git identity for the agent's written gitconfig (spec §8.2);
/// `None` when neither the repo nor the global config defines one.
fn git_identity(repo: &Path) -> Option<(String, String)> {
    let name = git_config(repo, "user.name")?;
    let email = git_config(repo, "user.email")?;
    Some((name, email))
}

fn git_config(repo: &Path, key: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The real [`LlmFactory`]: the configured models, their providers, and the
/// keys from the keychain (spec §10.1).
#[allow(clippy::type_complexity)]
fn default_llm_factory(
    settings: &AiSettings,
    tokens: &dyn TokenStore,
) -> Result<(Arc<dyn Llm>, Option<Arc<dyn Llm>>), String> {
    let build = |model: &ModelSettings| -> Result<Arc<dyn Llm>, String> {
        let provider = settings
            .provider(&model.provider)
            .ok_or_else(|| format!("unknown LLM provider {}", model.provider))?;
        let key = llm_key(tokens, &provider.name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no API key stored for {}", provider.name))?;
        llm::build_llm(provider, model, secrecy::SecretString::from(key))
            .map(|built| Arc::new(built) as Arc<dyn Llm>)
            .map_err(|e| e.to_string())
    };
    let main = settings
        .models
        .default
        .as_ref()
        .ok_or("no AI review model is configured — pick one in Settings")?;
    let main = build(main)?;
    let lookup = match settings.models.lookup.as_ref() {
        Some(model) => Some(build(model)?),
        None => None,
    };
    Ok((main, lookup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::checkout::RestoreOutcome;
    use crate::ai::events::{StageKind, StageStatus};
    use crate::ai::testing::{ScriptStep, ScriptedLlm};
    use crate::auth::store::MemoryStore;
    use crate::diff::parse::parse_patch;
    use crate::github::client::Endpoints;
    use crate::github::files::{FileStatus, LoadedFile, PatchState, PrFile};
    use crate::model::diff::FileDiff;
    use crate::runtime::{EventReceiver, event_channel};
    use crate::test_support::tempdir;

    use std::process::Command;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- fixtures: the checkout.rs git repo, the ai_pipeline snapshot ---

    fn gitx(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(repo)
            .arg("-c")
            .arg("user.name=t")
            .arg("-c")
            .arg("user.email=t@t")
            .arg("-c")
            .arg("commit.gpgsign=false")
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

    /// A bare `origin.git`, a clone on `main`, and a commit pushed as
    /// `refs/pull/7/head` of `acme/app` (the checkout.rs fixture).
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
        gitx(&clone, &["config", "user.name", "t"]);
        gitx(&clone, &["config", "user.email", "t@t"]);
        gitx(&clone, &["config", "commit.gpgsign", "false"]);
        std::fs::write(clone.join("a.txt"), "a\n").expect("write");
        gitx(&clone, &["add", "a.txt"]);
        gitx(&clone, &["commit", "-m", "initial"]);
        gitx(&clone, &["push", "origin", "main"]);
        gitx(&clone, &["checkout", "-b", "feature/x"]);
        std::fs::create_dir_all(clone.join("src")).expect("src dir");
        std::fs::write(clone.join("src/util.rs"), "pub fn helper() {}\n").expect("write");
        std::fs::write(clone.join("src/a.rs"), "fn one() {}\nfn two() {}\n").expect("write");
        gitx(&clone, &["add", "src"]);
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

    const A_PATCH: &str = "@@ -1,2 +1,3 @@\n fn one() {}\n fn two() {}\n+fn helper() {}\n";

    fn snapshot_inputs(head_sha: &str) -> SnapshotInputs {
        SnapshotInputs {
            files: vec![LoadedFile {
                file: PrFile {
                    path: "src/a.rs".to_owned(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    additions: 1,
                    deletions: 0,
                    changes: 1,
                    patch: Some(A_PATCH.to_owned()),
                    patch_state: PatchState::Present,
                    blob_sha: "b-a".to_owned(),
                },
                diff: Some(FileDiff {
                    hunks: parse_patch(A_PATCH).expect("the test patch parses"),
                }),
            }],
            metadata: crate::test_support::meta(head_sha),
        }
    }

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    fn brief_one_file() -> serde_json::Value {
        serde_json::json!({
            "intent": "adds a helper",
            "description_matches": true,
            "groups": [
                { "title": "core", "risk": "medium", "reason": "new logic",
                  "files": ["src/a.rs"] },
            ],
        })
    }

    /// The checklist items the app assigns a source file.
    const SOURCE_ITEMS: [&str; 10] = [
        "intent",
        "correctness",
        "callers",
        "consistency",
        "duplication",
        "tests",
        "api_surface",
        "security",
        "performance",
        "ci",
    ];

    /// The script for a run that completes with no findings: the scout files
    /// its brief, the reviewer checks everything and finishes, the assembler
    /// approves.
    fn clean_scripts() -> BTreeMap<String, Vec<ScriptStep>> {
        let mut calls: Vec<(String, serde_json::Value)> = SOURCE_ITEMS
            .iter()
            .map(|item| {
                (
                    "record_check".to_string(),
                    serde_json::json!({
                        "file": "src/a.rs", "item": item,
                        "result": "ok", "note": "checked",
                    }),
                )
            })
            .collect();
        calls.push((
            "done".to_string(),
            serde_json::json!({"summary": "all checked"}),
        ));
        let mut scripts = BTreeMap::new();
        scripts.insert(
            "scout".into(),
            vec![ScriptStep::ToolCalls(vec![(
                "submit_brief".to_string(),
                brief_one_file(),
            )])],
        );
        scripts.insert(
            "reviewer".into(),
            vec![
                ScriptStep::ToolCalls(calls),
                ScriptStep::Text("all checked".into()),
            ],
        );
        scripts.insert(
            "assembler".into(),
            vec![ScriptStep::ToolCalls(vec![(
                "submit_review_draft".to_string(),
                serde_json::json!({
                    "summary": "a clean change",
                    "verdict": "approve",
                    "rationale": "no findings",
                    "rewrites": [],
                }),
            )])],
        );
        scripts
    }

    /// The clean script whose reviewer first asks the user a question, so
    /// the run parks until the test answers or cancels.
    fn ask_then_clean_scripts() -> BTreeMap<String, Vec<ScriptStep>> {
        let mut scripts = clean_scripts();
        let mut calls: Vec<(String, serde_json::Value)> = SOURCE_ITEMS
            .iter()
            .map(|item| {
                (
                    "record_check".to_string(),
                    serde_json::json!({
                        "file": "src/a.rs", "item": item,
                        "result": "ok", "note": "checked",
                    }),
                )
            })
            .collect();
        calls.push((
            "done".to_string(),
            serde_json::json!({"summary": "all checked"}),
        ));
        scripts.insert(
            "reviewer".into(),
            vec![
                ScriptStep::ToolCalls(vec![(
                    "ask_user".to_string(),
                    serde_json::json!({"question": "which style?"}),
                )]),
                ScriptStep::ToolCalls(calls),
                ScriptStep::Text("all checked".into()),
            ],
        );
        scripts
    }

    // --- harness ---

    struct HubFixture {
        hub: Arc<AiHub>,
        tx: EventSender,
        events: EventReceiver,
        #[allow(dead_code)]
        store: Arc<Store>,
        /// The scripted model, so a test can queue late steps and read the
        /// requests each agent made.
        llm: Arc<ScriptedLlm>,
        client: GithubClient,
        _dir: TempDir,
        clone: PathBuf,
        pr: PrTarget,
        key: PrKey,
        _server: MockServer,
    }

    impl HubFixture {
        fn start(&self) {
            let hub = self.hub.clone();
            let client = self.client.clone();
            let key = self.key.clone();
            let pr = self.pr.clone();
            let inputs = snapshot_inputs(&self.pr.head_sha);
            let events = self.tx.clone();
            tokio::spawn(async move {
                hub.start(
                    client,
                    SessionId(1),
                    key,
                    pr,
                    inputs,
                    RunMode::Full,
                    &events,
                )
                .await;
            });
        }
    }

    /// A hub wired to the fixture: in-memory store, `MemoryStore` tokens,
    /// the `launcher` seam, and `scripts` as the model.
    async fn harness(
        scripts: BTreeMap<String, Vec<ScriptStep>>,
        launcher: SandboxLauncher,
    ) -> HubFixture {
        harness_usage(scripts, launcher, 0, 0).await
    }

    /// [`harness`] whose model reports `input`/`output` tokens on every
    /// reply, for tests that watch the run's stored usage totals.
    async fn harness_usage(
        scripts: BTreeMap<String, Vec<ScriptStep>>,
        launcher: SandboxLauncher,
        input: u64,
        output: u64,
    ) -> HubFixture {
        let (dir, clone, pr) = fixture();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"repository": {"pullRequest": {"reviewThreads": {
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": [],
                }}}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/issues/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/repos/acme/app/commits/[0-9a-f]+/check-runs$",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"check_runs": []})),
            )
            .mount(&server)
            .await;
        let base = server.uri();
        let client = GithubClient::with_endpoints(
            Endpoints {
                web: base.clone(),
                rest: base.clone(),
                graphql: format!("{base}/graphql"),
            },
            "tok",
        );

        let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
        let llm: Arc<ScriptedLlm> =
            Arc::new(ScriptedLlm::per_agent(model(), scripts).usage_per_call(input, output));
        let llm_for_fixture = llm.clone();
        let llms: LlmFactory = Arc::new(move |_settings: &AiSettings, _tokens: &dyn TokenStore| {
            let main: Arc<dyn Llm> = llm.clone();
            let lookup: Option<Arc<dyn Llm>> = None;
            Ok((main, lookup))
        });
        let hub = Arc::new(AiHub::with_seams(
            store.clone(),
            Arc::new(MemoryStore::default()),
            launcher,
            llms,
        ));
        hub.set_settings(AiSettings {
            max_parallel_reviewers: 1,
            repos: vec![RepoAiSettings {
                repo: "github.com/acme/app".to_string(),
                path: clone.clone(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let (tx, events) = event_channel();
        HubFixture {
            hub,
            tx,
            events,
            store,
            llm: llm_for_fixture,
            client,
            _dir: dir,
            clone,
            pr,
            key: PrKey {
                host: "github.com".to_string(),
                owner: "acme".to_string(),
                repo: "app".to_string(),
                number: 7,
            },
            _server: server,
        }
    }

    /// The launcher the tests use: no OS sandbox, plain host bash.
    fn host_launcher() -> SandboxLauncher {
        Arc::new(|_policy, env| {
            Ok(Launch {
                program: OsString::from("bash"),
                args: Vec::new(),
                env,
            })
        })
    }

    fn unavailable_launcher() -> SandboxLauncher {
        Arc::new(|_policy, _env| Err(SandboxUnavailable::NoBwrap))
    }

    /// Wait up to 30s for the next event matching `wanted`, discarding the
    /// ones before it.
    fn wait_for(rx: &EventReceiver, wanted: impl Fn(&Event) -> bool) -> Event {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = rx
                .recv_timeout(remaining)
                .expect("timed out waiting for an event");
            if wanted(&event) {
                return event;
            }
        }
    }

    /// Start `pr` on the hub as a task, mirroring what the worker does for
    /// a confirmed `AiStart`.
    fn start_run(fx: &HubFixture, pr: PrTarget, mode: RunMode) {
        let hub = fx.hub.clone();
        let client = fx.client.clone();
        let key = fx.key.clone();
        let inputs = snapshot_inputs(&pr.head_sha);
        let events = fx.tx.clone();
        tokio::spawn(async move {
            hub.start(client, SessionId(1), key, pr, inputs, mode, &events)
                .await;
        });
    }

    /// The canonical path [`crate::ai::checkout::plan_checkout`] stores
    /// checkout records under.
    fn canonical(fx: &HubFixture) -> PathBuf {
        fx.clone.canonicalize().expect("the fixture clone exists")
    }

    fn is_run_end(event: &Event) -> bool {
        matches!(
            event,
            Event::AiPipeline {
                event: PipelineEvent::RunEnded { .. },
                ..
            }
        )
    }

    fn run_status(event: &Event) -> Option<RunStatus> {
        match event {
            Event::AiPipeline {
                event: PipelineEvent::RunEnded { status, .. },
                ..
            } => Some(*status),
            _ => None,
        }
    }

    // --- the tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn start_checks_out_runs_and_forwards_events_with_the_session() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        let session = SessionId(1);
        fx.start();

        let snapshot = match wait_for(&fx.events, |e| matches!(e, Event::AiSnapshot { .. })) {
            Event::AiSnapshot {
                snapshot,
                session: s,
            } => {
                assert_eq!(s, session);
                snapshot
            }
            _ => unreachable!(),
        };
        let checkout = snapshot.checkout.expect("the run holds a checkout");
        assert_eq!(checkout.pr_number, 7);
        assert_eq!(checkout.state, CheckoutState::Active);
        assert_eq!(checkout.head_sha, fx.pr.head_sha);
        assert_eq!(snapshot.bash_disabled, None);

        // The clone really moved to the PR head.
        assert_eq!(gitx(&fx.clone, &["rev-parse", "HEAD"]), fx.pr.head_sha);

        // The run completes, and every forwarded event carries the session.
        let mut saw_stage = false;
        loop {
            let event = fx
                .events
                .recv_timeout(Duration::from_secs(30))
                .expect("an event");
            match &event {
                Event::AiPipeline { session: s, event } => {
                    assert_eq!(*s, session, "every event carries the session");
                    if matches!(
                        event,
                        PipelineEvent::Stage {
                            stage: StageKind::Scout,
                            status: StageStatus::Started
                        }
                    ) {
                        saw_stage = true;
                    }
                }
                Event::AiSnapshot { session: s, .. } => assert_eq!(*s, session),
                Event::WorkerPanicked(message) => panic!("worker panicked: {message}"),
                _ => {}
            }
            if is_run_end(&event) {
                assert_eq!(run_status(&event), Some(RunStatus::Completed));
                break;
            }
        }
        assert!(saw_stage, "stage lifecycle events were forwarded");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_starts_only_run_one_pipeline() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        // The worker runs every command as its own task, so a double-clicked
        // Start lands as two overlapping `start` calls.
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let hub = fx.hub.clone();
            let client = fx.client.clone();
            let key = fx.key.clone();
            let pr = fx.pr.clone();
            let inputs = snapshot_inputs(&fx.pr.head_sha);
            let events = fx.tx.clone();
            tasks.push(tokio::spawn(async move {
                hub.start(
                    client,
                    SessionId(1),
                    key,
                    pr,
                    inputs,
                    RunMode::Full,
                    &events,
                )
                .await;
            }));
        }
        for task in tasks {
            task.await.expect("the start task runs to its end");
        }

        // The loser said so exactly once; the winner's snapshot arrived; and
        // the single run completed.
        let mut already_running = 0;
        let mut saw_snapshot = false;
        loop {
            let event = fx
                .events
                .recv_timeout(Duration::from_secs(30))
                .expect("an event");
            match &event {
                Event::Toast(message) if message.contains("already running") => {
                    already_running += 1
                }
                Event::AiSnapshot { .. } => saw_snapshot = true,
                Event::WorkerPanicked(message) => panic!("worker panicked: {message}"),
                _ => {}
            }
            if is_run_end(&event) {
                assert_eq!(run_status(&event), Some(RunStatus::Completed));
                break;
            }
        }
        assert_eq!(already_running, 1, "the second Start was rejected");
        assert!(saw_snapshot, "the winner sent its snapshot");
        let runs = fx.hub.runs.lock().unwrap();
        assert_eq!(runs.len(), 1, "exactly one run for the PR");
        let run = runs.get(&fx.key).expect("the run");
        assert!(!run.starting, "the placeholder became the real run");
        assert_eq!(run.checkout.state, CheckoutState::Active);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_rebinds_the_session_and_sends_a_snapshot() {
        let fx = harness(ask_then_clean_scripts(), host_launcher()).await;
        fx.start();

        let question = wait_for(&fx.events, |e| matches!(e, Event::AiQuestion { .. }));
        let (question_id, session) = match &question {
            Event::AiQuestion {
                id, session, text, ..
            } => {
                assert_eq!(*session, SessionId(1));
                assert_eq!(text, "which style?");
                (*id, *session)
            }
            _ => unreachable!(),
        };
        assert_eq!(session, SessionId(1));

        // The user leaves and re-enters the PR: a new session.
        fx.hub.attach(SessionId(2), fx.key.clone(), &fx.tx);
        wait_for(
            &fx.events,
            |e| matches!(e, Event::AiSnapshot { session, .. } if *session == SessionId(2)),
        );

        // The answer arrives on the new session and the run continues;
        // everything after the rebind carries session 2.
        fx.hub.answer(&fx.key, question_id, "dark".to_string());
        let event = wait_for(&fx.events, |e| matches!(e, Event::AiPipeline { .. }));
        match &event {
            Event::AiPipeline { session, .. } => assert_eq!(*session, SessionId(2)),
            _ => unreachable!(),
        }
    }

    /// The run's assembler draft and token totals survive the run: a user
    /// who was on another PR when it finished — whose `DraftReady` the
    /// stale-event rule dropped — gets both back from the attach snapshot,
    /// so the pre-filled review and the "suggested" line still happen
    /// (spec §5.6, §10.3).
    #[tokio::test(flavor = "multi_thread")]
    async fn attach_replays_the_finished_run_draft_and_usage() {
        use crate::ai::stages::assemble::Verdict;

        let fx = harness_usage(clean_scripts(), host_launcher(), 100, 40).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });

        // The user re-enters the PR: a new session, and the snapshot that
        // rebuilds the tab carries the finished run's draft and usage.
        fx.hub.attach(SessionId(2), fx.key.clone(), &fx.tx);
        let snapshot = match wait_for(
            &fx.events,
            |e| matches!(e, Event::AiSnapshot { session, .. } if *session == SessionId(2)),
        ) {
            Event::AiSnapshot { snapshot, .. } => snapshot,
            _ => unreachable!(),
        };
        let draft = snapshot
            .draft
            .expect("the finished run's draft replays with the snapshot");
        assert_eq!(draft.summary, "a clean change");
        assert_eq!(draft.verdict, Verdict::Approve);
        assert!(
            snapshot.usage.0 > 0 && snapshot.usage.1 > 0,
            "the token tally rides the snapshot too: {:?}",
            snapshot.usage
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answer_after_cancel_is_ignored() {
        let fx = harness(ask_then_clean_scripts(), host_launcher()).await;
        fx.start();

        let question = wait_for(&fx.events, |e| matches!(e, Event::AiQuestion { .. }));
        let question_id = match question {
            Event::AiQuestion { id, .. } => id,
            _ => unreachable!(),
        };

        fx.hub.cancel(&fx.key, &fx.tx);
        let ended = wait_for(&fx.events, is_run_end);
        assert_eq!(run_status(&ended), Some(RunStatus::Cancelled));

        // The late answer finds nothing parked: it must not panic or wake
        // anything, and the hub stays usable.
        fx.hub.answer(&fx.key, question_id, "too late".to_string());
        let late = std::time::Instant::now() + Duration::from_millis(300);
        while let Some(event) = fx
            .events
            .recv_timeout(late.saturating_duration_since(std::time::Instant::now()))
        {
            assert!(
                !matches!(event, Event::WorkerPanicked(_)),
                "the late answer panicked the worker"
            );
        }
        fx.hub.attach(SessionId(3), fx.key.clone(), &fx.tx);
        wait_for(
            &fx.events,
            |e| matches!(e, Event::AiSnapshot { session, .. } if *session == SessionId(3)),
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sandbox_unavailable_disables_bash_and_says_why() {
        let fx = harness(clean_scripts(), unavailable_launcher()).await;
        fx.start();

        let snapshot = match wait_for(&fx.events, |e| matches!(e, Event::AiSnapshot { .. })) {
            Event::AiSnapshot { snapshot, .. } => snapshot,
            _ => unreachable!(),
        };
        assert_eq!(
            snapshot.bash_disabled.as_deref(),
            Some("bubblewrap (bwrap) is not installed")
        );

        // The run still completes without the sandbox.
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finish_restores_and_reports_needs_decision_when_dirty() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });

        // The run left a change behind (an agent wrote a file).
        std::fs::write(fx.clone.join("stray.txt"), "agent wrote me\n").expect("stray");

        // Ask-first: the changes come back for a decision, the run stays.
        fx.hub.finish(SessionId(1), &fx.key, None, &fx.tx).await;
        let decision = wait_for(&fx.events, |e| {
            matches!(e, Event::AiRestoreNeedsDecision { .. })
        });
        match decision {
            Event::AiRestoreNeedsDecision { changes, session } => {
                assert_eq!(session, SessionId(1));
                assert!(
                    changes.iter().any(|c| c.contains("stray.txt")),
                    "{changes:?}"
                );
            }
            _ => unreachable!(),
        }

        // Discard: the tree goes back and the run closes.
        fx.hub
            .finish(SessionId(1), &fx.key, Some(true), &fx.tx)
            .await;
        let restored = wait_for(&fx.events, |e| matches!(e, Event::AiRestored { .. }));
        match restored {
            Event::AiRestored { outcome, .. } => assert_eq!(outcome, RestoreOutcome::Restored),
            _ => unreachable!(),
        }
        assert_eq!(
            gitx(&fx.clone, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(!fx.clone.join("stray.txt").exists());
        assert!(
            !fx.hub.runs.lock().unwrap().contains_key(&fx.key),
            "the run is closed"
        );
    }

    /// The restore dialog's **Keep** answer (spec §7.3 step 1): the finish
    /// stops without touching the tree, marks the record `Failed` so it stays
    /// a recovery target, and tells the tab what state it left. The
    /// crash-recovery dialog then restores it, stash and all.
    #[tokio::test(flavor = "multi_thread")]
    async fn finish_keep_leaves_the_tree_and_recovery_restores_it() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        // The reviewer's own work before the run: stashed by the checkout and
        // re-applied by the restore.
        std::fs::write(fx.clone.join("a.txt"), "my local edit\n").expect("dirty");
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });

        // The run left a change behind (an agent wrote a file).
        std::fs::write(fx.clone.join("stray.txt"), "agent wrote me\n").expect("stray");
        let head = gitx(&fx.clone, &["rev-parse", "HEAD"]);
        assert_eq!(head, fx.pr.head_sha, "the run holds the PR head");

        // Keep: stop, leave everything where it is, say what state it is in.
        fx.hub
            .finish(SessionId(1), &fx.key, Some(false), &fx.tx)
            .await;
        let notice = wait_for(&fx.events, |e| matches!(e, Event::AiNotice { .. }));
        match notice {
            Event::AiNotice { session, text } => {
                assert_eq!(session, SessionId(1));
                assert!(text.contains("left the checkout as is"), "{text}");
                assert!(text.contains("branch main"), "{text}");
                assert!(
                    text.contains("another pull request"),
                    "the advice names the route that exists: {text}"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            gitx(&fx.clone, &["rev-parse", "HEAD"]),
            head,
            "Keep does not move the tree"
        );
        assert!(fx.clone.join("stray.txt").exists(), "Keep discards nothing");
        assert!(
            !fx.hub.runs.lock().unwrap().contains_key(&fx.key),
            "the run is closed"
        );

        // Still a recovery target: `Failed`, holding the repo.
        let unrestored = fx.store.checkouts_unrestored().expect("the recovery list");
        assert_eq!(unrestored.len(), 1);
        assert_eq!(unrestored[0].state, CheckoutState::Failed);
        assert_eq!(unrestored[0].pr_number, 7);

        // The recovery dialog's Restore: back to the original ref, stash
        // applied, stray work gone, record no longer unrestored.
        fx.hub.recover_restore(unrestored[0].id, &fx.tx).await;
        let toast = wait_for(&fx.events, |e| matches!(e, Event::Toast(_)));
        assert!(
            matches!(&toast, Event::Toast(text) if text.contains("restored")),
            "{toast:?}"
        );
        assert_eq!(
            gitx(&fx.clone, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(!fx.clone.join("stray.txt").exists());
        assert_eq!(
            std::fs::read_to_string(fx.clone.join("a.txt")).expect("a.txt"),
            "my local edit\n",
            "the pre-checkout stash came back"
        );
        assert!(
            fx.store.checkouts_unrestored().expect("list").is_empty(),
            "the record is restored"
        );
    }

    /// After Keep the record still holds the repo with no run behind it, so
    /// "finish that review" can never work. A review on another PR in the
    /// same repo must get a Held event that carries the record, letting the
    /// dialog restore it right there (Task 4 review finding 2).
    #[tokio::test(flavor = "multi_thread")]
    async fn held_after_keep_offers_the_restore_finish_cannot() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        std::fs::write(fx.clone.join("a.txt"), "my local edit\n").expect("dirty");
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        // The run's own change makes Keep the only option that keeps it.
        std::fs::write(fx.clone.join("stray.txt"), "agent wrote me\n").expect("stray");
        fx.hub
            .finish(SessionId(1), &fx.key, Some(false), &fx.tx)
            .await;
        wait_for(&fx.events, |e| matches!(e, Event::AiNotice { .. }));
        let rec = fx
            .store
            .checkouts_unrestored()
            .expect("the recovery list")
            .into_iter()
            .next()
            .expect("#7 still holds the repo");

        // A review on PR #8 in the same repository is held — and told where
        // the checkout can be restored from.
        let mut other_pr = fx.pr.clone();
        other_pr.number = 8;
        let other_key = PrKey {
            number: 8,
            ..fx.key.clone()
        };
        fx.hub
            .plan_checkout(SessionId(1), other_key, other_pr, &fx.tx)
            .await;
        match wait_for(&fx.events, |e| matches!(e, Event::AiCheckoutHeld { .. })) {
            Event::AiCheckoutHeld {
                pr_number, restore, ..
            } => {
                assert_eq!(pr_number, 7, "the holding PR is named");
                assert_eq!(
                    restore,
                    Some(rec.id),
                    "the dialog gets the record, not only the number"
                );
            }
            _ => unreachable!(),
        }

        // That restore is real: #8's next plan is free to proceed.
        fx.hub.recover_restore(rec.id, &fx.tx).await;
        wait_for(&fx.events, |e| matches!(e, Event::Toast(_)));
        assert!(
            fx.store.checkouts_unrestored().expect("list").is_empty(),
            "the holding record is restored"
        );
    }

    // --- checkout safety ---

    /// A stored record on the fixture's repo, for the recovery tests.
    fn stored_record(
        fx: &HubFixture,
        state: CheckoutState,
        stash_sha: Option<String>,
    ) -> CheckoutRecord {
        let mut rec = CheckoutRecord {
            id: 0,
            repo_path: canonical(fx),
            repo: "acme/app".to_owned(),
            pr_number: 7,
            original: OriginalRef::Branch("main".to_owned()),
            stash_sha,
            head_sha: fx.pr.head_sha.clone(),
            state,
        };
        rec.id = fx.store.checkout_begin(&rec).expect("record");
        rec
    }

    /// A stale or double-clicked recovery dialog must not reset the tree
    /// for a record that was already restored or replaced.
    #[tokio::test(flavor = "multi_thread")]
    async fn recover_restore_ignores_finished_records() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        for state in [CheckoutState::Restored, CheckoutState::Superseded] {
            let rec = stored_record(&fx, state, None);
            std::fs::write(fx.clone.join("a.txt"), "typed since\n").expect("edit");
            std::fs::write(fx.clone.join("mine.txt"), "mine\n").expect("new");
            fx.hub.recover_restore(rec.id, &fx.tx).await;
            wait_for(&fx.events, |e| matches!(e, Event::Toast(_)));
            assert_eq!(
                std::fs::read_to_string(fx.clone.join("a.txt")).unwrap(),
                "typed since\n",
                "{state:?} must not be reset"
            );
            assert!(fx.clone.join("mine.txt").exists(), "{state:?}");
            std::fs::remove_file(fx.clone.join("mine.txt")).unwrap();
            gitx(&fx.clone, &["checkout", "a.txt"]);
        }
    }

    /// A failed record with no stash may sit on the user's own work (a
    /// failed `git stash push`): recovery must not discard it.
    #[tokio::test(flavor = "multi_thread")]
    async fn recover_restore_keeps_unstashed_work_of_a_failed_record() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        let rec = stored_record(&fx, CheckoutState::Failed, None);
        std::fs::write(fx.clone.join("a.txt"), "precious\n").expect("edit");
        std::fs::write(fx.clone.join("mine.txt"), "mine\n").expect("new");
        fx.hub.recover_restore(rec.id, &fx.tx).await;
        match wait_for(&fx.events, |e| matches!(e, Event::Toast(_))) {
            Event::Toast(text) => assert!(text.contains("nothing was discarded"), "{text}"),
            _ => unreachable!(),
        }
        assert_eq!(
            std::fs::read_to_string(fx.clone.join("a.txt")).unwrap(),
            "precious\n"
        );
        assert!(fx.clone.join("mine.txt").exists());
        let held = fx.store.checkout_get(rec.id).unwrap().unwrap();
        assert_eq!(held.state, CheckoutState::Failed, "still recoverable");
    }

    /// A Start for a PR whose finish is restoring the tree must not replace
    /// the entry, and a handle can only remove or re-install its own slot.
    #[tokio::test(flavor = "multi_thread")]
    async fn start_waits_out_a_finish_and_handles_only_touch_their_own_slot() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        let live = {
            let mut runs = fx.hub.runs.lock().unwrap();
            let run = runs.get_mut(&fx.key).expect("the finished run stays");
            run.finishing = true;
            run.live.clone()
        };

        fx.hub
            .start(
                fx.client.clone(),
                SessionId(1),
                fx.key.clone(),
                fx.pr.clone(),
                snapshot_inputs(&fx.pr.head_sha),
                RunMode::Full,
                &fx.tx,
            )
            .await;
        match wait_for(&fx.events, |e| matches!(e, Event::Toast(_))) {
            Event::Toast(text) => assert!(text.contains("being restored"), "{text}"),
            _ => unreachable!(),
        }
        {
            let runs = fx.hub.runs.lock().unwrap();
            let run = runs.get(&fx.key).expect("the entry is untouched");
            assert!(Arc::ptr_eq(&run.live, &live), "the old run still owns it");
        }

        // A foreign handle can neither evict the entry nor overwrite it.
        let foreign = RunHandle::starting(Arc::new(Mutex::new(SessionId(9))), &fx.tx);
        fx.hub.close_run(&fx.key, foreign);
        let foreign = RunHandle::starting(Arc::new(Mutex::new(SessionId(9))), &fx.tx);
        fx.hub.put_back(&fx.key, foreign);
        let runs = fx.hub.runs.lock().unwrap();
        let run = runs.get(&fx.key).expect("still there");
        assert!(Arc::ptr_eq(&run.live, &live));
    }

    /// A pipeline that ignores the cancel is aborted (not detached), and a
    /// pipeline that stops by itself is left alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn halt_pipeline_aborts_a_stuck_task_and_waits_for_it() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Flag(dropped.clone());
        let stuck = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        assert!(halt_pipeline(stuck, Duration::from_millis(50)).await);
        assert!(
            dropped.load(Ordering::SeqCst),
            "the task is gone by the time halt returns"
        );

        let quick = tokio::spawn(async {});
        assert!(!halt_pipeline(quick, Duration::from_secs(5)).await);
    }

    /// The agent shell lives while any owner holds the sandbox; the restore
    /// must wait for the last owner, and give up if one never lets go.
    #[tokio::test(flavor = "multi_thread")]
    async fn sandbox_release_waits_for_the_last_owner() {
        let dir = tempfile::tempdir().expect("tmp");
        let host: Arc<dyn SandboxOps> = Arc::new(LocalHost::new(dir.path()).expect("host"));
        let weak = Arc::downgrade(&host);
        assert!(!sandbox_released(&weak, Duration::from_millis(60)).await);
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            drop(host);
        });
        assert!(sandbox_released(&weak, Duration::from_secs(5)).await);
        release.await.unwrap();
    }

    // --- the lifecycle (Task 8) ---

    /// A finish over a clean tree is a restore, not a question: the run
    /// closes with `AiRestored` and no `AiRestoreNeedsDecision` ever
    /// interrupts (spec §7.3, Task 8).
    #[tokio::test(flavor = "multi_thread")]
    async fn finish_with_clean_tree_restores_without_asking() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        assert!(
            close_requires_finish(&fx.hub.close_state()),
            "the finished run still holds the checkout"
        );

        fx.hub.finish(SessionId(1), &fx.key, None, &fx.tx).await;
        loop {
            let event = fx
                .events
                .recv_timeout(Duration::from_secs(30))
                .expect("an event");
            assert!(
                !matches!(event, Event::AiRestoreNeedsDecision { .. }),
                "a clean tree must not ask: {event:?}"
            );
            if matches!(event, Event::AiRestored { .. }) {
                break;
            }
        }

        assert_eq!(
            gitx(&fx.clone, &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
        assert!(
            !fx.hub.runs.lock().unwrap().contains_key(&fx.key),
            "the run is closed"
        );
        assert!(
            !close_requires_finish(&fx.hub.close_state()),
            "nothing holds a checkout any more"
        );
    }

    /// One repository, one checkout: while PR #7's run holds it, a plan for
    /// PR #8 is rejected with `Held` naming #7 — and once #7's review is
    /// over with no run left to finish, the event carries the record so the
    /// dialog can offer the restore right there (spec §4 step 7, Task 8).
    #[tokio::test(flavor = "multi_thread")]
    async fn held_repo_rejects_second_pr() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });

        // The run (finished, unrestored) still holds the checkout: #8 is
        // held, and its dialog points at that run's Finish, not a restore.
        let other_pr = PrTarget {
            number: 8,
            ..fx.pr.clone()
        };
        let other_key = PrKey {
            number: 8,
            ..fx.key.clone()
        };
        fx.hub
            .plan_checkout(SessionId(1), other_key.clone(), other_pr.clone(), &fx.tx)
            .await;
        match wait_for(&fx.events, |e| matches!(e, Event::AiCheckoutHeld { .. })) {
            Event::AiCheckoutHeld {
                pr_number, restore, ..
            } => {
                assert_eq!(pr_number, 7, "the holding PR is named");
                assert_eq!(restore, None, "the live Finish is the way out");
            }
            other => unreachable!("expected a held checkout: {other:?}"),
        }

        // The review's own change makes Keep the option that leaves the
        // checkout held with no run behind it.
        std::fs::write(fx.clone.join("stray.txt"), "agent wrote me\n").expect("stray");
        fx.hub
            .finish(SessionId(1), &fx.key, Some(false), &fx.tx)
            .await;
        wait_for(&fx.events, |e| matches!(e, Event::AiNotice { .. }));
        let rec = fx
            .store
            .checkouts_unrestored()
            .expect("the recovery list")
            .into_iter()
            .next()
            .expect("#7 still holds the repo");

        // #8 is still held — and now the event carries the record, so the
        // dialog offers the restore that is the only way out.
        fx.hub
            .plan_checkout(SessionId(1), other_key, other_pr, &fx.tx)
            .await;
        match wait_for(&fx.events, |e| matches!(e, Event::AiCheckoutHeld { .. })) {
            Event::AiCheckoutHeld {
                pr_number, restore, ..
            } => {
                assert_eq!(pr_number, 7);
                assert_eq!(restore, Some(rec.id), "the dialog gets the restore");
            }
            other => unreachable!("expected a held checkout: {other:?}"),
        }
    }

    /// Continue review while this PR still holds the checkout: the tree is
    /// already the PR head, so the plan skips the stash step (stashing
    /// would bury the review's build output under a new stash), the run
    /// re-fetches and checks out the NEW head, and the held record's stash
    /// and original ref carry into the new row (Task 8 ruling 2).
    #[tokio::test(flavor = "multi_thread")]
    async fn continue_while_held_refetches_without_stash() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        // The user's own work: the first checkout stashes it.
        std::fs::write(fx.clone.join("a.txt"), "my local edit\n").expect("dirty");
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        let first = fx
            .store
            .checkout_holding(&canonical(&fx))
            .expect("store")
            .expect("the run holds the checkout");
        let stash_sha = first
            .stash_sha
            .clone()
            .expect("the dirty start stashed the user's work");

        // The review's build output lands in the tree, and the PR head
        // moves: a new commit reaches `refs/pull/7/head`.
        std::fs::write(fx.clone.join("build-output.txt"), "from the review\n").expect("build");
        std::fs::write(fx.clone.join("ship.txt"), "ship\n").expect("ship");
        gitx(&fx.clone, &["add", "ship.txt"]);
        gitx(&fx.clone, &["commit", "-m", "second"]);
        gitx(&fx.clone, &["push", "origin", "HEAD:refs/pull/7/head"]);
        let new_head = gitx(&fx.clone, &["rev-parse", "HEAD"]);
        gitx(&fx.clone, &["checkout", "--detach", &fx.pr.head_sha]);
        assert!(!fx.clone.join("ship.txt").exists(), "back on the old head");

        // The continue plan: the same commands minus the stash line.
        let next = PrTarget {
            head_sha: new_head.clone(),
            ..fx.pr.clone()
        };
        fx.hub
            .plan_checkout(SessionId(1), fx.key.clone(), next.clone(), &fx.tx)
            .await;
        let plan = match wait_for(&fx.events, |e| matches!(e, Event::AiCheckoutPlanned { .. })) {
            Event::AiCheckoutPlanned { plan, .. } => plan,
            other => unreachable!("expected a plan: {other:?}"),
        };
        assert!(plan.dirty, "the build output is still in the tree");
        assert!(plan.skip_stash, "the held checkout skips the stash step");
        assert!(
            !plan.commands.iter().any(|c| c.contains("stash")),
            "no stash line: {:?}",
            plan.commands
        );
        assert_eq!(
            plan.commands,
            vec![
                "git fetch origin pull/7/head".to_string(),
                format!("git checkout --detach {new_head}"),
            ]
        );

        // The continue run: the new head is fetched and checked out, no new
        // stash is created, and the original stash and ref ride along. The
        // scripted model consumes its steps, so run two gets its own copy.
        for (agent, steps) in clean_scripts() {
            for step in steps {
                fx.llm.push_agent_step(&agent, step);
            }
        }
        start_run(&fx, next, RunMode::Full);
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        assert_eq!(gitx(&fx.clone, &["rev-parse", "HEAD"]), new_head);
        assert_eq!(
            gitx(&fx.clone, &["stash", "list", "--format=%H"]),
            stash_sha,
            "exactly the original stash entry, no new one"
        );
        let held = fx
            .store
            .checkout_holding(&canonical(&fx))
            .expect("store")
            .expect("the new run holds the checkout");
        assert_eq!(held.stash_sha.as_deref(), Some(stash_sha.as_str()));
        assert_eq!(held.original, OriginalRef::Branch("main".to_string()));
        assert_eq!(held.head_sha, new_head);
        assert_eq!(
            fx.store.checkouts_unrestored().expect("list").len(),
            1,
            "only the new row is a recovery target"
        );
    }

    /// The pure quit decision: the window may close freely when no run
    /// holds a checkout; any held checkout means the app must run the
    /// Finish flow first (spec §7.3, Task 8).
    #[test]
    fn quit_with_held_checkout_runs_finish() {
        let key = PrKey {
            host: "github.com".to_string(),
            owner: "acme".to_string(),
            repo: "app".to_string(),
            number: 7,
        };
        assert!(
            !close_requires_finish(&HubCloseState::default()),
            "no runs, nothing to finish"
        );
        assert!(
            !close_requires_finish(&HubCloseState {
                runs: vec![RunCloseState {
                    key: key.clone(),
                    session: SessionId(1),
                    holds_checkout: false,
                }],
            }),
            "a start placeholder holds no checkout"
        );
        assert!(
            close_requires_finish(&HubCloseState {
                runs: vec![
                    RunCloseState {
                        key: key.clone(),
                        session: SessionId(1),
                        holds_checkout: false,
                    },
                    RunCloseState {
                        key: key.clone(),
                        session: SessionId(2),
                        holds_checkout: true,
                    },
                ],
            }),
            "one held checkout is enough to hold the window open"
        );
    }

    /// A finish whose preview fails must not wedge the run: the run goes
    /// back into the map, so the next finish actually attempts the restore
    /// again instead of being silently swallowed by the `finishing` guard
    /// forever (Task 8 review).
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_preview_leaves_the_run_finishable() {
        let fx = harness(clean_scripts(), host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });

        // The clone vanishes: the preview's `git status` cannot run.
        std::fs::remove_dir_all(&fx.clone).expect("remove the clone");

        fx.hub.finish(SessionId(1), &fx.key, None, &fx.tx).await;
        let first = wait_for(&fx.events, |e| matches!(e, Event::Toast(_)));
        assert!(
            matches!(&first, Event::Toast(t) if t.contains("could not inspect")),
            "the failed preview is reported: {first:?}"
        );

        // The run is finishable again: the second attempt really runs —
        // its own toast arrives — instead of being silently ignored.
        fx.hub.finish(SessionId(1), &fx.key, None, &fx.tx).await;
        let second = wait_for(&fx.events, |e| matches!(e, Event::Toast(_)));
        assert!(
            matches!(&second, Event::Toast(t) if t.contains("could not inspect")),
            "the second finish attempted the restore again: {second:?}"
        );
    }

    // --- the chat agent (Task 7) ---

    /// Collect this turn's chat events up to and including `TurnDone`,
    /// discarding everything else on the stream.
    fn wait_chat_turn(rx: &EventReceiver) -> Vec<ChatEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx
                .recv_timeout(remaining)
                .expect("the chat turn finished in time")
            {
                Event::AiChat { event, .. } => {
                    let done = matches!(event, ChatEvent::TurnDone);
                    events.push(event);
                    if done {
                        return events;
                    }
                }
                Event::WorkerPanicked(message) => panic!("worker panicked: {message}"),
                _ => {}
            }
        }
    }

    /// Every text an agent's request carries: text blocks and tool results.
    fn all_text(messages: &[review_llm::Message]) -> String {
        let mut out = String::new();
        for message in messages {
            for block in &message.content {
                match block {
                    review_llm::ContentBlock::Text { text } => {
                        out.push_str(text);
                        out.push('\n');
                    }
                    review_llm::ContentBlock::ToolResult { content, .. } => {
                        for part in content {
                            if let review_llm::ToolResultPart::Text { text } = part {
                                out.push_str(text);
                                out.push('\n');
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// The live run's chat task id, or `None` before the first message.
    fn chat_task_id(fx: &HubFixture) -> Option<tokio::task::Id> {
        fx.hub
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&fx.key)
            .and_then(|run| run.chat_task.as_ref())
            .map(tokio::task::JoinHandle::id)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_message_spawns_chat_and_later_messages_reuse_it() {
        let mut scripts = clean_scripts();
        scripts.insert(
            "chat".into(),
            vec![
                ScriptStep::Text("the helper is new code".into()),
                ScriptStep::Text("the tests cover it".into()),
            ],
        );
        let fx = harness(scripts, host_launcher()).await;
        fx.start();
        wait_for(&fx.events, |e| matches!(e, Event::AiSnapshot { .. }));
        assert!(
            chat_task_id(&fx).is_none(),
            "the chat task spawns on the first message, not at start"
        );

        fx.hub.user_message(
            SessionId(1),
            &fx.key,
            "what does the PR do?".to_string(),
            &fx.tx,
        );
        let turn = wait_chat_turn(&fx.events);
        assert!(
            turn.contains(&ChatEvent::User {
                text: "what does the PR do?".to_string()
            }),
            "the message is echoed into the stream: {turn:?}"
        );
        assert!(
            turn.contains(&ChatEvent::Text {
                delta: "the helper is new code".into()
            }),
            "the reply streams: {turn:?}"
        );
        let first = chat_task_id(&fx).expect("the first message spawned the chat task");

        fx.hub
            .user_message(SessionId(1), &fx.key, "and the tests?".to_string(), &fx.tx);
        let turn = wait_chat_turn(&fx.events);
        assert_eq!(
            turn.last(),
            Some(&ChatEvent::TurnDone),
            "the second turn ended"
        );
        assert_eq!(
            chat_task_id(&fx),
            Some(first),
            "the second message reused the same chat task"
        );

        // One model turn per message, and the second turn carried the first
        // message: the long-lived task kept its history.
        let requests = fx.llm.requests_for("chat");
        assert_eq!(requests.len(), 2, "one model call per message");
        assert!(
            all_text(&requests[1].messages).contains("what does the PR do?"),
            "the first message rode along on the second turn"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chat_can_pause_and_note_a_running_reviewer() {
        let fx = harness(ask_then_clean_scripts(), host_launcher()).await;
        fx.start();
        let question = wait_for(&fx.events, |e| matches!(e, Event::AiQuestion { .. }));
        let (question_id, reviewer_id) = match &question {
            Event::AiQuestion { id, agent, .. } => (*id, agent.id),
            _ => unreachable!(),
        };

        // The chat's turn is scripted once the reviewer's id is known.
        fx.llm.push_agent_step(
            "chat",
            ScriptStep::ToolCalls(vec![
                ("pause_run".to_string(), serde_json::json!({})),
                (
                    "note_agent".to_string(),
                    serde_json::json!({
                        "agent_id": reviewer_id,
                        "text": "focus on the parser",
                    }),
                ),
            ]),
        );
        fx.llm
            .push_agent_step("chat", ScriptStep::Text("paused and noted".into()));

        fx.hub.user_message(
            SessionId(1),
            &fx.key,
            "pause it and tell the reviewer to focus on the parser".to_string(),
            &fx.tx,
        );
        // Typing never pauses the chat itself (spec §6.5): the turn runs to
        // completion even though its first tool pauses the pipeline.
        let turn = wait_chat_turn(&fx.events);
        let tools: Vec<&str> = turn
            .iter()
            .filter_map(|event| match event {
                ChatEvent::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, vec!["pause_run", "note_agent"], "{turn:?}");

        // The pipeline is held, and the reviewer — awake now that the
        // question is answered — stops before its next model call.
        let paused = fx
            .hub
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&fx.key)
            .and_then(|run| run.control.as_ref())
            .map(|control| control.is_paused())
            .expect("the live run has control");
        assert!(paused, "pause_run held the pipeline");
        fx.hub
            .answer(&fx.key, question_id, "the old one".to_string());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            fx.llm.requests_for("reviewer").len(),
            1,
            "the pause holds the reviewer before its next model call"
        );

        // Resume: the reviewer continues, and its next request carries the
        // note the chat delivered.
        fx.hub.resume(&fx.key, &fx.tx);
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        let requests = fx.llm.requests_for("reviewer");
        assert!(requests.len() >= 2, "the reviewer continued");
        assert!(
            all_text(&requests[1].messages).contains("[note from the user] focus on the parser"),
            "note_agent's note reached the reviewer's next turn"
        );

        // RunEnded clears the run's liveness: from here the chat's steering
        // tools answer "the review has finished" (spec §6.5).
        let live = fx
            .hub
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&fx.key)
            .map(|run| run.live.load(Ordering::SeqCst));
        assert_eq!(
            live,
            Some(false),
            "the forwarder cleared `live` on RunEnded"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answer_question_resolves_the_pending_ask() {
        let fx = harness(ask_then_clean_scripts(), host_launcher()).await;
        fx.start();
        let question = wait_for(&fx.events, |e| matches!(e, Event::AiQuestion { .. }));
        let question_id = match &question {
            Event::AiQuestion { id, .. } => *id,
            _ => unreachable!(),
        };

        fx.llm.push_agent_step(
            "chat",
            ScriptStep::ToolCalls(vec![(
                "answer_question".to_string(),
                serde_json::json!({
                    "question_id": question_id,
                    "answer": "the old one",
                }),
            )]),
        );
        fx.llm
            .push_agent_step("chat", ScriptStep::Text("answered it for you".into()));

        fx.hub.user_message(
            SessionId(1),
            &fx.key,
            "the reviewer asks about style; say the old one".to_string(),
            &fx.tx,
        );
        let turn = wait_chat_turn(&fx.events);
        assert_eq!(turn.last(), Some(&ChatEvent::TurnDone));

        // The digest the chat got listed the parked question by its id —
        // that is where the model learned which id to answer.
        let chat_requests = fx.llm.requests_for("chat");
        assert!(
            all_text(&chat_requests[0].messages)
                .contains(&format!("open questions: #{question_id}")),
            "the digest carries the open question id"
        );

        // No hub-level answer was given: the chat's tool resolved the parked
        // ask, the reviewer continued, and its next turn carries the answer.
        wait_for(&fx.events, |e| {
            is_run_end(e) && run_status(e) == Some(RunStatus::Completed)
        });
        let requests = fx.llm.requests_for("reviewer");
        assert!(requests.len() >= 2, "the reviewer continued");
        assert!(
            all_text(&requests[1].messages).contains("the old one"),
            "the chat's answer reached the parked ask_user"
        );
    }
}
