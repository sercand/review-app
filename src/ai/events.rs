//! Events the review pipeline emits as it works (spec §9.3).
//!
//! [`crate::ai::review::RunState`] emits the finding and coverage events now;
//! Task 9 adds the transcript events and Task 15 the stage lifecycle. The UI
//! drains the channel once per frame, so a send failure means the tab has
//! detached and is ignored — the store already holds the data.

/// One thing that happened in a review run.
#[derive(Clone, Debug, PartialEq)]
pub enum PipelineEvent {
    /// A finding was recorded; `id` is its `ai_findings` row id.
    FindingAdded { id: i64 },
    /// A finding's state changed (kept, dropped, posted, …).
    FindingChanged { id: i64 },
    /// A coverage entry was recorded for `(file, item)`.
    Coverage { file: String, item: String },
}
