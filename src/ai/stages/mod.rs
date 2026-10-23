//! The pipeline stages (spec §5): each stage is one agent run with its own
//! prompt, tool subset, and structured finish tool. [`StageInputs`] is what
//! every stage gets; [`StageError`] is how every stage fails.

pub mod assemble;
pub mod challenge;
pub mod prior;
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
    /// Why `bash` is unavailable in this run (spec §8.2 — e.g. `bwrap` is
    /// missing). `Some(reason)`: no registry includes `bash` or
    /// `request_prepare`, and every agent's seed message ends with
    /// [`bash_note`]'s line.
    pub bash_disabled: Option<String>,
    /// Files changed since the previous run's head (spec §12), `None` for a
    /// full review. The scout narrows its grouping with it, and the
    /// pipeline skips review groups it does not touch.
    pub incremental: Option<Vec<String>>,
    /// `git log --oneline` since the previous run's head, shown to the
    /// scout; `None` for a full review.
    pub incremental_log: Option<String>,
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

/// The exact line every seed message ends with when bash is unavailable
/// (spec §8.2): the agent must know *why* the tool is missing, not just
/// that it is.
pub(crate) fn bash_note(bash_disabled: Option<&str>) -> Option<String> {
    bash_disabled.map(|reason| format!("bash is unavailable in this review: {reason}"))
}

/// Appends [`bash_note`] as the final line of a seed task message.
pub(crate) fn with_bash_note(mut text: String, bash_disabled: Option<&str>) -> String {
    if let Some(note) = bash_note(bash_disabled) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&note);
        text.push('\n');
    }
    text
}

/// Builds a seed task message: `text`, then the run's answered questions
/// as `## Known facts from the user` (spec §5.4 — every agent gets them,
/// so a user answer constrains all of them), then [`with_bash_note`] —
/// the bash note stays the very last line (spec §8.2).
pub(crate) fn seed_message(
    text: String,
    qa: &[(String, String)],
    bash_disabled: Option<&str>,
) -> String {
    let mut text = text;
    if !qa.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n## Known facts from the user\n\n");
        for (question, answer) in qa {
            text.push_str(&format!("Q: {question}\nA: {answer}\n"));
        }
    }
    with_bash_note(text, bash_disabled)
}

