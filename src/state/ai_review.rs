//! The AI review tab's UI state: a pure fold over the worker's AI events
//! (spec §12, Plan 3 Task 2).
//!
//! [`AiReviewState`] mirrors one PR's AI run for the ✦ AI tab: a session-like
//! [`TimelineEntry`] list, the findings and coverage the run recorded, the
//! draft review, and the checkout lifecycle phase. The reducer never does
//! I/O and never touches egui; `app.rs` routes events here after the
//! stale-event rule has matched the [`crate::runtime::SessionId`], the same
//! way [`crate::state::pr_detail::PrSession::on_event`] is fed.
//!
//! What the event surface does *not* carry, this state does not invent:
//! [`PipelineEvent::FindingAdded`] and [`PipelineEvent::Coverage`] carry ids
//! only, so the full [`Finding`] rows and coverage entries arrive with the
//! next [`Event::AiSnapshot`] (on attach, and again when a run starts).
//! [`PipelineEvent::Approval`] is ignored on purpose: it announces the same
//! approval as [`Event::AiApprovalRequest`] but in a different id space, and
//! only the latter's id answers it.

use std::collections::BTreeMap;

use crate::ai::checkout::{CheckoutPlan, RestoreOutcome};
use crate::ai::events::{PipelineEvent, RunStatus, StageKind, StageStatus};
use crate::ai::hub::{AiSnapshotData, AiTimelineEntry};
use crate::ai::review::{CheckResult, CoverageEntry, Finding, FindingState, Severity};
use crate::ai::stages::assemble::ReviewDraft;
use crate::ai::store::CheckoutRecord;
use crate::ai::tools::{AgentRef, AgentRole};
use crate::ai::ui_interaction::ApprovalKind;
use crate::runtime::Event;

/// Where the checkout/run lifecycle of this PR's AI review stands (spec §4).
/// `Paused` is UI-driven: the hub has no pause event, so Task 3's Pause
/// button calls [`AiReviewState::set_paused`] alongside its command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AiPhase {
    /// Nothing has been asked of this PR yet.
    #[default]
    Idle,
    /// A checkout plan is awaiting the user's confirmation.
    Planning,
    /// The start got past the plan: the checkout/prepare is in flight.
    CheckingOut,
    /// A pipeline run is executing.
    Running,
    /// The run is parked on the user's Resume.
    Paused,
    /// A run ended (completed, cancelled, or out of budget).
    Done,
    /// Finish found uncommitted changes; the user must keep or discard.
    Restoring,
    /// The checkout was restored; the session is closed.
    Finished,
    /// Checkout, prepare or the run failed; the payload says why.
    Failed(String),
}

/// One row of the tab's session-like timeline, in arrival order (spec §12).
#[derive(Clone, Debug, PartialEq)]
pub enum TimelineEntry {
    /// A pipeline stage changed state.
    Stage {
        /// Which stage.
        stage: StageKind,
        /// Its new lifecycle state.
        status: StageStatus,
    },
    /// One agent's running entry: its streamed text and tool calls coalesce
    /// here, `stop` is set when the agent loop ends.
    Agent {
        /// The agent.
        agent: AgentRef,
        /// The scout-assigned group, for reviewers and challengers.
        group: Option<String>,
        /// `(tool name, input summary)` in call order.
        tools: Vec<(String, String)>,
        /// The streamed assistant text so far.
        text: String,
        /// Why the loop ended, once it did.
        stop: Option<String>,
    },
    /// An agent asked the user something; `answer` is set locally by
    /// [`AiReviewState::record_answer`] when the user submits (the hub
    /// emits no answered event).
    Question {
        /// The id `Command::AiAnswer` carries.
        id: u64,
        /// The asking agent.
        agent: AgentRef,
        /// The question text.
        text: String,
        /// Quick-reply choices; empty means free text.
        options: Vec<String>,
        /// The submitted answer, once there is one.
        answer: Option<String>,
    },
    /// A command or the prepare stage awaits a decision; `decided` is set
    /// locally by [`AiReviewState::record_approval`].
    Approval {
        /// The id `Command::AiApprove` carries.
        id: u64,
        /// The asking agent.
        agent: AgentRef,
        /// What is being asked.
        kind: ApprovalKind,
        /// The decision's display text, once there is one.
        decided: Option<String>,
    },
    /// A finding was recorded; the row itself lives in
    /// [`AiReviewState::findings`].
    Finding {
        /// The `ai_findings` row id.
        id: i64,
    },
    /// The user's chat message (Task 7 fills these in).
    ChatUser {
        /// The message text.
        text: String,
    },
    /// The chat agent's reply (Task 7).
    ChatReply {
        /// The reply text.
        text: String,
    },
    /// A notice the app produced: a cancelled or budget-stopped run, the
    /// unsandboxed shell, a stash conflict.
    Notice {
        /// The text to show.
        text: String,
    },
}

/// The findings summary the tab and the diff markers show (spec §12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindingCounts {
    /// Live findings (`Draft | Kept | Posted`) by severity, indexed
    /// `[bug, risk, nit, question]`.
    pub by_severity: [u32; 4],
    /// Findings the verifier dropped, shown behind a toggle.
    pub dropped_by_verifier: u32,
    /// Coverage entries the app marked skipped.
    pub skipped_checks: u32,
    /// Re-review outcomes, indexed `[addressed, still_present, outdated]`.
    pub prior: [u32; 3],
}

