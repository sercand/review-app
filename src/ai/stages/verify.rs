//! Duplicate merging and the verifier stage (spec §5.5).
//!
//! [`dedupe`] clusters findings that could be the same issue — same path,
//! and overlapping line ranges on the same side (a summary anchor covers
//! its whole path) — so the app can put one cluster in a single verifier
//! batch and let the verifier judge whether they really are the same
//! issue. [`run_verifier`] then runs one fresh agent per batch (≤ 8
//! findings): the findings, the existing threads on the path, the CI
//! annotations for the path, the scoped instructions, and read-only tools.
//! The verifier never sees the reviewers' conversations — only the finding
//! content (spec §5.5). Its `submit_verdicts` call is applied to the run:
//! `drop` → [`FindingState::DroppedByVerifier`] with the reason,
//! `downgrade` → the lower severity kept as a draft, `duplicate_of` →
//! dropped as a duplicate of the named finding.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use review_llm::Message;

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::context::RepoContext;
use crate::ai::prompts::system_prompt;
use crate::ai::review::{Finding, FindingAnchor, FindingState, RunState};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::{
    StageError, StageInputs, anchor_text, evidence_text, persist_seed, seed_message, thread_text,
    without_bash,
};
use crate::ai::tools::{AgentRef, AgentRole, Registry, SharedVerdicts, SubmitVerdicts, Tool};
use crate::model::comment::Anchor;

/// Model calls a verifier may make (spec §6.2): re-open the cited
/// locations for at most eight findings, so a short budget.
const MAX_TURNS: u32 = 20;
/// Finish nudges before the loop gives up on `submit_verdicts`.
const MAX_FINISH_REMINDERS: u32 = 2;
/// Findings per verifier batch (spec §5.5: batched by file, bounded).
pub(crate) const MAX_BATCH: usize = 8;
/// CI annotation lines shown for the batch's path.
const MAX_ANNOTATIONS: usize = 20;

/// Cluster findings that could be the same issue (spec §5.5: "same path
/// and overlapping lines"): same path, same side, overlapping line ranges
/// (a missing `start_line` is a one-line range), and a summary anchor
/// overlaps everything on its path. Deterministic: clusters ordered by
/// smallest id, ids ascending within a cluster; singletons are clusters.
pub fn dedupe(findings: &[Finding]) -> Vec<Vec<i64>> {
    // Union-find over finding indices: every overlapping pair merges.
    let mut parent: Vec<usize> = (0..findings.len()).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for i in 0..findings.len() {
        for j in (i + 1)..findings.len() {
            if overlaps(&findings[i], &findings[j]) {
                let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
                if ri != rj {
                    parent[ri.max(rj)] = ri.min(rj);
                }
            }
        }
    }
    let mut clusters: BTreeMap<usize, Vec<i64>> = BTreeMap::new();
    for (index, finding) in findings.iter().enumerate() {
        clusters
            .entry(root(&mut parent, index))
            .or_default()
            .push(finding.id);
    }
    let mut clusters: Vec<Vec<i64>> = clusters.into_values().collect();
    for cluster in &mut clusters {
        cluster.sort_unstable();
    }
    clusters.sort_by_key(|cluster| cluster[0]);
    clusters
}

/// Whether two findings could be the same issue: same path, and either a
/// summary anchor (which covers the whole path, both sides) or line anchors
/// on the same side whose ranges intersect.
fn overlaps(a: &Finding, b: &Finding) -> bool {
    if a.anchor.path() != b.anchor.path() {
        return false;
    }
    match (&a.anchor, &b.anchor) {
        (FindingAnchor::Line(a), FindingAnchor::Line(b)) => {
            a.side == b.side && {
                let (a_low, a_high) = line_range(a);
                let (b_low, b_high) = line_range(b);
                a_low <= b_high && b_low <= a_high
            }
        }
        _ => true,
    }
}

/// The inclusive line range of a line anchor; `start_line` may be above
/// `line`, and its absence means a one-line range.
fn line_range(anchor: &Anchor) -> (u32, u32) {
    let start = anchor.start_line.unwrap_or(anchor.line);
    (start.min(anchor.line), start.max(anchor.line))
}

