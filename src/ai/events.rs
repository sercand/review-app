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
    /// An agent called a tool; `summary` is the first 120 chars of its input.
    ToolCall {
        agent: AgentRef,
        name: String,
        summary: String,
    },
    /// What the local command classifier thought of the agent's last tool
    /// call, announced after the call was decided. Arrives only when the
    /// classifier was consulted for it; the UI shows it on the tool line.
    ClassifierNote {
        agent: AgentRef,
        note: review_approval::model_stage::ClassifierNote,
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
    /// The scout filed its brief; the groups are in
    /// [`crate::ai::pipeline::PipelineStatus`] and the store.
    BriefReady,
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
