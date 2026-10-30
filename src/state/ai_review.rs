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
//! next [`Event::AiSnapshot`] (on attach, and again when a run starts) or
//! [`Event::AiFindings`] (as a live run adds and finalizes them).
//! [`PipelineEvent::Approval`] is ignored on purpose: it announces the same
//! approval as [`Event::AiApprovalRequest`] but in a different id space, and
//! only the latter's id answers it.

use std::collections::BTreeMap;

use crate::ai::checkout::{CheckoutPlan, RestoreOutcome};
pub use crate::ai::events::CommandDecision;
use crate::ai::events::{PipelineEvent, RunStatus, StageKind, StageStatus};
use crate::ai::hub::{AiSnapshotData, AiTimelineEntry, ChatEvent, RunLiveness};
use crate::ai::review::{
    CheckResult, CoverageEntry, Finding, FindingAnchor, FindingState, Severity,
};
use crate::ai::stages::assemble::ReviewDraft;
use crate::ai::store::{CheckoutRecord, CheckoutState};
use crate::ai::tools::{AgentRef, AgentRole};
use crate::ai::ui_interaction::ApprovalKind;
use crate::runtime::Event;
pub use review_approval::model_stage::ClassifierNote;

/// One tool line inside an agent's timeline entry. `decision` says who let a
/// `bash` call run or why it did not; `note` is the local classifier's
/// verdict, set when it was consulted for the call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolLine {
    /// The tool name.
    pub name: String,
    /// A one-line summary of its input.
    pub summary: String,
    /// The classifier's verdict, when it was asked.
    pub note: Option<ClassifierNote>,
    /// How a `bash` call was decided, once it was.
    pub decision: Option<CommandDecision>,
}

/// The tab's badge text for a classifier verdict.
pub fn classifier_note_text(note: ClassifierNote) -> &'static str {
    match note {
        ClassifierNote::Approved => "classifier: safe",
        ClassifierNote::Overruled => "classifier: safe, overruled",
        ClassifierNote::NeedsApproval => "classifier: needs approval",
        ClassifierNote::Dangerous => "classifier: flagged dangerous",
        ClassifierNote::Unavailable => "classifier: unavailable",
    }
}

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
        /// The tool calls, in call order; `note` is set on the `bash` lines
        /// the local classifier was consulted for.
        tools: Vec<ToolLine>,
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
    /// A tool the chat agent called (Task 7).
    ChatTool {
        /// The tool name.
        name: String,
        /// A one-line summary of its input.
        summary: String,
        /// What the local classifier said about a `bash` call; `None` when
        /// it was not consulted (every non-bash tool, and bash without one).
        note: Option<ClassifierNote>,
        /// How a `bash` call was decided, once it was.
        decision: Option<CommandDecision>,
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
    /// Whether the current chat reply is still streaming. A tool call (or a
    /// new user message) closes it, so later text starts a new bubble.
    /// UI-local; reset by snapshots like [`Self::chat_input`].
    pub chat_reply_open: bool,
    /// Whether the tab shows the verifier-dropped findings (UI-local;
    /// survives snapshots, like [`Self::chat_input`]).
    pub show_dropped: bool,
    /// The head the last completed review ran against (spec §5.8).
    pub last_reviewed_sha: Option<String>,
    /// How many agents each stage announced it would run
    /// ([`PipelineEvent::StagePlanned`]); live runs only.
    pub planned: Vec<(crate::ai::events::StageKind, u32)>,
    /// The scout's brief, shown in place of its `submit_brief` call: from
    /// the run's `BriefReady`, or the stored one when the tab loads.
    pub brief: Option<crate::ai::stages::scout::Brief>,
    /// The scout's suggested commands awaiting the tab's "Add to settings"
    /// action (spec §4 step 1), read from the snapshot's stored brief. The
    /// scout only fills them when the repo has no configured commands.
    pub suggestions: Option<BriefSuggestions>,
    /// The inline editor of one AI draft: the finding id and its buffer.
    /// UI-local; cleared when a snapshot replaces the findings.
    pub edit: Option<(i64, String)>,
    /// The `bash` lines in the order their decisions landed, oldest first:
    /// the tab's command log lists them newest first. Rebuilt timelines
    /// (snapshots) carry no decisions, so a snapshot clears it.
    pub decided: Vec<CommandRef>,
}