/// Runs one verifier over `batch` — findings of one path, at most
/// [`MAX_BATCH`] — and applies its verdicts to the run.
///
/// The batch's findings are shown in full in the seed task message
/// (persisted before the loop starts) together with the threads and CI
/// annotations on the path; the verifier re-opens the cited locations with
/// read-only tools and finishes through `submit_verdicts`. Running out of
/// finish nudges or turns is a *normal completion*: whatever verdicts made
/// it in are applied, and the rest stay drafts.
pub async fn run_verifier(
    inputs: &StageInputs,
    agent: &AgentRef,
    batch: Vec<Finding>,
) -> Result<(), StageError> {
    if batch.is_empty() {
        return Ok(());
    }
    debug_assert!(
        batch.len() <= MAX_BATCH,
        "one verifier batch holds at most {MAX_BATCH} findings"
    );
    if batch.len() > MAX_BATCH {
        return Err(StageError::Failed(format!(
            "verifier batch of {} findings exceeds the {MAX_BATCH}-finding cap",
            batch.len()
        )));
    }
    let path = batch[0].anchor.path().to_string();
    debug_assert!(
        batch.iter().all(|finding| finding.anchor.path() == path),
        "one batch, one path"
    );
    let agent = agent.clone();
    let label = agent.label.clone();
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();

    let ids: Vec<i64> = batch.iter().map(|finding| finding.id).collect();
    let submitted: SharedVerdicts = Arc::new(Mutex::new(None));
    let registry = Registry::new(verifier_tools(
        Arc::new(SubmitVerdicts::new(ids, submitted.clone())),
        inputs.bash_disabled.as_deref(),
    ));

    let text = seed_message(
        initial_message(&batch, cx.snapshot.as_deref(), cx.context.as_deref()),
        &cx.run.qa(),
        inputs.bash_disabled.as_deref(),
    );
    persist_seed(&cx, &label, &text);

    let cell = submitted.clone();
    let spec = AgentSpec {
        agent,
        system: system_prompt(AgentRole::Verifier, &label),
        initial: vec![Message::user_text(text)],
        registry,
        max_turns: MAX_TURNS,
        finish_tool: Some("submit_verdicts"),
        on_finish: Arc::new(move || {
            if cell.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                FinishCheck::Accept
            } else {
                FinishCheck::Continue(
                    "Call submit_verdicts with exactly one verdict for every \
                     finding before you finish."
                        .to_string(),
                )
            }
        }),
        max_finish_reminders: MAX_FINISH_REMINDERS,
        elide_threshold: f64::from(inputs.settings.context_threshold),
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

    match outcome.stop {
        AgentStop::Finished | AgentStop::NoFinish | AgentStop::MaxTurns => {
            let verdicts = submitted.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(verdicts) = verdicts {
                apply_verdicts(&cx.run, &verdicts);
            }
            Ok(())
        }
        AgentStop::Cancelled => Err(StageError::Cancelled),
        AgentStop::Failed(message) => Err(StageError::Failed(message)),
    }
}

/// The verifier's tools (spec §5.5): the read-only repo tools, the snapshot
/// `gh_*` tools, `load_skill`, the interaction tools, `done`, and its
/// finish tool `submit_verdicts`. No record tools — it judges, it does not
/// review.
fn verifier_tools(submit: Arc<dyn Tool>, bash_disabled: Option<&str>) -> Vec<Arc<dyn Tool>> {
    const NAMES: [&str; 18] = [
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
        "done",
    ];
    let mut tools: Vec<Arc<dyn Tool>> = crate::ai::tools::all()
        .into_iter()
        .filter(|tool| NAMES.contains(&tool.definition().name.as_str()))
        .collect();
    tools.push(submit);
    without_bash(tools, bash_disabled)
}

/// Apply the verdicts (spec §5.5): `keep` changes nothing; `drop` moves the
/// finding to [`FindingState::DroppedByVerifier`] with the reason;
/// `downgrade` lowers the severity and keeps the draft; `duplicate_of`
/// drops it as a duplicate of the named finding, whose pointer replaces
/// the verdict reason.
fn apply_verdicts(run: &RunState, verdicts: &[crate::ai::tools::Verdict]) {
    use crate::ai::tools::VerdictKind;

    for verdict in verdicts {
        let reason = (!verdict.reason.trim().is_empty()).then(|| verdict.reason.clone());
        let result = match verdict.duplicate_of {
            Some(other) => run.set_finding_state(
                verdict.finding_id,
                FindingState::DroppedByVerifier,
                Some(format!("duplicate of #{other}")),
            ),
            None => match verdict.verdict {
                VerdictKind::Keep => continue,
                VerdictKind::Drop => run.set_finding_state(
                    verdict.finding_id,
                    FindingState::DroppedByVerifier,
                    reason,
                ),
                VerdictKind::Downgrade => match verdict.severity {
                    Some(severity) => run.downgrade_finding(verdict.finding_id, severity, reason),
                    // The tool requires a severity on a downgrade; a
                    // hand-built verdict without one keeps the recording.
                    None => continue,
                },
            },
        };
        if let Err(error) = result {
            tracing::warn!(
                %error,
                finding = verdict.finding_id,
                "verifier: applying a verdict failed"
            );
        }
    }
}

