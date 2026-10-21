//! The scout stage (spec §5.2): orient on the PR and produce the review
//! brief.
//!
//! [`run_scout`] runs one agent with read-only tools and `submit_brief` as
//! its finish tool. The stage owns the seed task message (the agent loop
//! persists only what it appends, spec §9.2), the [`normalize_groups`] pass
//! that assigns group ids, forces Generated files into a skim group and
//! splits oversized groups, and the brief's persistence through
//! [`Store::session_set_brief`](crate::ai::store::Store::session_set_brief).

use std::sync::{Arc, Mutex};

use review_llm::{ContentBlock, Message};
use serde::{Deserialize, Serialize};

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::checklist::{self, FileKind};
use crate::ai::context::RepoContext;
use crate::ai::prompts::system_prompt;
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::{StageError, StageInputs};
use crate::ai::tools::{AgentRef, AgentRole, Registry, SharedBrief, SubmitBrief, Tool, ToolCx};
use crate::github::files::{LoadedFile, PatchState};
use crate::model::diff::{Hunk, LineKind};

/// The scout's agent label, also its transcript key.
pub const SCOUT_LABEL: &str = "scout";

/// Model calls the scout may make (spec §6.2).
const MAX_TURNS: u32 = 30;
/// Combined allowance for finish nudges before the stage gives up.
const MAX_FINISH_REMINDERS: u32 = 2;
/// A group part holds at most this many files (spec §5.2).
const MAX_GROUP_FILES: usize = 8;
/// ... and at most this many changed lines (spec §5.2).
const MAX_GROUP_LINES: u32 = 1_500;
/// The PR body is trimmed to this many characters in the task message.
const MAX_BODY_CHARS: usize = 2_000;
/// Annotation lines shown per failed check.
const MAX_ANNOTATION_LINES: usize = 3;

/// How seriously the scout rates a group (spec §5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    High,
    Medium,
    Low,
}

/// One file of a group; `hunks` is the inclusive hunk-index range when a
/// file over the line cap was split (spec §5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<(u32, u32)>,
}

/// One group of related files a reviewer takes as its context (spec §5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// `g1`.., assigned by [`normalize_groups`].
    #[serde(default)]
    pub id: String,
    pub title: String,
    pub risk: Risk,
    pub reason: String,
    pub files: Vec<GroupFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leads: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci_notes: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub skim: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The scout's map of the PR: intent, unexplained changes, and the review
/// groups (spec §5.2). Stored as the session's brief.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub intent: String,
    pub description_matches: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unexplained: Vec<String>,
    pub groups: Vec<Group>,
    /// Filled only when the repo has no configured commands (spec §4 step 1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_prepare: Vec<String>,
}

