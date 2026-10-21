//! The assembler stage (spec §5.6): the draft review.
//!
//! [`run_assembler`] runs one agent with **no repo tools** — everything it
//! needs is in the seed task message: the brief's intent, the kept findings
//! in full, the summary-anchored findings called out separately (they can
//! never become inline comments), and the [`coverage_digest`] so even a
//! clean review says what it looked at. It finishes through
//! `submit_review_draft`; unlike the verifier, a draft is mandatory here,
//! so running out of nudges or turns is [`StageError::NoFinish`].
//!
//! [`verdict_floor`] is the app's severity rule (spec §5.6: any kept `bug`
//! means request changes, only nits mean approve or comment). The model may
//! soften it — "unless clearly trivial" — but visibly: when its verdict is
//! less severe than the floor, the verdict stands and the rationale gains a
//! `(note: the findings suggest <floor>)` prefix.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use review_llm::Message;
use serde::{Deserialize, Serialize};

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::prompts::system_prompt;
use crate::ai::review::{CheckResult, CoverageEntry, Finding, FindingAnchor, Severity};
use crate::ai::stages::scout::Brief;
use crate::ai::stages::{
    StageError, StageInputs, anchor_text, evidence_text, next_agent_id, persist_seed,
};
use crate::ai::tools::{AgentRef, AgentRole, Registry, SharedDraft, SubmitReviewDraft, Tool};

/// The assembler's agent label, also its transcript key.
const LABEL: &str = "assembler";
/// Model calls the assembler may make (spec §6.2): it only writes, it does
/// not explore.
const MAX_TURNS: u32 = 12;
/// Combined allowance for finish nudges before the stage gives up.
const MAX_FINISH_REMINDERS: u32 = 2;

/// The suggested review verdict (spec §5.6); maps to `ReviewEvent` in
/// Plan 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Ship it.
    Approve,
    /// Comment without blocking.
    Comment,
    /// Fix before merge.
    RequestChanges,
}

impl Verdict {
    /// The stored and wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::Comment => "comment",
            Verdict::RequestChanges => "request_changes",
        }
    }

    /// Inverse of [`Verdict::as_str`].
    pub fn parse(raw: &str) -> anyhow::Result<Verdict> {
        match raw {
            "approve" => Ok(Verdict::Approve),
            "comment" => Ok(Verdict::Comment),
            "request_changes" => Ok(Verdict::RequestChanges),
            _ => anyhow::bail!("unrecognised verdict {raw:?}"),
        }
    }

    /// How blocking the verdict is; only used to compare against the floor.
    fn rank(&self) -> u8 {
        match self {
            Verdict::Approve => 0,
            Verdict::Comment => 1,
            Verdict::RequestChanges => 2,
        }
    }
}

/// The draft review the assembler produces (spec §5.6): the summary the
/// user posts, the suggested verdict with its one-line rationale, and the
/// body rewrites (finding id → new body) the user's kept findings carry
/// instead of their recorded bodies. Anchors and severities never change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewDraft {
    /// What the PR does, the main issues, and what was checked.
    pub summary: String,
    /// The suggested verdict.
    pub verdict: Verdict,
    /// The one-line rationale; may carry the floor note.
    pub rationale: String,
    /// finding id → rewritten body.
    pub rewrites: BTreeMap<i64, String>,
}

/// The app's verdict rule for `findings` (spec §5.6): any `bug` means
/// request changes; else any `risk` or `question` means comment; else
/// approve. The caller passes the kept findings — the floor is computed on
/// exactly what the review will carry.
pub fn verdict_floor(findings: &[Finding]) -> Verdict {
    if findings.iter().any(|f| f.severity == Severity::Bug) {
        Verdict::RequestChanges
    } else if findings
        .iter()
        .any(|f| matches!(f.severity, Severity::Risk | Severity::Question))
    {
        Verdict::Comment
    } else {
        Verdict::Approve
    }
}

