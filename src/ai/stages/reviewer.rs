//! The reviewer stage (spec §5.3, §5.4): one agent per brief group with an
//! enforced coverage log.
//!
//! [`run_reviewer`] runs one agent over one [`Group`]: the read-only repo
//! tools, the snapshot `gh_*` tools, `load_skill`, `bash` (reviewers run
//! tests; approvals still ask through [`crate::ai::interaction::Interaction`]),
//! the record tools and the interaction tools. The stage owns the seed task
//! message (persisted before the loop starts, like the scout's) and the
//! coverage enforcement: `on_finish` refuses while `(file, item)` pairs are
//! unrecorded, and when the two reminders run out every still-missing pair
//! is recorded as [`CheckResult::Skipped`] with note `"not checked"`, so a
//! gap is visible rather than silent (spec §5.4). Running out of reminders,
//! finish nudges or turns is a *normal completion* for this stage.

use std::collections::HashSet;
use std::sync::Arc;

use review_llm::Message;

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::checklist;
use crate::ai::context::{RepoCheck, RepoContext};
use crate::ai::prompts::system_prompt;
use crate::ai::review::{CheckResult, CoverageEntry, RunState};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::scout::{Brief, Group, GroupFile, Risk};
use crate::ai::stages::{
    StageError, StageInputs, agent_findings, persist_seed, reviewer_tools, seed_message,
    thread_text,
};
use crate::ai::tools::{AgentRef, AgentRole, Registry};
use crate::github::files::{LoadedFile, PatchState};
use crate::model::diff::{Hunk, LineKind};

/// Model calls a reviewer may make (spec §6.2).
const MAX_TURNS: u32 = 40;
/// Coverage reminders before the app records the gaps as skipped (spec §5.4).
const MAX_FINISH_REMINDERS: u32 = 2;
/// One file's patch is trimmed to this many characters in the task message.
const MAX_PATCH_CHARS: usize = 20_000;
/// Missing pairs listed in one coverage reminder before the list is cut.
const MAX_REMINDER_PAIRS: usize = 20;

/// What one reviewer run leaves behind.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewerOutcome {
    /// The `(file, item)` pairs the app had to record as skipped.
    pub skipped: Vec<(String, String)>,
    /// The ids of the findings this reviewer recorded during this call.
    pub findings: Vec<i64>,
}

