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
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::ai::checkout::{
    CheckoutError, CheckoutPlan, PrTarget, PrepareOutput, execute_checkout, restore,
    restore_preview, run_prepare,
};
use crate::ai::events::PipelineEvent;
use crate::ai::interaction::{ApprovalDecision, Interaction};
use crate::ai::llm;
use crate::ai::model::Llm;
use crate::ai::pipeline::{PipelineControl, PipelineInputs, RunMode, RunStatus, start_pipeline};
use crate::ai::review::{CoverageEntry, Finding, FindingState};
use crate::ai::snapshot::gather_snapshot;
use crate::ai::store::{CheckoutRecord, CheckoutState, PrKey, RunRecord, Store, ai_db_path};
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
    pub control: Option<PipelineControl>,
    /// The run's current session, rebound by [`AiHub::attach`] (spec §10.2).
    pub session: Arc<Mutex<SessionId>>,
    /// The parked questions and approvals, shared with the answer commands.
    pub interaction: Arc<UiInteraction>,
    /// The checkout this run holds until [`AiHub::finish`].
    pub checkout: CheckoutRecord,
    /// The chat agent's task (Task 7).
    pub chat: Option<JoinHandle<()>>,
    /// Why the agent shell is unsandboxed (spec §8.2).
    pub bash_disabled: Option<String>,
    /// Whether the pipeline is still running; the forwarder clears it.
    live: Arc<AtomicBool>,
    /// The orchestrator's handle, awaited by [`AiHub::finish`] so the agents
    /// are gone before the working tree is restored.
    pipeline: Option<JoinHandle<RunStatus>>,
    /// The run's agent-owned temp dir, removed when the run closes.
    tmp_dir: Option<PathBuf>,
}

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

    fn repo_settings(&self, key: &PrKey) -> Option<RepoAiSettings> {
        self.settings()
            .repo(&key.host, &key.owner, &key.repo)
            .cloned()
    }

    /// Rebind the run for `key` to `session` and replay its full state
    /// (spec §10.2). Works with no live run too: the stored findings and
    /// checkout still describe the PR.
    pub fn attach(&self, session: SessionId, key: PrKey, events: &EventSender) {
        let live = self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|run| {
                *run.session.lock().unwrap_or_else(|e| e.into_inner()) = session;
                (run.checkout.clone(), run.bash_disabled.clone())
            });
        match self.snapshot_data(&key, live.as_ref()) {
            Ok(snapshot) => events.send(Event::AiSnapshot { session, snapshot }),
            Err(error) => events.send(Event::Toast(format!(
                "could not load the AI review state: {error}"
            ))),
        }
    }

    fn snapshot_data(
        &self,
        key: &PrKey,
        live: Option<&(CheckoutRecord, Option<String>)>,
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
        let (checkout, bash_disabled) = match live {
            Some((rec, bash)) => (Some(rec.clone()), bash.clone()),
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
        // A live run for this PR owns the checkout; a finished one that was
        // never finished() is replaced.
        let replacing = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            match runs.get(&key) {
                Some(run) if run.live.load(Ordering::SeqCst) => {
                    events.send(Event::Toast(
                        "an AI review is already running for this PR".to_owned(),
                    ));
                    return;
                }
                Some(_) => runs.remove(&key),
                None => None,
            }
        };
        if let Some(old) = replacing {
            close_run_handle(old);
        }

        let Some(repo_settings) = self.repo_settings(&key) else {
            events.send(Event::Toast(format!(
                "no AI checkout folder is configured for {}/{}/{} — set it in Settings",
                key.host, key.owner, key.repo
            )));
            return;
        };
        let settings = self.settings();

        // Undo a checkout this start made when a later step fails.
        let hub_store = self.store.clone();
        let abort = move |rec: CheckoutRecord, events: EventSender, message: String| {
            let store = hub_store.clone();
            async move {
                events.send(Event::AiCheckoutFailed { session, message });
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
                            session,
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
                        session,
                        message: error.to_string(),
                    });
                    return;
                }
                Err(_) => {
                    events.send(Event::AiCheckoutFailed {
                        session,
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
                session,
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
                    session: Some(session),
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
        let session_cell = Arc::new(Mutex::new(session));
        let interaction = UiInteraction::new(
            events.clone(),
            session_cell.clone(),
            repo_settings.prepare.clone(),
        );
        let (control, mut rx, pipeline) = start_pipeline(PipelineInputs {
            store: self.store.clone(),
            session_id: ai_session_id,
            snapshot,
            repo_root: plan.repo_path.clone(),
            sandbox,
            settings,
            repo_settings,
            llm,
            lookup_llm,
            interaction: interaction.clone() as Arc<dyn Interaction>,
            mode,
            bash_disabled: bash_disabled.clone(),
        });
        if let Err(error) = self.store.checkout_set_state(rec.id, CheckoutState::Active) {
            tracing::warn!(%error, "ai hub: marking the checkout active failed");
        }

        let live = Arc::new(AtomicBool::new(true));
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key.clone(),
            RunHandle {
                control: Some(control),
                session: session_cell.clone(),
                interaction: interaction.clone(),
                checkout: rec,
                chat: None,
                bash_disabled: bash_disabled.clone(),
                live: live.clone(),
                pipeline: Some(pipeline),
                tmp_dir: Some(tmp_dir),
            },
        );

        let forward_events = events.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let session = *session_cell.lock().unwrap_or_else(|e| e.into_inner());
                forward_events.send(Event::AiPipeline { session, event });
            }
            live.store(false, Ordering::SeqCst);
            // Anything still parked (a question the user never answered)
            // resolves the way `NoInteraction` would.
            interaction.cancel_all();
        });

        // The UI needs the checkout record and the bash reason right away.
        let live_state = {
            let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            runs.get(&key)
                .map(|run| (run.checkout.clone(), run.bash_disabled.clone()))
        };
        match self.snapshot_data(&key, live_state.as_ref()) {
            Ok(snapshot) => events.send(Event::AiSnapshot { session, snapshot }),
            Err(error) => events.send(Event::Toast(format!(
                "could not load the AI review state: {error}"
            ))),
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

    /// Queue `text` as a user note for every agent of the live run.
    pub fn user_message(
        &self,
        session: SessionId,
        key: &PrKey,
        text: String,
        events: &EventSender,
    ) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key).and_then(|run| run.control.as_ref()) {
            Some(control) => {
                control.note_all(&text);
                events.send(Event::AiChat {
                    session,
                    event: ChatEvent::User { text },
                });
            }
            None => events.send(Event::Toast(
                "no AI review is running for this PR".to_owned(),
            )),
        }
    }

    /// Pause every running agent before its next model call.
    pub fn pause(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key).and_then(|run| run.control.as_ref()) {
            Some(control) => control.pause(),
            None => events.send(Event::Toast(
                "no AI review is running for this PR".to_owned(),
            )),
        }
    }

    /// Release a [`Self::pause`].
    pub fn resume(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key).and_then(|run| run.control.as_ref()) {
            Some(control) => control.resume(),
            None => events.send(Event::Toast(
                "no AI review is running for this PR".to_owned(),
            )),
        }
    }

    /// Stop the run; the checkout stays held so a re-run is cheap (spec §10.3).
    pub fn cancel(&self, key: &PrKey, events: &EventSender) {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(key).and_then(|run| run.control.as_ref()) {
            Some(control) => control.cancel(),
            None => events.send(Event::Toast(
                "no AI review is running for this PR".to_owned(),
            )),
        }
    }

    /// End the run and give the checkout back (spec §7.3, §10.6).
    ///
    /// `discard = None` asks first: changes made during the review come back
    /// as [`Event::AiRestoreNeedsDecision`] and the run stays so the user
    /// can decide. `discard = Some(_)` restores for good — `true` throws
    /// those changes away — and closes the run with [`Event::AiRestored`].
    pub async fn finish(
        &self,
        session: SessionId,
        key: &PrKey,
        discard: Option<bool>,
        events: &EventSender,
    ) {
        let taken = {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            runs.get_mut(key).map(|run| RunHandle {
                control: run.control.take(),
                session: run.session.clone(),
                interaction: run.interaction.clone(),
                checkout: run.checkout.clone(),
                chat: run.chat.take(),
                bash_disabled: run.bash_disabled.clone(),
                live: run.live.clone(),
                pipeline: run.pipeline.take(),
                tmp_dir: run.tmp_dir.clone(),
            })
        };
        let Some(mut run) = taken else {
            events.send(Event::Toast(
                "no AI review is running for this PR".to_owned(),
            ));
            return;
        };

        // The agents must be gone before the tree moves under them.
        if let Some(control) = run.control.take() {
            control.cancel();
        }
        run.interaction.cancel_all();
        if let Some(pipeline) = run.pipeline.take() {
            let _ = tokio::time::timeout(Duration::from_secs(30), pipeline).await;
        }

        let store = self.store.clone();
        let rec = run.checkout.clone();
        match discard {
            Some(discard) => {
                let outcome =
                    tokio::task::spawn_blocking(move || restore(&rec, discard, &store)).await;
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
                            Ok(Err(error)) => events.send(Event::Toast(format!(
                                "restoring the checkout failed: {error}"
                            ))),
                            Err(_) => {
                                events.send(Event::Toast("the restore was cancelled".to_owned()));
                                self.put_back(key, run);
                            }
                        }
                    }
                    Ok(Err(error)) => events.send(Event::Toast(format!(
                        "could not inspect the checkout: {error}"
                    ))),
                    Err(_) => {
                        events.send(Event::Toast("the restore was cancelled".to_owned()));
                        self.put_back(key, run);
                    }
                }
            }
        }
    }

    /// Put a run back after a restore that needs a decision: the control and
    /// pipeline are already gone, but the checkout and parked prompts stay.
    fn put_back(&self, key: &PrKey, run: RunHandle) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), run);
    }

    fn close_run(&self, key: &PrKey, run: RunHandle) {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
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
}

/// Tear down a run's leftovers: resolve anything parked, drop the chat agent,
/// remove the agent temp dir.
fn close_run_handle(run: RunHandle) {
    run.interaction.cancel_all();
    if let Some(chat) = run.chat {
        chat.abort();
    }
    if let Some(dir) = &run.tmp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
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
        let llm: Arc<ScriptedLlm> = Arc::new(ScriptedLlm::per_agent(model(), scripts));
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
}