/// One PR's AI review state, folded from the worker's AI events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiReviewState {
    /// The checkout/run lifecycle phase.
    pub phase: AiPhase,
    /// The timeline, oldest entry first.
    pub timeline: Vec<TimelineEntry>,
    /// Findings by row id. Populated from snapshots; live events carry ids.
    pub findings: BTreeMap<i64, Finding>,
    /// The latest run's coverage log.
    pub coverage: Vec<CoverageEntry>,
    /// The assembler's draft review, once filed.
    pub draft: Option<ReviewDraft>,
    /// Run token totals, `(input, output)`.
    pub usage: (u64, u64),
    /// The checkout this PR holds, until the restore closes it.
    pub checkout: Option<CheckoutRecord>,
    /// The checkout plan awaiting confirmation.
    pub plan: Option<CheckoutPlan>,
    /// The `git status` lines the finish is waiting on a decision for.
    pub restore_decision: Option<Vec<String>>,
    /// The chat composer's draft text (UI-local; survives snapshots).
    pub chat_input: String,
    /// Whether the tab shows the verifier-dropped findings (UI-local;
    /// survives snapshots, like [`Self::chat_input`]).
    pub show_dropped: bool,
    /// The head the last completed review ran against (spec §5.8).
    pub last_reviewed_sha: Option<String>,
    /// The scout's suggested commands awaiting the tab's "Add to settings"
    /// action (spec §4 step 1), read from the snapshot's stored brief. The
    /// scout only fills them when the repo has no configured commands.
    pub suggestions: Option<BriefSuggestions>,
}

/// The scout's build/test/lint suggestions for a repo with no configured
/// commands (spec §4 step 1), shown as a Notice until the user applies them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BriefSuggestions {
    /// Commands the agents may then run in the sandbox without asking
    /// (spec §8.1).
    pub commands: Vec<String>,
    /// Commands run outside the sandbox before the run (spec §8.5).
    pub prepare: Vec<String>,
}