/// Runs one reviewer over `group` of `brief`.
///
/// The seed task message is persisted before the loop starts, and
/// [`ToolCx::assigned`] carries the `(file, checklist item)` pairs derived
/// from [`checklist::assign`] for the group's files. Whatever stop path
/// ends the loop (except a model failure), every still-missing pair is
/// recorded as [`CheckResult::Skipped`] before the stage returns.
pub async fn run_reviewer(
    inputs: &StageInputs,
    agent: &AgentRef,
    brief: &Brief,
    group: &Group,
) -> Result<ReviewerOutcome, StageError> {
    let agent = agent.clone();
    let label = agent.label.clone();
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();
    let snapshot = cx
        .snapshot
        .clone()
        .ok_or_else(|| StageError::Failed(format!("{label}: no PR snapshot attached")))?;

    let repo_checks: &[RepoCheck] = cx
        .context
        .as_ref()
        .map(|context| context.checks.as_slice())
        .unwrap_or(&[]);
    let assigned = assignment(group, &snapshot, repo_checks);
    cx.assigned = Arc::new(assigned.clone());
    if !inputs.repo.prepare.is_empty() {
        // The repo's fixed prepare commands, formatted for the model; the
        // blocking `run_prepare` happens inside the closure, which the tool
        // runs on the blocking pool (spec §8.5).
        let repo_path = inputs.repo.path.clone();
        let commands = inputs.repo.prepare.clone();
        cx.prepare = Some(Arc::new(move || {
            let outputs =
                crate::ai::checkout::run_prepare(&repo_path, &commands).map_err(|failed| {
                    anyhow::anyhow!(
                        "`{}` exited {}:\n{}",
                        failed.command,
                        failed.status,
                        failed.output.trim_end()
                    )
                })?;
            let mut out = String::new();
            for output in &outputs {
                out.push_str(&format!(
                    "$ {}\n(exit {})\n{}\n",
                    output.command,
                    output.status,
                    output.output.trim_end()
                ));
            }
            Ok(out)
        }));
    }

    let text = seed_message(
        initial_message(brief, group, &snapshot, cx.context.as_deref(), &assigned),
        &cx.run.qa(),
        inputs.bash_disabled.as_deref(),
    );
    persist_seed(&cx, &label, &text);

    let run = cx.run.clone();
    let watch = assigned.clone();
    let spec = AgentSpec {
        agent: agent.clone(),
        system: system_prompt(AgentRole::Reviewer, &label),
        initial: vec![Message::user_text(text)],
        registry: Registry::new(reviewer_tools(inputs.bash_disabled.as_deref())),
        max_turns: MAX_TURNS,
        // The coverage check *is* the finish decision, and spec §5.4 wants
        // it consulted both when the model ends a turn without tool calls
        // and when it calls `done` — so no loop-side finish tool.
        finish_tool: None,
        on_finish: Arc::new(move || match run.missing(&watch) {
            missing if missing.is_empty() => FinishCheck::Accept,
            missing => FinishCheck::Continue(missing_text(&missing)),
        }),
        max_finish_reminders: MAX_FINISH_REMINDERS,
        elide_threshold: f64::from(inputs.settings.context_threshold),
    };
    let ctl = AgentControl {
        cancel: inputs.ctl.cancel.clone(),
        pause: inputs.ctl.pause.clone(),
        notes: inputs.ctl.notes.clone(),
    };

    let before = agent_findings(&cx.run, &agent);
    let outcome = run_agent(
        spec,
        cx.clone(),
        inputs.llm.clone(),
        ctl,
        inputs.events.clone(),
    )
    .await;
    let findings: Vec<i64> = agent_findings(&cx.run, &agent)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();

    // Running out of reminders, finish nudges or turns is a normal
    // completion here (spec §5.3/§5.4): the gaps get recorded as skipped and
    // the stage still succeeds. Only a model failure or a cancel fails it.
    let skipped = if matches!(outcome.stop, AgentStop::Failed(_)) {
        Vec::new()
    } else {
        record_skips(&cx.run, &assigned)
    };
    match outcome.stop {
        AgentStop::Finished | AgentStop::NoFinish | AgentStop::MaxTurns => {
            Ok(ReviewerOutcome { skipped, findings })
        }
        AgentStop::Cancelled => Err(StageError::Cancelled),
        AgentStop::Failed(message) => Err(StageError::Failed(message)),
    }
}

/// The `(file, item)` pairs this group must cover: [`checklist::assign`]
/// per file (kind defaults, matching repo checks, the scout's extras),
/// deduplicated in group order.
fn assignment(
    group: &Group,
    snapshot: &PrSnapshot,
    repo_checks: &[RepoCheck],
) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for file in &group.files {
        let binary = find_file(snapshot, &file.path)
            .is_some_and(|loaded| loaded.file.patch_state == PatchState::Binary);
        let kind = checklist::file_kind(&file.path, binary);
        for item in checklist::assign(&file.path, kind, repo_checks, &group.extra_checks) {
            if seen.insert((file.path.clone(), item.clone())) {
                pairs.push((file.path.clone(), item));
            }
        }
    }
    pairs
}

/// The coverage reminder the loop pushes while pairs are unrecorded.
fn missing_text(missing: &[(String, String)]) -> String {
    let mut pairs: Vec<String> = missing
        .iter()
        .take(MAX_REMINDER_PAIRS)
        .map(|(file, item)| format!("{file}:{item}"))
        .collect();
    if missing.len() > MAX_REMINDER_PAIRS {
        pairs.push("…".to_string());
    }
    format!(
        "You haven't recorded these checks yet: {}. Record each with record_check.",
        pairs.join(", ")
    )
}