/// Where one decided `bash` call sits in [`AiReviewState::timeline`]: the
/// entry index, and the tool index inside an [`TimelineEntry::Agent`]
/// entry (`None` for a [`TimelineEntry::ChatTool`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandRef {
    /// The timeline index.
    pub entry: usize,
    /// The tool index inside an agent entry; `None` for a chat tool.
    pub tool: Option<usize>,
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
    /// Fold one runtime event in. Non-AI events and [`Event::AiUnrestored`]
    /// (app-wide; Task 4's recovery dialog reads it in `app.rs`) are ignored
    /// here.
    ///
    /// The caller has already dropped events whose session is not the open
    /// one (the stale-event rule), so the session fields are not rechecked.
    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::AiSnapshot { snapshot, .. } => self.apply_snapshot(snapshot),
            Event::AiFindings {
                findings, coverage, ..
            } => {
                self.findings = findings.iter().map(|f| (f.id, f.clone())).collect();
                self.coverage = coverage.clone();
            }
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
            Event::AiChat { event, .. } => self.apply_chat(event),
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
                    // Task 8: the Notice carries the SHA, the exact command
                    // to apply the stash later, and where the entry lives
                    // until then (spec §7.3).
                    self.timeline.push(TimelineEntry::Notice {
                        text: format!(
                            "the stash did not apply cleanly; your changes are safe in stash \
                             {stash_sha} — apply them later with `git stash apply {stash_sha}` \
                             (the entry stays in `git stash list` until you drop it)"
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
    /// composer sends it (Task 3). The chat task echoes the same message back
    /// through [`Event::AiChat`]; [`Self::apply_chat`] drops that echo when it
    /// matches the last entry.
    pub fn push_chat_user(&mut self, text: String) {
        self.timeline.push(TimelineEntry::ChatUser { text });
        self.chat_reply_open = false;
    }

    /// Fold one chat-stream event (spec §6.5, Task 7).
    fn apply_chat(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::User { text } => {
                let already = matches!(
                    self.timeline.last(),
                    Some(TimelineEntry::ChatUser { text: last }) if last == text
                );
                if !already {
                    self.timeline
                        .push(TimelineEntry::ChatUser { text: text.clone() });
                }
                self.chat_reply_open = false;
            }
            ChatEvent::Text { delta } => {
                if self.chat_reply_open
                    && let Some(TimelineEntry::ChatReply { text }) = self.timeline.last_mut()
                {
                    text.push_str(delta);
                } else {
                    self.timeline.push(TimelineEntry::ChatReply {
                        text: delta.clone(),
                    });
                    self.chat_reply_open = true;
                }
            }
            ChatEvent::ToolCall { name, summary } => {
                self.timeline.push(TimelineEntry::ChatTool {
                    name: name.clone(),
                    summary: summary.clone(),
                    note: None,
                    decision: None,
                });
                // Later text is a new bubble, not a continuation.
                self.chat_reply_open = false;
            }
            ChatEvent::TurnDone => self.chat_reply_open = false,
        }
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

    /// Every finding the diff shows inline: open drafts, plus the verifier's
    /// drops while "show dropped" is on (spec §12, Task 5).
    pub fn inline_drafts(&self) -> impl Iterator<Item = &Finding> {
        let show_dropped = self.show_dropped;
        self.findings.values().filter(move |f| {
            f.state == FindingState::Draft
                || (show_dropped && f.state == FindingState::DroppedByVerifier)
        })
    }

    /// The summary-anchored findings the AI tab lists with Keep / Drop
    /// (spec §5.6): open drafts, plus the verifier's drops while
    /// `show_dropped` is on. These never reach the diff — `place_ai_draft`
    /// has no row to anchor a whole-file remark to — so the tab is their
    /// only Keep / Drop surface.
    pub fn summary_drafts(&self, show_dropped: bool) -> Vec<&Finding> {
        self.findings
            .values()
            .filter(move |f| {
                matches!(f.anchor, FindingAnchor::Summary { .. })
                    && (f.state == FindingState::Draft
                        || (show_dropped && f.state == FindingState::DroppedByVerifier))
            })
            .collect()
    }

    /// Open the inline editor of finding `id`, seeded with `body`.
    pub fn start_edit(&mut self, id: i64, body: String) {
        self.edit = Some((id, body));
    }

    /// The editor's buffer, while one is open.
    pub fn edit_buffer_mut(&mut self) -> Option<&mut String> {
        self.edit.as_mut().map(|(_, buffer)| buffer)
    }

    /// Whether the inline editor of finding `id` is open.
    pub fn editing(&self, id: i64) -> bool {
        matches!(&self.edit, Some((open, _)) if *open == id)
    }

    /// Close the editor, discarding its buffer.
    pub fn cancel_edit(&mut self) {
        self.edit = None;
    }

    /// Close the editor, writing its buffer back to the finding. Returns the
    /// id and text when there was anything to save, for the caller to
    /// persist (Task 5's `Command::AiEditFinding`).
    pub fn save_edit(&mut self) -> Option<(i64, String)> {
        let (id, text) = self.edit.take()?;
        let finding = self.findings.get_mut(&id)?;
        finding.body = text.clone();
        Some((id, text))
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
        // The transcript has no stop rows: with nothing running behind the
        // snapshot, every agent in it has ended.
        if !matches!(self.phase, AiPhase::Running | AiPhase::Paused) {
            for entry in &mut self.timeline {
                if let TimelineEntry::Agent {
                    stop: stop @ None, ..
                } = entry
                {
                    *stop = Some("finished".to_string());
                }
            }
        }
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
        self.brief = snap
            .brief
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok());
        // The snapshot replays the latest run's draft and usage (spec §10.3):
        // a run that finished while the user was on another PR loses its
        // `DraftReady` to the stale-event rule, and this is the second path
        // that lands the assembler's draft and the token tally. A newer run
        // that has not assembled yet carries neither, so the rebuild starts
        // those fresh.
        self.draft = snap.draft.clone();
        self.usage = snap.usage;
        self.restore_decision = None;
        self.edit = None;
        self.chat_reply_open = false;
        self.decided.clear();
    }

    /// Fold one forwarded pipeline event in.
    fn apply_pipeline(&mut self, ev: &PipelineEvent) {
        match ev {
            PipelineEvent::StagePlanned { stage, agents } => {
                match self.planned.iter_mut().find(|(seen, _)| seen == stage) {
                    Some((_, n)) => *n = *agents,
                    None => self.planned.push((*stage, *agents)),
                }
            }
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
                    tools.push(ToolLine {
                        name: name.clone(),
                        summary: summary.clone(),
                        ..ToolLine::default()
                    });
                }
            }
            PipelineEvent::CommandDecided {
                agent,
                decision,
                note,
            } => {
                // The decision is announced while the tool call is being
                // settled, so it lands on the tool line that was added last.
                // The chat agent's bash calls arrive here too (the tool
                // context is the run's); they mark the chat's tool line.
                if agent.role == AgentRole::Chat {
                    let found = self.timeline.iter().rposition(|entry| {
                        matches!(entry, TimelineEntry::ChatTool { name, .. } if name == "bash")
                    });
                    if let Some(index) = found
                        && let TimelineEntry::ChatTool {
                            note: note_slot,
                            decision: decision_slot,
                            ..
                        } = &mut self.timeline[index]
                    {
                        *note_slot = *note;
                        *decision_slot = Some(decision.clone());
                        self.note_decided(CommandRef {
                            entry: index,
                            tool: None,
                        });
                    }
                } else {
                    let index = self.ensure_agent_entry(agent);
                    if let TimelineEntry::Agent { tools, .. } = &mut self.timeline[index]
                        && let Some(last) = tools.last_mut()
                        && last.name == "bash"
                    {
                        last.note = *note;
                        last.decision = Some(decision.clone());
                        let tool = tools.len() - 1;
                        self.note_decided(CommandRef {
                            entry: index,
                            tool: Some(tool),
                        });
                    }
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
            // The full finding and coverage rows arrive with the
            // [`Event::AiFindings`] that follows; the card already exists
            // from `FindingAdded`.
            PipelineEvent::FindingChanged { .. } | PipelineEvent::Coverage { .. } => {}
            // Applied by the PR session, which owns the pending review.
            PipelineEvent::TriageRequested { .. } => {}
            PipelineEvent::BriefReady { brief } => self.brief = Some((**brief).clone()),
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

    /// Record that the line at `at` was decided (once: a re-announced
    /// decision moves it to the newest place).
    fn note_decided(&mut self, at: CommandRef) {
        self.decided.retain(|seen| *seen != at);
        self.decided.push(at);
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

/// What the tab says when a run row claims to be running and nothing is.
const INTERRUPTED: &str = "the run was interrupted";

/// The phase a snapshot describes (spec §10.3).
///
/// The run row says what happened; the hub's liveness says whether anything is
/// still running behind it. A `running` row with nothing live is a run that
/// died (the app quit mid-run, the pipeline task was lost, a finish parked the
/// run awaiting a restore decision), never a spinner with a Cancel button that
/// can only answer "no AI review is running".
fn snapshot_phase(snap: &AiSnapshotData) -> AiPhase {
    if snap.run_liveness == RunLiveness::Starting {
        return AiPhase::CheckingOut;
    }
    let Some(run) = snap.run.as_ref() else {
        return AiPhase::Idle;
    };
    match run.status.as_str() {
        "running" if snap.run_liveness == RunLiveness::Live => AiPhase::Running,
        "running" => AiPhase::Failed(INTERRUPTED.to_string()),
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

/// Rebuild the timeline from the stored transcript (spec §10.3), so a
/// reopened tab reads like the live one: assistant turns contribute their
/// text and tool calls to a per-agent entry (reviewers and challengers
/// under their group, as `AgentStarted` names them), and the chat's rows
/// become chat bubbles and tool lines, never an agent row. Tool inputs are
/// cut like [`PipelineEvent::ToolCall`]'s, so result tools stay whole for
/// their cards. Other user rows are task prompts and tool results.
fn timeline_from_transcript(rows: &[AiTimelineEntry]) -> Vec<TimelineEntry> {
    let mut out: Vec<TimelineEntry> = Vec::new();
    for row in rows {
        let Ok(blocks) = serde_json::from_str::<Vec<serde_json::Value>>(&row.content_json) else {
            continue;
        };
        let kind = |block: &serde_json::Value| {
            block
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let block_text = |block: &serde_json::Value| {
            block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let tool_line = |block: &serde_json::Value| {
            let name = block
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let summary = block
                .get("input")
                .map(|input| crate::ai::agent::tool_call_summary(&name, input))
                .unwrap_or_default();
            (name, summary)
        };
        if row.agent_id == crate::ai::chat::CHAT_LABEL {
            for block in &blocks {
                match (row.role.as_str(), kind(block).as_deref()) {
                    ("user", Some("text")) => {
                        let text = strip_chat_digest(&block_text(block));
                        if !text.is_empty() {
                            out.push(TimelineEntry::ChatUser { text });
                        }
                    }
                    ("assistant", Some("text")) => {
                        let text = block_text(block);
                        if text.trim().is_empty() {
                            continue;
                        }
                        match out.last_mut() {
                            Some(TimelineEntry::ChatReply { text: last }) => last.push_str(&text),
                            _ => out.push(TimelineEntry::ChatReply { text }),
                        }
                    }
                    ("assistant", Some("tool_use")) => {
                        let (name, summary) = tool_line(block);
                        out.push(TimelineEntry::ChatTool {
                            name,
                            summary,
                            note: None,
                            decision: None,
                        });
                    }
                    _ => {}
                }
            }
            continue;
        }
        if row.role != "assistant" {
            continue;
        }
        let mut text = String::new();
        let mut tools: Vec<ToolLine> = Vec::new();
        for block in &blocks {
            match kind(block).as_deref() {
                Some("text") => text.push_str(&block_text(block)),
                Some("tool_use") => {
                    let (name, summary) = tool_line(block);
                    tools.push(ToolLine {
                        name,
                        summary,
                        ..ToolLine::default()
                    });
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
                let agent = agent_ref_from_label(&row.agent_id);
                let group = match agent.role {
                    AgentRole::Reviewer | AgentRole::Challenger => row
                        .agent_id
                        .split_once(':')
                        .map(|(_, group)| group.to_string()),
                    _ => None,
                };
                out.push(TimelineEntry::Agent {
                    agent,
                    group,
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

/// A stored chat user turn without the status digest the app put before it
/// ([`crate::ai::tools::steer::SteerCtx::digest`], "## Run status" up to its
/// "open questions" line).
fn strip_chat_digest(stored: &str) -> String {
    if !stored.starts_with("## Run status") {
        return stored.trim().to_string();
    }
    match stored.find("\nopen questions:") {
        Some(at) => {
            let rest = &stored[at + 1..];
            rest.split_once('\n')
                .map_or("", |(_, after)| after)
                .trim()
                .to_string()
        }
        None => String::new(),
    }
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

/// Which stage's progress an agent role counts toward.
pub(crate) fn agent_stage(role: AgentRole) -> Option<StageKind> {
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
pub(crate) fn stage_order(stage: StageKind) -> u8 {
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

/// The chat question a draft's **Why?** asks about `f` (spec §12, Task 5).
pub fn why_question(f: &Finding) -> String {
    match &f.anchor {
        FindingAnchor::Line(anchor) => format!(
            "Why did you flag #{} ({}:{})?",
            f.id, anchor.path, anchor.line
        ),
        FindingAnchor::Summary { path } => format!("Why did you flag #{} ({})?", f.id, path),
    }
}

/// Why the repository's local folder cannot be changed or unlinked right
/// now, or `None` when it can: a live run, or a checkout of this repo still
/// awaiting its restore, depends on the path the link holds (spec: AI
/// settings — Unlink / Change folder is blocked while a review or a
/// checkout for that repo is active or unrestored).
///
/// `unrestored` is the app-wide recovery list ([`Event::AiUnrestored`]'s
/// records); the open session's own held checkout rides in `ai_state`.
/// Records name their repository by the `owner/repo` slug, matched
/// case-insensitively against the `host/owner/name` `repo_key`.
pub fn change_blocked(
    ai_state: &AiReviewState,
    unrestored: &[CheckoutRecord],
    repo_key: &str,
) -> Option<String> {
    change_blocked_for(ai_state, unrestored, repo_key, None)
}

/// The note shown while the open PR's own checkout holds the folder.
pub const HELD_BY_THIS_PR: &str =
    "The folder is on this pull request's checkout; Finish puts your branch back.";

/// [`change_blocked`] seen from PR `pr_number`: a checkout that PR itself
/// holds still blocks a folder change (its restore needs the folder), but
/// is named as this PR's own, not as a stray to restore.
pub fn change_blocked_for(
    ai_state: &AiReviewState,
    unrestored: &[CheckoutRecord],
    repo_key: &str,
    pr_number: Option<u64>,
) -> Option<String> {
    if let Some(reason) = start_blocked(ai_state, unrestored, repo_key, pr_number) {
        return Some(reason);
    }
    ai_state
        .checkout
        .iter()
        .chain(unrestored)
        .any(|record| unrestored_holds_repo(record, repo_key))
        .then(|| HELD_BY_THIS_PR.to_string())
}

/// What stops a review of PR `pr_number` from starting: a live run, or an
/// unrestored checkout of a *different* PR in the repo. The PR's own held
/// checkout does not: `plan_checkout` reuses it (a re-review after Keep or
/// a restart). With `pr_number: None` every record counts.
pub fn start_blocked(
    ai_state: &AiReviewState,
    unrestored: &[CheckoutRecord],
    repo_key: &str,
    pr_number: Option<u64>,
) -> Option<String> {
    if matches!(
        ai_state.phase,
        AiPhase::Planning
            | AiPhase::CheckingOut
            | AiPhase::Running
            | AiPhase::Paused
            | AiPhase::Restoring
    ) {
        return Some(
            "An AI review is running for this repository — finish it before changing the folder."
                .to_string(),
        );
    }
    ai_state
        .checkout
        .iter()
        .chain(unrestored)
        .filter(|record| pr_number != Some(record.pr_number))
        .any(|record| unrestored_holds_repo(record, repo_key))
        .then(|| {
            "This repository has an unrestored AI checkout — restore it before changing the folder or starting a review."
                .to_string()
        })
}

/// The checkout records for `repo_key` that await a restore and can be
/// restored right now: the rows behind [`change_blocked`]'s unrestored
/// message, minus nothing — with no live run they are all restorable, and a
/// live run reports no records (the dialog would fight the run).
pub fn unrestored_for_repo(
    ai_state: &AiReviewState,
    unrestored: &[CheckoutRecord],
    repo_key: &str,
) -> Vec<CheckoutRecord> {
    if matches!(
        ai_state.phase,
        AiPhase::Planning
            | AiPhase::CheckingOut
            | AiPhase::Running
            | AiPhase::Paused
            | AiPhase::Restoring
    ) {
        return Vec::new();
    }
    let records: Vec<CheckoutRecord> = ai_state
        .checkout
        .iter()
        .chain(unrestored)
        .filter(|record| unrestored_holds_repo(record, repo_key))
        .cloned()
        .collect();
    // The session's own row is also in the app-wide list once the hub
    // reports it; keep the ids distinct.
    let mut seen: Vec<i64> = Vec::with_capacity(records.len());
    records
        .into_iter()
        .filter(|record| {
            let fresh = !seen.contains(&record.id);
            seen.push(record.id);
            fresh
        })
        .collect()
}

/// Whether `record` is a checkout of `repo_key` that still awaits its
/// restore. `Restored` and `Superseded` rows are done with the folder.
fn unrestored_holds_repo(record: &CheckoutRecord, repo_key: &str) -> bool {
    if !matches!(
        record.state,
        CheckoutState::Started | CheckoutState::Active | CheckoutState::Failed
    ) {
        return false;
    }
    let slug = repo_key.split_once('/').map_or(repo_key, |(_, slug)| slug);
    record.repo.eq_ignore_ascii_case(slug)
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
                tools: vec![ToolLine {
                    name: "read_file".to_string(),
                    summary: "src/a.rs".to_string(),
                    note: None,
                    decision: None,
                }],
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

    /// The classifier's verdict badges the bash tool line it was consulted
    /// for — the agent's own line, or the chat's — and nothing else: lines
    /// the rules settled alone, and non-bash tools, stay bare.
    /// Decisions are remembered in the order they landed, for the command
    /// log; a snapshot (whose timeline carries no decisions) clears them.
    #[test]
    fn decided_lines_are_recorded_in_order() {
        let mut st = AiReviewState::default();
        let bash = |agent: AgentRef, command: &str| {
            pipeline(PipelineEvent::ToolCall {
                agent,
                name: "bash".to_string(),
                summary: format!("{{\"command\":\"{command}\"}}"),
            })
        };
        let decided = |agent: AgentRef| {
            pipeline(PipelineEvent::CommandDecided {
                agent,
                decision: CommandDecision::Classifier,
                note: None,
            })
        };
        st.apply(&bash(reviewer(), "ls"));
        st.apply(&decided(reviewer()));
        st.apply(&Event::AiChat {
            session: SESSION,
            event: ChatEvent::ToolCall {
                name: "bash".to_string(),
                summary: "{\"command\":\"pwd\"}".to_string(),
            },
        });
        st.apply(&decided(crate::ai::chat::chat_agent()));
        st.apply(&bash(reviewer(), "wc -l a"));
        st.apply(&decided(reviewer()));
        assert_eq!(
            st.decided,
            vec![
                CommandRef {
                    entry: 0,
                    tool: Some(0)
                },
                CommandRef {
                    entry: 1,
                    tool: None
                },
                CommandRef {
                    entry: 0,
                    tool: Some(1)
                },
            ]
        );
        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot: AiSnapshotData {
                findings: Vec::new(),
                coverage: Vec::new(),
                timeline: Vec::new(),
                brief: None,
                run: None,
                run_liveness: RunLiveness::Stopped,
                checkout: None,
                bash_disabled: None,
                last_reviewed_sha: None,
                draft: None,
                usage: (0, 0),
            },
        });
        assert!(st.decided.is_empty());
    }

    #[test]
    fn command_decisions_mark_the_last_bash_line() {
        use review_approval::model_stage::ClassifierNote;

        let mut st = AiReviewState::default();
        st.apply(&pipeline(PipelineEvent::ToolCall {
            agent: reviewer(),
            name: "bash".to_string(),
            summary: "{\"command\":\"rm x\"}".to_string(),
        }));
        st.apply(&pipeline(PipelineEvent::CommandDecided {
            agent: reviewer(),
            decision: CommandDecision::AutoDenied {
                reason: "rm x: not in the allowed commands".to_string(),
            },
            note: Some(ClassifierNote::NeedsApproval),
        }));
        st.apply(&pipeline(PipelineEvent::ToolCall {
            agent: reviewer(),
            name: "read_file".to_string(),
            summary: "src/a.rs".to_string(),
        }));
        st.apply(&pipeline(PipelineEvent::CommandDecided {
            agent: reviewer(),
            decision: CommandDecision::Classifier,
            note: Some(ClassifierNote::Dangerous),
        }));
        let TimelineEntry::Agent { tools, .. } = &st.timeline[0] else {
            panic!("agent entry");
        };
        assert_eq!(tools[0].note, Some(ClassifierNote::NeedsApproval));
        assert!(matches!(
            tools[0].decision,
            Some(CommandDecision::AutoDenied { .. })
        ));
        assert_eq!(
            (&tools[1].note, &tools[1].decision),
            (&None, &None),
            "the read_file line gets no badge: bash is the only tool that is decided"
        );

        // The chat agent's bash call badges the chat's tool line, not a
        // phantom agent entry.
        let mut st = AiReviewState::default();
        st.apply(&Event::AiChat {
            session: SESSION,
            event: crate::ai::hub::ChatEvent::ToolCall {
                name: "bash".to_string(),
                summary: "{\"command\":\"ls\"}".to_string(),
            },
        });
        st.apply(&pipeline(PipelineEvent::CommandDecided {
            agent: crate::ai::chat::chat_agent(),
            decision: CommandDecision::Classifier,
            note: Some(ClassifierNote::Approved),
        }));
        let TimelineEntry::ChatTool { note, decision, .. } = &st.timeline[0] else {
            panic!("chat tool entry, got {:?}", st.timeline);
        };
        assert_eq!(*note, Some(ClassifierNote::Approved));
        assert_eq!(*decision, Some(CommandDecision::Classifier));
        assert_eq!(st.timeline.len(), 1, "no phantom agent entry");
    }

    /// The chat-stream fold (Task 7): the optimistic user entry absorbs its
    /// echo, text deltas coalesce into one reply bubble, a tool call closes
    /// the bubble so later text starts a new one, and `TurnDone` closes it
    /// too.
    #[test]
    fn chat_stream_folds_into_the_timeline() {
        let chat = |event: ChatEvent| Event::AiChat {
            session: SESSION,
            event,
        };
        let mut st = AiReviewState::default();
        st.push_chat_user("why is this flagged?".to_string());
        // The chat task echoes the same message: no second entry.
        st.apply(&chat(ChatEvent::User {
            text: "why is this flagged?".to_string(),
        }));
        assert_eq!(
            st.timeline,
            vec![TimelineEntry::ChatUser {
                text: "why is this flagged?".to_string()
            }],
            "the echo reconciles with the optimistic entry"
        );

        // A message with no optimistic twin (the hub echoed one the composer
        // did not push) is appended.
        st.apply(&chat(ChatEvent::User {
            text: "look at the migration again".to_string(),
        }));

        st.apply(&chat(ChatEvent::Text {
            delta: "Because ".to_string(),
        }));
        st.apply(&chat(ChatEvent::Text {
            delta: "the guard moved.".to_string(),
        }));
        st.apply(&chat(ChatEvent::ToolCall {
            name: "get_finding".to_string(),
            summary: "{\"id\":3}".to_string(),
        }));
        st.apply(&chat(ChatEvent::Text {
            delta: "See finding #3.".to_string(),
        }));
        st.apply(&chat(ChatEvent::TurnDone));
        st.apply(&chat(ChatEvent::Text {
            delta: "a stray delta".to_string(),
        }));

        assert_eq!(
            st.timeline,
            vec![
                TimelineEntry::ChatUser {
                    text: "why is this flagged?".to_string()
                },
                TimelineEntry::ChatUser {
                    text: "look at the migration again".to_string()
                },
                TimelineEntry::ChatReply {
                    text: "Because the guard moved.".to_string()
                },
                TimelineEntry::ChatTool {
                    name: "get_finding".to_string(),
                    summary: "{\"id\":3}".to_string(),
                    note: None,
                    decision: None,
                },
                TimelineEntry::ChatReply {
                    text: "See finding #3.".to_string()
                },
                TimelineEntry::ChatReply {
                    text: "a stray delta".to_string()
                },
            ],
            "deltas coalesce; a tool call or TurnDone closes the bubble"
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

    /// The diff's inline rows follow the "show dropped" toggle: open drafts
    /// always, verifier drops only when it is on (spec §12).
    #[test]
    fn inline_drafts_shows_open_drafts_and_drops_only_when_asked() {
        let mut st = AiReviewState::default();
        for f in [
            line_finding(1, Severity::Bug, FindingState::Draft, "a.rs"),
            line_finding(2, Severity::Risk, FindingState::DroppedByVerifier, "a.rs"),
            line_finding(3, Severity::Nit, FindingState::Kept, "a.rs"),
        ] {
            st.findings.insert(f.id, f);
        }
        let ids = |st: &AiReviewState| st.inline_drafts().map(|f| f.id).collect::<Vec<_>>();
        assert_eq!(ids(&st), vec![1], "only the open draft is inline");
        st.show_dropped = true;
        assert_eq!(
            ids(&st),
            vec![1, 2],
            "the verifier drop joins it when asked"
        );
    }

    /// The AI tab's summary list is the Keep / Drop surface for findings the
    /// diff cannot anchor: open summary drafts always, verifier drops only
    /// with the toggle, and line-anchored or user-dropped ones never (spec
    /// §5.6).
    #[test]
    fn summary_drafts_lists_only_summary_anchored_items() {
        let summary = |id: i64, state: FindingState| Finding {
            anchor: crate::ai::review::FindingAnchor::Summary {
                path: format!("logo{id}.png"),
            },
            ..line_finding(id, Severity::Nit, state, "logo.png")
        };
        let mut st = AiReviewState::default();
        for f in [
            summary(1, FindingState::Draft),
            summary(2, FindingState::DroppedByVerifier),
            summary(3, FindingState::Kept),
            summary(4, FindingState::DroppedByUser),
            line_finding(5, Severity::Bug, FindingState::Draft, "a.rs"),
            line_finding(6, Severity::Risk, FindingState::DroppedByVerifier, "a.rs"),
        ] {
            st.findings.insert(f.id, f);
        }
        let ids = |st: &AiReviewState, show| {
            st.summary_drafts(show)
                .into_iter()
                .map(|f| f.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&st, false), vec![1], "only the open summary draft");
        st.show_dropped = true;
        assert_eq!(
            ids(&st, true),
            vec![1, 2],
            "the verifier drop joins with the toggle; kept, user-dropped and line-anchored ones never"
        );
    }

    /// A reopened tab reads like the live one: the chat's rows become
    /// bubbles and tool lines without the status digest, reviewers carry
    /// their group, and result tools keep their whole input for the cards.
    #[test]
    fn the_transcript_rebuilds_chat_groups_and_results() {
        let row = |agent: &str, role: &str, content: serde_json::Value| AiTimelineEntry {
            agent_id: agent.into(),
            role: role.into(),
            content_json: content.to_string(),
        };
        let long = "x".repeat(400);
        let rows = vec![
            row(
                "reviewer:g2",
                "assistant",
                serde_json::json!([
                    {"type": "tool_use", "id": "1", "name": "done", "input": {"summary": long}}
                ]),
            ),
            row(
                "chat",
                "user",
                serde_json::json!([
                    {"type": "text", "text": "## Run status\n\nstate: finished\nopen questions: none\n\n\nbring #97 back"}
                ]),
            ),
            row(
                "chat",
                "assistant",
                serde_json::json!([
                    {"type": "tool_use", "id": "2", "name": "list_findings", "input": {}},
                    {"type": "text", "text": "Done."}
                ]),
            ),
        ];
        let timeline = timeline_from_transcript(&rows);
        assert!(
            matches!(
                &timeline[0],
                TimelineEntry::Agent { group: Some(g), tools, .. }
                    if g == "g2" && tools[0].summary.contains(&long)
            ),
            "{timeline:?}"
        );
        assert_eq!(
            timeline[1..],
            [
                TimelineEntry::ChatUser {
                    text: "bring #97 back".into()
                },
                TimelineEntry::ChatTool {
                    name: "list_findings".into(),
                    summary: "{}".into(),
                    note: None,
                    decision: None,
                },
                TimelineEntry::ChatReply {
                    text: "Done.".into()
                },
            ]
        );
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
        use crate::ai::stages::assemble::{ReviewDraft, Verdict};

        let mut st = AiReviewState::default();
        st.timeline.push(TimelineEntry::Notice {
            text: "stale".to_string(),
        });
        st.findings.insert(
            99,
            line_finding(99, Severity::Bug, FindingState::Draft, "z.rs"),
        );
        st.chat_input = "half-typed".to_string();
        st.draft = Some(ReviewDraft {
            summary: "the old run's draft".to_string(),
            verdict: Verdict::Comment,
            rationale: String::new(),
            rewrites: Default::default(),
        });
        st.usage = (1, 1);

        let finding = line_finding(1, Severity::Risk, FindingState::Kept, "a.rs");
        let draft = ReviewDraft {
            summary: "adds the retry loop".to_string(),
            verdict: Verdict::RequestChanges,
            rationale: "a kept bug blocks the merge".to_string(),
            rewrites: Default::default(),
        };
        let snapshot = AiSnapshotData {
            findings: vec![finding.clone()],
            coverage: vec![coverage("a.rs", CheckResult::Ok)],
            timeline: vec![assistant("scout", r#""scouting done""#)],
            brief: None,
            run: Some(run("completed", None)),
            run_liveness: RunLiveness::Stopped,
            checkout: Some(checkout_record()),
            bash_disabled: Some("no bwrap".to_string()),
            last_reviewed_sha: Some("sha_old".to_string()),
            draft: Some(draft.clone()),
            usage: (4200, 310),
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
            st.draft.as_ref(),
            Some(&draft),
            "the finished run's draft replays instead of vanishing"
        );
        assert_eq!(
            st.usage,
            (4200, 310),
            "the token tally rides the snapshot too"
        );
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
                    stop: Some("finished".to_string()),
                },
                TimelineEntry::Notice {
                    text: "the agent shell is unsandboxed: no bwrap".to_string(),
                },
            ],
            "the transcript is rebuilt and the stale notice is gone"
        );
        assert!(st.can_continue("sha_new"), "a completed run can continue");
    }

    /// A snapshot whose newest run has not assembled yet (a fresh run that
    /// was started and re-attached to) carries no draft: the rebuild starts
    /// the draft and usage fresh, as before.
    #[test]
    fn snapshot_without_a_draft_clears_the_old_one() {
        use crate::ai::stages::assemble::{ReviewDraft, Verdict};

        let mut st = AiReviewState {
            draft: Some(ReviewDraft {
                summary: "the old run's draft".to_string(),
                verdict: Verdict::Comment,
                rationale: String::new(),
                rewrites: Default::default(),
            }),
            usage: (7, 7),
            ..AiReviewState::default()
        };
        let snapshot = AiSnapshotData {
            findings: Vec::new(),
            coverage: Vec::new(),
            timeline: Vec::new(),
            brief: None,
            run: Some(run("running", None)),
            run_liveness: RunLiveness::Live,
            checkout: Some(checkout_record()),
            bash_disabled: None,
            last_reviewed_sha: None,
            draft: None,
            usage: (0, 0),
        };
        st.apply(&Event::AiSnapshot {
            session: SESSION,
            snapshot,
        });
        assert_eq!(st.draft, None);
        assert_eq!(st.usage, (0, 0));
    }

    /// The phase comes from the row *and* the hub's liveness. A `running` row
    /// with nothing behind it (the app quit mid-run, the pipeline task was
    /// lost, a finish parked the run awaiting a decision) must not show a
    /// spinner with a Cancel button that can only answer "nothing is running".
    #[test]
    fn snapshot_phase_needs_a_live_run_for_a_running_row() {
        fn snapshot(
            run: Option<RunRecord>,
            run_liveness: RunLiveness,
            checkout: Option<CheckoutRecord>,
        ) -> AiSnapshotData {
            AiSnapshotData {
                findings: Vec::new(),
                coverage: Vec::new(),
                timeline: Vec::new(),
                brief: None,
                run,
                run_liveness,
                checkout,
                bash_disabled: None,
                last_reviewed_sha: None,
                draft: None,
                usage: (0, 0),
            }
        }
        let phase = |snap: &AiSnapshotData| {
            let mut st = AiReviewState::default();
            st.apply(&Event::AiSnapshot {
                session: SESSION,
                snapshot: snap.clone(),
            });
            st.phase
        };

        assert_eq!(
            phase(&snapshot(
                Some(run("running", None)),
                RunLiveness::Live,
                Some(checkout_record())
            )),
            AiPhase::Running
        );
        assert_eq!(
            phase(&snapshot(
                Some(run("running", None)),
                RunLiveness::Stopped,
                Some(checkout_record())
            )),
            AiPhase::Failed(INTERRUPTED.to_string()),
            "a dead run must offer Start again"
        );
        assert_eq!(
            phase(&snapshot(
                Some(run("running", None)),
                RunLiveness::Stopped,
                None
            )),
            AiPhase::Failed(INTERRUPTED.to_string())
        );
        // A held checkout with no run row is a blocker, not an endless
        // spinner: `start` installs its handle before it touches the tree.
        assert_eq!(
            phase(&snapshot(
                None,
                RunLiveness::Stopped,
                Some(checkout_record())
            )),
            AiPhase::Idle
        );
        // A start in flight reads as checkout even while the previous run's
        // row still says completed.
        assert_eq!(
            phase(&snapshot(
                Some(run("completed", None)),
                RunLiveness::Starting,
                Some(checkout_record())
            )),
            AiPhase::CheckingOut
        );
        assert_eq!(
            phase(&snapshot(None, RunLiveness::Starting, None)),
            AiPhase::CheckingOut
        );
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
                skip_stash: false,
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
            restore: Some(4),
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
                run_liveness: RunLiveness::Stopped,
                checkout: Some(checkout_record()),
                bash_disabled: None,
                last_reviewed_sha: None,
                draft: None,
                usage: (0, 0),
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
    fn findings_event_replaces_rows_and_coverage() {
        let mut st = AiReviewState::default();
        st.apply(&pipeline(PipelineEvent::FindingAdded { id: 1 }));
        assert!(st.findings.is_empty(), "the id event carries no row");
        st.apply(&Event::AiFindings {
            session: SESSION,
            findings: vec![line_finding(1, Severity::Bug, FindingState::Draft, "a.rs")],
            coverage: vec![coverage("a.rs", CheckResult::Ok)],
        });
        assert_eq!(st.findings.len(), 1);
        assert_eq!(st.findings[&1].title, "finding 1");
        assert_eq!(st.coverage.len(), 1);

        // A later update is the whole truth, not a merge.
        st.apply(&Event::AiFindings {
            session: SESSION,
            findings: vec![line_finding(2, Severity::Nit, FindingState::Draft, "b.rs")],
            coverage: Vec::new(),
        });
        assert_eq!(st.findings.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(st.coverage.is_empty());
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

    /// A stash that did not apply cleanly leaves the user's work in the
    /// stash: the Notice names the SHA, the exact apply command, and where
    /// the entry lives until it is dropped (spec §7.3, Task 8).
    #[test]
    fn stash_conflict_notice_carries_the_apply_command() {
        let mut st = AiReviewState {
            phase: AiPhase::Restoring,
            ..AiReviewState::default()
        };
        st.apply(&Event::AiRestored {
            session: SESSION,
            outcome: RestoreOutcome::StashConflict {
                stash_sha: "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567".to_string(),
            },
        });
        let Some(TimelineEntry::Notice { text }) = st.timeline.last() else {
            panic!("the conflict lands on the timeline as a Notice: {st:?}");
        };
        assert!(
            text.contains("0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"),
            "{text}"
        );
        assert!(
            text.contains("`git stash apply 0a1b2c3d4e5f60718293a4b5c6d7e8f901234567`"),
            "the exact command to apply it later: {text}"
        );
        assert!(
            text.contains("git stash list"),
            "where the entry stays: {text}"
        );
        assert_eq!(st.phase, AiPhase::Finished);
    }

    /// The inline editor (Task 5): the row seeds it with the finding's body,
    /// saving writes the text back to the finding, and cancelling keeps the
    /// old one.
    #[test]
    fn edit_reducers_seed_save_and_cancel() {
        let mut st = AiReviewState::default();
        let finding = line_finding(1, Severity::Bug, FindingState::Draft, "a.rs");
        let body = finding.body.clone();
        st.findings.insert(1, finding);

        st.start_edit(1, body.clone());
        assert!(st.editing(1));
        assert!(!st.editing(2));
        st.edit_buffer_mut().unwrap().push_str(" (edited)");
        assert_eq!(st.save_edit(), Some((1, format!("{body} (edited)"))));
        assert_eq!(
            st.findings[&1].body,
            format!("{body} (edited)"),
            "the finding's body follows the editor"
        );
        assert!(st.edit.is_none(), "saving closes the editor");
        assert_eq!(st.save_edit(), None, "nothing left to save");

        st.start_edit(1, "scratch".to_string());
        st.cancel_edit();
        assert!(!st.editing(1));
        assert_eq!(
            st.findings[&1].body,
            format!("{body} (edited)"),
            "a cancelled edit changes nothing"
        );
    }

    /// The chat question a draft row's **Why?** asks (spec §12).
    #[test]
    fn why_question_names_the_finding_and_its_line() {
        let line = line_finding(7, Severity::Bug, FindingState::Draft, "src/a.rs");
        assert_eq!(why_question(&line), "Why did you flag #7 (src/a.rs:3)?");

        let summary = Finding {
            anchor: crate::ai::review::FindingAnchor::Summary {
                path: "logo.png".to_string(),
            },
            ..line_finding(8, Severity::Nit, FindingState::Draft, "logo.png")
        };
        assert_eq!(
            why_question(&summary),
            "Why did you flag #8 (logo.png)?",
            "a summary anchor has no line to name"
        );
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

    /// Changing or unlinking the repo's folder is blocked while a run for
    /// it is live or a checkout of it awaits its restore — the restore
    /// logic depends on the path (spec: AI settings, Unlink / Change).
    /// The PR's own held checkout never blocks its review from starting
    /// and reads as a quiet note; another PR's still blocks both.
    #[test]
    fn the_prs_own_checkout_does_not_block_its_review() {
        let st = AiReviewState::default();
        let held = [checkout_record()];
        let key = "github.com/o/r";
        assert_eq!(start_blocked(&st, &held, key, Some(7)), None);
        assert_eq!(
            change_blocked_for(&st, &held, key, Some(7)).as_deref(),
            Some(HELD_BY_THIS_PR),
            "a folder change still waits for the restore"
        );
        assert!(start_blocked(&st, &held, key, Some(8)).is_some());
        assert_ne!(
            change_blocked_for(&st, &held, key, Some(8)).as_deref(),
            Some(HELD_BY_THIS_PR)
        );
    }

    #[test]
    fn change_blocked_while_a_run_is_live_or_unrestored() {
        let key = "github.com/acme/widgets";
        let record = || CheckoutRecord {
            repo: "acme/widgets".to_string(),
            ..checkout_record()
        };

        // Idle with nothing held: the link can move.
        let idle = AiReviewState::default();
        assert_eq!(change_blocked(&idle, &[], key), None);

        // Every live phase blocks, whatever else is or is not held.
        for phase in [
            AiPhase::Planning,
            AiPhase::CheckingOut,
            AiPhase::Running,
            AiPhase::Paused,
            AiPhase::Restoring,
        ] {
            let st = AiReviewState {
                phase: phase.clone(),
                ..AiReviewState::default()
            };
            let reason = change_blocked(&st, &[], key).expect("a live run blocks the change");
            assert!(reason.contains("running"), "{phase:?}: {reason}");
        }

        // A finished run that still holds the checkout blocks: the restore
        // needs the path it started with.
        let st = AiReviewState {
            phase: AiPhase::Done,
            checkout: Some(record()),
            ..AiReviewState::default()
        };
        let reason = change_blocked(&st, &[], key).expect("a held checkout blocks the change");
        assert!(reason.contains("unrestored"), "{reason}");

        // The app-wide recovery list blocks the same repo, matched
        // case-insensitively, and leaves every other repo free.
        let st = AiReviewState::default();
        assert!(change_blocked(&st, &[record()], "GitHub.com/Acme/Widgets").is_some());
        let other = CheckoutRecord {
            repo: "acme/api".to_string(),
            ..record()
        };
        assert_eq!(
            change_blocked(&st, &[other], key),
            None,
            "another repo's checkout is none of this row's business"
        );

        // A record already restored (or superseded) is not unrestored.
        for state in [CheckoutState::Restored, CheckoutState::Superseded] {
            let done = CheckoutRecord { state, ..record() };
            assert_eq!(change_blocked(&st, &[done], key), None, "{state:?}");
        }
    }

    /// The Restore… affordance lists exactly the records behind the
    /// unrestored blocker — none while a run is live, the session's held
    /// row, the app-wide rows, without duplicates — so the dialog it opens
    /// matches the message the row shows.
    #[test]
    fn unrestored_for_repo_lists_the_blocking_records() {
        let key = "github.com/acme/widgets";
        let record = |id: i64, repo: &str| CheckoutRecord {
            id,
            repo: repo.to_string(),
            ..checkout_record()
        };

        // Idle, nothing held.
        let idle = AiReviewState::default();
        assert!(unrestored_for_repo(&idle, &[], key).is_empty());

        // A live run: the dialog would fight it, so nothing is offered.
        for phase in [
            AiPhase::Running,
            AiPhase::Paused,
            AiPhase::Restoring,
            AiPhase::CheckingOut,
            AiPhase::Planning,
        ] {
            let st = AiReviewState {
                phase: phase.clone(),
                checkout: Some(record(1, "acme/widgets")),
                ..AiReviewState::default()
            };
            assert!(
                unrestored_for_repo(&st, &[record(2, "acme/widgets")], key).is_empty(),
                "{phase:?}"
            );
        }

        // The session's own held checkout after the run ended.
        let st = AiReviewState {
            phase: AiPhase::Done,
            checkout: Some(record(1, "acme/widgets")),
            ..AiReviewState::default()
        };
        let listed = unrestored_for_repo(&st, &[], key);
        assert_eq!(listed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1]);

        // App-wide rows, deduplicated by id, other repos left out.
        let listed = unrestored_for_repo(
            &st,
            &[
                record(1, "acme/widgets"),
                record(3, "Acme/Widgets"),
                record(4, "acme/api"),
            ],
            key,
        );
        assert_eq!(listed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
    }
}
