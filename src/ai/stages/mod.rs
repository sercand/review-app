//! The pipeline stages (spec §5): each stage is one agent run with its own
//! prompt, tool subset, and structured finish tool. [`StageInputs`] is what
//! every stage gets; [`StageError`] is how every stage fails.

pub mod challenge;
pub mod reviewer;
pub mod scout;
pub mod verify;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use review_llm::ContentBlock;
use tokio::sync::mpsc;

use crate::ai::agent::AgentControl;
use crate::ai::events::PipelineEvent;
use crate::ai::model::Llm;
use crate::ai::review::{Evidence, FindingAnchor, RunState};
use crate::ai::tools::{AgentRef, Tool, ToolCx};
use crate::model::diff::Side;
use crate::model::thread::ReviewThread;
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

/// The reviewer's tools (spec §5.4, §6.3): the read-only repo tools (with
/// `bash` — reviewers run tests, and approvals flow through `Interaction`),
/// the snapshot `gh_*` tools, `load_skill`, the interaction tools, and the
/// record tools. The challenger runs the same set (spec §5.4), so it lives
/// here as the single source of truth.
pub(crate) fn reviewer_tools() -> Vec<Arc<dyn Tool>> {
    const NAMES: [&str; 21] = [
        "bash",
        "read_file",
        "glob",
        "grep",
        "list_dir",
        "asset_stats",
        "git_show",
        "git_log",
        "git_diff",
        "gh_pr",
        "gh_pr_files",
        "gh_pr_patch",
        "gh_threads",
        "gh_checks",
        "load_skill",
        "ask_lookup",
        "ask_user",
        "request_prepare",
        "record_finding",
        "record_check",
        "done",
    ];
    crate::ai::tools::all()
        .into_iter()
        .filter(|tool| NAMES.contains(&tool.definition().name.as_str()))
        .collect()
}

/// The ids of the findings `agent` has recorded so far. Stages diff this
/// before and after a run to learn what that agent produced.
pub(crate) fn agent_findings(run: &RunState, agent: &AgentRef) -> Vec<i64> {
    run.findings()
        .iter()
        .filter(|finding| &finding.agent == agent)
        .map(|finding| finding.id)
        .collect()
}

/// Appends the seed task message through the run's store under `label`. A
/// failure is logged and skipped, exactly as the loop's own appends treat
/// theirs.
pub(crate) fn persist_seed(cx: &ToolCx, label: &str, text: &str) {
    let content = vec![ContentBlock::Text {
        text: text.to_string(),
    }];
    let Ok(content_json) = serde_json::to_string(&content) else {
        tracing::warn!("stage {label}: seed message did not serialize");
        return;
    };
    if let Err(error) = cx
        .run
        .store()
        .message_append(cx.run.run_id(), label, "user", &content_json)
    {
        tracing::warn!(%error, "stage {label}: seed message append failed");
    }
}

/// One anchor rendered for a task message: `path:start-end (SIDE)`,
/// `path:line (SIDE)`, or `path (file summary)`.
pub(crate) fn anchor_text(anchor: &FindingAnchor) -> String {
    match anchor {
        FindingAnchor::Line(anchor) => {
            let side = match anchor.side {
                Side::Right => "RIGHT",
                Side::Left => "LEFT",
            };
            match anchor.start_line {
                Some(start) => format!("{}:{start}-{} ({side})", anchor.path, anchor.line),
                None => format!("{}:{} ({side})", anchor.path, anchor.line),
            }
        }
        FindingAnchor::Summary { path } => format!("{path} (file summary)"),
    }
}

/// One evidence entry rendered for a task message: `path:line — note`.
pub(crate) fn evidence_text(evidence: &Evidence) -> String {
    let line = evidence
        .line
        .map(|line| format!(":{line}"))
        .unwrap_or_default();
    format!("{}{line} — {}", evidence.path, evidence.note)
}

/// Thread bodies shown per thread in a task message.
pub(crate) const MAX_THREAD_BODY_CHARS: usize = 200;

/// One existing thread rendered for a task message:
/// `path:line author: body… [resolved]`.
pub(crate) fn thread_text(thread: &ReviewThread) -> String {
    let line = thread
        .line
        .or(thread.start_line)
        .map(|line| line.to_string())
        .unwrap_or_default();
    let root = thread.comments.first();
    let author = root.map(|c| c.author.as_str()).unwrap_or("?");
    let body = root.map(|c| c.body.as_str()).unwrap_or("");
    let head: String = body.chars().take(MAX_THREAD_BODY_CHARS).collect();
    let cut = if body.chars().count() > MAX_THREAD_BODY_CHARS {
        "…"
    } else {
        ""
    };
    let resolved = if thread.is_resolved {
        " [resolved]"
    } else {
        ""
    };
    format!("{}:{line} {author}: {head}{cut}{resolved}", thread.path)
}
