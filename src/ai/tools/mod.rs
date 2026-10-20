//! The review agent's read-only repo tools: `bash`, `read_file`, `glob`,
//! `grep`, `list_dir`, `asset_stats` and `git_show`/`git_log`/`git_diff`, with
//! mono's names and schemas where they exist, run against any
//! [`review_sandbox::SandboxOps`] (a checkout's `LocalHost` in production, a
//! temp-dir host in tests). The git tools run `git` directly in the checkout,
//! so they work even when `bash` is blocked.
//!
//! Every text result is cut to [`MAX_TOOL_OUTPUT`] bytes by [`truncate_middle`].
//! Bad input and failed sandbox ops are tool errors (`is_error`) that carry the
//! message, so the model can correct itself; a non-zero exit from `bash` is not an
//! error.
//!
//! The set is deliberately read-only (spec §6.3): there is no edit, write, merge or
//! GitHub-write tool. [`all`] lists every tool this crate registers, and the
//! `registry_has_no_write_or_merge_tools` test guards it; an agent gets an explicit
//! subset through [`Registry::new`].

mod bash;
mod files;
mod git;
mod inspect;
mod search;
mod truncate;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use files::{IMAGE_MAX_PASSTHROUGH, IMAGE_MAX_SIDE, image_options};
pub use git::validate_rev;
pub use truncate::{MAX_TOOL_OUTPUT, truncate_middle};

/// Which kind of agent a tool call belongs to.
///
/// [`AgentRole::App`] never runs a model: it tags notices the app itself produces
/// (e.g. a force-push fallback message shown in the run's transcript).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Maps the PR and writes the review brief.
    Scout,
    /// Reviews one group of files against a checklist.
    Reviewer,
    /// Tries to break the reviewers' findings before they reach the user.
    Challenger,
    /// Answers a reviewer's side question without reviewing.
    Lookup,
    /// Checks findings against the code and the PR's CI.
    Verifier,
    /// Checks earlier review comments on the same lines.
    PriorCheck,
    /// Turns the surviving findings into the review draft.
    Assembler,
    /// Answers the user's questions about the run.
    Chat,
    /// An app-generated notice, not a model agent.
    App,
}

/// A stable handle to one agent instance of a run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentRef {
    /// The agent's number within the run, unique across roles.
    pub id: u32,
    /// What kind of agent it is.
    pub role: AgentRole,
    /// The short human label shown in the UI (e.g. `reviewer 2`).
    pub label: String,
}

/// What a tool hands back to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// The result content: text, or an image for `read_file` on an image.
    pub parts: Vec<review_llm::ToolResultPart>,
    /// The call failed; the text says why.
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful text result, cut to [`MAX_TOOL_OUTPUT`].
    pub fn text(text: impl AsRef<str>) -> ToolOutput {
        ToolOutput {
            parts: vec![review_llm::ToolResultPart::Text {
                text: truncate_middle(text.as_ref(), MAX_TOOL_OUTPUT),
            }],
            is_error: false,
        }
    }

    /// A successful text result the caller has already cut to size.
    fn raw_text(text: String) -> ToolOutput {
        ToolOutput {
            parts: vec![review_llm::ToolResultPart::Text { text }],
            is_error: false,
        }
    }

    /// A tool error carrying `message`, cut to [`MAX_TOOL_OUTPUT`].
    pub fn error(message: impl AsRef<str>) -> ToolOutput {
        ToolOutput {
            is_error: true,
            ..ToolOutput::text(message)
        }
    }
}

/// Everything a tool may touch: the sandbox, the checkout root and who is asking.
///
/// Later pipeline pieces add their shared state here by name (`snapshot`, `run`,
/// `interaction`, `context`, `spawner`, …), so a tool takes one context argument
/// and its signature never changes.
pub struct ToolCx {
    /// The sandbox every file and command op runs through.
    pub sandbox: Arc<dyn review_sandbox::SandboxOps>,
    /// The checkout the PR head is on; paths in tool input are relative to it.
    pub repo_root: PathBuf,
    /// The agent that made the call.
    pub agent: AgentRef,
    /// The user, for questions and command approvals.
    pub interaction: Arc<dyn crate::ai::interaction::Interaction>,
    /// Commands `bash` may run without asking: the repo's configured commands,
    /// seeded at run start, plus what the user approved "for this review".
    ///
    /// Shared by every agent of the run, so an approval in one reviewer is
    /// silent for the next. Guarded by a std mutex because it is only touched
    /// synchronously, never across an `.await`; the [`ToolCx::allowed_snapshot`]
    /// and [`ToolCx::allow_segments`] helpers recover from a poisoned lock so a
    /// panicking holder cannot turn a tool call into a panic.
    pub allowed: Arc<Mutex<Vec<String>>>,
}

