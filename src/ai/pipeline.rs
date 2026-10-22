//! The review pipeline (spec §5, §6): [`start_pipeline`] spawns one
//! orchestrator task that runs scout → reviewers → challengers → verifiers →
//! assembler, announcing every step on the event channel. The GUI thread
//! only holds the [`PipelineControl`] handle; all state lives behind its
//! locks, so steering (pause, notes, leads, reruns, skips) never waits on
//! the network.
//!
//! Usage is tracked per agent: each agent runs against a [`TrackingLlm`]
//! that tallies the `Done` usage of every reply, and the tally is reported
//! as a [`PipelineEvent::Usage`] when the agent finishes. The run total is
//! checked against `run_token_budget` after every agent (spec §6.3).

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use futures::StreamExt as _;
use review_llm::StreamEvent;
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::approval_setup::ApprovalSetup;
use crate::ai::context::load_repo_context;
use crate::ai::events::{PipelineEvent, StageKind, StageStatus};
use crate::ai::incremental::{IncrementalScope, commits_since, incremental_files};
use crate::ai::interaction::{ApprovalDecision, Interaction, Question};
use crate::ai::model::Llm;
use crate::ai::prompts::system_prompt;
use crate::ai::review::{Finding, FindingState, RunState, Severity};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::assemble::run_assembler;
use crate::ai::stages::challenge::{needs_challenge, run_challenger};
use crate::ai::stages::prior::{PRIOR_LABEL, run_prior_check};
use crate::ai::stages::reviewer::run_reviewer;
use crate::ai::stages::scout::{Brief, Group, Risk, SCOUT_LABEL, read_only_tools, run_scout};
use crate::ai::stages::verify::{MAX_BATCH, dedupe, run_verifier};
use crate::ai::stages::{
    Spawner, StageError, StageInputs, next_agent_id, seed_message, without_bash,
};
use crate::ai::store::RunKind;
use crate::ai::tools::{AgentRef, AgentRole, Registry, ToolCx};
use crate::persist::settings::{AiSettings, ModelSettings, RepoAiSettings};

pub use crate::ai::events::RunStatus;

/// The label the assembler runs under; also its script key in tests.
const ASSEMBLER_LABEL: &str = "assembler";
/// Model calls a lookup agent may make (spec §5.4).
const LOOKUP_MAX_TURNS: u32 = 15;
/// The lookup answer's word cap when handed back to the asking agent.
const MAX_LOOKUP_WORDS: usize = 300;

/// What this run reviews (spec §5.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// The whole PR diff.
    Full,
    /// Only the commits since `since_sha`; the previous brief still applies.
    Incremental { since_sha: String },
}

/// Everything the pipeline needs to start. The snapshot, sandbox and repo
/// context are gathered before the run (spec §4), so the orchestrator only
/// ever works on frozen data.
pub struct PipelineInputs {
    /// The AI database.
    pub store: Arc<crate::ai::store::Store>,
    /// The PR's session row.
    pub session_id: i64,
    /// The frozen PR (spec §4 step 2).
    pub snapshot: Arc<PrSnapshot>,
    /// The checkout the PR head is on.
    pub repo_root: PathBuf,
    /// The sandbox every file and command op runs through.
    pub sandbox: Arc<dyn review_sandbox::SandboxOps>,
    /// The global AI settings.
    pub settings: AiSettings,
    /// This repo's settings.
    pub repo_settings: RepoAiSettings,
    /// The model every stage streams from.
    pub llm: Arc<dyn Llm>,
    /// A cheaper model for lookup agents; `llm` when `None`.
    pub lookup_llm: Option<Arc<dyn Llm>>,
    /// The user, for questions and approvals.
    pub interaction: Arc<dyn Interaction>,
    /// Full diff or incremental re-review.
    pub mode: RunMode,
    /// Why `bash` is unavailable (spec §8.2); see [`StageInputs`].
    pub bash_disabled: Option<String>,
    /// The local command classifier for bash approval, if one is installed;
    /// used only when `settings.auto_approve.local_classifier` is on.
    pub classifier: Option<Arc<dyn review_approval::model_stage::CommandClassifier>>,
}

/// Where one review group is in the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupState {
    /// Waiting for a free reviewer slot.
    Pending,
    /// A reviewer is running over it.
    Running,
    /// Its reviewer finished (coverage gaps included).
    Done,
    /// Skipped without a reviewer: the user skipped it, or the token
    /// budget ran out (spec §6.3).
    Skipped,
    /// Its reviewer failed or was cancelled without a rerun request.
    Failed,
}

/// A point-in-time view for the UI (spec §6.2).
#[derive(Clone, Debug, PartialEq)]
pub struct PipelineStatus {
    /// Each started stage's latest status, in start order.
    pub stages: Vec<(StageKind, StageStatus)>,
    /// The brief's groups with their scheduler state.
    pub groups: Vec<(String, GroupState)>,
    /// The agents running right now.
    pub running_agents: Vec<AgentRef>,
    /// Findings recorded so far, counted by `(severity, state)`. Sorted by
    /// severity (Bug, Risk, Nit, Question) then state (declaration order);
    /// pairs with no findings are omitted.
    pub findings: Vec<(Severity, FindingState, u32)>,
    /// `ask_user` calls waiting on an answer.
    pub open_questions: usize,
}

/// The UI's handle on a running pipeline: cheap to clone, never blocks on
/// the network, and every method is safe to call at any time.
#[derive(Clone)]
pub struct PipelineControl {
    ctl: Arc<Ctl>,
}

impl PipelineControl {
    /// Hold every running agent before its next model call. Already
    /// streaming replies finish first.
    pub fn pause(&self) {
        let _ = self.ctl.pause.send(true);
    }

    /// Release a [`Self::pause`].
    pub fn resume(&self) {
        let _ = self.ctl.pause.send(false);
    }

    /// Stop the whole run: every agent is cancelled and the run ends
    /// [`RunStatus::Cancelled`]. What is already persisted stays. A paused
    /// run is released too: an agent parked in the loop's pause wait only
    /// wakes on a watch change, so cancelling nudges the watch.
    pub fn cancel(&self) {
        self.ctl.cancel.cancel();
        let _ = self.ctl.pause.send(false);
        self.ctl.notify.notify_waiters();
    }

    /// Queue `text` as a user note for every current and future agent.
    /// The global list is a broadcast list — never drained — so an agent
    /// starting later still receives every note; agents running right now
    /// get the note pushed onto their own queue directly, once.
    pub fn note_all(&self, text: &str) {
        {
            self.ctl
                .global_notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(text.to_string());
            let state = self.ctl.state();
            for slot in state
                .groups
                .iter()
                .filter(|s| s.state == GroupState::Running)
            {
                slot.notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(text.to_string());
            }
            for running in state.running.iter() {
                running
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(text.to_string());
            }
        }
        self.ctl.notify.notify_waiters();
    }

    /// Queue `text` as a user note for one agent, by [`AgentRef::id`].
    /// Unknown ids fall back to [`Self::note_all`].
    pub fn note_agent(&self, agent_id: u32, text: &str) {
        let routed = {
            let state = self.ctl.state();
            state
                .groups
                .iter()
                .find(|slot| {
                    slot.state == GroupState::Running
                        && slot.agent.as_ref().is_some_and(|a| a.id == agent_id)
                })
                .map(|slot| slot.notes.clone())
                .or_else(|| {
                    state
                        .running
                        .iter()
                        .find(|r| r.agent.id == agent_id)
                        .map(|r| r.notes.clone())
                })
        };
        match routed {
            Some(notes) => notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(text.to_string()),
            None => self.note_all(text),
        }
        self.ctl.notify.notify_waiters();
    }

    /// Add a lead to a group the reviewer has not started yet; it lands in
    /// the reviewer's seed message (spec §6.3).
    pub fn add_lead(&self, group_id: &str, text: &str) -> Result<(), String> {
        self.edit_group(group_id, "lead", |group| group.leads.push(text.to_string()))
    }