/// Records every still-missing pair as [`CheckResult::Skipped`] with note
/// `"not checked"` and returns the pairs it skipped (spec §5.4).
fn record_skips(run: &RunState, assigned: &[(String, String)]) -> Vec<(String, String)> {
    let missing = run.missing(assigned);
    for (file, item) in &missing {
        if let Err(error) = run.record_check(CoverageEntry {
            file: file.clone(),
            item: item.clone(),
            result: CheckResult::Skipped,
            note: "not checked".to_string(),
        }) {
            tracing::warn!(%error, "reviewer: recording a skipped check failed");
        }
    }
    missing
}

/// The reviewer's single task message (spec §5.4): the intent, the group,
/// its patches (or hunk ranges), the assigned checklist with instruction
/// text, the scoped instructions, the skill index and the threads on these
/// files. The user Q&A is not part of it: [`seed_message`] appends the one
/// shared `## Known facts from the user` section.
fn initial_message(
    brief: &Brief,
    group: &Group,
    snapshot: &PrSnapshot,
    context: Option<&RepoContext>,
    assigned: &[(String, String)],
) -> String {
    let mut out = String::from("## Intent\n\n");
    out.push_str(brief.intent.trim_end());
    out.push('\n');

    out.push_str("\n## Your group\n\n");
    out.push_str(&format!(
        "{}: {} (risk: {})\n{}\n\nFiles:\n",
        group.id,
        group.title,
        risk_slug(group.risk),
        group.reason
    ));
    for file in &group.files {
        let loaded = find_file(snapshot, &file.path);
        let (additions, deletions) = loaded
            .map(|loaded| (loaded.file.additions, loaded.file.deletions))
            .unwrap_or((0, 0));
        out.push_str(&format!("- {} (+{additions} −{deletions})", file.path));
        if let Some((from, to)) = file.hunks {
            let total = loaded
                .and_then(|loaded| loaded.diff.as_ref())
                .map(|diff| diff.hunks.len())
                .unwrap_or(0);
            out.push_str(&format!(" [hunks {from}-{to} of {total}]"));
        }
        out.push('\n');
    }
    if !group.leads.is_empty() {
        out.push_str("\nLeads:\n");
        for lead in &group.leads {
            out.push_str(&format!("- {lead}\n"));
        }
    }
    if !group.ci_notes.is_empty() {
        out.push_str("\nCI notes:\n");
        for note in &group.ci_notes {
            out.push_str(&format!("- {note}\n"));
        }
    }

    out.push_str("\n## Patches\n\n");
    for file in &group.files {
        out.push_str(&format!("### {}\n\n", file.path));
        out.push_str(&patch_text(snapshot, file));
        out.push('\n');
    }

    out.push_str("\n## Checklist\n\n");
    for file in &group.files {
        out.push_str(&format!("### {}\n\n", file.path));
        for (_, item) in assigned.iter().filter(|(f, _)| *f == file.path) {
            match item_text(item, context) {
                Some(text) => out.push_str(&format!("- {item}: {text}\n")),
                None => out.push_str(&format!("- {item}\n")),
            }
        }
        out.push('\n');
    }

    let paths: Vec<&str> = group.files.iter().map(|f| f.path.as_str()).collect();
    let instructions: Vec<&crate::ai::context::InstructionFile> = context
        .map(|context| context.instructions_for(&paths))
        .unwrap_or_default();
    if !instructions.is_empty() {
        out.push_str("\n## Instructions\n\n");
        for file in instructions {
            out.push_str(&format!(
                "### {}\n\n{}\n",
                file.path,
                file.content.trim_end()
            ));
        }
    }

    if let Some(context) = context {
        let index = context.skill_index();
        if !index.is_empty() {
            out.push_str("\n## Skills\n\n");
            out.push_str(&index);
            out.push('\n');
        }
    }

    let threads: Vec<&crate::model::thread::ReviewThread> = snapshot
        .threads
        .iter()
        .filter(|thread| paths.contains(&thread.path.as_str()))
        .collect();
    if !threads.is_empty() {
        out.push_str("\n## Existing threads\n\n");
        for thread in threads {
            out.push_str(&format!("- {}\n", thread_text(thread)));
        }
    }
    out
}