/// Runs the scout and returns its normalized, persisted brief.
///
/// The seed task message is persisted before the loop starts (the loop
/// persists only what it appends). A `submit_brief` call that passed the
/// tool's coverage check fills the shared cell; the finish check accepts
/// only then. On success the brief is normalized ([`normalize_groups`]), the
/// suggestion lists are dropped when the repo already has configured
/// commands (spec §4 step 1), and the result is stored as the session brief.
pub async fn run_scout(inputs: &StageInputs, prev: Option<&Brief>) -> Result<Brief, StageError> {
    let agent = AgentRef {
        id: 1,
        role: AgentRole::Scout,
        label: SCOUT_LABEL.to_string(),
    };
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();
    let snapshot = cx
        .snapshot
        .clone()
        .ok_or_else(|| StageError::Failed("scout: no PR snapshot attached".to_string()))?;

    let submitted: SharedBrief = Arc::new(Mutex::new(None));
    let registry = Registry::new(scout_tools(Arc::new(SubmitBrief::new(submitted.clone()))));

    let text = initial_message(
        &snapshot,
        cx.context.as_deref(),
        &inputs.repo.commands,
        prev,
    );
    persist_seed(&cx, &text);

    let cell = submitted.clone();
    let spec = AgentSpec {
        agent,
        system: system_prompt(AgentRole::Scout, SCOUT_LABEL),
        initial: vec![Message::user_text(text)],
        registry,
        max_turns: MAX_TURNS,
        finish_tool: Some("submit_brief"),
        on_finish: Arc::new(move || {
            if cell.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                FinishCheck::Accept
            } else {
                FinishCheck::Continue(
                    "Call submit_brief with the complete brief before you finish.".to_string(),
                )
            }
        }),
        max_finish_reminders: MAX_FINISH_REMINDERS,
    };
    let ctl = AgentControl {
        cancel: inputs.ctl.cancel.clone(),
        pause: inputs.ctl.pause.clone(),
        notes: inputs.ctl.notes.clone(),
    };
    let outcome = run_agent(
        spec,
        cx.clone(),
        inputs.llm.clone(),
        ctl,
        inputs.events.clone(),
    )
    .await;

    let brief = submitted.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (outcome.stop, brief) {
        (AgentStop::Finished, Some(mut brief)) => {
            normalize_groups(&mut brief, &snapshot);
            if !inputs.repo.commands.is_empty() {
                // The repo already has commands; suggestions are noise
                // (spec §4 step 1).
                brief.suggested_commands.clear();
                brief.suggested_prepare.clear();
            }
            let json = serde_json::to_string(&brief)
                .map_err(|e| StageError::Failed(format!("scout: brief did not serialize: {e}")))?;
            cx.run
                .store()
                .session_set_brief(cx.run.session_id(), &json)
                .map_err(|e| StageError::Failed(format!("scout: storing the brief failed: {e}")))?;
            Ok(brief)
        }
        // The loop only finishes with the finish tool accepted, so `None`
        // here means the reminders ran out on an empty cell.
        (AgentStop::Finished, None) => Err(StageError::NoFinish),
        (AgentStop::NoFinish, _) => Err(StageError::NoFinish),
        (AgentStop::Cancelled, _) => Err(StageError::Cancelled),
        (AgentStop::Failed(message), _) => Err(StageError::Failed(message)),
        (AgentStop::MaxTurns, _) => Err(StageError::Failed(format!(
            "scout ran out of turns after {} model calls",
            outcome.turns
        ))),
    }
}

/// The scout's tools (spec §5.2): the read-only repo tools, the snapshot
/// `gh_*` tools, `load_skill`, `bash` (test-less inspection; approvals still
/// ask), and its finish tool `submit_brief`.
fn scout_tools(submit: Arc<dyn Tool>) -> Vec<Arc<dyn Tool>> {
    const READ_ONLY: [&str; 15] = [
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
    ];
    let mut tools: Vec<Arc<dyn Tool>> = crate::ai::tools::all()
        .into_iter()
        .filter(|tool| READ_ONLY.contains(&tool.definition().name.as_str()))
        .collect();
    tools.push(submit);
    tools
}

/// Appends the seed task message through the run's store. A failure is
/// logged and skipped, exactly as the loop's own appends treat theirs.
fn persist_seed(cx: &ToolCx, text: &str) {
    let content = vec![ContentBlock::Text {
        text: text.to_string(),
    }];
    let Ok(content_json) = serde_json::to_string(&content) else {
        tracing::warn!("scout: seed message did not serialize");
        return;
    };
    if let Err(error) =
        cx.run
            .store()
            .message_append(cx.run.run_id(), SCOUT_LABEL, "user", &content_json)
    {
        tracing::warn!(%error, "scout: seed message append failed");
    }
}

