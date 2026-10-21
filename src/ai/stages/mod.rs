//! The pipeline stages (spec §5): each stage is one agent run with its own
//! prompt, tool subset, and structured finish tool. [`StageInputs`] is what
//! every stage gets; [`StageError`] is how every stage fails.

pub mod reviewer;
pub mod scout;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::mpsc;

use crate::ai::agent::AgentControl;
use crate::ai::events::PipelineEvent;
use crate::ai::model::Llm;
use crate::ai::tools::{AgentRef, ToolCx};
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

/// Runs a reviewer's side question through a fresh lookup agent (spec §5.4).
/// The pipeline implements this (Task 15); the reviewer stage reaches it
/// through [`ToolCx::spawner`] and the `ask_lookup` tool.
#[async_trait::async_trait]
pub trait Spawner: Send + Sync {
    /// Answer `question` on `from`'s behalf, with a fresh bounded context,
    /// and return the answer text.
    async fn lookup(&self, from: &AgentRef, question: &str) -> String;
}

/// The next agent number for a run ([`AgentRef::id`] is unique across
/// roles): the scout takes 1, every later stage takes the next number.
/// Process-wide, so ids are unique across runs too — more than the spec
/// requires, never less.
pub(crate) fn next_agent_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(2);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