/// The file's patch text for the task message: the raw patch, or the
/// hunks inside a hunk-range group file, capped at [`MAX_PATCH_CHARS`].
pub(crate) fn patch_text(snapshot: &PrSnapshot, file: &GroupFile) -> String {
    let Some(loaded) = find_file(snapshot, &file.path) else {
        return "(the PR does not carry this file)".to_string();
    };
    let text = match file.hunks {
        None => loaded
            .file
            .patch
            .clone()
            .unwrap_or_else(|| "(no patch: binary or too-large file)".to_string()),
        Some((from, to)) => {
            let hunks = loaded
                .diff
                .as_ref()
                .map(|diff| diff.hunks.as_slice())
                .unwrap_or(&[]);
            if hunks.is_empty() {
                "(no patch: binary or too-large file)".to_string()
            } else {
                let from = (from as usize).min(hunks.len() - 1);
                let to = (to as usize).min(hunks.len() - 1);
                hunks[from..=to]
                    .iter()
                    .map(render_hunk)
                    .collect::<Vec<_>>()
                    .join("")
            }
        }
    };
    if text.chars().count() <= MAX_PATCH_CHARS {
        return text;
    }
    let head: String = text.chars().take(MAX_PATCH_CHARS).collect();
    format!("{head}\n[patch truncated]")
}

/// Re-renders one parsed hunk as unified-diff text.
fn render_hunk(hunk: &Hunk) -> String {
    let old = hunk
        .rows
        .iter()
        .filter(|row| row.kind != LineKind::Added)
        .count();
    let new = hunk
        .rows
        .iter()
        .filter(|row| row.kind != LineKind::Removed)
        .count();
    let section = if hunk.section.is_empty() {
        String::new()
    } else {
        format!(" {}", hunk.section)
    };
    let mut out = format!(
        "@@ -{},{} +{},{} @@{section}\n",
        hunk.old_start, old, hunk.new_start, new
    );
    for row in &hunk.rows {
        let prefix = match row.kind {
            LineKind::Context => " ",
            LineKind::Added => "+",
            LineKind::Removed => "-",
        };
        out.push_str(prefix);
        out.push_str(&row.text);
        out.push('\n');
        if row.no_newline_at_eof {
            out.push_str("\\ No newline at end of file\n");
        }
    }
    out
}

/// The instruction text for one checklist id: the built-in item's, a repo
/// check's, or `None` for a scout extra the app has no text for.
fn item_text(item: &str, context: Option<&RepoContext>) -> Option<String> {
    context
        .and_then(|context| context.checks.iter().find(|check| check.id == item))
        .map(|check| check.text.clone())
        .or_else(|| {
            checklist::builtin_items()
                .into_iter()
                .find(|builtin| builtin.id == item)
                .map(|builtin| builtin.instruction)
        })
}

pub(crate) fn find_file<'a>(snapshot: &'a PrSnapshot, path: &str) -> Option<&'a LoadedFile> {
    snapshot
        .files
        .iter()
        .find(|loaded| loaded.file.path == path)
}