/// The scout's single task message (spec §5.1, §5.2): the PR overview, the
/// file list with +/− and kinds, the CI summary, the repo's own rules and
/// commands, and — on a re-review — the previous brief.
fn initial_message(
    snapshot: &PrSnapshot,
    context: Option<&RepoContext>,
    commands: &[String],
    prev: Option<&Brief>,
) -> String {
    let meta = &snapshot.metadata;
    let mut out = String::from("## Pull request\n\n");
    out.push_str(&format!("{}\n\n", meta.title));
    out.push_str(&format!("Author: {}\n", meta.author));
    out.push_str(&format!(
        "Base `{}` ← head `{}`\nSize: +{} −{} across {} files\n",
        meta.base_ref, meta.head_ref, meta.additions, meta.deletions, meta.changed_files
    ));
    let body = meta.body.trim();
    if !body.is_empty() {
        let body = if body.chars().count() > MAX_BODY_CHARS {
            let head: String = body.chars().take(MAX_BODY_CHARS).collect();
            format!("{head}\n…")
        } else {
            body.to_string()
        };
        out.push_str(&format!("\n{body}\n"));
    }

    out.push_str("\n## Files\n\n");
    for loaded in &snapshot.files {
        out.push_str(&format!(
            "{} (+{} −{}) [{}]\n",
            loaded.file.path,
            loaded.file.additions,
            loaded.file.deletions,
            kind_slug(loaded)
        ));
    }

    if !snapshot.checks.is_empty() {
        out.push_str("\n## CI\n\n");
        for check in &snapshot.checks {
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
    }

    if let Some(rules) = context.and_then(|ctx| ctx.review_md.as_deref()) {
        out.push_str("\n## Repo review rules\n\n");
        out.push_str(rules.trim_end());
        out.push('\n');
    }

    let root: Vec<&crate::ai::context::InstructionFile> = context
        .map(|ctx| {
            ctx.instructions
                .iter()
                .filter(|f| f.dir.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !root.is_empty() {
        out.push_str("\n## Root instructions\n\n");
        for file in root {
            out.push_str(&format!(
                "### {}\n\n{}\n",
                file.path,
                file.content.trim_end()
            ));
        }
    }

    if !commands.is_empty() {
        out.push_str("\n## Repo commands\n\n");
        for command in commands {
            out.push_str(command);
            out.push('\n');
        }
    }

    if let Some(prev) = prev {
        out.push_str("\n## Previous brief\n\n");
        let json = serde_json::to_string_pretty(prev).unwrap_or_default();
        out.push_str(&json);
        out.push_str(
            "\n\n## New commits since last review\n\nThe previous review ran against a \
             different head; re-check anything time-sensitive.\n",
        );
    }
    out
}

/// The task-message kind label for one file.
fn kind_slug(loaded: &LoadedFile) -> &'static str {
    match checklist::file_kind(
        &loaded.file.path,
        loaded.file.patch_state == PatchState::Binary,
    ) {
        FileKind::Source => "source",
        FileKind::Test => "test",
        FileKind::Asset => "asset",
        FileKind::Config => "config",
        FileKind::Generated => "generated",
    }
}

/// The app-side normalization of the scout's groups (spec §5.2): assign
/// `g1`.. ids, move every Generated-kind file into one dedicated skim group
/// appended last, and split oversized groups into `(part k)` sub-groups.
/// Deterministic: the scout's group order and file order are preserved.
pub fn normalize_groups(brief: &mut Brief, snapshot: &PrSnapshot) {
    let mut generated: Vec<GroupFile> = Vec::new();
    let mut sized: Vec<Group> = Vec::new();
    for group in std::mem::take(&mut brief.groups) {
        let mut files = Vec::new();
        for file in group.files {
            if is_generated(snapshot, &file.path) {
                generated.push(GroupFile {
                    path: file.path,
                    hunks: None,
                });
            } else {
                files.push(file);
            }
        }
        if !files.is_empty() {
            sized.extend(split_group(Group { files, ..group }, snapshot));
        }
    }
    if !generated.is_empty() {
        sized.extend(split_group(
            Group {
                id: String::new(),
                title: "Generated files".to_string(),
                risk: Risk::Low,
                reason: "generated files".to_string(),
                files: generated,
                leads: Vec::new(),
                extra_checks: Vec::new(),
                ci_notes: Vec::new(),
                skim: true,
            },
            snapshot,
        ));
    }
    for (index, group) in sized.iter_mut().enumerate() {
        group.id = format!("g{}", index + 1);
    }
    brief.groups = sized;
}

/// Splits one group into parts of at most [`MAX_GROUP_FILES`] entries and
/// [`MAX_GROUP_LINES`] changed lines. Files over the line cap are first cut
/// into hunk-index ranges — never inside a hunk. Parts inherit everything
/// from the parent; only the title gains `(part k)`, and only when the group
/// actually splits.
fn split_group(group: Group, snapshot: &PrSnapshot) -> Vec<Group> {
    let Group {
        id: _,
        title,
        risk,
        reason,
        files,
        leads,
        extra_checks,
        ci_notes,
        skim,
    } = group;
    let entries: Vec<(GroupFile, u32)> = files
        .iter()
        .flat_map(|file| expand_file(snapshot, file))
        .collect();
    let total: u32 = entries.iter().map(|(_, lines)| *lines).sum();
    if entries.len() <= MAX_GROUP_FILES && total <= MAX_GROUP_LINES {
        return vec![Group {
            id: String::new(),
            title,
            risk,
            reason,
            files: entries.into_iter().map(|(file, _)| file).collect(),
            leads,
            extra_checks,
            ci_notes,
            skim,
        }];
    }
    let mut parts: Vec<Vec<(GroupFile, u32)>> = Vec::new();
    for entry in entries {
        let fits = parts.last().is_some_and(|part| {
            part.len() < MAX_GROUP_FILES
                && part.iter().map(|(_, lines)| *lines).sum::<u32>() + entry.1 <= MAX_GROUP_LINES
        });
        if fits {
            parts.last_mut().expect("just checked").push(entry);
        } else {
            parts.push(vec![entry]);
        }
    }
    let many = parts.len() > 1;
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| Group {
            id: String::new(),
            title: if many {
                format!("{title} (part {})", index + 1)
            } else {
                title.clone()
            },
            risk,
            reason: reason.clone(),
            files: part.into_iter().map(|(file, _)| file).collect(),
            leads: leads.clone(),
            extra_checks: extra_checks.clone(),
            ci_notes: ci_notes.clone(),
            skim,
        })
        .collect()
}

/// Cuts one file into hunk-index parts when it is over the line cap and its
/// parsed diff allows it; otherwise it stays whole. Each part comes back
/// with its changed-line count for the group's size math.
fn expand_file(snapshot: &PrSnapshot, file: &GroupFile) -> Vec<(GroupFile, u32)> {
    let total = changed_lines(snapshot, &file.path);
    let hunks: &[Hunk] = snapshot
        .files
        .iter()
        .find(|loaded| loaded.file.path == file.path)
        .and_then(|loaded| loaded.diff.as_ref())
        .map(|diff| diff.hunks.as_slice())
        .unwrap_or(&[]);
    if total <= MAX_GROUP_LINES || file.hunks.is_some() || hunks.is_empty() {
        return vec![(file.clone(), total)];
    }
    let mut parts: Vec<(GroupFile, u32)> = Vec::new();
    let mut start = 0usize;
    let mut lines: u32 = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let hunk_lines = hunk_changed_lines(hunk);
        if index > start && lines + hunk_lines > MAX_GROUP_LINES {
            parts.push((hunk_part(&file.path, start, index - 1), lines));
            start = index;
            lines = 0;
        }
        lines += hunk_lines;
    }
    parts.push((hunk_part(&file.path, start, hunks.len() - 1), lines));
    parts
}

/// A group file covering hunks `start..=end` of `path`.
fn hunk_part(path: &str, start: usize, end: usize) -> GroupFile {
    GroupFile {
        path: path.to_string(),
        hunks: Some((start as u32, end as u32)),
    }
}

/// The added and removed rows of one hunk.
fn hunk_changed_lines(hunk: &Hunk) -> u32 {
    hunk.rows
        .iter()
        .filter(|row| matches!(row.kind, LineKind::Added | LineKind::Removed))
        .count() as u32
}

/// The file's changed lines as GitHub counted them; unknown paths count 0.
fn changed_lines(snapshot: &PrSnapshot, path: &str) -> u32 {
    snapshot
        .files
        .iter()
        .find(|loaded| loaded.file.path == path)
        .map(|loaded| loaded.file.additions + loaded.file.deletions)
        .unwrap_or(0)
}

/// Whether the checklist classifies the path as Generated (lockfiles,
/// vendored, minified, …) — the same rule the file list shows.
fn is_generated(snapshot: &PrSnapshot, path: &str) -> bool {
    let binary = snapshot
        .files
        .iter()
        .find(|loaded| loaded.file.path == path)
        .is_some_and(|loaded| loaded.file.patch_state == PatchState::Binary);
    checklist::file_kind(path, binary) == FileKind::Generated
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use review_llm::ContentBlock;
    use review_sandbox::LocalHost;

    use super::*;
    use crate::ai::agent::AgentControl;
    use crate::ai::context::{InstructionFile, RepoContext};
    use crate::ai::interaction::ApprovalDecision;
    use crate::ai::snapshot::{CheckSummary, PrSnapshot};
    use crate::ai::stages::{StageError, StageInputs};
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
    use crate::github::checks::CheckAnnotation;
    use crate::github::files::LoadedFile;
    use crate::model::diff::{DiffRow, FileDiff, Hunk, LineKind, RowOrigin};
    use crate::persist::settings::{AiSettings, ModelSettings, RepoAiSettings};
    use crate::test_support;

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    /// A snapshot over one [`test_support::file`] per path (+1 −1 each).
    fn snapshot_with(paths: &[&str]) -> PrSnapshot {
        PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files: paths
                .iter()
                .map(|path| LoadedFile::parse(test_support::file(path)))
                .collect(),
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn group(title: &str, risk: Risk, paths: &[&str]) -> Group {
        Group {
            id: String::new(),
            title: title.to_string(),
            risk,
            reason: "reason".to_string(),
            files: paths
                .iter()
                .map(|path| GroupFile {
                    path: path.to_string(),
                    hunks: None,
                })
                .collect(),
            leads: Vec::new(),
            extra_checks: Vec::new(),
            ci_notes: Vec::new(),
            skim: false,
        }
    }

    fn brief_with(groups: Vec<Group>) -> Brief {
        Brief {
            intent: "the intent".to_string(),
            description_matches: true,
            unexplained: Vec::new(),
            groups,
            suggested_commands: Vec::new(),
            suggested_prepare: Vec::new(),
        }
    }

    /// A hunk of `lines` added rows.
    fn added_hunk(lines: u32) -> Hunk {
        Hunk {
            old_start: 1,
            new_start: 1,
            section: String::new(),
            rows: (0..lines)
                .map(|_| DiffRow {
                    kind: LineKind::Added,
                    old_no: None,
                    new_no: Some(1),
                    text: "x".to_string(),
                    no_newline_at_eof: false,
                    origin: RowOrigin::Patch,
                })
                .collect(),
        }
    }

    /// A `StageInputs` over a temp-dir [`LocalHost`] carrying `snapshot`.
    fn stage_inputs(
        dir: &tempfile::TempDir,
        snapshot: PrSnapshot,
        llm: Arc<ScriptedLlm>,
        interaction: ScriptedInteraction,
        repo: RepoAiSettings,
    ) -> StageInputs {
        let host = LocalHost::new(dir.path()).expect("local host");
        let (tx, _rx) = mpsc::unbounded_channel();
        StageInputs {
            cx: ToolCx {
                sandbox: Arc::new(host.clone()),
                repo_root: host.root().path().to_path_buf(),
                agent: AgentRef {
                    id: 0,
                    role: AgentRole::Scout,
                    label: "template".to_string(),
                },
                interaction: Arc::new(interaction),
                allowed: Arc::new(Mutex::new(Vec::new())),
                for_review: Arc::new(Mutex::new(Vec::new())),
                snapshot: Some(Arc::new(snapshot)),
                context: None,
                run: test_support::ai_run_state(),
                assigned: Arc::new(Vec::new()),
                spawner: None,
                prepare: None,
            },
            llm,
            ctl: AgentControl {
                cancel: CancellationToken::new(),
                pause: watch::channel(false).1,
                notes: Arc::new(Mutex::new(VecDeque::new())),
            },
            events: tx,
            settings: AiSettings::default(),
            repo,
        }
    }

    #[test]
    fn normalize_splits_large_groups_and_huge_files() {
        // Twelve small files in one group become two parts of eight and four.
        let paths: Vec<String> = (1..=12).map(|i| format!("src/f{i}.rs")).collect();
        let str_paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        let snapshot = snapshot_with(&str_paths);
        let mut first = group("feature", Risk::High, &str_paths);
        first.leads = vec!["watch the locking".to_string()];
        let mut brief = brief_with(vec![first]);

        normalize_groups(&mut brief, &snapshot);

        assert_eq!(brief.groups.len(), 2);
        assert_eq!(brief.groups[0].title, "feature (part 1)");
        assert_eq!(brief.groups[1].title, "feature (part 2)");
        assert_eq!(brief.groups[0].files.len(), 8);
        assert_eq!(brief.groups[1].files.len(), 4);
        assert_eq!(brief.groups[0].id, "g1");
        assert_eq!(brief.groups[1].id, "g2");
        assert_eq!(brief.groups[0].risk, Risk::High, "parts inherit the risk");
        assert_eq!(
            brief.groups[1].leads,
            vec!["watch the locking".to_string()],
            "parts inherit the leads"
        );
        assert_eq!(
            brief.groups[0].files[0].path, "src/f1.rs",
            "order preserved"
        );
        assert_eq!(
            brief.groups[1].files[0].path, "src/f9.rs",
            "order preserved"
        );

        // A 20k-line file with four 600-line hunks becomes hunk-range parts
        // whose ranges are contiguous and cover every hunk.
        let mut snapshot = snapshot_with(&["src/huge.rs"]);
        snapshot.files[0].file.additions = 20_000;
        snapshot.files[0].file.deletions = 0;
        snapshot.files[0].file.changes = 20_000;
        snapshot.files[0].diff = Some(FileDiff {
            hunks: vec![added_hunk(600); 4],
        });
        let mut brief = brief_with(vec![group("huge", Risk::Medium, &["src/huge.rs"])]);

        normalize_groups(&mut brief, &snapshot);

        let ranges: Vec<(u32, u32)> = brief
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .filter(|f| f.path == "src/huge.rs")
            .map(|f| f.hunks.expect("hunk range"))
            .collect();
        assert_eq!(
            ranges,
            vec![(0, 1), (2, 3)],
            "contiguous, covering all four hunks"
        );
        assert_eq!(brief.groups[0].title, "huge (part 1)");
        assert_eq!(brief.groups[1].title, "huge (part 2)");

        // A single hunk over the cap stays one part: never split inside it.
        let mut snapshot = snapshot_with(&["src/huge.rs"]);
        snapshot.files[0].file.additions = 2_000;
        snapshot.files[0].file.changes = 2_000;
        snapshot.files[0].diff = Some(FileDiff {
            hunks: vec![added_hunk(2_000)],
        });
        let mut brief = brief_with(vec![group("huge", Risk::Low, &["src/huge.rs"])]);

        normalize_groups(&mut brief, &snapshot);

        assert_eq!(brief.groups.len(), 1, "one hunk cannot be split further");
        assert_eq!(brief.groups[0].files.len(), 1);
        assert_eq!(brief.groups[0].files[0].hunks, Some((0, 0)));
        assert_eq!(
            brief.groups[0].title, "huge",
            "a single part keeps the title"
        );
    }

    #[test]
    fn lockfile_only_pr_becomes_a_skim_group() {
        let snapshot = snapshot_with(&["Cargo.lock", "package-lock.json"]);
        // The scout filed the lockfiles as a normal, non-skim group.
        let mut brief = brief_with(vec![group(
            "deps",
            Risk::Low,
            &["Cargo.lock", "package-lock.json"],
        )]);

        normalize_groups(&mut brief, &snapshot);

        assert_eq!(brief.groups.len(), 1, "only the generated group is left");
        let generated = &brief.groups[0];
        assert_eq!(generated.title, "Generated files");
        assert!(generated.skim, "the generated group is always a skim group");
        assert_eq!(generated.risk, Risk::Low);
        assert_eq!(generated.reason, "generated files");
        assert_eq!(generated.id, "g1");
        let paths: Vec<&str> = generated.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Cargo.lock", "package-lock.json"]);
    }

    #[test]
    fn generated_files_move_out_of_mixed_groups() {
        let snapshot = snapshot_with(&["src/a.rs", "Cargo.lock"]);
        let mut brief = brief_with(vec![group(
            "mixed",
            Risk::High,
            &["src/a.rs", "Cargo.lock"],
        )]);

        normalize_groups(&mut brief, &snapshot);

        assert_eq!(brief.groups.len(), 2);
        assert_eq!(brief.groups[0].title, "mixed");
        assert_eq!(brief.groups[0].files.len(), 1);
        assert_eq!(brief.groups[1].title, "Generated files");
        assert!(brief.groups[1].skim);
        assert_eq!(
            brief.groups[1].id, "g2",
            "the generated group is appended last"
        );
    }

    #[test]
    fn initial_message_sections_in_order() {
        let mut snapshot = snapshot_with(&["src/a.rs", "Cargo.lock"]);
        snapshot.metadata.body = "b".repeat(3_000);
        snapshot.checks = vec![CheckSummary {
            name: "test".to_string(),
            conclusion: Some("failure".to_string()),
            summary: None,
            text: None,
            annotations: (0..5)
                .map(|i| CheckAnnotation {
                    path: "src/a.rs".to_string(),
                    start_line: i,
                    end_line: i,
                    level: "failure".to_string(),
                    title: None,
                    message: format!("m{i}"),
                })
                .collect(),
        }];
        let context = RepoContext {
            review_md: Some("review rules here".to_string()),
            checks: Vec::new(),
            instructions: vec![
                InstructionFile {
                    dir: String::new(),
                    path: "AGENTS.md".to_string(),
                    content: "root rules".to_string(),
                },
                InstructionFile {
                    dir: "src".to_string(),
                    path: "src/AGENTS.md".to_string(),
                    content: "nested rules".to_string(),
                },
            ],
            skills: Vec::new(),
        };
        let prev = brief_with(Vec::new());

        let text = initial_message(
            &snapshot,
            Some(&context),
            &["cargo test".to_string()],
            Some(&prev),
        );

        let order = [
            "## Pull request",
            "## Files",
            "## CI",
            "## Repo review rules",
            "## Root instructions",
            "## Repo commands",
            "## Previous brief",
            "## New commits since last review",
        ];
        let mut last = 0;
        for section in order {
            let at = text
                .find(section)
                .unwrap_or_else(|| panic!("missing {section}:\n{text}"));
            assert!(at >= last, "{section} is out of order:\n{text}");
            last = at;
        }

        assert!(text.contains("src/a.rs (+1 −1) [source]"), "{text}");
        assert!(text.contains("Cargo.lock (+1 −1) [generated]"), "{text}");
        assert!(text.contains("test: failure"), "{text}");
        assert_eq!(
            text.matches("  - src/a.rs:").count(),
            3,
            "up to three annotation lines:\n{text}"
        );
        assert!(text.contains(&"b".repeat(2_000)), "body kept to 2000 chars");
        assert!(
            !text.contains(&"b".repeat(2_001)),
            "body trimmed at 2000 chars"
        );
        assert!(text.contains("review rules here"), "{text}");
        assert!(text.contains("root rules"), "{text}");
        assert!(
            !text.contains("nested rules"),
            "only root instruction files:\n{text}"
        );
        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("different head"), "{text}");

        // Without a previous brief the re-review sections are gone.
        let text = initial_message(&snapshot, None, &[], None);
        assert!(!text.contains("## Previous brief"), "{text}");
        assert!(!text.contains("## Repo commands"), "{text}");
    }

    #[tokio::test]
    async fn scout_returns_a_normalized_brief() {
        let dir = crate::test_support::tempdir();
        std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write a");
        std::fs::write(dir.path().join("src/b.rs"), "fn b() {}\n").expect("write b");
        let snapshot = snapshot_with(&["src/a.rs", "src/b.rs"]);
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(vec![("read_file".into(), json!({"path": "src/a.rs"}))]),
                ScriptStep::ToolCalls(vec![(
                    "submit_brief".into(),
                    json!({
                        "intent": "adds a and b",
                        "description_matches": true,
                        "suggested_commands": ["cargo test"],
                        "groups": [
                            {
                                "title": "core",
                                "risk": "medium",
                                "reason": "new logic",
                                "files": ["src/a.rs"],
                                "leads": ["compare with src/b.rs"],
                            },
                            {
                                "title": "rest",
                                "risk": "low",
                                "reason": "plumbing",
                                "files": ["src/b.rs"],
                            },
                        ],
                    }),
                )]),
            ],
        ));
        let inputs = stage_inputs(
            &dir,
            snapshot,
            llm.clone(),
            ScriptedInteraction::new().approve(ApprovalDecision::Once),
            RepoAiSettings::default(),
        );
        let run = inputs.cx.run.clone();

        let brief = run_scout(&inputs, None).await.expect("a normalized brief");

        assert_eq!(brief.groups.len(), 2);
        assert_eq!(brief.groups[0].id, "g1", "ids assigned");
        assert_eq!(brief.groups[1].id, "g2", "ids assigned");
        assert_eq!(brief.groups[0].title, "core");
        assert_eq!(brief.groups[0].risk, Risk::Medium);
        assert_eq!(
            brief.suggested_commands,
            vec!["cargo test".to_string()],
            "kept: the repo has no configured commands"
        );

        // The normalized brief is in the store and parses back.
        let stored = run
            .store()
            .session_brief(run.session_id())
            .expect("store read")
            .expect("brief stored");
        assert_eq!(
            serde_json::from_str::<Brief>(&stored).expect("stored brief parses"),
            brief
        );

        // The seed task message was persisted before the loop ran.
        let transcript = run
            .store()
            .messages(run.run_id(), "scout")
            .expect("transcript");
        assert_eq!(transcript[0].0, "user", "the seed is the first message");
        let seed: Vec<ContentBlock> = serde_json::from_str(&transcript[0].1).expect("seed json");
        let ContentBlock::Text { text } = &seed[0] else {
            panic!("the seed is a text block: {seed:?}")
        };
        assert!(text.contains("## Pull request"), "{text}");
        assert!(text.contains("src/a.rs (+1 −1) [source]"), "{text}");
    }

    #[tokio::test]
    async fn repo_commands_clear_the_suggestions() {
        let dir = crate::test_support::tempdir();
        let snapshot = snapshot_with(&["src/a.rs"]);
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::ToolCalls(vec![(
                "submit_brief".into(),
                json!({
                    "intent": "x",
                    "description_matches": true,
                    "suggested_commands": ["cargo fmt"],
                    "suggested_prepare": ["cargo fetch"],
                    "groups": [
                        { "title": "all", "risk": "low", "reason": "r", "files": ["src/a.rs"] },
                    ],
                }),
            )])],
        ));
        let repo = RepoAiSettings {
            commands: vec!["cargo test".to_string()],
            ..RepoAiSettings::default()
        };
        let inputs = stage_inputs(&dir, snapshot, llm, ScriptedInteraction::new(), repo);

        let brief = run_scout(&inputs, None).await.expect("brief");

        assert!(
            brief.suggested_commands.is_empty(),
            "the repo has commands already"
        );
        assert!(
            brief.suggested_prepare.is_empty(),
            "the repo has commands already"
        );
    }

    #[tokio::test]
    async fn scout_without_submit_is_no_finish() {
        let dir = crate::test_support::tempdir();
        let snapshot = snapshot_with(&["src/a.rs"]);
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("looks fine to me".into()),
                ScriptStep::Text("really, no brief needed".into()),
                ScriptStep::Text("last word".into()),
            ],
        ));
        let inputs = stage_inputs(
            &dir,
            snapshot,
            llm,
            ScriptedInteraction::new(),
            RepoAiSettings::default(),
        );

        let err = run_scout(&inputs, None)
            .await
            .expect_err("no submit_brief call means no finish");
        assert_eq!(err, StageError::NoFinish);
    }
}