impl ToolCx {
    /// A copy of [`ToolCx::allowed`], for [`review_sandbox::classify`].
    pub fn allowed_snapshot(&self) -> Vec<String> {
        allowed_lock(&self.allowed).clone()
    }

    /// Adds `segments` to [`ToolCx::allowed`], skipping empty and duplicate
    /// entries so the list stays as small as the approvals that built it.
    pub fn allow_segments(&self, segments: impl IntoIterator<Item = String>) {
        let mut allowed = allowed_lock(&self.allowed);
        for segment in segments {
            if !segment.is_empty() && !allowed.contains(&segment) {
                allowed.push(segment);
            }
        }
    }
}

/// Locks the allowed-commands list, recovering from a poisoned mutex: the list
/// is a cache of approvals, and refusing to read it would break every later
/// command in the run.
fn allowed_lock(allowed: &Mutex<Vec<String>>) -> std::sync::MutexGuard<'_, Vec<String>> {
    allowed.lock().unwrap_or_else(|e| e.into_inner())
}

/// A tool an agent can call.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The tool's name, description and JSON input schema.
    fn definition(&self) -> review_llm::Tool;
    /// Runs the tool. Failures come back as [`ToolOutput::error`], never as a panic.
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput;
}

/// The tools of one agent, looked up by name.
///
/// An agent's registry is an explicit subset of [`all`]: a scout gets file tools, a
/// verifier gets CI tools, and nobody gets a write tool.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// A registry over exactly `tools`.
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Registry {
        Registry { tools }
    }

    /// Every tool's definition, in registry order, for the LLM request.
    pub fn definitions(&self) -> Vec<review_llm::Tool> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// The tool called `name`, if the registry has it.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|t| t.definition().name == name)
            .cloned()
    }
}

/// Every tool this crate registers, in presentation order.
///
/// It exists so the safety test can sweep the whole set as it grows; agents get
/// explicit subsets built with [`Registry::new`].
pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(bash::Bash),
        Arc::new(files::ReadFile),
        Arc::new(search::Glob),
        Arc::new(search::Grep),
        Arc::new(inspect::ListDir),
        Arc::new(inspect::AssetStats),
        Arc::new(git::GitShow),
        Arc::new(git::GitLog),
        Arc::new(git::GitDiff),
    ]
}

/// Fragments a tool name may not contain: each names a write or merge action.
///
/// Matched case-insensitively as substrings, so `merge`, `merge_pr`, `auto_merge`
/// and `Post_Comment` are all out.
#[cfg(test)]
const FORBIDDEN_TOOL_FRAGMENTS: [&str; 6] = [
    "merge",
    "approve",
    "post",
    "resolve",
    "edit_file",
    "write_file",
];

/// A GitHub review submission. It is forbidden too, matched the same way, with one
/// ruled exception below.
#[cfg(test)]
const FORBIDDEN_SUBMIT_REVIEW: &str = "submit_review";

/// The local draft tool (Task 14): its name contains a forbidden fragment, but it
/// posts nothing — it fills a draft the app shows the user for approval.
#[cfg(test)]
const LOCAL_DRAFT_TOOL: &str = "submit_review_draft";

/// Whether `name` is a write or merge tool (see [`FORBIDDEN_TOOL_FRAGMENTS`]).
#[cfg(test)]
fn is_forbidden_tool_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == LOCAL_DRAFT_TOOL {
        return false;
    }
    name.contains(FORBIDDEN_SUBMIT_REVIEW)
        || FORBIDDEN_TOOL_FRAGMENTS
            .iter()
            .any(|bad| name.contains(bad))
}

/// Parses a tool's input, or the tool error to return.
fn parse<T: DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    // A model may send `null` for a tool without required fields.
    let input = if input.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        input
    };
    serde_json::from_value(input).map_err(|e| ToolOutput::error(format!("invalid input: {e}")))
}

/// A tool definition from a static schema. The schemas are fixed and checked by tests.
fn definition(name: &str, description: &str, schema: Value) -> review_llm::Tool {
    review_llm::Tool::new(name, description, schema)
        .unwrap_or_else(|e| unreachable!("tool {name}: invalid built-in schema: {e}"))
}

/// The directory a search runs under: the given one, or the checkout root.
fn search_root(directory: &str) -> &str {
    if directory.trim().is_empty() {
        "."
    } else {
        directory
    }
}