impl AiReviewState {
    /// Fold one runtime event in. Non-AI events, [`Event::AiUnrestored`]
    /// (app-wide; Task 4's recovery dialog reads it in `app.rs`) and
    /// [`Event::AiChat`] (Task 7) are ignored here.
    ///
    /// The caller has already dropped events whose session is not the open
    /// one (the stale-event rule), so the session fields are not rechecked.
    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::AiSnapshot { snapshot, .. } => self.apply_snapshot(snapshot),
            Event::AiCheckoutPlanned { plan, .. } => {
                self.plan = Some(plan.clone());
                if matches!(
                    self.phase,
                    AiPhase::Idle | AiPhase::Done | AiPhase::Finished | AiPhase::Failed(_)
                ) {
                    self.phase = AiPhase::Planning;
                }
            }
            Event::AiCheckoutFailed { message, .. } => {
                self.plan = None;
                self.phase = AiPhase::Failed(message.clone());
            }
            Event::AiCheckoutHeld { pr_number, .. } => {
                self.plan = None;
                self.phase = AiPhase::Failed(format!(
                    "PR #{pr_number} is already checked out in this repository"
                ));
            }
            Event::AiNotice { text, .. } => {
                self.timeline
                    .push(TimelineEntry::Notice { text: text.clone() });
                // The only producer today is the "leave as is" answer to a
                // restore decision: the run is closed and the checkout was
                // left in place, so the tab rests at Done, not Restoring,
                // and the decision is answered.
                if self.phase == AiPhase::Restoring {
                    self.phase = AiPhase::Done;
                }
                self.restore_decision = None;
            }
            // There is no "checkout started" event; the prepare output is the
            // first signal that the confirmed plan got past the checkout
            // (spec §10.3 step 2), so that is where CheckingOut begins.
            Event::AiPrepareOutput { .. } => {
                if matches!(self.phase, AiPhase::Idle | AiPhase::Planning) {
                    self.phase = AiPhase::CheckingOut;
                }
            }
            Event::AiPipeline { event, .. } => self.apply_pipeline(event),
            Event::AiQuestion {
                id,
                agent,
                text,
                options,
                ..
            } => self.upsert_question(*id, agent, text, options),
            Event::AiApprovalRequest {
                id, agent, kind, ..
            } => {
                let asked = self.timeline.iter().any(
                    |entry| matches!(entry, TimelineEntry::Approval { id: seen, .. } if seen == id),
                );
                if !asked {
                    self.timeline.push(TimelineEntry::Approval {
                        id: *id,
                        agent: agent.clone(),
                        kind: kind.clone(),
                        decided: None,
                    });
                }
            }
            Event::AiRestoreNeedsDecision { changes, .. } => {
                self.restore_decision = Some(changes.clone());
                self.phase = AiPhase::Restoring;
            }
            Event::AiRestored { outcome, .. } => {
                if let RestoreOutcome::StashConflict { stash_sha } = outcome {
                    self.timeline.push(TimelineEntry::Notice {
                        text: format!(
                            "the stash did not apply cleanly; your changes are still in stash {stash_sha}"
                        ),
                    });
                }
                self.restore_decision = None;
                self.checkout = None;
                self.plan = None;
                self.phase = AiPhase::Finished;
            }
            _ => {}
        }
    }

    /// Record the user's answer to question `id` locally: the hub resolves
    /// the parked oneshot but emits no "answered" event (Task 3 submits
    /// both this and the [`crate::runtime::Command::AiAnswer`] together).
    pub fn record_answer(&mut self, id: u64, answer: String) {
        let question =
            self.timeline.iter_mut().rev().find(
                |entry| matches!(entry, TimelineEntry::Question { id: seen, .. } if *seen == id),
            );
        if let Some(TimelineEntry::Question { answer: slot, .. }) = question {
            *slot = Some(answer);
        }
    }

    /// Record the user's decision on approval `id` locally, for the same
    /// reason as [`Self::record_answer`].
    pub fn record_approval(&mut self, id: u64, decision: String) {
        let approval =
            self.timeline.iter_mut().rev().find(
                |entry| matches!(entry, TimelineEntry::Approval { id: seen, .. } if *seen == id),
            );
        if let Some(TimelineEntry::Approval { decided: slot, .. }) = approval {
            *slot = Some(decision);
        }
    }

    /// Park or release the run indicator when the user presses Pause or
    /// Resume; only meaningful while [`AiPhase::Running`] /
    /// [`AiPhase::Paused`], every other phase is left alone.
    pub fn set_paused(&mut self, paused: bool) {
        match (&self.phase, paused) {
            (AiPhase::Running, true) => self.phase = AiPhase::Paused,
            (AiPhase::Paused, false) => self.phase = AiPhase::Running,
            _ => {}
        }
    }

    /// Append the user's chat message to the timeline as soon as the
    /// composer sends it (Task 3); the chat agent's own echo arrives with
    /// Task 7's [`Event::AiChat`].
    pub fn push_chat_user(&mut self, text: String) {
        self.timeline.push(TimelineEntry::ChatUser { text });
    }

    /// The user cancelled a plan that was awaiting confirmation (Task 4's
    /// dialog): the tab returns to rest so Start can be pressed again.
    /// Every other phase is left alone.
    pub fn cancel_planning(&mut self) {
        if self.phase == AiPhase::Planning {
            self.phase = AiPhase::Idle;
            self.plan = None;
        }
    }

    /// The stage strip: one entry per stage that has announced itself, in
    /// pipeline execution order, with its latest status and an agent
    /// progress label (`"2/3"` = two of three agents finished, empty when
    /// the stage runs no counted agents).
    pub fn stage_strip(&self) -> Vec<(StageKind, StageStatus, String)> {
        let mut strip: Vec<(StageKind, StageStatus, String)> = Vec::new();
        for entry in &self.timeline {
            let TimelineEntry::Stage { stage, status } = entry else {
                continue;
            };
            match strip.iter_mut().find(|(seen, ..)| seen == stage) {
                Some((_, last, _)) => *last = status.clone(),
                None => strip.push((*stage, status.clone(), String::new())),
            }
        }
        for (stage, _, progress) in &mut strip {
            let (started, finished) = self.stage_agents(*stage);
            if started > 0 {
                *progress = format!("{finished}/{started}");
            }
        }
        strip.sort_by_key(|(stage, ..)| stage_order(*stage));
        strip
    }

    /// The findings summary over the current [`Self::findings`] and
    /// [`Self::coverage`].
    pub fn counts(&self) -> FindingCounts {
        let mut counts = FindingCounts::default();
        for finding in self.findings.values() {
            match finding.state {
                FindingState::Draft | FindingState::Kept | FindingState::Posted => {
                    counts.by_severity[severity_index(finding.severity)] += 1;
                }
                FindingState::DroppedByVerifier => counts.dropped_by_verifier += 1,
                FindingState::Addressed => counts.prior[0] += 1,
                FindingState::StillPresent => counts.prior[1] += 1,
                FindingState::Outdated => counts.prior[2] += 1,
                FindingState::DroppedByUser => {}
            }
        }
        counts.skipped_checks = self
            .coverage
            .iter()
            .filter(|entry| entry.result == CheckResult::Skipped)
            .count() as u32;
        counts
    }

    /// The draft findings anchored to `path`, the inline AI drafts the diff
    /// shows (spec §12).
    pub fn drafts_for(&self, path: &str) -> impl Iterator<Item = &Finding> {
        self.findings
            .values()
            .filter(move |f| f.state == FindingState::Draft && f.anchor.path() == path)
    }

    /// Whether an incremental re-review makes sense: the last completed
    /// review ran against a different head, and nothing is in flight.
    pub fn can_continue(&self, head_sha: &str) -> bool {
        matches!(
            self.phase,
            AiPhase::Idle | AiPhase::Done | AiPhase::Finished | AiPhase::Failed(_)
        ) && self
            .last_reviewed_sha
            .as_deref()
            .is_some_and(|sha| sha != head_sha)
    }

    /// Rebuild the whole state from an attach/start snapshot (spec §10.3).
    /// `plan` and `chat_input` survive: the plan lives in the hub and the
    /// composer text is the user's half-typed message.
    fn apply_snapshot(&mut self, snap: &AiSnapshotData) {
        self.phase = snapshot_phase(snap);
        self.timeline = timeline_from_transcript(&snap.timeline);
        if let Some(reason) = &snap.bash_disabled {
            self.timeline.push(TimelineEntry::Notice {
                text: format!("the agent shell is unsandboxed: {reason}"),
            });
        }
        self.findings = snap.findings.iter().map(|f| (f.id, f.clone())).collect();
        self.coverage = snap.coverage.clone();
        self.checkout = snap.checkout.clone();
        self.last_reviewed_sha = snap.last_reviewed_sha.clone();
        self.suggestions = brief_suggestions(snap.brief.as_deref());
        // The snapshot carries neither the draft nor the usage tally, so a
        // rebuild starts those fresh; the next run refills them.
        self.draft = None;
        self.usage = (0, 0);
        self.restore_decision = None;
    }

    /// Fold one forwarded pipeline event in.
    fn apply_pipeline(&mut self, ev: &PipelineEvent) {
        match ev {
            PipelineEvent::Stage { stage, status } => {
                self.timeline.push(TimelineEntry::Stage {
                    stage: *stage,
                    status: status.clone(),
                });
                if matches!(
                    self.phase,
                    AiPhase::Idle | AiPhase::Planning | AiPhase::CheckingOut
                ) {
                    self.phase = AiPhase::Running;
                }
            }
            PipelineEvent::AgentStarted { agent, group } => {
                let index = self.ensure_agent_entry(agent);
                if let TimelineEntry::Agent { group: slot, .. } = &mut self.timeline[index] {
                    *slot = group.clone();
                }
            }
            PipelineEvent::AgentFinished { agent, stop } => {
                let index = self.ensure_agent_entry(agent);
                if let TimelineEntry::Agent { stop: slot, .. } = &mut self.timeline[index] {
                    *slot = Some(stop.clone());
                }
            }
            PipelineEvent::AgentText { agent, delta } => {
                let index = self.ensure_agent_entry(agent);
                if let TimelineEntry::Agent { text, .. } = &mut self.timeline[index] {
                    text.push_str(delta);
                }
            }
            PipelineEvent::ToolCall {
                agent,
                name,
                summary,
            } => {
                let index = self.ensure_agent_entry(agent);
                if let TimelineEntry::Agent { tools, .. } = &mut self.timeline[index] {
                    tools.push((name.clone(), summary.clone()));
                }
            }
            PipelineEvent::Question { agent, question } => {
                self.upsert_question(question.id, agent, &question.text, &question.options);
            }
            // The same approval as the [`Event::AiApprovalRequest`] that
            // follows it, announced in the pipeline's own id space; the
            // request event owns the card because its id answers it.
            PipelineEvent::Approval { .. } => {}
            PipelineEvent::Usage { input, output, .. } => {
                self.usage.0 += input;
                self.usage.1 += output;
            }
            PipelineEvent::DraftReady { draft } => self.draft = Some(draft.clone()),
            PipelineEvent::FindingAdded { id } => {
                let seen = self.timeline.iter().any(
                    |entry| matches!(entry, TimelineEntry::Finding { id: known } if known == id),
                );
                if !seen {
                    self.timeline.push(TimelineEntry::Finding { id: *id });
                }
            }
            // The full finding and coverage rows arrive with the next
            // snapshot; the card already exists from `FindingAdded`.
            PipelineEvent::FindingChanged { .. }
            | PipelineEvent::Coverage { .. }
            | PipelineEvent::BriefReady => {}
            PipelineEvent::RunEnded {
                status,
                stop_reason,
                ..
            } => {
                self.phase = match status {
                    RunStatus::Completed => AiPhase::Done,
                    RunStatus::Failed => AiPhase::Failed(
                        stop_reason
                            .clone()
                            .unwrap_or_else(|| "the review failed".to_string()),
                    ),
                    RunStatus::Cancelled => {
                        self.timeline.push(TimelineEntry::Notice {
                            text: "the review was cancelled".to_string(),
                        });
                        AiPhase::Done
                    }
                    RunStatus::BudgetExhausted => {
                        self.timeline.push(TimelineEntry::Notice {
                            text: "the review stopped: token budget exhausted".to_string(),
                        });
                        AiPhase::Done
                    }
                };
            }
        }
    }

    /// The index of `agent`'s timeline entry, creating an empty one at the
    /// end when a transcript event arrives before its `AgentStarted`.
    fn ensure_agent_entry(&mut self, agent: &AgentRef) -> usize {
        if let Some(index) = self.timeline.iter().rposition(
            |entry| matches!(entry, TimelineEntry::Agent { agent: known, .. } if known == agent),
        ) {
            return index;
        }
        self.timeline.push(TimelineEntry::Agent {
            agent: agent.clone(),
            group: None,
            tools: Vec::new(),
            text: String::new(),
            stop: None,
        });
        self.timeline.len() - 1
    }

    /// Insert or refresh the question card for `id`: the live run announces
    /// every question twice, through [`Event::AiQuestion`] and
    /// [`PipelineEvent::Question`], both carrying the same id.
    fn upsert_question(&mut self, id: u64, agent: &AgentRef, text: &str, options: &[String]) {
        let existing =
            self.timeline.iter_mut().rev().find(
                |entry| matches!(entry, TimelineEntry::Question { id: seen, .. } if *seen == id),
            );
        if let Some(TimelineEntry::Question {
            text: slot,
            options: option_slot,
            ..
        }) = existing
        {
            *slot = text.to_string();
            *option_slot = options.to_vec();
        } else {
            self.timeline.push(TimelineEntry::Question {
                id,
                agent: agent.clone(),
                text: text.to_string(),
                options: options.to_vec(),
                answer: None,
            });
        }
    }

    /// `(started, finished)` agent entries counted against `stage`.
    fn stage_agents(&self, stage: StageKind) -> (u32, u32) {
        let mut started = 0;
        let mut finished = 0;
        for entry in &self.timeline {
            if let TimelineEntry::Agent { agent, stop, .. } = entry
                && agent_stage(agent.role) == Some(stage)
            {
                started += 1;
                if stop.is_some() {
                    finished += 1;
                }
            }
        }
        (started, finished)
    }
}

