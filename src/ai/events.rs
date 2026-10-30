//! Events the review pipeline emits as it works (spec §6, §9.3).
//!
//! [`crate::ai::review::RunState`] emits the finding and coverage events,
//! [`crate::ai::agent::run_agent`] the transcript ones, and
//! [`crate::ai::pipeline`] the stage lifecycle, agent announcements, and
//! usage. The UI drains the channel once per frame, so a send failure means
//! the tab has detached and is ignored — the store already holds the data.

use crate::ai::stages::assemble::ReviewDraft;
use crate::ai::tools::AgentRef;

/// Which phase of the pipeline a [`PipelineEvent::Stage`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageKind {
    Scout,
    Review,
    Challenge,
    Verify,
    /// Re-checking the previous run's findings against the new head (Task 16).
    PriorCheck,
    Assemble,
}

/// Lifecycle of one pipeline stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageStatus {
    Started,
    Done,
    /// The stage failed; the payload says why. A failed *group* does not
    /// fail the `Review` stage — it is visible through
    /// [`crate::ai::pipeline::PipelineStatus`] instead.
    Failed(String),
}

/// How a whole run ended; also the string stored in `ai_runs.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

impl RunStatus {
    /// The stored and wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Completed => "completed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Failed => "failed",
            RunStatus::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// One thing that happened in a review run.
#[derive(Clone, Debug, PartialEq)]
pub enum PipelineEvent {
    /// A finding was recorded; `id` is its `ai_findings` row id.
    FindingAdded { id: i64 },
    /// A finding's state changed (kept, dropped, posted, …).
    FindingChanged { id: i64 },
    /// A coverage entry was recorded for `(file, item)`.
    Coverage { file: String, item: String },
    /// An agent streamed a text delta.
    AgentText { agent: AgentRef, delta: String },
    /// An agent called a tool; `summary` is the first 120 chars of its
    /// input, or all of it (to 16k chars) for a tool whose input is the
    /// agent's result (`done`, `submit_verdicts`, …), shown as a card.
    ToolCall {
        agent: AgentRef,
        name: String,
        summary: String,
    },
    /// How the agent's last `bash` call was decided, announced once it was:
    /// who let it run or why it did not. `note` is what the local classifier
    /// said, when it was consulted. The UI shows both on the tool line.
    CommandDecided {
        agent: AgentRef,
        decision: CommandDecision,
        note: Option<review_approval::model_stage::ClassifierNote>,
    },
    /// An agent asked the user a question; the UI answers through
    /// [`crate::ai::interaction::Interaction`].
    Question {
        agent: AgentRef,
        question: crate::ai::interaction::Question,
    },
    /// A command needs the user's approval; `id` pairs the eventual
    /// decision with the request.
    Approval {
        agent: AgentRef,
        id: u64,
        command: String,
        reason: String,
    },
    /// A pipeline stage changed state.
    Stage {
        stage: StageKind,
        status: StageStatus,
    },
    /// The chat asked the app to keep and drop these draft findings (spec
    /// §6.5): only the app can add a kept finding to the pending review,
    /// so the request is applied there, like the user's own Keep / Drop.
    TriageRequested { keep: Vec<i64>, drop: Vec<i64> },
    /// How many agents a stage is about to run, announced when it starts,
    /// so progress reads "done of all", not "done of started so far".
    StagePlanned { stage: StageKind, agents: u32 },
    /// The scout filed its brief (normalized, as stored); the groups are
    /// also in [`crate::ai::pipeline::PipelineStatus`] and the store.
    BriefReady {
        brief: Box<crate::ai::stages::scout::Brief>,
    },
    /// An agent started. Per-agent detail (text, tool calls) arrives on the
    /// transcript events under the same [`AgentRef`]; `group` is set for
    /// reviewers and challengers.
    AgentStarted {
        agent: AgentRef,
        group: Option<String>,
    },
    /// An agent finished; `stop` is why its loop ended
    /// (`finished|no_finish|cancelled|failed|max_turns`).
    AgentFinished { agent: AgentRef, stop: String },
    /// The tokens one agent spent, reported when it finishes.
    Usage {
        agent: AgentRef,
        input: u64,
        output: u64,
    },
    /// The assembler filed the draft review.
    DraftReady { draft: ReviewDraft },
    /// Terminal. The run row is already closed when this arrives.
    RunEnded {
        run_id: i64,
        status: RunStatus,
        /// Why the run stopped early, when it did: a failure message,
        /// `"cancelled"`, or `"token budget"`.
        stop_reason: Option<String>,
    },
}

/// Who decided a `bash` command, for the tab's command log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandDecision {
    /// A rule policy vouched for it; `source` names it (`"read-only"`, …).
    Rules { source: &'static str },
    /// The local classifier approved what the rules could not.
    Classifier,
    /// Full trust: nothing was checked.
    Trust,
    /// The user approved this exact line earlier "for this review".
    Remembered,
    /// The user approved it now; `for_review` when "for this review".
    User { for_review: bool },
    /// The user declined it.
    UserDenied,
    /// A deny rule (a secret path) blocked it.
    Blocked { reason: String },
    /// Nothing vouched for it and the mode never asks: refused, the agent
    /// was told why.
    AutoDenied { reason: String },
}

impl CommandDecision {
    /// Whether the command ran.
    pub fn ran(&self) -> bool {
        !matches!(
            self,
            CommandDecision::UserDenied
                | CommandDecision::Blocked { .. }
                | CommandDecision::AutoDenied { .. }
        )
    }
}