    /// Add an extra checklist item to a group the reviewer has not started
    /// yet; it joins the group's assigned coverage pairs.
    pub fn add_check(&self, group_id: &str, item: &str) -> Result<(), String> {
        self.edit_group(group_id, "check", |group| {
            group.extra_checks.push(item.to_string())
        })
    }

    fn edit_group(
        &self,
        group_id: &str,
        what: &str,
        edit: impl FnOnce(&mut Group),
    ) -> Result<(), String> {
        {
            let mut state = self.ctl.state();
            let slot = state
                .groups
                .iter_mut()
                .find(|slot| slot.group.id == group_id)
                .ok_or_else(|| format!("no group {group_id:?} in this run"))?;
            if slot.state != GroupState::Pending {
                return Err(format!(
                    "group {group_id} already started: too late for a {what}"
                ));
            }
            edit(&mut slot.group);
        }
        self.ctl.notify.notify_waiters();
        Ok(())
    }

    /// Cancel a group's reviewer and start it again when the cancel lands.
    /// A group that has not started is simply still pending; a finished
    /// group cannot be rerun.
    pub fn rerun_group(&self, group_id: &str) -> Result<(), String> {
        {
            let mut state = self.ctl.state();
            let slot = state
                .groups
                .iter_mut()
                .find(|slot| slot.group.id == group_id)
                .ok_or_else(|| format!("no group {group_id:?} in this run"))?;
            match slot.state {
                GroupState::Pending => {}
                GroupState::Running => {
                    slot.rerun = true;
                    slot.cancel.cancel();
                }
                _ => return Err(format!("group {group_id} already finished")),
            }
        }
        self.ctl.notify.notify_waiters();
        Ok(())
    }

    /// Skip a group: a pending group never starts, a running group is
    /// cancelled and marked [`GroupState::Skipped`].
    pub fn skip_group(&self, group_id: &str) -> Result<(), String> {
        {
            let mut state = self.ctl.state();
            let slot = state
                .groups
                .iter_mut()
                .find(|slot| slot.group.id == group_id)
                .ok_or_else(|| format!("no group {group_id:?} in this run"))?;
            match slot.state {
                GroupState::Pending | GroupState::Running => {
                    if slot.state == GroupState::Running {
                        slot.cancel.cancel();
                    }
                    slot.state = GroupState::Skipped;
                }
                _ => return Err(format!("group {group_id} already finished")),
            }
        }
        self.ctl.notify.notify_waiters();
        Ok(())
    }

    /// The current view for the UI.
    pub fn status(&self) -> PipelineStatus {
        let state = self.ctl.state();
        PipelineStatus {
            stages: state.stages.clone(),
            groups: state
                .groups
                .iter()
                .map(|slot| (slot.group.id.clone(), slot.state))
                .collect(),
            running_agents: state
                .groups
                .iter()
                .filter_map(|slot| slot.agent.clone())
                .chain(state.running.iter().map(|r| r.agent.clone()))
                .collect(),
            findings: self
                .ctl
                .run
                .get()
                .map(|run| finding_counts(&run.findings()))
                .unwrap_or_default(),
            open_questions: self.ctl.questions.load(Ordering::Relaxed),
        }
    }

    /// Whether the run is held at a [`Self::pause`] (spec §6.2). Read by the
    /// chat agent's status digest.
    pub fn is_paused(&self) -> bool {
        *self.ctl.pause.borrow()
    }

    /// The run's cancellation token. The chat agent runs under it, but with
    /// its own unpaused watch receiver: typing never pauses the review
    /// (spec §6.5).
    pub fn cancel_token(&self) -> CancellationToken {
        self.ctl.cancel.clone()
    }

    /// The run's shared findings/coverage state, once the orchestrator has
    /// created it. The chat's record tools write here.
    pub fn run_state(&self) -> Option<Arc<RunState>> {
        self.ctl.run.get().cloned()
    }

    /// A [`ToolCx`] for the chat agent (spec §6.5): the run's frozen
    /// snapshot, sandbox, context, interaction and findings, with `agent` as
    /// the caller and no checklist assignment (`record_check` then accepts
    /// any pair). `None` before the orchestrator's template exists.
    pub fn chat_tool_cx(&self, agent: AgentRef) -> Option<ToolCx> {
        let mut cx = self.ctl.chat_template.get()?.clone();
        cx.agent = agent;
        cx.assigned = Arc::new(Vec::new());
        Some(cx)
    }
}

/// Severity in report order: declaration order (Bug, Risk, Nit, Question).
const SEVERITY_ORDER: [Severity; 4] = [
    Severity::Bug,
    Severity::Risk,
    Severity::Nit,
    Severity::Question,
];

/// Finding state in report order: declaration order.
const STATE_ORDER: [FindingState; 8] = [
    FindingState::Draft,
    FindingState::Kept,
    FindingState::DroppedByUser,
    FindingState::DroppedByVerifier,
    FindingState::Posted,
    FindingState::Addressed,
    FindingState::StillPresent,
    FindingState::Outdated,
];

/// Count `findings` by `(severity, state)`, sorted by [`SEVERITY_ORDER`]
/// then [`STATE_ORDER`]; empty pairs are omitted.
fn finding_counts(findings: &[Finding]) -> Vec<(Severity, FindingState, u32)> {
    let mut counts: BTreeMap<(u8, u8), u32> = BTreeMap::new();
    for finding in findings {
        let severity = SEVERITY_ORDER
            .iter()
            .position(|s| *s == finding.severity)
            .expect("every severity is in the order");
        let state = STATE_ORDER
            .iter()
            .position(|s| *s == finding.state)
            .expect("every state is in the order");
        *counts.entry((severity as u8, state as u8)).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((severity, state), count)| {
            (
                SEVERITY_ORDER[severity as usize],
                STATE_ORDER[state as usize],
                count,
            )
        })
        .collect()
}

/// The shared orchestrator state. One mutex, always taken briefly and
/// never across an `.await`.
struct Ctl {
    state: Mutex<RunCtl>,
    notify: Notify,
    cancel: CancellationToken,
    pause: watch::Sender<bool>,
    global_notes: Arc<Mutex<VecDeque<String>>>,
    questions: AtomicUsize,
    approvals: AtomicUsize,
    /// `run_token_budget`; 0 means unlimited.
    budget: u64,
    /// Tokens spent by every agent so far, lookups included.
    total: Tally,
    events: mpsc::UnboundedSender<PipelineEvent>,
    /// Set once the run state exists, for [`PipelineControl::status`].
    run: OnceLock<Arc<RunState>>,
    /// The context template the chat agent clones (spec §6.5); set by
    /// [`start_pipeline`] once the lookup spawner is attached.
    chat_template: OnceLock<ToolCx>,
}

struct RunCtl {
    stages: Vec<(StageKind, StageStatus)>,
    groups: Vec<GroupSlot>,
    /// Stage agents not tied to a group (scout, challenger, verifier,
    /// assembler, lookup); group reviewers are tracked on their slots.
    running: Vec<RunningAgent>,
}

/// A running non-group agent and the note queue [`PipelineControl::note_all`]
/// pushes into, so a broadcast note reaches it without draining the global
/// list for everyone else.
struct RunningAgent {
    agent: AgentRef,
    notes: Arc<Mutex<VecDeque<String>>>,
}

/// One brief group plus everything the scheduler needs to steer it.
struct GroupSlot {
    group: Group,
    state: GroupState,
    /// A rerun was requested while the reviewer was running.
    rerun: bool,
    /// This group's note queue: running reviewers read it directly, and a
    /// reviewer starting later is seeded with the global queue instead.
    notes: Arc<Mutex<VecDeque<String>>>,
    /// A child of the run token: skipping or rerunning one group cancels
    /// only this one.
    cancel: CancellationToken,
    /// The reviewer while the group is running.
    agent: Option<AgentRef>,
}

