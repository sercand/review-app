//! The review agent's read-only repo tools: `bash`, `read_file`, `glob`,
//! `grep`, `list_dir`, `asset_stats`, `git_show`/`git_log`/`git_diff`, the
//! snapshot-backed `gh_pr`/`gh_pr_files`/`gh_pr_patch`/`gh_threads`/`gh_checks`,
//! the context-backed `load_skill`, the interaction tools `ask_lookup`,
//! `ask_user` and `request_prepare`, and the review-output `record_finding`,
//! `record_check` and `done`, with mono's names and schemas where
//! they exist, run against any
//! [`review_sandbox::SandboxOps`] (a checkout's `LocalHost` in production, a
//! temp-dir host in tests). The git tools run `git` directly in the checkout,
//! so they work even when `bash` is blocked; the `gh_*` tools read
//! [`ToolCx::snapshot`] and never touch the network.
//!
//! Every text result is cut to [`MAX_TOOL_OUTPUT`] bytes by [`truncate_middle`].
//! Bad input and failed sandbox ops are tool errors (`is_error`) that carry the
//! message, so the model can correct itself; a non-zero exit from `bash` is not an
//! error.
//!
//! The set is deliberately read-only (spec §6.3): there is no edit, write, merge or
//! GitHub-write tool — the review-output tools write only to the run's own
//! [`crate::ai::review::RunState`]. [`all`] lists every tool this crate registers, and the
//! `registry_has_no_write_or_merge_tools` test guards it; an agent gets an explicit
//! subset through [`Registry::new`].

mod bash;
mod brief;
mod draft;
mod files;
mod gh;
mod git;
mod inspect;
mod interact;
mod prior;
mod record;
mod search;
mod skills;
pub mod steer;
mod truncate;
mod verdict;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use brief::{SharedBrief, SubmitBrief};
pub use draft::{SharedDraft, SubmitReviewDraft};
pub use files::{IMAGE_MAX_PASSTHROUGH, IMAGE_MAX_SIDE, image_options};
pub use git::validate_rev;
pub use prior::{NewAnchor, PriorState, PriorStateKind, SharedPriorStates, SubmitPriorStates};
pub(crate) use record::resolve_side;
pub use truncate::{MAX_TOOL_OUTPUT, truncate_middle};
pub use verdict::{SharedVerdicts, SubmitVerdicts, Verdict, VerdictKind};

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