/// The verifier's single task message (spec §5.5): the findings in full,
/// the existing threads on the path, the CI annotations for the path, and
/// the scoped instructions.
fn initial_message(
    batch: &[Finding],
    snapshot: Option<&PrSnapshot>,
    context: Option<&RepoContext>,
) -> String {
    let path = batch[0].anchor.path();
    let mut out = String::from("## Findings to verify\n\n");
    for finding in batch {
        out.push_str(&format!(
            "### Finding #{} ({})\n\n",
            finding.id,
            finding.severity.as_str()
        ));
        out.push_str(&format!("Anchor: {}\n", anchor_text(&finding.anchor)));
        out.push_str(&format!(
            "Title: {}\n\n{}\n\n",
            finding.title,
            finding.body.trim_end()
        ));
        out.push_str("Evidence:\n");
        for evidence in &finding.evidence {
            out.push_str(&format!("- {}\n", evidence_text(evidence)));
        }
        if let Some(item) = &finding.checklist_item {
            out.push_str(&format!("Checklist item: {item}\n"));
        }
        out.push('\n');
    }

    if let Some(snapshot) = snapshot {
        let threads: Vec<&crate::model::thread::ReviewThread> = snapshot
            .threads
            .iter()
            .filter(|thread| thread.path == path)
            .collect();
        if !threads.is_empty() {
            out.push_str("## Existing threads\n\n");
            for thread in threads {
                out.push_str(&format!("- {}\n", thread_text(thread)));
            }
            out.push('\n');
        }

        let annotations = annotation_lines(snapshot, path);
        if !annotations.is_empty() {
            out.push_str("## CI annotations\n\n");
            for line in &annotations {
                out.push_str(&format!("- {line}\n"));
            }
            out.push('\n');
        }
    }

    if let Some(context) = context {
        let instructions = context.instructions_for(&[path]);
        if !instructions.is_empty() {
            out.push_str("## Instructions\n\n");
            for file in instructions {
                out.push_str(&format!(
                    "### {}\n\n{}\n",
                    file.path,
                    file.content.trim_end()
                ));
            }
        }
    }
    out
}