pub(crate) fn risk_slug(risk: Risk) -> &'static str {
    match risk {
        Risk::High => "high",
        Risk::Medium => "medium",
        Risk::Low => "low",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use review_llm::{ContentBlock, GenerateRequest};
    use review_sandbox::LocalHost;
    use serde_json::{Value, json};
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::ai::agent::AgentControl;
    use crate::ai::checklist::{FileKind, default_items};
    use crate::ai::context::{InstructionFile, RepoCheck, RepoContext, SkillInfo};
    use crate::ai::review::{Evidence, FindingAnchor, NewFinding, Severity};
    use crate::ai::stages::{StageError, StageInputs};
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
    use crate::github::files::LoadedFile;
    use crate::model::diff::{DiffRow, FileDiff, Hunk, LineKind, RowOrigin};
    use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
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
        snapshot_of(
            paths
                .iter()
                .map(|path| LoadedFile::parse(test_support::file(path)))
                .collect(),
        )
    }

    fn snapshot_of(files: Vec<LoadedFile>) -> PrSnapshot {
        PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files,
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn group(id: &str, title: &str, risk: Risk, paths: &[&str]) -> Group {
        Group {
            id: id.to_string(),
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

    /// A hunk of `lines` added rows starting at `old_start`.
    fn added_hunk(old_start: u32, lines: u32) -> Hunk {
        Hunk {
            old_start,
            new_start: old_start + 100,
            section: String::new(),
            rows: (0..lines)
                .map(|_| DiffRow {
                    kind: LineKind::Added,
                    old_no: None,
                    new_no: Some(old_start + 100),
                    text: "x".to_string(),
                    no_newline_at_eof: false,
                    origin: RowOrigin::Patch,
                })
                .collect(),
        }
    }

    /// A thread on `path` with one comment by `author`.
    fn thread(
        path: &str,
        line: Option<u32>,
        author: &str,
        body: &str,
        resolved: bool,
    ) -> ReviewThread {
        ReviewThread {
            id: "T1".to_string(),
            is_resolved: resolved,
            is_outdated: false,
            path: path.to_string(),
            line,
            start_line: None,
            side: None,
            subject_type: SubjectType::Line,
            comments: vec![ThreadComment {
                database_id: 1,
                author: author.to_string(),
                avatar_url: None,
                body: body.to_string(),
                created_at: "2026-10-20T00:00:00Z".to_string(),
                pending: false,
            }],
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
            bash_disabled: None,
            incremental: None,
            incremental_log: None,
        }
    }

    /// All text blocks of a request, concatenated.
    fn request_text(req: &GenerateRequest) -> String {
        req.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The reviewer agent reference, as the pipeline injects it.
    fn reviewer_agent() -> AgentRef {
        AgentRef {
            id: 2,
            role: AgentRole::Reviewer,
            label: "reviewer:g1".to_string(),
        }
    }

    /// The `(src/a.rs, item)` pairs a source file is assigned.
    fn source_pairs(path: &str) -> Vec<(String, String)> {
        default_items(FileKind::Source)
            .iter()
            .map(|item| (path.to_string(), item.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn reviewer_is_reprompted_for_missing_checks_then_skips_are_recorded() {
        let dir = crate::test_support::tempdir();
        let snapshot = snapshot_with(&["src/a.rs"]);
        // Three text-only turns: never a record_check.
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("looks fine".into()),
                ScriptStep::Text("really, done".into()),
                ScriptStep::Text("last word".into()),
            ],
        ));
        let inputs = stage_inputs(
            &dir,
            snapshot,
            llm.clone(),
            ScriptedInteraction::new(),
            RepoAiSettings::default(),
        );
        let run = inputs.cx.run.clone();
        let brief = brief_with(vec![group("g1", "core", Risk::High, &["src/a.rs"])]);
        let assigned = source_pairs("src/a.rs");

        let outcome = run_reviewer(&inputs, &reviewer_agent(), &brief, &brief.groups[0])
            .await
            .expect("running out of reminders is a normal completion");

        assert_eq!(outcome.skipped, assigned, "every pair was skipped");
        assert!(outcome.findings.is_empty());

        // The two coverage reminders reached the model, in requests 2 and 3.
        let requests = llm.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            !request_text(&requests[0]).contains("You haven't recorded"),
            "the seed carries no reminder"
        );
        for req in &requests[1..] {
            let text = request_text(req);
            assert!(
                text.contains("You haven't recorded these checks yet: src/a.rs:intent"),
                "{text}"
            );
            assert!(text.contains("src/a.rs:correctness"), "{text}");
            assert!(text.contains(". Record each with record_check."), "{text}");
        }

        // Every skipped pair is in the coverage log, noted as such.
        let coverage = run.coverage();
        assert_eq!(coverage.len(), assigned.len());
        for entry in &coverage {
            assert_eq!(entry.result, CheckResult::Skipped, "{entry:?}");
            assert_eq!(entry.note, "not checked", "{entry:?}");
        }

        // The seed task message was persisted before the loop ran.
        let transcript = run
            .store()
            .messages(run.run_id(), "reviewer:g1")
            .expect("transcript");
        assert_eq!(transcript[0].0, "user", "the seed is the first message");
        let seed: Vec<ContentBlock> = serde_json::from_str(&transcript[0].1).expect("seed json");
        let ContentBlock::Text { text } = &seed[0] else {
            panic!("the seed is a text block: {seed:?}")
        };
        assert!(text.contains("## Intent"), "{text}");
        assert!(text.contains("## Your group"), "{text}");
        assert!(text.contains("src/a.rs (+1 −1)"), "{text}");
        assert!(text.contains("## Patches"), "{text}");
        assert!(
            text.contains("- correctness: Read the whole changed function"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn reviewer_can_record_a_finding_and_complete_coverage() {
        let dir = crate::test_support::tempdir();
        // RIGHT-side lines 10..=14.
        let file = test_support::loaded("src/a.rs", "@@ -10,5 +10,5 @@\n c1\n c2\n c3\n c4\n c5\n");
        let snapshot = snapshot_of(vec![file]);
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(recording_calls("src/a.rs", 12)),
                ScriptStep::Text("all checked".into()),
            ],
        ));
        let inputs = stage_inputs(
            &dir,
            snapshot,
            llm,
            ScriptedInteraction::new(),
            RepoAiSettings::default(),
        );
        let run = inputs.cx.run.clone();
        // Another agent's finding must not show up in this reviewer's outcome.
        let other_id = run
            .add_finding(NewFinding {
                agent: AgentRef {
                    id: 99,
                    role: AgentRole::Reviewer,
                    label: "reviewer:g9".to_string(),
                },
                anchor: FindingAnchor::Summary {
                    path: "src/a.rs".to_string(),
                },
                severity: Severity::Nit,
                title: "other".to_string(),
                body: "not mine".to_string(),
                evidence: vec![Evidence {
                    path: "src/a.rs".to_string(),
                    line: Some(11),
                    note: "there".to_string(),
                }],
                checklist_item: None,
            })
            .expect("other finding");
        let brief = brief_with(vec![group("g1", "core", Risk::Medium, &["src/a.rs"])]);

        let outcome = run_reviewer(&inputs, &reviewer_agent(), &brief, &brief.groups[0])
            .await
            .expect("a finished reviewer");

        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        assert_eq!(outcome.findings.len(), 1, "{:?}", outcome.findings);
        assert_ne!(outcome.findings[0], other_id, "only this agent's findings");
        let finding = run
            .findings()
            .into_iter()
            .find(|f| f.id == outcome.findings[0])
            .expect("the finding is stored");
        assert_eq!(finding.agent.label, "reviewer:g1");

        // Full coverage: one entry per assigned pair, none skipped.
        let coverage = run.coverage();
        assert_eq!(coverage.len(), source_pairs("src/a.rs").len());
        assert!(
            coverage
                .iter()
                .all(|entry| entry.result != CheckResult::Skipped)
        );
    }

    /// `record_finding`, a `record_check` per assigned pair, then `done`.
    fn recording_calls(path: &str, line: u32) -> Vec<(String, Value)> {
        let mut calls = vec![(
            "record_finding".to_string(),
            json!({
                "path": path,
                "line": line,
                "severity": "bug",
                "title": "panics on empty",
                "body": "indexes without a length check",
                "evidence": [{"path": path, "line": line, "note": "the index"}],
            }),
        )];
        for item in default_items(FileKind::Source) {
            calls.push((
                "record_check".to_string(),
                json!({"file": path, "item": item, "result": "ok", "note": "checked"}),
            ));
        }
        calls.push(("done".to_string(), json!({"summary": "all checked"})));
        calls
    }

    #[test]
    fn initial_message_sections_in_order() {
        let mut snapshot = snapshot_with(&["src/a.rs", "src/huge.rs"]);
        snapshot.files[1].diff = Some(FileDiff {
            hunks: vec![added_hunk(1, 2), added_hunk(101, 3), added_hunk(201, 4)],
        });
        snapshot.threads = vec![
            thread("src/a.rs", Some(12), "alice", "already flagged this", true),
            thread("web/other.ts", Some(3), "bob", "not our file", false),
        ];
        let context = RepoContext {
            review_md: None,
            checks: vec![RepoCheck {
                id: "repo:1".to_string(),
                text: "no unwrap in FFI".to_string(),
                glob: None,
            }],
            instructions: vec![
                InstructionFile {
                    dir: String::new(),
                    path: "AGENTS.md".to_string(),
                    content: "root rules".to_string(),
                },
                InstructionFile {
                    dir: "src".to_string(),
                    path: "src/AGENTS.md".to_string(),
                    content: "src rules".to_string(),
                },
                InstructionFile {
                    dir: "web".to_string(),
                    path: "web/AGENTS.md".to_string(),
                    content: "web rules".to_string(),
                },
            ],
            skills: vec![SkillInfo {
                name: "pdf".to_string(),
                description: "Do things with PDFs.".to_string(),
                dir: ".claude/skills/pdf".to_string(),
            }],
        };
        let mut core = group("g1", "core", Risk::High, &["src/a.rs", "src/huge.rs"]);
        core.files[1].hunks = Some((0, 1));
        core.leads = vec!["compare with src/b.rs".to_string()];
        core.extra_checks = vec!["custom:changelog".to_string()];
        core.ci_notes = vec!["the e2e failure is flaky".to_string()];
        let brief = brief_with(vec![core.clone()]);
        let assigned = assignment(&core, &snapshot, &context.checks);

        // The seed is what the agent sees: the shared helper owns the Q&A
        // section, rendered exactly once.
        let qa = [("is empty allowed?".to_string(), "yes".to_string())];
        let text = seed_message(
            initial_message(&brief, &core, &snapshot, Some(&context), &assigned),
            &qa,
            None,
        );

        let order = [
            "## Intent",
            "## Your group",
            "## Patches",
            "## Checklist",
            "## Instructions",
            "## Skills",
            "## Existing threads",
            "## Known facts from the user",
        ];
        let mut last = 0;
        for section in order {
            let at = text
                .find(section)
                .unwrap_or_else(|| panic!("missing {section}:\n{text}"));
            assert!(at >= last, "{section} is out of order:\n{text}");
            last = at;
        }

        assert!(text.contains("the intent"), "{text}");
        assert!(text.contains("g1: core (risk: high)"), "{text}");
        assert!(text.contains("compare with src/b.rs"), "{text}");
        assert!(text.contains("the e2e failure is flaky"), "{text}");
        assert!(
            text.contains("src/huge.rs (+1 −1) [hunks 0-1 of 3]"),
            "{text}"
        );
        // The hunk-range file contributes only the hunks in range.
        assert!(text.contains("@@ -1,"), "{text}");
        assert!(text.contains("@@ -101,"), "{text}");
        assert!(
            !text.contains("@@ -201,"),
            "hunk 2 is out of range:\n{text}"
        );
        // Checklist: builtin text, repo check text, and the bare scout extra.
        assert!(
            text.contains("- intent: Compare the change with the brief's intent"),
            "{text}"
        );
        assert!(text.contains("- repo:1: no unwrap in FFI"), "{text}");
        assert!(text.contains("- custom:changelog"), "{text}");
        // Instructions are scoped to the group's paths.
        assert!(text.contains("root rules"), "{text}");
        assert!(text.contains("src rules"), "{text}");
        assert!(!text.contains("web rules"), "{text}");
        assert!(text.contains("- pdf: Do things with PDFs."), "{text}");
        // Threads only on the group's files, with author and resolved state.
        assert!(
            text.contains("src/a.rs:12 alice: already flagged this [resolved]"),
            "{text}"
        );
        assert!(!text.contains("web/other.ts"), "{text}");
        assert!(text.contains("Q: is empty allowed?\nA: yes"), "{text}");
        assert_eq!(
            text.matches("Q: is empty allowed?").count(),
            1,
            "the Q&A renders exactly once:\n{text}"
        );
    }

    #[test]
    fn initial_message_caps_a_huge_patch() {
        let mut snapshot = snapshot_with(&["src/a.rs"]);
        snapshot.files[0].file.patch = Some("x".repeat(25_000));
        let core = group("g1", "core", Risk::Low, &["src/a.rs"]);
        let brief = brief_with(vec![core.clone()]);
        let assigned = assignment(&core, &snapshot, &[]);

        let text = initial_message(&brief, &core, &snapshot, None, &assigned);

        assert!(text.contains("[patch truncated]"), "patch was not capped");
        assert!(
            !text.contains("## Skills"),
            "empty sections are omitted:\n{text}"
        );
        assert!(!text.contains("## Instructions"), "{text}");
        assert!(
            !text.contains("## Existing threads"),
            "no threads, no section:\n{text}"
        );
        assert!(
            !text.contains("## Known facts"),
            "the shared helper adds the Q&A section, not this message:\n{text}"
        );
    }

    #[test]
    fn reminder_caps_the_list_at_twenty_pairs() {
        // The exact template with room to spare: nothing is cut.
        assert_eq!(
            missing_text(&[
                ("a.rs".to_string(), "errors".to_string()),
                ("b.rs".to_string(), "tests".to_string()),
            ]),
            "You haven't recorded these checks yet: a.rs:errors, b.rs:tests. \
             Record each with record_check."
        );

        // Twenty-one pairs: exactly the first twenty are listed, then the
        // ellipsis; the twenty-first never appears.
        let pairs: Vec<(String, String)> = (1..=21)
            .map(|i| (format!("f{i}.rs"), format!("item{i}")))
            .collect();
        let text = missing_text(&pairs);

        assert!(
            text.starts_with("You haven't recorded these checks yet: f1.rs:item1, "),
            "{text}"
        );
        assert!(
            text.ends_with(", …. Record each with record_check."),
            "{text}"
        );
        assert!(!text.contains("f21.rs"), "the 21st pair leaked: {text}");
        let list = text
            .strip_prefix("You haven't recorded these checks yet: ")
            .and_then(|t| t.strip_suffix(". Record each with record_check."))
            .expect("the exact reminder template");
        let elements: Vec<&str> = list.split(", ").collect();
        assert_eq!(
            elements.len(),
            21,
            "20 pairs plus the ellipsis: {elements:?}"
        );
        assert_eq!(elements[0], "f1.rs:item1");
        assert_eq!(elements[19], "f20.rs:item20");
        assert_eq!(elements[20], "…");
    }

    #[tokio::test]
    async fn reviewer_without_a_snapshot_fails() {
        let dir = crate::test_support::tempdir();
        let llm = Arc::new(ScriptedLlm::new(model(), vec![]));
        let mut inputs = stage_inputs(
            &dir,
            snapshot_with(&["src/a.rs"]),
            llm,
            ScriptedInteraction::new(),
            RepoAiSettings::default(),
        );
        inputs.cx.snapshot = None;
        let brief = brief_with(vec![group("g1", "core", Risk::Low, &["src/a.rs"])]);

        let err = run_reviewer(&inputs, &reviewer_agent(), &brief, &brief.groups[0])
            .await
            .expect_err("no snapshot means no review");
        assert!(matches!(err, StageError::Failed(_)), "{err:?}");
    }
}