impl AgentRole {
    /// Every role, in pipeline order.
    pub const ALL: &'static [AgentRole] = &[
        AgentRole::Scout,
        AgentRole::Reviewer,
        AgentRole::Challenger,
        AgentRole::Lookup,
        AgentRole::Verifier,
        AgentRole::PriorCheck,
        AgentRole::Assembler,
        AgentRole::Chat,
        AgentRole::App,
    ];

    /// The stable snake_case name of the role, used for golden prompt file
    /// names (spec §9.1 `prompts.rs`) and as a default agent label.
    pub fn slug(&self) -> &'static str {
        match self {
            AgentRole::Scout => "scout",
            AgentRole::Reviewer => "reviewer",
            AgentRole::Challenger => "challenger",
            AgentRole::Lookup => "lookup",
            AgentRole::Verifier => "verifier",
            AgentRole::PriorCheck => "prior_check",
            AgentRole::Assembler => "assembler",
            AgentRole::Chat => "chat",
            AgentRole::App => "app",
        }
    }
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
/// and its signature never changes. Cloning shares every `Arc` and copies the
/// paths; a stage clones the pipeline's template and fills in its own agent.
#[derive(Clone)]
pub struct ToolCx {
    /// The sandbox every file and command op runs through.
    pub sandbox: Arc<dyn review_sandbox::SandboxOps>,
    /// The checkout the PR head is on; paths in tool input are relative to it.
    pub repo_root: PathBuf,
    /// The agent that made the call.
    pub agent: AgentRef,
    /// The user, for questions and command approvals.
    pub interaction: Arc<dyn crate::ai::interaction::Interaction>,
    /// Commands `bash` may run without asking: the repo's configured commands
    /// and nothing else. Entries match by whole-word prefix, so configure
    /// `cargo test` and not `cargo`.
    pub allowed: Arc<Mutex<Vec<String>>>,
    /// Exact, whitespace-normalized command lines the user approved "for this
    /// review".
    ///
    /// `bash` checks this list **before** the approval engine (`review_approval::classify`), so an
    /// approved line runs without asking again for the rest of the run. It is
    /// deliberately never prefix-matched: approving `touch a` does not grant
    /// `touch a b`, `/usr/bin/env sh proxy`, or an extended `find … -exec`. A
    /// line is stored with runs of whitespace collapsed and everything else
    /// (quotes included) verbatim, so it must match exactly.
    pub for_review: Arc<Mutex<Vec<String>>>,
    /// The PR under review, frozen by
    /// [`crate::ai::snapshot::gather_snapshot`] before the run starts.
    ///
    /// `None` means no PR context is attached — tool tests, and chat before a
    /// PR has loaded. The `gh_*` tools then return an error rather than
    /// guessing, and nothing else reads this field.
    pub snapshot: Option<Arc<crate::ai::snapshot::PrSnapshot>>,
    /// The repo's own instructions — `.github/REVIEW.md`, the instruction
    /// files and the skills index — gathered by
    /// [`crate::ai::context::load_repo_context`] from the checkout before the
    /// run starts.
    ///
    /// `None` means no repo context is attached — tool tests, and runs started
    /// before the checkout was scanned. `load_skill` then returns an error
    /// rather than guessing, and nothing else reads this field.
    pub context: Option<Arc<crate::ai::context::RepoContext>>,
    /// The run's shared findings/coverage/Q&A state, written by the
    /// `record_*` tools and read by the pipeline (spec §5.4).
    pub run: Arc<crate::ai::review::RunState>,
    /// The `(file, checklist item)` pairs assigned to this agent.
    ///
    /// `record_check` refuses items outside this list; an empty list means
    /// anything may be recorded (the scout, chat, and tool tests).
    pub assigned: Arc<Vec<(String, String)>>,
    /// The pipeline's lookup spawner, for `ask_lookup` (spec §5.4).
    ///
    /// `None` means no pipeline is attached — tool tests, and stages that
    /// do not spawn; `ask_lookup` then returns an error rather than
    /// guessing.
    pub spawner: Option<Arc<dyn crate::ai::stages::Spawner>>,
    /// The repo's prepare commands, run outside the sandbox as the user
    /// (spec §8.5) once the user approves `request_prepare`.
    ///
    /// The closure does the blocking work itself (the tool runs it on the
    /// blocking pool) and returns the already-formatted combined output.
    /// `None` when the repo has no prepare commands configured; the tool
    /// then returns an error rather than running nothing silently.
    pub prepare: Option<Arc<dyn Fn() -> anyhow::Result<String> + Send + Sync>>,
    /// Settings for the bash approval engine (`review_approval`).
    pub approval: Arc<crate::ai::approval_setup::ApprovalSetup>,
}

impl ToolCx {
    /// A copy of [`ToolCx::allowed`], for `review_approval::classify`.
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

    /// Whether the user already approved exactly `line` for this review.
    pub fn for_review_contains(&self, line: &str) -> bool {
        for_review_lock(&self.for_review)
            .iter()
            .any(|entry| entry == line)
    }

    /// Remembers `line` as approved for this review, once. An empty line is
    /// never remembered: there is no command to approve.
    pub fn remember_for_review(&self, line: String) {
        let mut lines = for_review_lock(&self.for_review);
        if !line.is_empty() && !lines.contains(&line) {
            lines.push(line);
        }
    }
}

/// Locks the allowed-commands list, recovering from a poisoned mutex: the list
/// is a cache of approvals, and refusing to read it would break every later
/// command in the run.
fn allowed_lock(allowed: &Mutex<Vec<String>>) -> std::sync::MutexGuard<'_, Vec<String>> {
    allowed.lock().unwrap_or_else(|e| e.into_inner())
}

/// Locks the for-review line list, recovering from poisoning for the same
/// reason as [`allowed_lock`].
fn for_review_lock(lines: &Mutex<Vec<String>>) -> std::sync::MutexGuard<'_, Vec<String>> {
    lines.lock().unwrap_or_else(|e| e.into_inner())
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
        Arc::new(gh::GhPr),
        Arc::new(gh::GhPrFiles),
        Arc::new(gh::GhPrPatch),
        Arc::new(gh::GhThreads),
        Arc::new(gh::GhChecks),
        Arc::new(skills::LoadSkill),
        Arc::new(interact::AskLookup),
        Arc::new(interact::AskUser),
        Arc::new(interact::RequestPrepare),
        Arc::new(record::RecordFinding),
        Arc::new(record::RecordCheck),
        Arc::new(record::Done),
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
