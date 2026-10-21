//! The pipeline stages (spec §5): each stage is one agent run with its own
//! prompt, tool subset, and structured finish tool. [`StageInputs`] is what
//! every stage gets; [`StageError`] is how every stage fails.

pub mod scout;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::ai::agent::AgentControl;
use crate::ai::events::PipelineEvent;
use crate::ai::model::Llm;
use crate::ai::tools::ToolCx;
use crate::persist::settings::{AiSettings, RepoAiSettings};

/// Everything a stage needs: a [`ToolCx`] template (the stage clones it and
/// fills in its own agent), the model, the shared control handles, the UI
/// event channel, and the settings the stage reads.
pub struct StageInputs {
    /// Template context: snapshot, run state, sandbox, interaction. The
    /// stage copies it and sets [`ToolCx::agent`] to its own agent.
    pub cx: ToolCx,
    /// The model every turn streams from.
    pub llm: Arc<dyn Llm>,
    /// Cancel / pause / notes handles, shared with the pipeline.
    pub ctl: AgentControl,
    /// Where stage progress goes.
    pub events: mpsc::UnboundedSender<PipelineEvent>,
    /// The global AI settings.
    pub settings: AiSettings,
    /// This repo's settings: the configured commands and the checkout folder
    /// (spec §4 step 1, §8.1).
    pub repo: RepoAiSettings,
}

/// Why a stage stopped without producing its artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageError {
    /// The agent stopped without ever calling its finish tool successfully.
    NoFinish,
    /// The model failed, or the stage ran out of turns.
    Failed(String),
    /// The run was cancelled.
    Cancelled,
}