/// The coverage digest for the task message: one line per checklist item
/// aggregated across files, sorted by item, plus a total line. Format:
/// `item: ok=N issue=N n/a=N skipped=N`.
pub fn coverage_digest(coverage: &[CoverageEntry]) -> String {
    // Per item: [ok, issue, n/a, skipped].
    let mut per_item: BTreeMap<&str, [u32; 4]> = BTreeMap::new();
    let mut total = [0u32; 4];
    for entry in coverage {
        let index = match entry.result {
            CheckResult::Ok => 0,
            CheckResult::Issue => 1,
            CheckResult::NotApplicable => 2,
            CheckResult::Skipped => 3,
        };
        per_item.entry(entry.item.as_str()).or_default()[index] += 1;
        total[index] += 1;
    }
    let mut out = String::new();
    for (item, counts) in &per_item {
        out.push_str(&digest_line(item, counts));
    }
    out.push_str(&digest_line("total", &total));
    out
}

/// One digest line: `label: ok=N issue=N n/a=N skipped=N`.
fn digest_line(label: &str, counts: &[u32; 4]) -> String {
    format!(
        "{label}: ok={} issue={} n/a={} skipped={}\n",
        counts[0], counts[1], counts[2], counts[3]
    )
}

/// Runs the assembler and returns its draft.
///
/// The seed task message is persisted before the loop starts. The registry
/// is the draft tool and `done` — no repo tools, no interaction tools. A
/// `submit_review_draft` call that passed the tool's validation fills the
/// shared cell; the finish check accepts only then. A draft is mandatory:
/// running out of nudges or turns is [`StageError::NoFinish`]. On success
/// the floor note ([`verdict_floor`]) is applied to the returned draft.
pub async fn run_assembler(
    inputs: &StageInputs,
    brief: &Brief,
    kept: &[Finding],
    coverage: &[CoverageEntry],
) -> Result<ReviewDraft, StageError> {
    let agent = AgentRef {
        id: next_agent_id(),
        role: AgentRole::Assembler,
        label: LABEL.to_string(),
    };
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();

    let ids: Vec<i64> = kept.iter().map(|finding| finding.id).collect();
    let submitted: SharedDraft = Arc::new(Mutex::new(None));
    let registry = Registry::new(assembler_tools(Arc::new(SubmitReviewDraft::new(
        ids,
        submitted.clone(),
    ))));

    let text = initial_message(brief, kept, coverage);
    persist_seed(&cx, LABEL, &text);

    let cell = submitted.clone();
    let spec = AgentSpec {
        agent,
        system: system_prompt(AgentRole::Assembler, LABEL),
        initial: vec![Message::user_text(text)],
        registry,
        max_turns: MAX_TURNS,
        finish_tool: Some("submit_review_draft"),
        on_finish: Arc::new(move || {
            if cell.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                FinishCheck::Accept
            } else {
                FinishCheck::Continue(
                    "Call submit_review_draft with the complete review draft \
                     before you finish."
                        .to_string(),
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

    let draft = submitted.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (outcome.stop, draft) {
        (AgentStop::Finished, Some(mut draft)) => {
            apply_floor_note(&mut draft, kept);
            Ok(draft)
        }
        // The loop only finishes with the finish tool accepted, so `None`
        // here means the reminders ran out on an empty cell. A draft is
        // mandatory: there is nothing to show the user without one.
        (AgentStop::Finished, None) => Err(StageError::NoFinish),
        (AgentStop::NoFinish, _) | (AgentStop::MaxTurns, _) => Err(StageError::NoFinish),
        (AgentStop::Cancelled, _) => Err(StageError::Cancelled),
        (AgentStop::Failed(message), _) => Err(StageError::Failed(message)),
    }
}

/// Soften the model's verdict below the floor and the rationale says so
/// (spec §5.6 "unless clearly trivial", visibly): the verdict is kept, the
/// rationale gains `(note: the findings suggest <floor>) `. Equal or more
/// severe leaves the draft untouched.
fn apply_floor_note(draft: &mut ReviewDraft, kept: &[Finding]) {
    let floor = verdict_floor(kept);
    if draft.verdict.rank() < floor.rank() {
        draft.rationale = format!(
            "(note: the findings suggest {}) {}",
            floor.as_str(),
            draft.rationale
        );
    }
}

/// The assembler's tools (spec §5.6): no repo tools — everything it needs
/// is in its context — just its finish tool `submit_review_draft` and
/// `done`.
fn assembler_tools(submit: Arc<dyn Tool>) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = crate::ai::tools::all()
        .into_iter()
        .filter(|tool| tool.definition().name == "done")
        .collect();
    tools.push(submit);
    tools
}

/// The assembler's single task message (spec §5.6): the intent, the kept
/// findings in full, the summary-anchored findings called out, the coverage
/// digest, and the task reminder.
fn initial_message(brief: &Brief, kept: &[Finding], coverage: &[CoverageEntry]) -> String {
    let mut out = String::from("## Intent\n\n");
    out.push_str(brief.intent.trim_end());
    out.push_str("\n\n## Kept findings\n\n");
    if kept.is_empty() {
        out.push_str("None.\n\n");
    }
    for finding in kept {
        out.push_str(&format!(
            "#{} [{}] {}\n",
            finding.id,
            finding.severity.as_str(),
            anchor_text(&finding.anchor)
        ));
        out.push_str(&format!(
            "Title: {}\n\n{}\n\n",
            finding.title,
            finding.body.trim_end()
        ));
        out.push_str("Evidence:\n");
        for evidence in &finding.evidence {
            out.push_str(&format!("- {}\n", evidence_text(evidence)));
        }
        out.push('\n');
    }

    out.push_str("## Summary findings\n\n");
    let summary: Vec<&Finding> = kept
        .iter()
        .filter(|finding| matches!(finding.anchor, FindingAnchor::Summary { .. }))
        .collect();
    if summary.is_empty() {
        out.push_str("None.\n");
    } else {
        for finding in summary {
            out.push_str(&format!(
                "- #{} {} — this finding cannot be an inline comment; cover \
                 it in the review summary.\n",
                finding.id,
                finding.anchor.path()
            ));
        }
    }
    out.push('\n');

    out.push_str("## Coverage\n\n");
    out.push_str(&coverage_digest(coverage));
    out.push('\n');

    out.push_str(
        "## Task\n\n\
         Write the review draft and call submit_review_draft. The summary \
         must say what the PR does, name the main issues, and state what was \
         checked and what was skipped — take that from the coverage digest \
         above, so a clean review still says what it looked at. You may \
         rewrite kept finding bodies for tone and concision through \
         `rewrites`; anchors and severities do not change.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use review_llm::{ContentBlock, ToolResultPart};
    use review_sandbox::LocalHost;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use crate::ai::agent::AgentControl;
    use crate::ai::review::{CheckResult, Evidence, FindingAnchor, FindingState, Severity};
    use crate::ai::stages::scout::Brief;
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
    use crate::persist::settings::{AiSettings, ModelSettings, RepoAiSettings};
    use crate::test_support;

    use super::*;

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: None,
        }
    }

    /// A `StageInputs` over a temp-dir [`LocalHost`]; the assembler needs no
    /// snapshot and no repo tools, so the template carries neither.
    fn stage_inputs(dir: &tempfile::TempDir, llm: Arc<ScriptedLlm>) -> StageInputs {
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
                snapshot: None,
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
            repo: RepoAiSettings::default(),
        }
    }

    /// A line finding with id `id` for the stage tests.
    fn line(id: i64, severity: Severity, path: &str, line_no: u32) -> Finding {
        Finding {
            id,
            run_id: 1,
            agent: AgentRef {
                id: 2,
                role: AgentRole::Reviewer,
                label: "reviewer:g1".to_string(),
            },
            anchor: FindingAnchor::Line(test_support::line_anchor(path, line_no)),
            severity,
            title: format!("f{id}"),
            body: format!("body {id}"),
            evidence: vec![Evidence {
                path: "src/b.rs".to_string(),
                line: Some(4),
                note: "the caller".to_string(),
            }],
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        }
    }

    /// A summary finding with id `id`.
    fn summary(id: i64, severity: Severity, path: &str) -> Finding {
        Finding {
            anchor: FindingAnchor::Summary {
                path: path.to_string(),
            },
            ..line(id, severity, path, 1)
        }
    }

    fn check(file: &str, item: &str, result: CheckResult) -> CoverageEntry {
        CoverageEntry {
            file: file.to_string(),
            item: item.to_string(),
            result,
            note: "checked".to_string(),
        }
    }

    fn brief() -> Brief {
        Brief {
            intent: "adds the retry loop".to_string(),
            description_matches: true,
            unexplained: Vec::new(),
            groups: Vec::new(),
            suggested_commands: Vec::new(),
            suggested_prepare: Vec::new(),
        }
    }

    /// The seed task text of the first request.
    fn request_user_text(request: &review_llm::GenerateRequest) -> String {
        request.messages[0]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Every tool-result text across a request's history.
    fn tool_result_text(request: &review_llm::GenerateRequest) -> String {
        request
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|part| match part {
                            ToolResultPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn verdict_floor_table() {
        assert_eq!(verdict_floor(&[]), Verdict::Approve, "nothing kept");
        assert_eq!(
            verdict_floor(&[line(1, Severity::Nit, "a.rs", 1)]),
            Verdict::Approve,
            "nits never block"
        );
        assert_eq!(
            verdict_floor(&[
                line(1, Severity::Nit, "a.rs", 1),
                line(2, Severity::Nit, "b.rs", 2),
            ]),
            Verdict::Approve,
            "nit-only approves (or comments) per spec §5.6"
        );
        assert_eq!(
            verdict_floor(&[line(1, Severity::Question, "a.rs", 1)]),
            Verdict::Comment
        );
        assert_eq!(
            verdict_floor(&[line(1, Severity::Risk, "a.rs", 1)]),
            Verdict::Comment
        );
        assert_eq!(
            verdict_floor(&[
                line(1, Severity::Nit, "a.rs", 1),
                line(2, Severity::Question, "b.rs", 2),
            ]),
            Verdict::Comment,
            "a question lifts nits to a comment"
        );
        assert_eq!(
            verdict_floor(&[line(1, Severity::Bug, "a.rs", 1)]),
            Verdict::RequestChanges
        );
        assert_eq!(
            verdict_floor(&[
                line(1, Severity::Risk, "a.rs", 1),
                line(2, Severity::Bug, "b.rs", 2),
            ]),
            Verdict::RequestChanges,
            "a bug beats the risk"
        );
        assert_eq!(
            verdict_floor(&[
                line(1, Severity::Bug, "a.rs", 1),
                line(2, Severity::Risk, "b.rs", 2),
                line(3, Severity::Question, "c.rs", 3),
                line(4, Severity::Nit, "d.rs", 4),
            ]),
            Verdict::RequestChanges,
            "mixed: the bug decides"
        );

        // The wire spelling Plan 3 maps to `ReviewEvent`.
        assert_eq!(
            serde_json::to_value(Verdict::Approve).unwrap(),
            json!("approve")
        );
        assert_eq!(
            serde_json::to_value(Verdict::RequestChanges).unwrap(),
            json!("request_changes")
        );
    }

    #[tokio::test]
    async fn assembler_draft_applies_rewrites_to_kept_only() {
        let dir = crate::test_support::tempdir();
        let kept = vec![
            line(1, Severity::Bug, "src/a.rs", 12),
            line(2, Severity::Nit, "src/b.rs", 8),
        ];
        let draft_call = |rewrites: serde_json::Value| {
            ScriptStep::ToolCalls(vec![(
                "submit_review_draft".to_string(),
                json!({
                    "summary": "adds retries",
                    "verdict": "approve",
                    "rationale": "ship it",
                    "rewrites": rewrites,
                }),
            )])
        };
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                // A rewrite aimed at finding 9 — recorded but dropped, so
                // outside the kept set — rejects the whole call.
                draft_call(json!([
                    {"finding_id": 1, "body": "rewritten #1"},
                    {"finding_id": 9, "body": "not kept"},
                ])),
                draft_call(json!([{"finding_id": 1, "body": "rewritten #1"}])),
            ],
        ));
        let inputs = stage_inputs(&dir, llm.clone());

        let draft = run_assembler(&inputs, &brief(), &kept, &[])
            .await
            .expect("the resubmitted draft finishes the stage");

        assert_eq!(
            draft.verdict,
            Verdict::Approve,
            "the model's verdict stands"
        );
        assert_eq!(
            draft.rationale, "(note: the findings suggest request_changes) ship it",
            "a kept bug softens the verdict visibly, not silently"
        );
        let expected: BTreeMap<i64, String> = [(1i64, "rewritten #1")]
            .into_iter()
            .map(|(id, body)| (id, body.to_string()))
            .collect();
        assert_eq!(
            draft.rewrites, expected,
            "only the kept rewrite is in the draft"
        );

        // The rejection gave the model a turn to correct itself, and named
        // the offender.
        let requests = llm.requests();
        assert_eq!(requests.len(), 2, "the rejected call was retried");
        let error = tool_result_text(&requests[1]);
        assert!(error.contains("9"), "{error}");
        assert!(error.contains("kept"), "{error}");
    }

    #[tokio::test]
    async fn assembler_request_includes_coverage_digest() {
        let dir = crate::test_support::tempdir();
        let coverage = vec![
            check("src/a.rs", "correctness", CheckResult::Issue),
            check("src/a.rs", "correctness", CheckResult::Ok),
            check("src/b.rs", "correctness", CheckResult::NotApplicable),
            check("src/a.rs", "tests", CheckResult::Ok),
            check("src/b.rs", "tests", CheckResult::Skipped),
        ];
        // The pure renderer, pinned exactly.
        assert_eq!(
            coverage_digest(&coverage),
            "correctness: ok=1 issue=1 n/a=1 skipped=0\n\
             tests: ok=1 issue=0 n/a=0 skipped=1\n\
             total: ok=2 issue=1 n/a=1 skipped=1\n"
        );

        let kept = vec![
            line(1, Severity::Nit, "src/a.rs", 12),
            summary(2, Severity::Nit, "logo.png"),
        ];
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![ScriptStep::ToolCalls(vec![(
                "submit_review_draft".to_string(),
                json!({
                    "summary": "adds retries",
                    "verdict": "comment",
                    "rationale": "one nit",
                    "rewrites": [],
                }),
            )])],
        ));
        let inputs = stage_inputs(&dir, llm.clone());

        let draft = run_assembler(&inputs, &brief(), &kept, &coverage)
            .await
            .expect("a submitted draft");
        assert_eq!(
            draft.rationale, "one nit",
            "comment meets the Approve floor: no note"
        );

        let requests = llm.requests();
        let text = request_user_text(&requests[0]);
        assert!(
            text.contains(
                "## Coverage\n\n\
                 correctness: ok=1 issue=1 n/a=1 skipped=0\n\
                 tests: ok=1 issue=0 n/a=0 skipped=1\n\
                 total: ok=2 issue=1 n/a=1 skipped=1\n"
            ),
            "{text}"
        );

        // The sections, in order.
        let mut last = 0;
        for section in [
            "## Intent",
            "## Kept findings",
            "## Summary findings",
            "## Coverage",
            "## Task",
        ] {
            let at = text
                .find(section)
                .unwrap_or_else(|| panic!("missing {section}:\n{text}"));
            assert!(at >= last, "{section} is out of order:\n{text}");
            last = at;
        }

        assert!(text.contains("adds the retry loop"), "{text}");
        assert!(text.contains("#1 [nit] src/a.rs:12 (RIGHT)"), "{text}");
        assert!(text.contains("body 1"), "{text}");
        assert!(text.contains("src/b.rs:4 — the caller"), "{text}");
        assert!(text.contains("#2 [nit] logo.png (file summary)"), "{text}");
        assert!(
            text.contains("- #2 logo.png"),
            "summary findings listed by id and path:\n{text}"
        );
        assert!(
            text.contains("cannot be an inline comment"),
            "the summary items are called out:\n{text}"
        );
        assert!(
            text.contains("skipped"),
            "the task reminds that the summary says what was skipped:\n{text}"
        );

        // No repo tools: the registry is the draft tool and `done`, nothing
        // else.
        let names: Vec<String> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(names, vec!["done", "submit_review_draft"]);

        // The seed was persisted under the assembler label before the loop.
        let run = inputs.cx.run.clone();
        let transcript = run
            .store()
            .messages(run.run_id(), "assembler")
            .expect("transcript");
        assert_eq!(transcript[0].0, "user", "the seed is the first message");
    }

    #[tokio::test]
    async fn assembler_without_a_draft_is_no_finish() {
        let dir = crate::test_support::tempdir();
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::Text("looks fine to me".into()),
                ScriptStep::Text("really, no draft needed".into()),
                ScriptStep::Text("last word".into()),
            ],
        ));
        let inputs = stage_inputs(&dir, llm);

        let err = run_assembler(&inputs, &brief(), &[], &[])
            .await
            .expect_err("a review draft is mandatory here");
        assert_eq!(err, StageError::NoFinish);
    }
}