impl Ctl {
    fn new(budget: u64, events: mpsc::UnboundedSender<PipelineEvent>) -> Ctl {
        let (pause, _) = watch::channel(false);
        Ctl {
            state: Mutex::new(RunCtl {
                stages: Vec::new(),
                groups: Vec::new(),
                running: Vec::new(),
            }),
            notify: Notify::new(),
            cancel: CancellationToken::new(),
            pause,
            global_notes: Arc::new(Mutex::new(VecDeque::new())),
            questions: AtomicUsize::new(0),
            approvals: AtomicUsize::new(0),
            budget,
            total: Tally::default(),
            events,
            run: OnceLock::new(),
            chat_template: OnceLock::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunCtl> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record and announce a stage status change.
    fn stage(&self, stage: StageKind, status: StageStatus) {
        {
            let mut state = self.state();
            match state.stages.iter_mut().find(|(kind, _)| *kind == stage) {
                Some(entry) => entry.1 = status.clone(),
                None => state.stages.push((stage, status.clone())),
            }
        }
        let _ = self.events.send(PipelineEvent::Stage { stage, status });
    }

    fn register_groups(&self, groups: &[Group]) {
        let mut state = self.state();
        for group in groups {
            state.groups.push(GroupSlot {
                group: group.clone(),
                state: GroupState::Pending,
                rerun: false,
                notes: Arc::new(Mutex::new(VecDeque::new())),
                cancel: self.cancel.child_token(),
                agent: None,
            });
        }
    }

    /// Take the next startable group in `order`, marking it Running and
    /// seeding its note queue with a clone of the broadcast list.
    fn take_next_pending(&self, order: &[usize]) -> Option<(usize, Group, AgentControl)> {
        let mut state = self.state();
        for &idx in order {
            let slot = &mut state.groups[idx];
            if slot.state != GroupState::Pending {
                continue;
            }
            slot.state = GroupState::Running;
            let group = slot.group.clone();
            let notes = slot.notes.clone();
            {
                let global = self.global_notes.lock().unwrap_or_else(|e| e.into_inner());
                let mut queue = notes.lock().unwrap_or_else(|e| e.into_inner());
                *queue = global.clone();
            }
            let ctl = AgentControl {
                cancel: slot.cancel.clone(),
                pause: self.pause.subscribe(),
                notes,
            };
            return Some((idx, group, ctl));
        }
        None
    }

    fn set_group_agent(&self, idx: usize, agent: &AgentRef) {
        self.state().groups[idx].agent = Some(agent.clone());
    }

    fn end_agent(&self, agent: &AgentRef) {
        let mut state = self.state();
        state.running.retain(|running| running.agent.id != agent.id);
        for slot in state.groups.iter_mut() {
            if slot.agent.as_ref().is_some_and(|a| a.id == agent.id) {
                slot.agent = None;
            }
        }
    }

    /// Apply a reviewer's outcome. A cancelled group that the user asked to
    /// rerun goes back to `Pending` with a fresh cancel token (the old one
    /// is spent); one the user skipped stays `Skipped` whatever the stop.
    fn apply_group_stop(&self, idx: usize, stop: GroupStopKind) {
        let mut state = self.state();
        let slot = &mut state.groups[idx];
        slot.agent = None;
        let rerun = std::mem::take(&mut slot.rerun);
        if slot.state == GroupState::Skipped {
            return;
        }
        slot.state = match stop {
            GroupStopKind::Done => GroupState::Done,
            GroupStopKind::Failed => GroupState::Failed,
            GroupStopKind::Cancelled if rerun => {
                slot.cancel = self.cancel.child_token();
                GroupState::Pending
            }
            GroupStopKind::Cancelled => GroupState::Failed,
        };
    }

    fn group_snapshot(&self, idx: usize) -> (Group, GroupState) {
        let state = self.state();
        (state.groups[idx].group.clone(), state.groups[idx].state)
    }

    fn group_count(&self) -> usize {
        self.state().groups.len()
    }

    /// Mark every still-pending group skipped (budget or cancel).
    fn skip_pending(&self) {
        let mut state = self.state();
        for slot in state.groups.iter_mut() {
            if slot.state == GroupState::Pending {
                slot.state = GroupState::Skipped;
            }
        }
    }

    /// Mark every pending group that no changed file touches as skipped
    /// (spec §12): the previous run reviewed it and nothing since has
    /// touched it. Event-free — the scheduler simply never starts those
    /// reviewers.
    fn skip_untouched_groups(&self, changed: &[String]) {
        let mut state = self.state();
        for slot in state.groups.iter_mut() {
            if slot.state != GroupState::Pending {
                continue;
            }
            let touched = slot
                .group
                .files
                .iter()
                .any(|file| changed.iter().any(|path| path == &file.path));
            if !touched {
                slot.state = GroupState::Skipped;
            }
        }
    }

    /// Cancel the reviewers that are running right now.
    fn cancel_running(&self) {
        let state = self.state();
        for slot in state
            .groups
            .iter()
            .filter(|s| s.state == GroupState::Running)
        {
            slot.cancel.cancel();
        }
    }

    /// Whether the run has spent its token budget (spec §6.3).
    fn over_budget(&self) -> bool {
        let spent =
            self.total.input.load(Ordering::Relaxed) + self.total.output.load(Ordering::Relaxed);
        self.budget > 0 && spent > self.budget
    }

    /// Control handle for a stage agent that is not tied to a group: it
    /// gets its own note queue seeded with a clone of the broadcast list
    /// and is registered so [`PipelineControl::note_all`] reaches it while
    /// it runs.
    fn agent_ctl(&self, agent: &AgentRef) -> AgentControl {
        let notes = Arc::new(Mutex::new(
            self.global_notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        ));
        self.state().running.push(RunningAgent {
            agent: agent.clone(),
            notes: notes.clone(),
        });
        AgentControl {
            cancel: self.cancel.clone(),
            pause: self.pause.subscribe(),
            notes,
        }
    }

    /// A control handle for a template that never runs an agent itself.
    fn unregistered_ctl(&self) -> AgentControl {
        AgentControl {
            cancel: self.cancel.clone(),
            pause: self.pause.subscribe(),
            notes: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

/// Why a group's reviewer stopped.
#[derive(Clone, Copy)]
enum GroupStopKind {
    Done,
    Failed,
    Cancelled,
}

struct GroupResult {
    idx: usize,
    stop: GroupStopKind,
    /// The run went over its token budget on this agent.
    over: bool,
}

/// Start a review run: open the run row, spawn the orchestrator, and hand
/// back its control handle, event stream, and join handle.
pub fn start_pipeline(
    inputs: PipelineInputs,
) -> (
    PipelineControl,
    mpsc::UnboundedReceiver<PipelineEvent>,
    JoinHandle<RunStatus>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let head_sha = inputs.snapshot.metadata.head_sha.clone();
    let (kind, base_sha) = match &inputs.mode {
        RunMode::Full => (RunKind::Full, None),
        RunMode::Incremental { since_sha } => (RunKind::Incremental, Some(since_sha.clone())),
    };
    let run_id =
        match inputs
            .store
            .run_begin(inputs.session_id, kind, base_sha.as_deref(), &head_sha)
        {
            Ok(run_id) => run_id,
            Err(error) => {
                tracing::error!(%error, "pipeline: opening the run row failed");
                let _ = tx.send(PipelineEvent::RunEnded {
                    run_id: 0,
                    status: RunStatus::Failed,
                    stop_reason: Some(error.to_string()),
                });
                let ctl = Arc::new(Ctl::new(0, tx.clone()));
                return (
                    PipelineControl { ctl },
                    rx,
                    tokio::spawn(async { RunStatus::Failed }),
                );
            }
        };
    let run = Arc::new(RunState::new(
        inputs.store.clone(),
        inputs.session_id,
        run_id,
        tx.clone(),
    ));
    let ctl = Arc::new(Ctl::new(inputs.settings.run_token_budget, tx.clone()));
    let _ = ctl.run.set(run.clone());

    let template = StageInputs {
        cx: ToolCx {
            sandbox: inputs.sandbox.clone(),
            repo_root: inputs.repo_root.clone(),
            agent: AgentRef {
                id: 0,
                role: AgentRole::Scout,
                label: "template".to_string(),
            },
            interaction: Arc::new(TrackingInteraction {
                inner: inputs.interaction.clone(),
                ctl: ctl.clone(),
                events: tx.clone(),
            }),
            allowed: Arc::new(Mutex::new(inputs.repo_settings.commands.clone())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: Some(inputs.snapshot.clone()),
            context: Some(Arc::new(load_repo_context(&inputs.repo_root))),
            run: run.clone(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
            // One setup per run, so the classifier's cache spans the run and
            // every agent (the scout, reviewers, chat) shares it.
            approval: Arc::new(ApprovalSetup::from_settings(
                &inputs.settings.auto_approve,
                inputs.classifier.clone(),
            )),
        },
        llm: inputs.llm.clone(),
        ctl: ctl.unregistered_ctl(),
        events: tx.clone(),
        settings: inputs.settings.clone(),
        repo: inputs.repo_settings.clone(),
        bash_disabled: inputs.bash_disabled.clone(),
        // Filled by the orchestrator once the incremental scope is known.
        incremental: None,
        incremental_log: None,
    };

    // The lookup spawner holds its own template (with `spawner: None`, so
    // lookups cannot recurse into more lookups); the stages get the
    // template back with the spawner attached.
    let lookup_llm = inputs
        .lookup_llm
        .clone()
        .unwrap_or_else(|| inputs.llm.clone());
    let spawner = Arc::new(PipelineSpawner {
        base: stage_clone(&template, lookup_llm, ctl.unregistered_ctl()),
        ctl: ctl.clone(),
    });
    let mut template = template;
    template.cx.spawner = Some(spawner);
    let _ = ctl.chat_template.set(template.cx.clone());

    let control = PipelineControl { ctl: ctl.clone() };
    let handle = tokio::spawn(orchestrate(
        template,
        inputs.mode.clone(),
        ctl,
        run,
        run_id,
        head_sha,
        tx,
    ));
    (control, rx, handle)
}

/// Clone a template for one agent run with its own tracked `llm` and
/// control handles.
fn stage_clone(base: &StageInputs, llm: Arc<dyn Llm>, ctl: AgentControl) -> StageInputs {
    StageInputs {
        cx: base.cx.clone(),
        llm,
        ctl,
        events: base.events.clone(),
        settings: base.settings.clone(),
        repo: base.repo.clone(),
        bash_disabled: base.bash_disabled.clone(),
        incremental: base.incremental.clone(),
        incremental_log: base.incremental_log.clone(),
    }
}

/// Wrap `inner`'s llm for one agent so its usage can be reported.
fn tracked_llm(base: &StageInputs, tally: Arc<Tally>) -> Arc<dyn Llm> {
    Arc::new(TrackingLlm {
        inner: base.llm.clone(),
        tally,
    })
}

/// The orchestrator task (spec §5 flow). Every stage runs inline here; only
/// reviewers get their own tasks, so the scheduler can steer them.
async fn orchestrate(
    mut base: StageInputs,
    mode: RunMode,
    ctl: Arc<Ctl>,
    run: Arc<RunState>,
    run_id: i64,
    head_sha: String,
    tx: mpsc::UnboundedSender<PipelineEvent>,
) -> RunStatus {
    // 0. Incremental scope (spec §12): which files the new commits touched,
    // and what became of the findings still open from previous runs. A
    // force-push (or a git that cannot run) degrades the run to a full
    // review of this head; the run row keeps its incremental kind.
    let mut changed: Option<Vec<String>> = None;
    let mut prior_over = false;
    // Findings the prior check moved, for the assembler seed (spec §5.7.3);
    // empty on a full run.
    let mut prior_updates: Vec<Finding> = Vec::new();
    if let RunMode::Incremental { since_sha } = &mode {
        let repo = &base.cx.repo_root;
        match incremental_files(repo, since_sha, &head_sha).await {
            Ok(IncrementalScope::Files(files)) => {
                base.incremental_log = match commits_since(repo, since_sha, &head_sha, 50).await {
                    Ok(log) => Some(log),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "pipeline: the commit log for the incremental run failed"
                        );
                        None
                    }
                };
                changed = Some(files);
            }
            Ok(IncrementalScope::FullFallback { reason }) => {
                tracing::info!(%reason, "pipeline: incremental run falls back to a full review");
                let _ = tx.send(PipelineEvent::AgentText {
                    agent: AgentRef {
                        id: 0,
                        role: AgentRole::App,
                        label: "app".to_string(),
                    },
                    delta: reason,
                });
            }
            Err(error) => {
                tracing::warn!(%error, "pipeline: incremental scope failed; running a full review");
            }
        }
        base.incremental = changed.clone();

        // The prior-findings check, before the scout so its verdicts shape
        // the run (spec §12). No open findings: no stage, no events.
        let open = match run.store().finding_list(run.session_id()) {
            Ok(findings) => findings
                .into_iter()
                .filter(|finding| {
                    matches!(
                        finding.state,
                        FindingState::Draft | FindingState::Kept | FindingState::Posted
                    )
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "pipeline: loading the open findings failed");
                Vec::new()
            }
        };
        if !open.is_empty() {
            ctl.stage(StageKind::PriorCheck, StageStatus::Started);
            let agent = AgentRef {
                id: next_agent_id(),
                role: AgentRole::PriorCheck,
                label: PRIOR_LABEL.to_string(),
            };
            let tally = Arc::new(Tally::default());
            let inputs = stage_clone(
                &base,
                tracked_llm(&base, tally.clone()),
                ctl.agent_ctl(&agent),
            );
            let _ = tx.send(PipelineEvent::AgentStarted {
                agent: agent.clone(),
                group: None,
            });
            let outcome = run_prior_check(&inputs, &agent, open.clone()).await;
            prior_over = agent_finished(&ctl, &tally, &agent, stop_of(&outcome));
            match outcome {
                Ok(()) => {
                    ctl.stage(StageKind::PriorCheck, StageStatus::Done);
                    // Whatever the check moved goes to the assembler seed.
                    // The run cache only holds this run's findings, so the
                    // comparison reads the store, which the check updated.
                    let before: BTreeMap<i64, FindingState> = open
                        .iter()
                        .map(|finding| (finding.id, finding.state))
                        .collect();
                    let after = run.store().finding_list(run.session_id());
                    match after {
                        Ok(findings) => {
                            prior_updates = findings
                                .into_iter()
                                .filter(|finding| {
                                    before
                                        .get(&finding.id)
                                        .is_some_and(|state| *state != finding.state)
                                })
                                .collect();
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "pipeline: reloading findings for the assembler seed failed"
                            );
                        }
                    }
                }
                Err(_) if ctl.cancel.is_cancelled() => {
                    ctl.stage(
                        StageKind::PriorCheck,
                        StageStatus::Failed("cancelled".to_string()),
                    );
                    return finish_run(
                        &ctl,
                        &run,
                        run_id,
                        RunStatus::Cancelled,
                        Some("cancelled".to_string()),
                        &head_sha,
                        false,
                    );
                }
                Err(error) => {
                    // Like a verifier failure: the stage failed, the run goes on.
                    ctl.stage(
                        StageKind::PriorCheck,
                        StageStatus::Failed(error_text(&error)),
                    );
                }
            }
        }
    }

    // 1. Scout. Its failure ends the whole run (spec §5.2). An incremental
    // scout builds on the previous brief (spec §12).
    let prev = changed.as_ref().and_then(|_| previous_brief(&run));
    ctl.stage(StageKind::Scout, StageStatus::Started);
    let scout = AgentRef {
        id: 1,
        role: AgentRole::Scout,
        label: SCOUT_LABEL.to_string(),
    };
    let tally = Arc::new(Tally::default());
    let inputs = stage_clone(
        &base,
        tracked_llm(&base, tally.clone()),
        ctl.agent_ctl(&scout),
    );
    let _ = tx.send(PipelineEvent::AgentStarted {
        agent: scout.clone(),
        group: None,
    });
    let scouted = run_scout(&inputs, &scout, prev.as_ref()).await;
    let over = agent_finished(&ctl, &tally, &scout, stop_of(&scouted));
    let brief = match scouted {
        Ok(brief) => brief,
        Err(error) => {
            let reason = error_text(&error);
            let status = if ctl.cancel.is_cancelled() {
                ctl.stage(
                    StageKind::Scout,
                    StageStatus::Failed("cancelled".to_string()),
                );
                RunStatus::Cancelled
            } else {
                ctl.stage(StageKind::Scout, StageStatus::Failed(reason.clone()));
                RunStatus::Failed
            };
            return finish_run(&ctl, &run, run_id, status, Some(reason), &head_sha, false);
        }
    };
    ctl.stage(StageKind::Scout, StageStatus::Done);
    let _ = tx.send(PipelineEvent::BriefReady);
    ctl.register_groups(&brief.groups);
    if let Some(changed) = &changed {
        // Groups no changed file touches were reviewed in the previous run:
        // they are Skipped before the scheduler ever sees them (spec §12).
        ctl.skip_untouched_groups(changed);
    }

    // The budget is checked after every agent; once it is spent the rest
    // of the review is skipped but the assembler still runs (spec §6.3).
    let mut budget_hit = over || prior_over;
    if budget_hit {
        ctl.skip_pending();
    }

    // 2. Reviewers, skim groups first then by risk, under the parallel cap.
    if !budget_hit {
        ctl.stage(StageKind::Review, StageStatus::Started);
        budget_hit = review_pass(&base, &ctl, &brief).await;
        if ctl.cancel.is_cancelled() {
            ctl.stage(
                StageKind::Review,
                StageStatus::Failed("cancelled".to_string()),
            );
            return finish_run(
                &ctl,
                &run,
                run_id,
                RunStatus::Cancelled,
                Some("cancelled".to_string()),
                &head_sha,
                false,
            );
        }
        ctl.stage(StageKind::Review, StageStatus::Done);
        if budget_hit {
            ctl.skip_pending();
        }
    }

    // 3. Challengers for High-risk groups whose reviewer found nothing
    // worth challenging (spec §5.4).
    if !budget_hit {
        ctl.stage(StageKind::Challenge, StageStatus::Started);
        for idx in 0..ctl.group_count() {
            let (group, state) = ctl.group_snapshot(idx);
            if state != GroupState::Done || !needs_challenge(&group, &run.findings()) {
                continue;
            }
            let agent = AgentRef {
                id: next_agent_id(),
                role: AgentRole::Challenger,
                label: format!("challenger:{}", group.id),
            };
            let tally = Arc::new(Tally::default());
            let inputs = stage_clone(
                &base,
                tracked_llm(&base, tally.clone()),
                ctl.agent_ctl(&agent),
            );
            let _ = tx.send(PipelineEvent::AgentStarted {
                agent: agent.clone(),
                group: Some(group.id.clone()),
            });
            let outcome = run_challenger(&inputs, &agent, &brief, &group).await;
            let over = agent_finished(&ctl, &tally, &agent, stop_of(&outcome));
            if over {
                budget_hit = true;
                break;
            }
            if ctl.cancel.is_cancelled() {
                break;
            }
        }
        if ctl.cancel.is_cancelled() {
            ctl.stage(
                StageKind::Challenge,
                StageStatus::Failed("cancelled".to_string()),
            );
            return finish_run(
                &ctl,
                &run,
                run_id,
                RunStatus::Cancelled,
                Some("cancelled".to_string()),
                &head_sha,
                false,
            );
        }
        ctl.stage(StageKind::Challenge, StageStatus::Done);
        if budget_hit {
            ctl.skip_pending();
        }
    }

    // 4. Dedupe, then one verifier per path batch (spec §5.5).
    if !budget_hit {
        ctl.stage(StageKind::Verify, StageStatus::Started);
        for batch in verifier_batches(&run.findings()) {
            let path = batch[0].anchor.path().to_string();
            let agent = AgentRef {
                id: next_agent_id(),
                role: AgentRole::Verifier,
                label: format!("verifier:{}", path.replace('/', "-")),
            };
            let tally = Arc::new(Tally::default());
            let inputs = stage_clone(
                &base,
                tracked_llm(&base, tally.clone()),
                ctl.agent_ctl(&agent),
            );
            let _ = tx.send(PipelineEvent::AgentStarted {
                agent: agent.clone(),
                group: None,
            });
            let outcome = run_verifier(&inputs, &agent, batch).await;
            let over = agent_finished(&ctl, &tally, &agent, stop_of(&outcome));
            if over {
                budget_hit = true;
                break;
            }
            if ctl.cancel.is_cancelled() {
                break;
            }
        }
        if ctl.cancel.is_cancelled() {
            ctl.stage(
                StageKind::Verify,
                StageStatus::Failed("cancelled".to_string()),
            );
            return finish_run(
                &ctl,
                &run,
                run_id,
                RunStatus::Cancelled,
                Some("cancelled".to_string()),
                &head_sha,
                false,
            );
        }
        ctl.stage(StageKind::Verify, StageStatus::Done);
    }

    // 5. The assembler, on what survived — always, even after the budget
    // ran out, so the user gets a draft of what was reviewed (spec §6.3).
    ctl.stage(StageKind::Assemble, StageStatus::Started);
    let kept: Vec<Finding> = run
        .findings()
        .into_iter()
        .filter(|finding| finding.state == FindingState::Draft)
        .collect();
    let assembler = AgentRef {
        id: next_agent_id(),
        role: AgentRole::Assembler,
        label: ASSEMBLER_LABEL.to_string(),
    };
    let tally = Arc::new(Tally::default());
    let inputs = stage_clone(
        &base,
        tracked_llm(&base, tally.clone()),
        ctl.agent_ctl(&assembler),
    );
    let _ = tx.send(PipelineEvent::AgentStarted {
        agent: assembler.clone(),
        group: None,
    });
    let drafted = run_assembler(
        &inputs,
        &assembler,
        &brief,
        &kept,
        &run.coverage(),
        &prior_updates,
    )
    .await;
    agent_finished(&ctl, &tally, &assembler, stop_of(&drafted));
    let draft = match drafted {
        Ok(draft) => draft,
        Err(error) => {
            let reason = error_text(&error);
            let status = if ctl.cancel.is_cancelled() {
                ctl.stage(
                    StageKind::Assemble,
                    StageStatus::Failed("cancelled".to_string()),
                );
                RunStatus::Cancelled
            } else {
                ctl.stage(StageKind::Assemble, StageStatus::Failed(reason.clone()));
                RunStatus::Failed
            };
            return finish_run(&ctl, &run, run_id, status, Some(reason), &head_sha, false);
        }
    };
    ctl.stage(StageKind::Assemble, StageStatus::Done);
    // Persist the draft with the run before announcing it: a run that
    // finishes while the user is on another PR loses `DraftReady` to the
    // stale-event rule, and the attach snapshot replays this row instead
    // (spec §10.3). A failure here costs only the replay, not the run.
    match serde_json::to_string(&draft) {
        Ok(json) => {
            if let Err(error) = run.store().run_set_draft(run_id, &json) {
                tracing::warn!(%error, "pipeline: persisting the assembler draft failed");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "pipeline: serializing the assembler draft failed");
        }
    }
    let _ = tx.send(PipelineEvent::DraftReady { draft });

    let status = if budget_hit {
        RunStatus::BudgetExhausted
    } else {
        RunStatus::Completed
    };
    let reason = budget_hit.then(|| "token budget".to_string());
    finish_run(&ctl, &run, run_id, status, reason, &head_sha, true)
}

/// The session's stored brief, shown to an incremental scout (spec §12);
/// a stored brief that no longer parses is dropped with a warning.
fn previous_brief(run: &RunState) -> Option<Brief> {
    let json = run.store().session_brief(run.session_id()).ok().flatten()?;
    match serde_json::from_str(&json) {
        Ok(brief) => Some(brief),
        Err(error) => {
            tracing::warn!(%error, "pipeline: the stored brief did not parse");
            None
        }
    }
}

/// Run every group through a reviewer under the parallel cap, handling
/// steering (leads, checks, reruns, skips) and the token budget along the
/// way. Returns whether the budget was spent.
async fn review_pass(base: &StageInputs, ctl: &Arc<Ctl>, brief: &Brief) -> bool {
    let max = base.settings.max_parallel_reviewers.max(1) as usize;
    let sem = Arc::new(Semaphore::new(max));
    let (rtx, mut rrx) = mpsc::unbounded_channel::<GroupResult>();
    let order = {
        let state = ctl.state();
        schedule_order(
            &state
                .groups
                .iter()
                .map(|slot| slot.group.clone())
                .collect::<Vec<_>>(),
        )
    };
    let mut budget_hit = false;
    let mut running = 0usize;

    loop {
        // Armed before the spawn pass so a steering command that lands in
        // between still wakes the `select!` below.
        let notified = ctl.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if !budget_hit && !ctl.cancel.is_cancelled() {
            while let Ok(permit) = sem.clone().try_acquire_owned() {
                let Some((idx, group, group_ctl)) = ctl.take_next_pending(&order) else {
                    drop(permit);
                    break;
                };
                let agent = AgentRef {
                    id: next_agent_id(),
                    role: AgentRole::Reviewer,
                    label: format!("reviewer:{}", group.id),
                };
                ctl.set_group_agent(idx, &agent);
                let tally = Arc::new(Tally::default());
                let inputs = stage_clone(base, tracked_llm(base, tally.clone()), group_ctl);
                let _ = ctl.events.send(PipelineEvent::AgentStarted {
                    agent: agent.clone(),
                    group: Some(group.id.clone()),
                });
                let (ctl, rtx, tally, brief, permit) =
                    (ctl.clone(), rtx.clone(), tally, brief.clone(), permit);
                // A panicking reviewer must not strand the run waiting on its
                // result, so the task is joined and a join error is a failure.
                let task = tokio::spawn({
                    let agent = agent.clone();
                    async move { run_reviewer(&inputs, &agent, &brief, &group).await }
                });
                tokio::spawn(async move {
                    let outcome = match task.await {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            Err(StageError::Failed(format!("reviewer panicked: {error}")))
                        }
                    };
                    let over = agent_finished(&ctl, &tally, &agent, stop_of(&outcome));
                    let stop = match &outcome {
                        Ok(_) => GroupStopKind::Done,
                        Err(StageError::Cancelled) => GroupStopKind::Cancelled,
                        Err(_) => GroupStopKind::Failed,
                    };
                    drop(permit);
                    let _ = rtx.send(GroupResult { idx, stop, over });
                });
                running += 1;
            }
        }
        if budget_hit || ctl.cancel.is_cancelled() {
            ctl.skip_pending();
        }
        if running == 0 {
            break;
        }

        tokio::select! {
            Some(result) = rrx.recv() => {
                running -= 1;
                ctl.apply_group_stop(result.idx, result.stop);
                if result.over {
                    budget_hit = true;
                    ctl.cancel_running();
                }
            }
            _ = notified.as_mut() => {}
        }
    }
    budget_hit
}

/// Announce an agent's finish, fold its usage into the run total, and
/// report whether the budget is now spent.
fn agent_finished(ctl: &Ctl, tally: &Tally, agent: &AgentRef, stop: String) -> bool {
    let _ = ctl.events.send(PipelineEvent::AgentFinished {
        agent: agent.clone(),
        stop,
    });
    let input = tally.input.load(Ordering::Relaxed);
    let output = tally.output.load(Ordering::Relaxed);
    ctl.total.input.fetch_add(input, Ordering::Relaxed);
    ctl.total.output.fetch_add(output, Ordering::Relaxed);
    let _ = ctl.events.send(PipelineEvent::Usage {
        agent: agent.clone(),
        input,
        output,
    });
    ctl.end_agent(agent);
    ctl.over_budget()
}

/// Close the run row, pin `last_reviewed_sha` on a successful pass, and
/// announce the end.
fn finish_run(
    ctl: &Ctl,
    run: &RunState,
    run_id: i64,
    status: RunStatus,
    stop_reason: Option<String>,
    head_sha: &str,
    pin_reviewed: bool,
) -> RunStatus {
    if pin_reviewed
        && let Err(error) = run
            .store()
            .session_set_last_reviewed(run.session_id(), head_sha)
    {
        tracing::warn!(%error, "pipeline: pinning last_reviewed_sha failed");
    }
    let usage = serde_json::json!({
        "input": ctl.total.input.load(Ordering::Relaxed),
        "output": ctl.total.output.load(Ordering::Relaxed),
    });
    if let Err(error) = run.store().run_end(
        run_id,
        status.as_str(),
        stop_reason.as_deref(),
        Some(&usage.to_string()),
    ) {
        tracing::warn!(%error, "pipeline: closing the run row failed");
    }
    let _ = ctl.events.send(PipelineEvent::RunEnded {
        run_id,
        status,
        stop_reason,
    });
    status
}

/// Reviewer start order: skim groups first, then the rest; risk High →
/// Medium → Low within each class, stable within a risk (spec §5.3).
fn schedule_order(groups: &[Group]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&i| (!groups[i].skim, risk_rank(groups[i].risk)));
    order
}

fn risk_rank(risk: Risk) -> u8 {
    match risk {
        Risk::High => 0,
        Risk::Medium => 1,
        Risk::Low => 2,
    }
}

/// Dedupe the drafts, then pack the clusters into one batch per path, cut
/// at [`MAX_BATCH`] findings (spec §5.5). Clusters keep their
/// smallest-id-first order, so batches are deterministic.
fn verifier_batches(findings: &[Finding]) -> Vec<Vec<Finding>> {
    let drafts: Vec<Finding> = findings
        .iter()
        .filter(|finding| finding.state == FindingState::Draft)
        .cloned()
        .collect();
    let by_id: BTreeMap<i64, &Finding> = findings.iter().map(|f| (f.id, f)).collect();
    let mut paths: Vec<String> = Vec::new();
    let mut buckets: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for cluster in dedupe(&drafts) {
        let path = by_id[&cluster[0]].anchor.path().to_string();
        if !paths.contains(&path) {
            paths.push(path.clone());
        }
        buckets.entry(path).or_default().extend(cluster);
    }
    let mut batches = Vec::new();
    for path in paths {
        let ids = buckets.get(&path).map(Vec::as_slice).unwrap_or(&[]);
        for chunk in ids.chunks(MAX_BATCH) {
            batches.push(chunk.iter().map(|id| (*by_id[id]).clone()).collect());
        }
    }
    batches
}

/// The lookup answer's cap: `max` words, with an ellipsis when cut.
fn cut_words(text: &str, max: usize) -> String {
    let mut words = text.split_whitespace();
    let kept: Vec<&str> = words.by_ref().take(max).collect();
    if words.next().is_none() {
        return text.to_string();
    }
    format!("{} […]", kept.join(" "))
}

/// The `stop` string for an agent whose stage result is `result`.
fn stop_of<T>(result: &Result<T, StageError>) -> String {
    match result {
        Ok(_) => "finished".to_string(),
        Err(StageError::NoFinish) => "no_finish".to_string(),
        Err(StageError::Cancelled) => "cancelled".to_string(),
        Err(StageError::Failed(_)) => "failed".to_string(),
    }
}

/// The human-readable failure reason for a stage error.
fn error_text(error: &StageError) -> String {
    match error {
        StageError::NoFinish => "the agent finished without filing its result".to_string(),
        StageError::Cancelled => "cancelled".to_string(),
        StageError::Failed(message) => message.clone(),
    }
}

/// The run-wide token tally: every [`TrackingLlm`] adds to its own copy,
/// and the orchestrator folds agent tallies into the run total.
#[derive(Default)]
struct Tally {
    input: AtomicU64,
    output: AtomicU64,
}

/// An [`Llm`] that adds every reply's usage to `tally` as the stream
/// passes through (spec §6.3 usage tracking; the loop's own outcome never
/// leaves the stage functions).
struct TrackingLlm {
    inner: Arc<dyn Llm>,
    tally: Arc<Tally>,
}

#[async_trait]
impl Llm for TrackingLlm {
    async fn stream(
        &self,
        req: review_llm::GenerateRequest,
    ) -> review_llm::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = review_llm::Result<StreamEvent>> + Send>>,
    > {
        let tally = self.tally.clone();
        let stream = self.inner.stream(req).await?;
        Ok(stream
            .then(move |event| {
                if let Ok(StreamEvent::Done(reply)) = &event {
                    tally.input.fetch_add(reply.usage.input, Ordering::Relaxed);
                    tally
                        .output
                        .fetch_add(reply.usage.output, Ordering::Relaxed);
                }
                async move { event }
            })
            .boxed())
    }

    fn model(&self) -> &ModelSettings {
        self.inner.model()
    }
}

/// The user's [`Interaction`] with events: every question and approval
/// request is announced before it is awaited, so the UI can show it
/// (spec §6.1).
struct TrackingInteraction {
    inner: Arc<dyn Interaction>,
    ctl: Arc<Ctl>,
    events: mpsc::UnboundedSender<PipelineEvent>,
}

#[async_trait]
impl Interaction for TrackingInteraction {
    async fn ask_user(&self, agent: &AgentRef, question: Question) -> String {
        let _ = self.events.send(PipelineEvent::Question {
            agent: agent.clone(),
            question: question.clone(),
        });
        self.ctl.questions.fetch_add(1, Ordering::Relaxed);
        let answer = self.inner.ask_user(agent, question).await;
        self.ctl.questions.fetch_sub(1, Ordering::Relaxed);
        answer
    }

    async fn approve_command(
        &self,
        agent: &AgentRef,
        command: &str,
        reason: &str,
    ) -> ApprovalDecision {
        let id = self.ctl.approvals.fetch_add(1, Ordering::Relaxed) as u64 + 1;
        let _ = self.events.send(PipelineEvent::Approval {
            agent: agent.clone(),
            id,
            command: command.to_string(),
            reason: reason.to_string(),
        });
        self.inner.approve_command(agent, command, reason).await
    }

    async fn approve_prepare(&self, agent: &AgentRef, reason: &str) -> bool {
        self.inner.approve_prepare(agent, reason).await
    }
}

/// Runs [`Spawner::lookup`] agents: a read-only explorer with a fresh
/// context, answering one question for the asking agent (spec §5.4).
struct PipelineSpawner {
    /// A stage template whose own `spawner` is `None`: a lookup cannot
    /// ask for another lookup.
    base: StageInputs,
    /// Lookup usage folds into the run total, and lookup agents appear in
    /// the running set, through the shared control state.
    ctl: Arc<Ctl>,
}

#[async_trait]
impl Spawner for PipelineSpawner {
    async fn lookup(&self, from: &AgentRef, question: &str) -> String {
        let agent = AgentRef {
            id: next_agent_id(),
            role: AgentRole::Lookup,
            label: format!("lookup:{}", from.label),
        };
        let registry = Registry::new(without_bash(
            read_only_tools(),
            self.base.bash_disabled.as_deref(),
        ));
        let mut cx = self.base.cx.clone();
        cx.agent = agent.clone();
        let spec = AgentSpec {
            agent: agent.clone(),
            system: system_prompt(AgentRole::Lookup, &agent.label),
            initial: vec![review_llm::Message::user_text(seed_message(
                question.to_string(),
                &cx.run.qa(),
                self.base.bash_disabled.as_deref(),
            ))],
            registry,
            max_turns: LOOKUP_MAX_TURNS,
            finish_tool: None,
            on_finish: Arc::new(|| FinishCheck::Accept),
            max_finish_reminders: 0,
            elide_threshold: f64::from(self.base.settings.context_threshold),
        };
        let tally = Arc::new(Tally::default());
        let llm: Arc<dyn Llm> = Arc::new(TrackingLlm {
            inner: self.base.llm.clone(),
            tally: tally.clone(),
        });
        let _ = self.ctl.events.send(PipelineEvent::AgentStarted {
            agent: agent.clone(),
            group: None,
        });
        let outcome = run_agent(
            spec,
            cx,
            llm,
            self.ctl.agent_ctl(&agent),
            self.base.events.clone(),
        )
        .await;
        let stop = match outcome.stop {
            AgentStop::Finished => "finished",
            AgentStop::NoFinish => "no_finish",
            AgentStop::Cancelled => "cancelled",
            AgentStop::MaxTurns => "max_turns",
            AgentStop::Failed(_) => "failed",
        };
        let _ = self.ctl.events.send(PipelineEvent::AgentFinished {
            agent: agent.clone(),
            stop: stop.to_string(),
        });
        let input = tally.input.load(Ordering::Relaxed);
        let output = tally.output.load(Ordering::Relaxed);
        let _ = self.ctl.events.send(PipelineEvent::Usage {
            agent: agent.clone(),
            input,
            output,
        });
        self.ctl.total.input.fetch_add(input, Ordering::Relaxed);
        self.ctl.total.output.fetch_add(output, Ordering::Relaxed);
        self.ctl.end_agent(&agent);
        cut_words(&outcome.last_text, MAX_LOOKUP_WORDS)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentControl, Ctl, Finding, Group, GroupState, GroupStopKind, PipelineControl, cut_words,
        finding_counts, risk_rank, schedule_order, verifier_batches,
    };
    use crate::ai::review::{Evidence, FindingAnchor, FindingState, Severity};
    use crate::ai::stages::scout::Risk;
    use crate::ai::tools::{AgentRef, AgentRole};
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn group(id: &str, risk: Risk, skim: bool) -> Group {
        Group {
            id: id.to_string(),
            title: id.to_string(),
            risk,
            reason: String::new(),
            files: Vec::new(),
            leads: Vec::new(),
            extra_checks: Vec::new(),
            ci_notes: Vec::new(),
            skim,
        }
    }

    /// A control handle over registered groups with no run behind it:
    /// enough to exercise the steering commands.
    fn control_with(groups: &[Group]) -> (PipelineControl, Arc<Ctl>) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ctl = Arc::new(Ctl::new(0, tx));
        ctl.register_groups(groups);
        (PipelineControl { ctl: ctl.clone() }, ctl)
    }

    fn drain_notes(ctl: &AgentControl) -> Vec<String> {
        let mut queue = ctl.notes.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    }

    #[test]
    fn schedule_puts_skim_first_then_risk_descending_and_keeps_ties() {
        let groups = vec![
            group("g1", Risk::Medium, false),
            group("g2", Risk::High, false),
            group("g3", Risk::Low, true),
            group("g4", Risk::Low, false),
            group("g5", Risk::High, true),
        ];
        let order = schedule_order(&groups);
        let ids: Vec<&str> = order.iter().map(|&i| groups[i].id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["g5", "g3", "g2", "g1", "g4"],
            "skim groups first (risk-descending within the class), then the rest"
        );
        assert!(risk_rank(Risk::High) < risk_rank(Risk::Medium));
    }

    fn finding(id: i64, path: &str, line: u32) -> Finding {
        Finding {
            id,
            run_id: 1,
            agent: AgentRef {
                id: 2,
                role: AgentRole::Reviewer,
                label: "reviewer:g1".to_string(),
            },
            anchor: FindingAnchor::Line(Anchor {
                path: path.to_string(),
                side: Side::Right,
                line,
                start_line: None,
                start_side: None,
            }),
            severity: crate::ai::review::Severity::Risk,
            title: format!("finding {id}"),
            body: "body".to_string(),
            evidence: vec![Evidence {
                path: path.to_string(),
                line: Some(line),
                note: "checked".to_string(),
            }],
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        }
    }

    #[test]
    fn verifier_batches_group_by_path_and_cut_at_the_cap() {
        // Two paths, interleaved ids, and a third path with more findings
        // than one verifier may take.
        let mut findings = vec![
            finding(1, "src/a.rs", 10),
            finding(2, "docs/b.md", 3),
            finding(3, "src/a.rs", 20),
        ];
        for id in 4..=12 {
            findings.push(finding(id, "src/c.rs", id as u32));
        }
        let batches = verifier_batches(&findings);
        let shape: Vec<(String, Vec<i64>)> = batches
            .iter()
            .map(|batch| {
                (
                    batch[0].anchor.path().to_string(),
                    batch.iter().map(|f| f.id).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("src/a.rs".to_string(), vec![1, 3]),
                ("docs/b.md".to_string(), vec![2]),
                ("src/c.rs".to_string(), vec![4, 5, 6, 7, 8, 9, 10, 11]),
                ("src/c.rs".to_string(), vec![12]),
            ],
            "paths in first-appearance order, ids ascending, cut at 8"
        );

        // Non-draft findings never reach a verifier.
        let mut dropped = finding(20, "src/a.rs", 10);
        dropped.state = FindingState::DroppedByVerifier;
        assert_eq!(verifier_batches(&[dropped]).len(), 0);
    }

    #[test]
    fn cut_words_keeps_short_text_and_marks_cuts() {
        assert_eq!(cut_words("one two three", 5), "one two three");
        assert_eq!(cut_words("one two three", 2), "one two […]");
        assert_eq!(cut_words("", 3), "");
    }

    #[test]
    fn finding_counts_sorts_by_severity_then_state() {
        let mut bug_draft = finding(1, "a.rs", 1);
        bug_draft.severity = Severity::Bug;
        let mut bug_draft_again = finding(2, "a.rs", 2);
        bug_draft_again.severity = Severity::Bug;
        let mut bug_kept = finding(3, "a.rs", 3);
        bug_kept.severity = Severity::Bug;
        bug_kept.state = FindingState::Kept;
        let mut nit_draft = finding(4, "a.rs", 4);
        nit_draft.severity = Severity::Nit;
        let mut question_draft = finding(5, "a.rs", 5);
        question_draft.severity = Severity::Question;

        let counts = finding_counts(&[
            question_draft,
            nit_draft,
            bug_kept,
            bug_draft_again,
            bug_draft,
        ]);
        assert_eq!(
            counts,
            vec![
                (Severity::Bug, FindingState::Draft, 2),
                (Severity::Bug, FindingState::Kept, 1),
                (Severity::Nit, FindingState::Draft, 1),
                (Severity::Question, FindingState::Draft, 1),
            ],
            "Bug first, then Risk, Nit, Question; states in declaration order; empty pairs omitted"
        );
        assert!(finding_counts(&[]).is_empty());
    }

    #[test]
    fn rerun_and_skip_reject_finished_groups() {
        let (control, ctl) = control_with(&[group("g1", Risk::High, false)]);
        for finished in [GroupState::Done, GroupState::Failed, GroupState::Skipped] {
            ctl.state().groups[0].state = finished;
            let rerun = control.rerun_group("g1").unwrap_err();
            assert!(rerun.contains("already finished"), "{rerun}");
            let skip = control.skip_group("g1").unwrap_err();
            assert!(skip.contains("already finished"), "{skip}");
            // The rejection left the state alone.
            assert_eq!(ctl.state().groups[0].state, finished);
        }
        assert!(
            control
                .rerun_group("nope")
                .unwrap_err()
                .contains("no group")
        );
    }

    #[test]
    fn skip_pending_marks_the_group_skipped() {
        let (control, _ctl) = control_with(&[group("g1", Risk::High, false)]);
        control.skip_group("g1").unwrap();
        assert_eq!(
            control.status().groups,
            vec![("g1".to_string(), GroupState::Skipped)]
        );
    }

    #[test]
    fn skip_running_cancels_only_that_group() {
        let (control, ctl) = control_with(&[
            group("g1", Risk::High, false),
            group("g2", Risk::Medium, false),
        ]);
        {
            let mut state = ctl.state();
            state.groups[0].state = GroupState::Running;
            state.groups[1].state = GroupState::Running;
        }
        control.skip_group("g1").unwrap();
        let state = ctl.state();
        assert_eq!(state.groups[0].state, GroupState::Skipped);
        assert!(state.groups[0].cancel.is_cancelled());
        assert_eq!(state.groups[1].state, GroupState::Running);
        assert!(
            !state.groups[1].cancel.is_cancelled(),
            "the other reviewer keeps running"
        );
    }

    #[test]
    fn a_skipped_group_stays_skipped_whatever_the_stop() {
        for stop in [
            GroupStopKind::Done,
            GroupStopKind::Failed,
            GroupStopKind::Cancelled,
        ] {
            let (control, ctl) = control_with(&[group("g1", Risk::High, false)]);
            ctl.state().groups[0].state = GroupState::Running;
            control.skip_group("g1").unwrap();
            ctl.apply_group_stop(0, stop);
            assert_eq!(ctl.state().groups[0].state, GroupState::Skipped);
        }
    }

    #[test]
    fn a_plain_cancel_without_a_rerun_fails_the_group() {
        let (_control, ctl) = control_with(&[group("g1", Risk::High, false)]);
        ctl.state().groups[0].state = GroupState::Running;
        ctl.apply_group_stop(0, GroupStopKind::Cancelled);
        assert_eq!(ctl.state().groups[0].state, GroupState::Failed);
    }

    #[test]
    fn rerun_cancels_the_reviewer_and_restarts_it_with_a_fresh_token() {
        let (control, ctl) = control_with(&[
            group("g1", Risk::High, false),
            group("g2", Risk::Medium, false),
        ]);
        {
            let mut state = ctl.state();
            state.groups[0].state = GroupState::Running;
            state.groups[1].state = GroupState::Running;
        }
        control.rerun_group("g1").unwrap();
        let state = ctl.state();
        assert!(state.groups[0].rerun);
        assert!(state.groups[0].cancel.is_cancelled());
        assert_eq!(
            state.groups[0].state,
            GroupState::Running,
            "the group goes back to Pending only when the cancel lands"
        );
        assert!(!state.groups[1].rerun);
        assert!(!state.groups[1].cancel.is_cancelled());
        drop(state);

        ctl.apply_group_stop(0, GroupStopKind::Cancelled);
        let state = ctl.state();
        assert_eq!(state.groups[0].state, GroupState::Pending);
        assert!(!state.groups[0].rerun, "the flag is consumed");
        assert!(
            !state.groups[0].cancel.is_cancelled(),
            "the restarted reviewer must not start cancelled"
        );
    }

    #[test]
    fn add_lead_and_check_need_a_pending_group() {
        let (control, ctl) = control_with(&[group("g1", Risk::High, false)]);
        control.add_lead("g1", "check error paths").unwrap();
        control.add_check("g1", "docs updated").unwrap();
        {
            let state = ctl.state();
            assert_eq!(state.groups[0].group.leads, vec!["check error paths"]);
            assert_eq!(state.groups[0].group.extra_checks, vec!["docs updated"]);
        }
        ctl.state().groups[0].state = GroupState::Running;
        let lead = control.add_lead("g1", "late").unwrap_err();
        assert!(lead.contains("already started"), "{lead}");
        let check = control.add_check("g1", "late").unwrap_err();
        assert!(check.contains("already started"), "{check}");
    }

    #[test]
    fn note_all_broadcasts_without_draining() {
        let (control, ctl) = control_with(&[
            group("g1", Risk::High, false),
            group("g2", Risk::Medium, false),
        ]);
        control.note_all("focus on error paths");

        // A group that started running gets later notes pushed onto its
        // own queue, once.
        ctl.state().groups[0].state = GroupState::Running;
        control.note_all("second note");
        {
            let state = ctl.state();
            let queue = state.groups[0]
                .notes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                queue.iter().cloned().collect::<Vec<_>>(),
                vec!["second note".to_string()],
                "the running reviewer gets the new note directly, once"
            );
        }

        // The broadcast list is never drained: every later agent is seeded
        // with a clone of it.
        let (_idx, _group, first) = ctl.take_next_pending(&[1]).unwrap();
        assert_eq!(
            drain_notes(&first),
            vec![
                "focus on error paths".to_string(),
                "second note".to_string()
            ]
        );
        ctl.state().groups[0].state = GroupState::Pending; // e.g. a rerun
        let (_idx, _group, second) = ctl.take_next_pending(&[0]).unwrap();
        assert_eq!(
            drain_notes(&second),
            vec![
                "focus on error paths".to_string(),
                "second note".to_string()
            ],
            "the next agent still sees every note"
        );
    }
}
