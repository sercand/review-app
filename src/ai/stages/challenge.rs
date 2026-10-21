//! The challenge pass (spec §5.4 "Challenge pass"): one short adversarial
//! agent on a high-risk group whose reviewer ended with no `bug`/`risk`
//! finding.
//!
//! [`needs_challenge`] is the app's trigger; [`run_challenger`] runs the
//! agent with the reviewer's tool set and `done` as its finish tool — no
//! coverage enforcement, since the challenger inherits the group's coverage
//! from its reviewer. Its findings go through the same dedupe and verifier
//! as the reviewers', which is where false positives are filtered. Running
//! out of finish nudges or turns is a *normal completion* for this stage:
//! whatever the agent recorded stands.

use std::sync::Arc;

use review_llm::Message;

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::prompts::system_prompt;
use crate::ai::review::{Finding, Severity};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::reviewer::{find_file, patch_text, risk_slug};
use crate::ai::stages::scout::{Brief, Group, Risk};
use crate::ai::stages::{
    StageError, StageInputs, agent_findings, anchor_text, evidence_text, persist_seed,
    reviewer_tools, seed_message,
};
use crate::ai::tools::{AgentRef, AgentRole, Registry};

/// Model calls a challenger may make (spec §6.2): a short pass, well under
/// the reviewer's budget.
const MAX_TURNS: u32 = 20;
/// Finish nudges before the loop gives up on `done`.
const MAX_FINISH_REMINDERS: u32 = 2;

/// Whether the app should run the challenge pass on `group`: it is high
/// risk and its reviewer produced no `bug`/`risk` finding (spec §5.4 — the
/// typical small-model failure is to finish a high-risk group empty).
pub fn needs_challenge(group: &Group, findings: &[Finding]) -> bool {
    if group.risk != Risk::High {
        return false;
    }
    let reviewer = format!("reviewer:{}", group.id);
    !findings.iter().any(|finding| {
        finding.agent.label == reviewer
            && matches!(finding.severity, Severity::Bug | Severity::Risk)
    })
}