/// The `level: title: message` lines of `snapshot`'s check annotations on
/// `path`, capped at [`MAX_ANNOTATIONS`].
fn annotation_lines(snapshot: &PrSnapshot, path: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    'checks: for check in &snapshot.checks {
        for annotation in &check.annotations {
            if annotation.path != path {
                continue;
            }
            match annotation
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
            {
                Some(title) => lines.push(format!(
                    "{}: {}: {}",
                    annotation.level, title, annotation.message
                )),
                None => lines.push(format!("{}: {}", annotation.level, annotation.message)),
            }
            if lines.len() == MAX_ANNOTATIONS {
                break 'checks;
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use review_llm::ContentBlock;
    use review_sandbox::LocalHost;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::ai::agent::AgentControl;
    use crate::ai::context::{InstructionFile, RepoContext};
    use crate::ai::events::PipelineEvent;
    use crate::ai::review::{Evidence, NewFinding, Severity};
    use crate::ai::stages::{StageInputs, next_agent_id};
    use crate::ai::store::{PrKey, RunKind, Store};
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
    use crate::github::checks::CheckAnnotation;
    use crate::github::files::LoadedFile;
    use crate::model::diff::Side;
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

    /// A snapshot over one [`test_support::file`] per path.
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

    /// A thread on `path` with one comment by `author`.
    fn thread(path: &str, line: Option<u32>, author: &str, body: &str) -> ReviewThread {
        ReviewThread {
            id: "T1".to_string(),
            is_resolved: false,
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

    /// A fresh run state with its event receiver kept for assertions.
    fn run_with_events() -> (Arc<RunState>, mpsc::UnboundedReceiver<PipelineEvent>) {
        let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
        let session = store
            .session_id(&PrKey {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 7,
            })
            .expect("session row");
        let run = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .expect("run row");
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(RunState::new(store, session, run, tx)), rx)
    }

    /// A `StageInputs` over a temp-dir [`LocalHost`] carrying `snapshot`
    /// and `run`.
    /// The verifier agent reference for `path`, as the pipeline injects it.
    fn verifier_agent(path: &str) -> AgentRef {
        AgentRef {
            id: next_agent_id(),
            role: AgentRole::Verifier,
            label: format!("verifier:{}", path.replace('/', "-")),
        }
    }

    fn stage_inputs(
        dir: &tempfile::TempDir,
        snapshot: PrSnapshot,
        llm: Arc<ScriptedLlm>,
        run: Arc<RunState>,
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
                interaction: Arc::new(ScriptedInteraction::new()),
                allowed: Arc::new(Mutex::new(Vec::new())),
                for_review: Arc::new(Mutex::new(Vec::new())),
                snapshot: Some(Arc::new(snapshot)),
                context: None,
                run,
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
            repo: RepoAiSettings::default(),
            bash_disabled: None,
            incremental: None,
            incremental_log: None,
        }
    }

    /// A line finding with id `id` for the dedupe table.
    fn line(id: i64, path: &str, side: Side, start: u32, end: u32) -> Finding {
        Finding {
            id,
            run_id: 1,
            agent: AgentRef {
                id: 2,
                role: AgentRole::Reviewer,
                label: "reviewer:g1".to_string(),
            },
            anchor: FindingAnchor::Line(Anchor {
                path: path.to_string(),
                side,
                line: end,
                start_line: (start != end).then_some(start),
                start_side: (start != end).then_some(side),
            }),
            severity: Severity::Bug,
            title: format!("f{id}"),
            body: "b".to_string(),
            evidence: Vec::new(),
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        }
    }

    /// A summary finding with id `id` for the dedupe table.
    fn summary(id: i64, path: &str) -> Finding {
        Finding {
            anchor: FindingAnchor::Summary {
                path: path.to_string(),
            },
            ..line(id, path, Side::Right, 1, 1)
        }
    }

    #[test]
    fn dedupe_clusters_overlapping_ranges() {
        // Overlapping ranges on one path and side cluster; a disjoint
        // range on the same path does not.
        let findings = vec![
            line(1, "src/a.rs", Side::Right, 10, 14),
            line(2, "src/a.rs", Side::Right, 12, 16),
            line(3, "src/a.rs", Side::Right, 40, 42),
        ];
        assert_eq!(dedupe(&findings), vec![vec![1, 2], vec![3]]);

        // Another path never clusters, even at the same lines.
        let findings = vec![
            line(1, "src/a.rs", Side::Right, 10, 14),
            line(2, "src/b.rs", Side::Right, 10, 14),
        ];
        assert_eq!(dedupe(&findings), vec![vec![1], vec![2]]);

        // The same range on the other side is a different location.
        let findings = vec![
            line(1, "src/a.rs", Side::Right, 10, 14),
            line(2, "src/a.rs", Side::Left, 10, 14),
        ];
        assert_eq!(dedupe(&findings), vec![vec![1], vec![2]]);

        // A single-line anchor (no start_line) is its own one-line range:
        // it touches 8-10 but not 11-12.
        let findings = vec![
            line(1, "src/a.rs", Side::Right, 10, 10),
            line(2, "src/a.rs", Side::Right, 8, 10),
            line(3, "src/a.rs", Side::Right, 11, 12),
        ];
        assert_eq!(dedupe(&findings), vec![vec![1, 2], vec![3]]);

        // Overlap is transitive: 1-5 touches 4-9, which touches 10-12 —
        // two clusters, not three.
        let findings = vec![
            line(3, "src/a.rs", Side::Right, 10, 12),
            line(1, "src/a.rs", Side::Right, 1, 5),
            line(2, "src/a.rs", Side::Right, 4, 9),
        ];
        assert_eq!(dedupe(&findings), vec![vec![1, 2], vec![3]]);

        // A summary anchor covers its whole path, both sides; input order
        // does not matter — clusters sort by smallest id, ids ascending.
        let findings = vec![
            summary(4, "src/a.rs"),
            line(3, "src/a.rs", Side::Left, 40, 42),
            line(2, "src/a.rs", Side::Right, 10, 14),
            summary(5, "src/b.rs"),
        ];
        assert_eq!(dedupe(&findings), vec![vec![2, 3, 4], vec![5]]);

        // The empty input is the empty clustering.
        assert_eq!(dedupe(&[]), Vec::<Vec<i64>>::new());
    }

    #[tokio::test]
    async fn verifier_drops_with_reason_and_downgrades() {
        let dir = crate::test_support::tempdir();
        let mut snapshot = snapshot_with(&["src/a.rs"]);
        snapshot.threads = vec![
            thread("src/a.rs", Some(12), "alice", "already flagged this"),
            thread("web/other.ts", Some(3), "bob", "not our file"),
        ];
        snapshot.checks = vec![crate::ai::snapshot::CheckSummary {
            name: "lint".to_string(),
            conclusion: Some("failure".to_string()),
            summary: None,
            text: None,
            annotations: vec![
                CheckAnnotation {
                    path: "src/a.rs".to_string(),
                    start_line: 12,
                    end_line: 12,
                    level: "failure".to_string(),
                    title: Some("clippy".to_string()),
                    message: "possible index out of bounds".to_string(),
                },
                CheckAnnotation {
                    path: "web/other.ts".to_string(),
                    start_line: 3,
                    end_line: 3,
                    level: "warning".to_string(),
                    title: None,
                    message: "unused variable".to_string(),
                },
            ],
        }];
        let (run, mut rx) = run_with_events();
        let mut inputs = stage_inputs(
            &dir,
            snapshot,
            Arc::new(ScriptedLlm::new(model(), vec![])),
            run.clone(),
        );
        inputs.cx.context = Some(Arc::new(RepoContext {
            review_md: None,
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
                    content: "src rules".to_string(),
                },
                InstructionFile {
                    dir: "web".to_string(),
                    path: "web/AGENTS.md".to_string(),
                    content: "web rules".to_string(),
                },
            ],
            skills: Vec::new(),
        }));

        let mut batch = Vec::new();
        for (index, (title, severity, line_no)) in [
            ("panics on empty", Severity::Bug, 12u32),
            ("racy counter", Severity::Risk, 20),
            ("is empty allowed?", Severity::Question, 30),
        ]
        .iter()
        .enumerate()
        {
            batch.push(
                run.add_finding(NewFinding {
                    agent: AgentRef {
                        id: 2,
                        role: AgentRole::Reviewer,
                        label: "reviewer:g1".to_string(),
                    },
                    anchor: FindingAnchor::Line(test_support::line_anchor("src/a.rs", *line_no)),
                    severity: *severity,
                    title: title.to_string(),
                    body: format!("body {index}"),
                    evidence: vec![Evidence {
                        path: "src/b.rs".to_string(),
                        line: Some(4),
                        note: "the caller".to_string(),
                    }],
                    checklist_item: Some("correctness".to_string()),
                })
                .expect("finding"),
            );
        }
        let (drop_id, downgrade_id, duplicate_id) = (batch[0], batch[1], batch[2]);

        // One turn: the verdicts for the whole batch.
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::ToolCalls(vec![(
                "submit_verdicts".to_string(),
                json!({"verdicts": [
                    {"finding_id": drop_id, "verdict": "drop",
                     "reason": "the is_empty() guard at line 11 covers it"},
                    {"finding_id": downgrade_id, "verdict": "downgrade", "severity": "nit",
                     "reason": "real, but a style point"},
                    {"finding_id": duplicate_id, "verdict": "drop", "reason": "same issue",
                     "duplicate_of": drop_id},
                ]}),
            )])],
        ));
        inputs.llm = llm;

        let findings = run.findings();
        // The three FindingAdded events come first; drain them.
        for id in [drop_id, downgrade_id, duplicate_id] {
            assert_eq!(rx.try_recv(), Ok(PipelineEvent::FindingAdded { id }));
        }
        run_verifier(&inputs, &verifier_agent("src/a.rs"), findings)
            .await
            .expect("a finished verifier");

        let by_id = |id: i64| {
            run.findings()
                .into_iter()
                .find(|finding| finding.id == id)
                .expect("the finding")
        };
        let dropped = by_id(drop_id);
        assert_eq!(dropped.state, FindingState::DroppedByVerifier);
        assert_eq!(
            dropped.state_reason.as_deref(),
            Some("the is_empty() guard at line 11 covers it")
        );
        assert_eq!(dropped.severity, Severity::Bug, "a drop keeps its severity");
        let downgraded = by_id(downgrade_id);
        assert_eq!(downgraded.severity, Severity::Nit);
        assert_eq!(
            downgraded.state,
            FindingState::Draft,
            "a downgrade stays a draft"
        );
        assert_eq!(
            downgraded.state_reason.as_deref(),
            Some("real, but a style point")
        );
        let duplicated = by_id(duplicate_id);
        assert_eq!(duplicated.state, FindingState::DroppedByVerifier);
        assert_eq!(
            duplicated.state_reason.as_deref(),
            Some(format!("duplicate of #{drop_id}").as_str()),
            "the duplicate pointer replaces the verdict reason"
        );

        // The store carries the same rows, and the UI was told each change.
        let rows = run.store().finding_list(run.session_id()).expect("rows");
        assert_eq!(rows, run.findings());
        assert_eq!(
            rx.try_recv(),
            Ok(PipelineEvent::FindingChanged { id: drop_id })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(PipelineEvent::FindingChanged { id: downgrade_id })
        );
        assert_eq!(
            rx.try_recv(),
            Ok(PipelineEvent::FindingChanged { id: duplicate_id })
        );

        // The seed was persisted under the path-slug label and carries the
        // findings in full, the threads and annotations on the path, and
        // the scoped instructions.
        let transcript = run
            .store()
            .messages(run.run_id(), "verifier:src-a.rs")
            .expect("transcript");
        assert_eq!(transcript[0].0, "user", "the seed is the first message");
        let seed: Vec<ContentBlock> = serde_json::from_str(&transcript[0].1).expect("seed json");
        let ContentBlock::Text { text } = &seed[0] else {
            panic!("the seed is a text block: {seed:?}")
        };
        assert!(text.contains("## Findings to verify"), "{text}");
        assert!(
            text.contains(&format!("### Finding #{} (bug)", dropped.id)),
            "{text}"
        );
        assert!(text.contains("Anchor: src/a.rs:12 (RIGHT)"), "{text}");
        assert!(text.contains("Title: panics on empty"), "{text}");
        assert!(text.contains("src/b.rs:4 — the caller"), "{text}");
        assert!(text.contains("Checklist item: correctness"), "{text}");
        assert!(text.contains("## Existing threads"), "{text}");
        assert!(
            text.contains("src/a.rs:12 alice: already flagged this"),
            "{text}"
        );
        assert!(!text.contains("not our file"), "{text}");
        assert!(text.contains("## CI annotations"), "{text}");
        assert!(
            text.contains("failure: clippy: possible index out of bounds"),
            "{text}"
        );
        assert!(!text.contains("unused variable"), "{text}");
        assert!(text.contains("## Instructions"), "{text}");
        assert!(text.contains("root rules"), "{text}");
        assert!(text.contains("src rules"), "{text}");
        assert!(!text.contains("web rules"), "{text}");
    }

    #[tokio::test]
    async fn verifier_without_verdicts_leaves_findings_untouched() {
        let dir = crate::test_support::tempdir();
        let (run, _rx) = run_with_events();
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("I need to look around".into()),
                ScriptStep::Text("really, done".into()),
                ScriptStep::Text("I did call it…".into()),
            ],
        ));
        let inputs = stage_inputs(&dir, snapshot_with(&["src/a.rs"]), llm, run.clone());
        let id = run
            .add_finding(NewFinding {
                agent: AgentRef {
                    id: 2,
                    role: AgentRole::Reviewer,
                    label: "reviewer:g1".to_string(),
                },
                anchor: FindingAnchor::Line(test_support::line_anchor("src/a.rs", 12)),
                severity: Severity::Bug,
                title: "panics on empty".to_string(),
                body: "line 12 indexes without a check".to_string(),
                evidence: vec![Evidence {
                    path: "src/a.rs".to_string(),
                    line: Some(12),
                    note: "the index".to_string(),
                }],
                checklist_item: None,
            })
            .expect("finding");

        run_verifier(&inputs, &verifier_agent("src/a.rs"), run.findings())
            .await
            .expect("running out of nudges is a normal completion");

        let finding = run
            .findings()
            .into_iter()
            .find(|f| f.id == id)
            .expect("finding");
        assert_eq!(finding.state, FindingState::Draft);
        assert_eq!(finding.severity, Severity::Bug);
        assert_eq!(finding.state_reason, None);
    }
}