/// Drops `bash` and `request_prepare` from `tools` when bash is unavailable
/// (spec §8.2): without a shell there is nothing for the prepare request to
/// offer either.
pub(crate) fn without_bash(
    tools: Vec<Arc<dyn Tool>>,
    bash_disabled: Option<&str>,
) -> Vec<Arc<dyn Tool>> {
    if bash_disabled.is_none() {
        return tools;
    }
    tools
        .into_iter()
        .filter(|tool| !matches!(tool.definition().name.as_str(), "bash" | "request_prepare"))
        .collect()
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
/// record tools. `bash_disabled` drops `bash` and `request_prepare`
/// (spec §8.2).
pub(crate) fn reviewer_tools(bash_disabled: Option<&str>) -> Vec<Arc<dyn Tool>> {
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
    let tools: Vec<Arc<dyn Tool>> = crate::ai::tools::all()
        .into_iter()
        .filter(|tool| NAMES.contains(&tool.definition().name.as_str()))
        .collect();
    without_bash(tools, bash_disabled)
}

/// The challenger's tools (spec §5.4): the reviewer's set minus
/// `request_prepare` — `ToolCx::prepare` is never wired for a challenger,
/// so offering the tool could only ever answer with an error.
pub(crate) fn challenger_tools(bash_disabled: Option<&str>) -> Vec<Arc<dyn Tool>> {
    reviewer_tools(bash_disabled)
        .into_iter()
        .filter(|tool| tool.definition().name != "request_prepare")
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

/// Annotation lines one check may contribute to a seed's `## CI` section.
const MAX_ANNOTATION_LINES: usize = 3;

/// The `## CI` section shared by the scout and assembler seeds (spec §5.2,
/// §5.6): `name: conclusion` per check, each with up to
/// [`MAX_ANNOTATION_LINES`] annotation lines. `None` when there are no
/// checks, so the section is omitted.
pub(crate) fn ci_section(checks: &[crate::ai::snapshot::CheckSummary]) -> Option<String> {
    if checks.is_empty() {
        return None;
    }
    let mut out = String::from("\n## CI\n\n");
    for check in checks {
        out.push_str(&format!(
            "{}: {}\n",
            check.name,
            check.conclusion.as_deref().unwrap_or("pending")
        ));
        for annotation in check.annotations.iter().take(MAX_ANNOTATION_LINES) {
            out.push_str(&format!(
                "  - {}:{} {}\n",
                annotation.path, annotation.start_line, annotation.message
            ));
        }
    }
    Some(out)
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

#[cfg(test)]
mod tests {
    use super::{
        bash_note, challenger_tools, reviewer_tools, seed_message, with_bash_note, without_bash,
    };
    use crate::ai::tools;

    #[test]
    fn bash_note_is_the_exact_line() {
        assert_eq!(bash_note(None), None);
        assert_eq!(
            bash_note(Some("bwrap is not installed")).as_deref(),
            Some("bash is unavailable in this review: bwrap is not installed")
        );
    }

    #[test]
    fn bash_note_ends_the_seed_message() {
        // Nothing is appended when bash is available.
        assert_eq!(with_bash_note("seed\n".to_string(), None), "seed\n");

        // A seed ending in text gets a newline before the note; the note is
        // the last line either way.
        let text = with_bash_note("seed".to_string(), Some("no bwrap"));
        assert_eq!(text, "seed\nbash is unavailable in this review: no bwrap\n");
        let text = with_bash_note("seed\n".to_string(), Some("no bwrap"));
        assert_eq!(text, "seed\nbash is unavailable in this review: no bwrap\n");
    }

    #[test]
    fn seed_message_appends_known_facts_then_bash_note() {
        let qa = vec![("is empty allowed?".to_string(), "yes".to_string())];

        // No questions: exactly what with_bash_note produces.
        assert_eq!(
            seed_message("seed\n".to_string(), &[], Some("no bwrap")),
            "seed\nbash is unavailable in this review: no bwrap\n"
        );
        assert_eq!(seed_message("seed\n".to_string(), &[], None), "seed\n");

        // Questions come after the task text; the bash note stays last.
        assert_eq!(
            seed_message("seed\n".to_string(), &qa, Some("no bwrap")),
            "seed\n\n## Known facts from the user\n\nQ: is empty allowed?\nA: yes\nbash is unavailable in this review: no bwrap\n"
        );
        assert_eq!(
            seed_message("seed".to_string(), &qa, None),
            "seed\n\n## Known facts from the user\n\nQ: is empty allowed?\nA: yes\n"
        );
    }

    #[test]
    fn without_bash_drops_bash_and_request_prepare_only() {
        let all = tools::all();
        let names: Vec<String> = all.iter().map(|t| t.definition().name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"request_prepare".to_string()));

        // Available: the list passes through untouched.
        assert_eq!(without_bash(all.clone(), None).len(), all.len());

        // Disabled: exactly those two tools disappear.
        let kept = without_bash(all.clone(), Some("no bwrap"));
        let kept_names: Vec<String> = kept.iter().map(|t| t.definition().name).collect();
        assert_eq!(kept.len(), all.len() - 2);
        assert!(!kept_names.contains(&"bash".to_string()));
        assert!(!kept_names.contains(&"request_prepare".to_string()));
        assert!(kept_names.contains(&"read_file".to_string()));
    }

    #[test]
    fn reviewer_registry_follows_bash_disabled() {
        let with_bash = reviewer_tools(None);
        let names: Vec<String> = with_bash.iter().map(|t| t.definition().name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"request_prepare".to_string()));

        let without = reviewer_tools(Some("no bwrap"));
        let names: Vec<String> = without.iter().map(|t| t.definition().name).collect();
        assert!(!names.contains(&"bash".to_string()));
        assert!(!names.contains(&"request_prepare".to_string()));
        assert_eq!(without.len(), with_bash.len() - 2);
    }

    #[test]
    fn challenger_tools_drop_request_prepare() {
        // `ToolCx::prepare` is never wired for a challenger, so the tool
        // must not be offered to it.
        let names: Vec<String> = challenger_tools(None)
            .iter()
            .map(|t| t.definition().name)
            .collect();
        assert!(
            !names.contains(&"request_prepare".to_string()),
            "no prepare tool for challengers: {names:?}"
        );
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"record_finding".to_string()));
        assert_eq!(names.len(), reviewer_tools(None).len() - 1);

        // `bash_disabled` still strips bash on top.
        let names: Vec<String> = challenger_tools(Some("no bwrap"))
            .iter()
            .map(|t| t.definition().name)
            .collect();
        assert!(!names.contains(&"bash".to_string()));
        assert!(!names.contains(&"request_prepare".to_string()));
    }
}