/// Runs one challenger over `group` of `brief` and returns the ids of the
/// findings *this challenger* recorded.
///
/// The seed task message is persisted before the loop starts; it carries
/// the intent, the group's reviewer findings, and the group's patches. The
/// finish check always accepts — `done` (or running out of nudges or
/// turns) ends the pass.
pub async fn run_challenger(
    inputs: &StageInputs,
    agent: &AgentRef,
    brief: &Brief,
    group: &Group,
) -> Result<Vec<i64>, StageError> {
    let agent = agent.clone();
    let label = agent.label.clone();
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();
    let snapshot = cx
        .snapshot
        .clone()
        .ok_or_else(|| StageError::Failed(format!("{label}: no PR snapshot attached")))?;

    let reviewer = format!("reviewer:{}", group.id);
    let findings: Vec<Finding> = cx
        .run
        .findings()
        .into_iter()
        .filter(|finding| finding.agent.label == reviewer)
        .collect();

    let text = seed_message(
        initial_message(brief, group, &snapshot, &findings),
        &cx.run.qa(),
        inputs.bash_disabled.as_deref(),
    );
    persist_seed(&cx, &label, &text);

    let spec = AgentSpec {
        agent: agent.clone(),
        system: system_prompt(AgentRole::Challenger, &label),
        initial: vec![Message::user_text(text)],
        registry: Registry::new(reviewer_tools(inputs.bash_disabled.as_deref())),
        max_turns: MAX_TURNS,
        finish_tool: Some("done"),
        // No coverage enforcement here: the challenger's findings are
        // filtered by the verifier like every other finding (spec §5.4).
        on_finish: Arc::new(|| FinishCheck::Accept),
        max_finish_reminders: MAX_FINISH_REMINDERS,
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

    // Running out of finish nudges or turns is a normal completion: what
    // the challenger recorded stands. Only a model failure or a cancel
    // fails the stage.
    match outcome.stop {
        AgentStop::Finished | AgentStop::NoFinish | AgentStop::MaxTurns => Ok(findings),
        AgentStop::Cancelled => Err(StageError::Cancelled),
        AgentStop::Failed(message) => Err(StageError::Failed(message)),
    }
}

/// The challenger's single task message: the intent, the group, the
/// findings under challenge, and the group's patches.
fn initial_message(
    brief: &Brief,
    group: &Group,
    snapshot: &PrSnapshot,
    findings: &[Finding],
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
        out.push_str(&format!("- {} (+{additions} −{deletions})\n", file.path));
    }

    out.push_str("\n## Findings\n\n");
    if findings.is_empty() {
        out.push_str("(the reviewer recorded no findings for this group)\n\n");
    }
    for finding in findings {
        out.push_str(&format!(
            "### Finding #{} ({}): {}\n\n{}\n\n",
            finding.id,
            finding.severity.as_str(),
            finding.title,
            finding.body.trim_end()
        ));
        out.push_str(&format!("Anchor: {}\n", anchor_text(&finding.anchor)));
        out.push_str("Evidence:\n");
        for evidence in &finding.evidence {
            out.push_str(&format!("- {}\n", evidence_text(evidence)));
        }
        if let Some(item) = &finding.checklist_item {
            out.push_str(&format!("Checklist item: {item}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Patches\n\n");
    for file in &group.files {
        out.push_str(&format!("### {}\n\n", file.path));
        out.push_str(&patch_text(snapshot, file));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use review_llm::{ContentBlock, GenerateRequest};
    use review_sandbox::LocalHost;
    use serde_json::json;
    use tokio::sync::{mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::ai::agent::AgentControl;
    use crate::ai::review::{Evidence, FindingAnchor, FindingState, NewFinding};
    use crate::ai::stages::scout::GroupFile;
    use crate::ai::stages::{StageError, StageInputs, next_agent_id};

    /// The challenger agent reference, as the pipeline injects it.
    fn challenger_agent() -> AgentRef {
        AgentRef {
            id: next_agent_id(),
            role: AgentRole::Challenger,
            label: "challenger:g1".to_string(),
        }
    }
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
    use crate::github::files::LoadedFile;
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

    fn group(id: &str, risk: Risk, paths: &[&str]) -> Group {
        Group {
            id: id.to_string(),
            title: "core".to_string(),
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

    /// A finding by `label` with `severity`, for the trigger table.
    fn finding(label: &str, severity: Severity) -> Finding {
        Finding {
            id: 1,
            run_id: 1,
            agent: AgentRef {
                id: 2,
                role: AgentRole::Reviewer,
                label: label.to_string(),
            },
            anchor: FindingAnchor::Summary {
                path: "src/a.rs".to_string(),
            },
            severity,
            title: "t".to_string(),
            body: "b".to_string(),
            evidence: Vec::new(),
            checklist_item: None,
            state: FindingState::Draft,
            state_reason: None,
            pending_local_id: None,
        }
    }

    /// A `StageInputs` over a temp-dir [`LocalHost`] carrying `snapshot`.
    fn stage_inputs(
        dir: &tempfile::TempDir,
        snapshot: PrSnapshot,
        llm: Arc<ScriptedLlm>,
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

    #[test]
    fn challenge_only_for_high_risk_without_bugs() {
        let high = group("g1", Risk::High, &["src/a.rs"]);
        let medium = group("g1", Risk::Medium, &["src/a.rs"]);
        let low = group("g1", Risk::Low, &["src/a.rs"]);

        // High risk with nothing recorded is exactly the failure mode.
        assert!(needs_challenge(&high, &[]), "high risk, no findings");
        assert!(!needs_challenge(&medium, &[]), "medium risk is enough");
        assert!(!needs_challenge(&low, &[]), "low risk is enough");

        // The group's own reviewer: only bug/risk findings count as evidence.
        for severity in [Severity::Nit, Severity::Question] {
            assert!(
                needs_challenge(&high, &[finding("reviewer:g1", severity)]),
                "a {} finding is not a defect",
                severity.as_str()
            );
        }
        for severity in [Severity::Bug, Severity::Risk] {
            assert!(
                !needs_challenge(&high, &[finding("reviewer:g1", severity)]),
                "a {} finding means the reviewer found something",
                severity.as_str()
            );
        }

        // Only this group's reviewer counts — another group's or the
        // challenger's own bugs say nothing about this group.
        assert!(needs_challenge(
            &high,
            &[finding("reviewer:g2", Severity::Bug)]
        ));
        assert!(needs_challenge(
            &high,
            &[finding("challenger:g1", Severity::Bug)]
        ));
        assert!(needs_challenge(
            &high,
            &[
                finding("reviewer:g1", Severity::Nit),
                finding("reviewer:g2", Severity::Bug),
            ]
        ));
    }

    #[tokio::test]
    async fn challenger_records_a_finding_and_returns_its_id() {
        let dir = crate::test_support::tempdir();
        // RIGHT-side lines 10..=14.
        let file = test_support::loaded("src/a.rs", "@@ -10,5 +10,5 @@\n c1\n c2\n c3\n c4\n c5\n");
        let snapshot = snapshot_of(vec![file]);
        let llm = Arc::new(ScriptedLlm::new(
            model(),
            vec![
                ScriptStep::ToolCalls(vec![(
                    "record_finding".to_string(),
                    json!({
                        "path": "src/a.rs",
                        "line": 12,
                        "severity": "bug",
                        "title": "missed overflow",
                        "body": "the counter wraps at u8",
                        "evidence": [{"path": "src/a.rs", "line": 12, "note": "the increment"}],
                    }),
                )]),
                ScriptStep::ToolCalls(vec![("done".to_string(), json!({"summary": "found it"}))]),
            ],
        ));
        let inputs = stage_inputs(&dir, snapshot, llm.clone());
        let run = inputs.cx.run.clone();
        // The reviewer's nit shows up in the seed as the finding under
        // challenge.
        let reviewer_id = run
            .add_finding(NewFinding {
                agent: AgentRef {
                    id: 2,
                    role: AgentRole::Reviewer,
                    label: "reviewer:g1".to_string(),
                },
                anchor: FindingAnchor::Line(test_support::line_anchor("src/a.rs", 11)),
                severity: Severity::Nit,
                title: "already noted".to_string(),
                body: "naming".to_string(),
                evidence: vec![Evidence {
                    path: "src/a.rs".to_string(),
                    line: Some(11),
                    note: "the name".to_string(),
                }],
                checklist_item: Some("consistency".to_string()),
            })
            .expect("reviewer finding");
        let brief = brief_with(vec![group("g1", Risk::High, &["src/a.rs"])]);

        let ids = run_challenger(&inputs, &challenger_agent(), &brief, &brief.groups[0])
            .await
            .expect("a finished challenger");

        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_ne!(ids[0], reviewer_id, "only the challenger's findings");
        let finding = run
            .findings()
            .into_iter()
            .find(|f| f.id == ids[0])
            .expect("the finding is stored");
        assert_eq!(finding.agent.label, "challenger:g1");
        assert_eq!(finding.agent.role, AgentRole::Challenger);

        // The seed was persisted under the challenger's label and carries
        // the intent, the finding under challenge, and the patch.
        let transcript = run
            .store()
            .messages(run.run_id(), "challenger:g1")
            .expect("transcript");
        assert_eq!(transcript[0].0, "user", "the seed is the first message");
        let seed: Vec<ContentBlock> = serde_json::from_str(&transcript[0].1).expect("seed json");
        let ContentBlock::Text { text } = &seed[0] else {
            panic!("the seed is a text block: {seed:?}")
        };
        assert!(text.contains("## Intent"), "{text}");
        assert!(text.contains("the intent"), "{text}");
        assert!(text.contains("g1: core (risk: high)"), "{text}");
        assert!(text.contains("## Findings"), "{text}");
        assert!(
            text.contains(&format!("### Finding #{reviewer_id} (nit): already noted")),
            "{text}"
        );
        assert!(text.contains("Anchor: src/a.rs:11 (RIGHT)"), "{text}");
        assert!(text.contains("src/a.rs:11 — the name"), "{text}");
        assert!(text.contains("Checklist item: consistency"), "{text}");
        assert!(text.contains("## Patches"), "{text}");
        assert!(text.contains("@@ -10,5 +10,5 @@"), "{text}");

        // The model saw the seed, and `done` finished the run.
        let requests = llm.requests();
        assert_eq!(requests.len(), 2);
        assert!(request_text(&requests[0]).contains("## Findings"));
    }

    #[tokio::test]
    async fn challenger_without_a_snapshot_fails() {
        let dir = crate::test_support::tempdir();
        let llm = Arc::new(ScriptedLlm::new(model(), vec![]));
        let mut inputs = stage_inputs(&dir, snapshot_of(Vec::new()), llm);
        inputs.cx.snapshot = None;
        let brief = brief_with(vec![group("g1", Risk::High, &["src/a.rs"])]);

        let err = run_challenger(&inputs, &challenger_agent(), &brief, &brief.groups[0])
            .await
            .expect_err("no snapshot means no patches");
        assert!(matches!(err, StageError::Failed(_)), "{err:?}");
    }
}
