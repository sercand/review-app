//! Events the review pipeline emits as it works (spec §9.3).
//!
//! [`crate::ai::review::RunState`] emits the finding and coverage events and
//! [`crate::ai::agent::run_agent`] the transcript ones; Task 15 adds the stage
//! lifecycle. The UI drains the channel once per frame, so a send failure
//! means the tab has detached and is ignored — the store already holds the
//! data.

use crate::ai::tools::AgentRef;

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
}