/// The command suggestions carried by a stored brief JSON (spec §4 step 1).
/// The scout fills these only when the repo has no configured commands, and
/// a brief that fails to parse suggests nothing.
fn brief_suggestions(raw: Option<&str>) -> Option<BriefSuggestions> {
    let brief: crate::ai::stages::scout::Brief = serde_json::from_str(raw?).ok()?;
    if brief.suggested_commands.is_empty() && brief.suggested_prepare.is_empty() {
        return None;
    }
    Some(BriefSuggestions {
        commands: brief.suggested_commands,
        prepare: brief.suggested_prepare,
    })
}

/// The phase a snapshot's run row and checkout describe (spec §10.3).
fn snapshot_phase(snap: &AiSnapshotData) -> AiPhase {
    let Some(run) = snap.run.as_ref() else {
        // A checkout with no run row: the start is still in flight.
        return if snap.checkout.is_some() {
            AiPhase::CheckingOut
        } else {
            AiPhase::Idle
        };
    };
    match run.status.as_str() {
        "running" => AiPhase::Running,
        "completed" | "cancelled" | "budget_exhausted" => AiPhase::Done,
        "failed" => AiPhase::Failed(
            run.stop_reason
                .clone()
                .unwrap_or_else(|| "the review failed".to_string()),
        ),
        other => {
            tracing::warn!(status = other, "ai state: unrecognised run status");
            AiPhase::Idle
        }
    }
}

/// Rebuild the timeline from the stored transcript (spec §10.3): assistant
/// turns contribute their text and tool calls to a per-agent entry. User
/// rows are task prompts and tool results, nothing the tab shows.
fn timeline_from_transcript(rows: &[AiTimelineEntry]) -> Vec<TimelineEntry> {
    let mut out: Vec<TimelineEntry> = Vec::new();
    for row in rows {
        if row.role != "assistant" {
            continue;
        }
        let Ok(blocks) = serde_json::from_str::<Vec<serde_json::Value>>(&row.content_json) else {
            continue;
        };
        let mut text = String::new();
        let mut tools: Vec<(String, String)> = Vec::new();
        for block in &blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => {
                    text.push_str(
                        block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or(""),
                    );
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let summary = block
                        .get("input")
                        .map(|input| summarize_input(&input.to_string()))
                        .unwrap_or_default();
                    tools.push((name, summary));
                }
                _ => {}
            }
        }
        if text.is_empty() && tools.is_empty() {
            continue;
        }
        let index = match out
            .iter()
            .rposition(|entry| matches!(entry, TimelineEntry::Agent { agent, .. } if agent.label == row.agent_id))
        {
            Some(index) => index,
            None => {
                out.push(TimelineEntry::Agent {
                    agent: agent_ref_from_label(&row.agent_id),
                    group: None,
                    tools: Vec::new(),
                    text: String::new(),
                    stop: None,
                });
                out.len() - 1
            }
        };
        if let TimelineEntry::Agent {
            tools: entry_tools,
            text: entry_text,
            ..
        } = &mut out[index]
        {
            entry_tools.extend(tools);
            entry_text.push_str(&text);
        }
    }
    out
}

/// The transcript stores agents by their pipeline label (spec §9.2):
/// `scout`, `reviewer:<group>`, `challenger:<group>`, `verifier:<path>`,
/// `lookup:<asker>`, `prior_check`, `assembler`, `chat`. The label is all
/// the row carries, so the id is unknown (0) and the role is inferred.
fn agent_ref_from_label(label: &str) -> AgentRef {
    let role = match label.split(':').next().unwrap_or(label) {
        "scout" => AgentRole::Scout,
        "reviewer" => AgentRole::Reviewer,
        "challenger" => AgentRole::Challenger,
        "verifier" => AgentRole::Verifier,
        "lookup" => AgentRole::Lookup,
        "prior_check" => AgentRole::PriorCheck,
        "assembler" => AgentRole::Assembler,
        "chat" => AgentRole::Chat,
        _ => AgentRole::App,
    };
    AgentRef {
        id: 0,
        role,
        label: label.to_string(),
    }
}

/// A tool input summary, cut like [`PipelineEvent::ToolCall`]'s.
fn summarize_input(input: &str) -> String {
    const MAX: usize = 120;
    let count = input.chars().count();
    if count <= MAX {
        return input.to_string();
    }
    let cut: String = input.chars().take(MAX).collect();
    format!("{cut}…")
}

/// Which stage's progress an agent role counts toward.
fn agent_stage(role: AgentRole) -> Option<StageKind> {
    match role {
        AgentRole::Scout => Some(StageKind::Scout),
        AgentRole::Reviewer => Some(StageKind::Review),
        AgentRole::Challenger => Some(StageKind::Challenge),
        AgentRole::Verifier => Some(StageKind::Verify),
        AgentRole::PriorCheck => Some(StageKind::PriorCheck),
        AgentRole::Assembler => Some(StageKind::Assemble),
        AgentRole::Lookup | AgentRole::Chat | AgentRole::App => None,
    }
}

/// Pipeline execution order, the order the strip shows (spec §6).
fn stage_order(stage: StageKind) -> u8 {
    match stage {
        StageKind::PriorCheck => 0,
        StageKind::Scout => 1,
        StageKind::Review => 2,
        StageKind::Challenge => 3,
        StageKind::Verify => 4,
        StageKind::Assemble => 5,
    }
}

/// The index of `severity` in [`FindingCounts::by_severity`].
fn severity_index(severity: Severity) -> usize {
    match severity {
        Severity::Bug => 0,
        Severity::Risk => 1,
        Severity::Nit => 2,
        Severity::Question => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::interaction::Question;
    use crate::ai::store::{CheckoutState, OriginalRef, RunRecord};
    use crate::ai::tools::AgentRole;
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;
    use crate::runtime::SessionId;
    use std::path::PathBuf;

    const SESSION: SessionId = SessionId(1);

    fn reviewer() -> AgentRef {
        AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer:g1".to_string(),
        }
    }

    fn scout() -> AgentRef {
        AgentRef {
            id: 2,
            role: AgentRole::Scout,
            label: "scout".to_string(),
        }
    }

    fn pipeline(event: PipelineEvent) -> Event {
        Event::AiPipeline {
            session: SESSION,
            event,
        }
    }

    fn agent_text(agent: &AgentRef, delta: &str) -> Event {
        pipeline(PipelineEvent::AgentText {
            agent: agent.clone(),
            delta: delta.to_string(),
        })
    }

    fn line_finding(id: i64, severity: Severity, state: FindingState, path: &str) -> Finding {
        Finding {
            id,
            run_id: 9,
            agent: reviewer(),
            anchor: crate::ai::review::FindingAnchor::Line(Anchor {
                path: path.to_string(),
                side: Side::Right,
                line: 3,
                start_line: None,
                start_side: None,
            }),
            severity,
            title: format!("finding {id}"),
            body: "body".to_string(),
            evidence: vec![],
            checklist_item: None,
            state,
            state_reason: None,
            pending_local_id: None,
        }
    }

    fn coverage(file: &str, result: CheckResult) -> CoverageEntry {
        CoverageEntry {
            file: file.to_string(),
            item: "errors".to_string(),
            result,
            note: String::new(),
        }
    }

    fn checkout_record() -> CheckoutRecord {
        CheckoutRecord {
            id: 4,
            repo_path: PathBuf::from("/repo"),
            repo: "o/r".to_string(),
            pr_number: 7,
            original: OriginalRef::Branch("main".to_string()),
            stash_sha: None,
            head_sha: "sha_head".to_string(),
            state: CheckoutState::Active,
        }
    }

    fn run(status: &str, stop_reason: Option<&str>) -> RunRecord {
        RunRecord {
            id: 9,
            kind: "full".to_string(),
            base_sha: None,
            head_sha: "sha_head".to_string(),
            status: status.to_string(),
            stop_reason: stop_reason.map(str::to_string),
        }
    }

    fn assistant(agent_id: &str, text: &str) -> AiTimelineEntry {
        AiTimelineEntry {
            agent_id: agent_id.to_string(),
            role: "assistant".to_string(),
            content_json: format!(r#"[{{"type":"text","text":{text}}}]"#),
        }
    }

    #[test]
    fn text_deltas_coalesce_per_agent() {
        let mut st = AiReviewState::default();
        st.apply(&pipeline(PipelineEvent::AgentStarted {
            agent: reviewer(),
            group: Some("g1".to_string()),
        }));
        st.apply(&pipeline(PipelineEvent::AgentStarted {
            agent: scout(),
            group: None,
        }));
        st.apply(&agent_text(&reviewer(), "Hel"));
        st.apply(&agent_text(&scout(), "By"));
        st.apply(&agent_text(&reviewer(), "lo"));
        st.apply(&pipeline(PipelineEvent::ToolCall {
            agent: reviewer(),
            name: "read_file".to_string(),
            summary: "src/a.rs".to_string(),
        }));
        st.apply(&pipeline(PipelineEvent::AgentFinished {
            agent: reviewer(),
            stop: "finished".to_string(),
        }));

        let agents: Vec<&TimelineEntry> = st
            .timeline
            .iter()
            .filter(|e| matches!(e, TimelineEntry::Agent { .. }))
            .collect();
        assert_eq!(agents.len(), 2, "one entry per agent, arrival order");
        assert_eq!(
            agents[0],
            &TimelineEntry::Agent {
                agent: reviewer(),
                group: Some("g1".to_string()),
                tools: vec![("read_file".to_string(), "src/a.rs".to_string())],
                text: "Hello".to_string(),
                stop: Some("finished".to_string()),
            },
            "the reviewer's deltas and tool call coalesced into its entry"
        );
        assert_eq!(
            agents[1],
            &TimelineEntry::Agent {
                agent: scout(),
                group: None,
                tools: Vec::new(),
                text: "By".to_string(),
                stop: None,
            },
            "the other agent's stream is untouched"
        );
    }

    #[test]
    fn question_then_answer_marks_entry() {
        let mut st = AiReviewState::default();
        st.apply(&Event::AiQuestion {
            session: SESSION,
            id: 7,
            agent: reviewer(),
            text: "is empty input allowed?".to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
        });
        // The pipeline announces the same question through its own channel;
        // it must not double the card.
        st.apply(&pipeline(PipelineEvent::Question {
            agent: reviewer(),
            question: Question {
                id: 7,
                text: "is empty input allowed?".to_string(),
                options: vec!["yes".to_string(), "no".to_string()],
            },
        }));
        assert_eq!(
            st.timeline
                .iter()
                .filter(|e| matches!(e, TimelineEntry::Question { .. }))
                .count(),
            1
        );

        st.record_answer(7, "yes".to_string());
        assert_eq!(
            st.timeline,
            vec![TimelineEntry::Question {
                id: 7,
                agent: reviewer(),
                text: "is empty input allowed?".to_string(),
                options: vec!["yes".to_string(), "no".to_string()],
                answer: Some("yes".to_string()),
            }]
        );

        st.apply(&Event::AiApprovalRequest {
            session: SESSION,
            id: 3,
            agent: reviewer(),
            kind: ApprovalKind::Command {
                command: "cargo test".to_string(),
                reason: "runs code".to_string(),
            },
        });
        st.record_approval(3, "Once".to_string());
        assert_eq!(
            st.timeline[1],
            TimelineEntry::Approval {
                id: 3,
                agent: reviewer(),
                kind: ApprovalKind::Command {
                    command: "cargo test".to_string(),
                    reason: "runs code".to_string(),
                },
                decided: Some("Once".to_string()),
            }
        );
    }

    #[test]
    fn counts_track_states() {
        let mut st = AiReviewState::default();
        for f in [
            line_finding(1, Severity::Bug, FindingState::Draft, "a.rs"),
            line_finding(2, Severity::Risk, FindingState::Kept, "a.rs"),
            line_finding(3, Severity::Nit, FindingState::Posted, "b.rs"),
            line_finding(
                4,
                Severity::Question,
                FindingState::DroppedByVerifier,
                "a.rs",
            ),
            line_finding(5, Severity::Bug, FindingState::DroppedByVerifier, "a.rs"),
            line_finding(6, Severity::Risk, FindingState::Addressed, "a.rs"),
            line_finding(7, Severity::Risk, FindingState::StillPresent, "a.rs"),
            line_finding(8, Severity::Nit, FindingState::Outdated, "a.rs"),
            line_finding(9, Severity::Bug, FindingState::DroppedByUser, "a.rs"),
        ] {
            st.findings.insert(f.id, f);
        }
        st.coverage = vec![
            coverage("a.rs", CheckResult::Skipped),
            coverage("b.rs", CheckResult::Skipped),
            coverage("b.rs", CheckResult::Ok),
        ];

        let counts = st.counts();
        assert_eq!(
            counts.by_severity,
            [1, 1, 1, 0],
            "live findings only, indexed [bug, risk, nit, question]"
        );
        assert_eq!(counts.dropped_by_verifier, 2);
        assert_eq!(counts.skipped_checks, 2);
        assert_eq!(counts.prior, [1, 1, 1], "[addressed, still, outdated]");

        let drafts: Vec<&Finding> = st.drafts_for("a.rs").collect();
        assert_eq!(drafts.len(), 1, "only the Draft finding on a.rs");
        assert_eq!(drafts[0].id, 1);
        assert_eq!(st.drafts_for("b.rs").count(), 0, "b.rs has none in Draft");
    }

    #[test]
    fn can_continue_only_when_head_moved_and_not_running() {
        let mut st = AiReviewState::default();
        assert!(
            !st.can_continue("sha_new"),
            "nothing was ever reviewed, so there is nothing to continue from"
        );

        st.last_reviewed_sha = Some("sha_old".to_string());
        assert!(
            !st.can_continue("sha_old"),
            "the head has not moved since the last review"
        );
        assert!(st.can_continue("sha_new"), "idle and the head moved");

        for (phase, expected) in [
            (AiPhase::Idle, true),
            (AiPhase::Done, true),
            (AiPhase::Finished, true),
            (AiPhase::Failed("boom".to_string()), true),
            (AiPhase::Planning, false),
            (AiPhase::CheckingOut, false),
            (AiPhase::Running, false),
            (AiPhase::Paused, false),
            (AiPhase::Restoring, false),
        ] {
            st.phase = phase.clone();
            assert_eq!(st.can_continue("sha_new"), expected, "phase {phase:?}");
        }
    }

    #[test]
    fn snapshot_replaces_state() {
        let mut st = AiReviewState::default();
        st.timeline.push(TimelineEntry::Notice {
            text: "stale".to_string(),
        });
        st.findings.insert(
            99,
            line_finding(99, Severity::Bug, FindingState::Draft, "z.rs"),
        );
        st.chat_input = "half-typed".to_string();

        let finding = line_finding(1, Severity::Risk, FindingState::Kept, "a.rs");
        let snapshot = AiSnapshotData {
            findings: vec![finding.clone()],
            coverage: vec![coverage("a.rs", CheckResult::Ok)],
            timeline: vec![assistant("scout", r#""scouting done""#)],
            brief: None,
            run: Some(run("completed", None)),
            checkout: Some(checkout_record()),
            bash_disabled: Some("no bwrap".to_string()),
            last_reviewed_sha: Some("sha_old".to_string()),
        };
        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot,
        });

        assert_eq!(st.phase, AiPhase::Done, "the run row says completed");
        assert_eq!(
            st.findings.keys().copied().collect::<Vec<_>>(),
            vec![1],
            "the old finding is gone, the snapshot's is in"
        );
        assert_eq!(st.findings[&1], finding);
        assert_eq!(st.coverage, vec![coverage("a.rs", CheckResult::Ok)]);
        assert_eq!(st.checkout, Some(checkout_record()));
        assert_eq!(st.last_reviewed_sha.as_deref(), Some("sha_old"));
        assert_eq!(st.chat_input, "half-typed", "the composer draft survives");
        assert_eq!(
            st.timeline,
            vec![
                TimelineEntry::Agent {
                    agent: AgentRef {
                        id: 0,
                        role: AgentRole::Scout,
                        label: "scout".to_string(),
                    },
                    group: None,
                    tools: Vec::new(),
                    text: "scouting done".to_string(),
                    stop: None,
                },
                TimelineEntry::Notice {
                    text: "the agent shell is unsandboxed: no bwrap".to_string(),
                },
            ],
            "the transcript is rebuilt and the stale notice is gone"
        );
        assert!(st.can_continue("sha_new"), "a completed run can continue");
    }

    /// A tab awaiting a checkout confirmation.
    fn planned() -> AiReviewState {
        AiReviewState {
            phase: AiPhase::Planning,
            plan: Some(CheckoutPlan {
                repo_path: PathBuf::from("/repo"),
                remote: "origin".to_string(),
                original: OriginalRef::Branch("main".to_string()),
                dirty: false,
                commands: Vec::new(),
            }),
            ..AiReviewState::default()
        }
    }

    /// Task 4's dialog-driven events: a held checkout fails the tab with a
    /// message naming the PR, and the "leave as is" notice rests a tab that
    /// was waiting on the restore.
    #[test]
    fn held_fails_the_tab_and_notice_rests_it() {
        let mut st = planned();
        st.apply(&Event::AiCheckoutHeld {
            session: SESSION,
            pr_number: 9,
        });
        assert!(
            matches!(&st.phase, AiPhase::Failed(message) if message.contains("#9")),
            "a held checkout names the PR that holds the checkout: {st:?}"
        );
        assert!(st.plan.is_none(), "the unstartable plan is dropped");

        st.phase = AiPhase::Restoring;
        st.restore_decision = Some(vec!["?? stray.txt".to_string()]);
        st.apply(&Event::AiNotice {
            session: SESSION,
            text: "left PR #7's checkout in place".to_string(),
        });
        assert_eq!(
            st.phase,
            AiPhase::Done,
            "the leave-as-is notice rests the tab"
        );
        assert!(
            st.restore_decision.is_none(),
            "the notice answers the restore decision"
        );
        assert!(
            matches!(st.timeline.last(),
                Some(TimelineEntry::Notice { text }) if text.contains("in place")),
            "the notice lands on the timeline"
        );
    }

    #[test]
    fn cancel_planning_only_rests_a_planning_tab() {
        let mut st = planned();
        st.cancel_planning();
        assert_eq!(st.phase, AiPhase::Idle, "a cancelled plan returns to rest");
        assert!(st.plan.is_none());

        st.phase = AiPhase::Running;
        st.cancel_planning();
        assert_eq!(st.phase, AiPhase::Running, "a live run is left alone");
    }

    /// The suggested-commands card's data comes from the snapshot's stored
    /// brief (spec §4 step 1); an absent, unparseable, or empty brief
    /// suggests nothing.
    #[test]
    fn snapshot_extracts_brief_suggestions() {
        fn snapshot_with(brief: Option<&str>) -> AiSnapshotData {
            AiSnapshotData {
                findings: Vec::new(),
                coverage: Vec::new(),
                timeline: Vec::new(),
                brief: brief.map(str::to_string),
                run: Some(run("completed", None)),
                checkout: Some(checkout_record()),
                bash_disabled: None,
                last_reviewed_sha: None,
            }
        }

        let mut st = AiReviewState::default();
        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot: snapshot_with(Some(
                r#"{"intent":"s","description_matches":true,"groups":[],"suggested_commands":["cargo test"],"suggested_prepare":["cargo fetch"]}"#,
            )),
        });
        assert_eq!(
            st.suggestions,
            Some(BriefSuggestions {
                commands: vec!["cargo test".to_string()],
                prepare: vec!["cargo fetch".to_string()],
            }),
            "the brief's proposals surface as suggestions"
        );

        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot: snapshot_with(Some(
                r#"{"intent":"s","description_matches":true,"groups":[]}"#,
            )),
        });
        assert_eq!(
            st.suggestions, None,
            "a brief with no proposals suggests nothing"
        );

        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot: snapshot_with(Some("not json")),
        });
        assert_eq!(
            st.suggestions, None,
            "an unparseable brief suggests nothing"
        );
    }

    #[test]
    fn phase_follows_the_run_lifecycle() {
        let mut st = AiReviewState::default();
        st.apply(&pipeline(PipelineEvent::Stage {
            stage: StageKind::Scout,
            status: StageStatus::Started,
        }));
        assert_eq!(st.phase, AiPhase::Running);

        st.set_paused(true);
        assert_eq!(st.phase, AiPhase::Paused);
        st.apply(&agent_text(&reviewer(), "hi"));
        assert_eq!(
            st.phase,
            AiPhase::Paused,
            "transcript events keep the pause"
        );

        st.apply(&pipeline(PipelineEvent::RunEnded {
            run_id: 9,
            status: RunStatus::BudgetExhausted,
            stop_reason: Some("token budget".to_string()),
        }));
        assert_eq!(st.phase, AiPhase::Done);
        assert!(
            st.timeline
                .iter()
                .any(|e| matches!(e, TimelineEntry::Notice { text } if text.contains("budget"))),
            "the budget stop is visible as a notice"
        );

        st.apply(&Event::AiRestoreNeedsDecision {
            session: SESSION,
            changes: vec!["M src/a.rs".to_string()],
        });
        assert_eq!(st.phase, AiPhase::Restoring);
        assert_eq!(st.restore_decision, Some(vec!["M src/a.rs".to_string()]));

        st.apply(&Event::AiRestored {
            session: SESSION,
            outcome: RestoreOutcome::Restored,
        });
        assert_eq!(st.phase, AiPhase::Finished);
        assert_eq!(st.restore_decision, None);
        assert_eq!(st.checkout, None, "nothing is held any more");
    }

    #[test]
    fn stage_strip_orders_stages_and_counts_agents() {
        let mut st = AiReviewState::default();
        st.apply(&pipeline(PipelineEvent::Stage {
            stage: StageKind::Review,
            status: StageStatus::Started,
        }));
        st.apply(&pipeline(PipelineEvent::Stage {
            stage: StageKind::Scout,
            status: StageStatus::Started,
        }));
        st.apply(&pipeline(PipelineEvent::AgentStarted {
            agent: reviewer(),
            group: Some("g1".to_string()),
        }));
        st.apply(&pipeline(PipelineEvent::AgentStarted {
            agent: AgentRef {
                id: 3,
                role: AgentRole::Reviewer,
                label: "reviewer:g2".to_string(),
            },
            group: Some("g2".to_string()),
        }));
        st.apply(&pipeline(PipelineEvent::AgentFinished {
            agent: reviewer(),
            stop: "finished".to_string(),
        }));
        st.apply(&pipeline(PipelineEvent::Stage {
            stage: StageKind::Scout,
            status: StageStatus::Done,
        }));

        assert_eq!(
            st.stage_strip(),
            vec![
                (StageKind::Scout, StageStatus::Done, String::new()),
                (StageKind::Review, StageStatus::Started, "1/2".to_string()),
            ],
            "pipeline order, latest status, agent progress"
        );
    }
}
