//! The prior-findings check stage (spec §12): on an incremental re-review,
//! one agent decides what happened to every finding still open from previous
//! runs — addressed, still present (possibly at a new line), or outdated.
//!
//! [`run_prior_check`] runs one agent with read-only tools and
//! `submit_prior_states` as its finish tool. Whatever it submits is applied
//! to the run: states move findings, and a `new_anchor` that validates
//! against the current snapshot re-anchors them (an invalid one degrades to
//! a file summary on the new path, exactly like `record_finding` does for
//! uncommentable files).

use std::sync::{Arc, Mutex};

use review_llm::Message;

use crate::ai::agent::{AgentControl, AgentSpec, AgentStop, FinishCheck, run_agent};
use crate::ai::prompts::system_prompt;
use crate::ai::review::{Finding, FindingAnchor, FindingState, RunState};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::scout::read_only_tools;
use crate::ai::stages::{
    StageError, StageInputs, anchor_text, evidence_text, persist_seed, seed_message, without_bash,
};
use crate::ai::tools::{
    AgentRef, AgentRole, PriorState, PriorStateKind, Registry, SharedPriorStates,
    SubmitPriorStates, Tool, resolve_side,
};
use crate::model::comment::Anchor;

/// The prior-check agent's label, also its transcript key.
pub const PRIOR_LABEL: &str = "prior_check";

/// Model calls the prior check may make: one pass over the open findings
/// with read-only tools, like a verifier batch.
const MAX_TURNS: u32 = 20;
/// Finish nudges before the stage gives up on `submit_prior_states`.
const MAX_FINISH_REMINDERS: u32 = 2;

/// Runs the prior check over `open` (findings in `Draft`, `Kept` or
/// `Posted` state from previous runs) and applies the submitted states to
/// the run.
///
/// The seed task message lists every open finding in full and the files
/// changed since the previous head (persisted before the loop starts, spec
/// §9.2). Running out of finish nudges or turns is a *normal completion*:
/// whatever states made it in are applied and the rest keep their state.
pub async fn run_prior_check(
    inputs: &StageInputs,
    agent: &AgentRef,
    open: Vec<Finding>,
) -> Result<(), StageError> {
    if open.is_empty() {
        return Ok(());
    }
    let agent = agent.clone();
    let label = agent.label.clone();
    let mut cx = inputs.cx.clone();
    cx.agent = agent.clone();

    let ids: Vec<i64> = open.iter().map(|finding| finding.id).collect();
    let submitted: SharedPriorStates = Arc::new(Mutex::new(None));
    let registry = Registry::new(prior_tools(
        Arc::new(SubmitPriorStates::new(ids, submitted.clone())),
        inputs.bash_disabled.as_deref(),
    ));

    let text = seed_message(
        initial_message(&open, inputs.incremental.as_deref()),
        &cx.run.qa(),
        inputs.bash_disabled.as_deref(),
    );
    persist_seed(&cx, &label, &text);

    let cell = submitted.clone();
    let spec = AgentSpec {
        agent,
        system: system_prompt(AgentRole::PriorCheck, &label),
        initial: vec![Message::user_text(text)],
        registry,
        max_turns: MAX_TURNS,
        finish_tool: Some("submit_prior_states"),
        on_finish: Arc::new(move || {
            if cell.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                FinishCheck::Accept
            } else {
                FinishCheck::Continue(
                    "Call submit_prior_states with exactly one state for every \
                     open finding before you finish."
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

    match outcome.stop {
        AgentStop::Finished | AgentStop::NoFinish | AgentStop::MaxTurns => {
            let states = submitted.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(states) = states {
                apply_states(&cx.run, cx.snapshot.as_deref(), &states);
            }
            Ok(())
        }
        AgentStop::Cancelled => Err(StageError::Cancelled),
        AgentStop::Failed(message) => Err(StageError::Failed(message)),
    }
}

/// The prior check's tools (spec §12): the read-only repo tools, `done`,
/// and its finish tool `submit_prior_states`. No record tools — it judges
/// what happened, it does not review.
fn prior_tools(submit: Arc<dyn Tool>, bash_disabled: Option<&str>) -> Vec<Arc<dyn Tool>> {
    let mut tools = read_only_tools();
    tools.extend(
        crate::ai::tools::all()
            .into_iter()
            .filter(|tool| tool.definition().name == "done"),
    );
    tools.push(submit);
    without_bash(tools, bash_disabled)
}

/// The prior check's task message: every open finding in full, the files
/// changed since the previous head, and what to do with them.
fn initial_message(open: &[Finding], changed: Option<&[String]>) -> String {
    let mut out = String::from("## Open findings from previous reviews\n\n");
    for finding in open {
        out.push_str(&format!(
            "### #{} ({}, {}) — {}\n",
            finding.id,
            finding.state.as_str(),
            finding.severity.as_str(),
            anchor_text(&finding.anchor)
        ));
        out.push_str(&format!("title: {}\n", finding.title));
        out.push_str(&format!("body: {}\n", finding.body.trim_end()));
        if !finding.evidence.is_empty() {
            out.push_str("evidence:\n");
            for evidence in &finding.evidence {
                out.push_str(&format!("- {}\n", evidence_text(evidence)));
            }
        }
        if let Some(reason) = finding.state_reason.as_deref() {
            out.push_str(&format!("state_reason: {reason}\n"));
        }
        out.push('\n');
    }

    out.push_str("## Changed files since last review\n\n");
    match changed {
        Some(files) if !files.is_empty() => {
            for path in files {
                out.push_str(path);
                out.push('\n');
            }
        }
        Some(_) => out.push_str("(no files changed since the last review)\n"),
        None => out.push_str("(the changed-file list is unavailable; inspect the diff yourself)\n"),
    }

    out.push_str(
        "\n## Instructions\n\n\
         For every open finding above, check the current head: open the file \
         at the anchor, compare against `git diff` of the changed files, and \
         decide `addressed` (the changes fixed it), `still_present` (the issue \
         remains) or `outdated` (the code it pointed at is gone). When a \
         still-present issue moved, add `new_anchor` ({path, line, side}) at \
         its new line in the current diff. Finish by calling \
         submit_prior_states with exactly one state for every finding id.\n",
    );
    out
}

/// Apply the submitted states (spec §12): `addressed` →
/// [`FindingState::Addressed`], `outdated` → [`FindingState::Outdated`],
/// `still_present` → [`FindingState::StillPresent`]. A `new_anchor` is
/// validated like `record_finding` does: the path must be in the PR and the
/// line a commentable diff line; valid anchors re-anchor the finding,
/// invalid ones degrade to a file summary on the new path.
fn apply_states(run: &RunState, snapshot: Option<&PrSnapshot>, states: &[PriorState]) {
    for state in states {
        let finding_state = match state.state {
            PriorStateKind::Addressed => FindingState::Addressed,
            PriorStateKind::Outdated => FindingState::Outdated,
            PriorStateKind::StillPresent => FindingState::StillPresent,
        };
        let reason = (!state.reason.trim().is_empty()).then(|| state.reason.clone());
        let anchor = state
            .new_anchor
            .as_ref()
            .and_then(|new| reanchor(snapshot, new));
        if let Err(error) =
            run.set_finding_state_and_anchor(state.finding_id, finding_state, reason, anchor)
        {
            tracing::warn!(%error, finding = state.finding_id, "prior check: applying state failed");
        }
    }
}

/// The anchor a validated `new_anchor` produces: a line anchor when the
/// path is in the PR and the line is commentable, a file summary on the new
/// path otherwise (the same degradation `record_finding` applies to files
/// without commentable lines). `None` only when no snapshot is attached.
fn reanchor(
    snapshot: Option<&PrSnapshot>,
    new: &crate::ai::tools::NewAnchor,
) -> Option<FindingAnchor> {
    let snapshot = snapshot?;
    let side = match resolve_side(new.side.as_deref()) {
        Ok(side) => side,
        Err(_) => crate::model::diff::Side::Right,
    };
    let in_pr = snapshot
        .files
        .iter()
        .any(|loaded| loaded.file.path == new.path);
    let diff_lines = snapshot.diff_lines(&new.path);
    if in_pr
        && !diff_lines.is_empty()
        && diff_lines.iter().any(|(s, l)| *s == side && *l == new.line)
    {
        return Some(FindingAnchor::Line(Anchor {
            path: new.path.clone(),
            side,
            line: new.line,
            start_line: None,
            start_side: None,
        }));
    }
    Some(FindingAnchor::Summary {
        path: new.path.clone(),
    })
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
    use crate::ai::review::{Evidence, NewFinding, Severity};
    use crate::ai::snapshot::PrSnapshot;
    use crate::ai::testing::{ScriptStep, ScriptedInteraction, ScriptedLlm};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx};
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

    /// A snapshot over one [`test_support::loaded`] file: RIGHT lines
    /// 10..=14 of `src/a.rs` are commentable.
    fn snapshot() -> PrSnapshot {
        PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files: vec![test_support::loaded(
                "src/a.rs",
                "@@ -10,5 +10,5 @@\n c1\n c2\n c3\n c4\n c5\n",
            )],
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        }
    }

    /// A `StageInputs` over a temp-dir [`LocalHost`] carrying [`snapshot`].
    fn stage_inputs(dir: &tempfile::TempDir, llm: Arc<ScriptedLlm>) -> StageInputs {
        let host = LocalHost::new(dir.path()).expect("local host");
        let (tx, _rx) = mpsc::unbounded_channel();
        StageInputs {
            cx: ToolCx {
                sandbox: Arc::new(host.clone()),
                repo_root: host.root().path().to_path_buf(),
                agent: AgentRef {
                    id: 0,
                    role: AgentRole::PriorCheck,
                    label: "template".to_string(),
                },
                interaction: Arc::new(ScriptedInteraction::new()),
                allowed: Arc::new(Mutex::new(Vec::new())),
                for_review: Arc::new(Mutex::new(Vec::new())),
                snapshot: Some(Arc::new(snapshot())),
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
            incremental: Some(vec!["src/a.rs".to_string()]),
            incremental_log: None,
        }
    }

    fn prior_agent() -> AgentRef {
        AgentRef {
            id: 9,
            role: AgentRole::PriorCheck,
            label: PRIOR_LABEL.to_string(),
        }
    }

    /// Record one open finding at `line` through the run state.
    fn open_finding(run: &RunState, line: u32) -> i64 {
        run.add_finding(NewFinding {
            agent: AgentRef {
                id: 4,
                role: AgentRole::Reviewer,
                label: "reviewer:src-a.rs".to_string(),
            },
            anchor: FindingAnchor::Line(test_support::line_anchor("src/a.rs", line)),
            severity: Severity::Bug,
            title: format!("bug at {line}"),
            body: "the body".to_string(),
            evidence: vec![Evidence {
                path: "src/a.rs".to_string(),
                line: Some(line),
                note: "saw it here".to_string(),
            }],
            checklist_item: None,
        })
        .expect("finding recorded")
    }

    #[tokio::test]
    async fn prior_check_marks_and_reanchors() {
        let dir = crate::test_support::tempdir();
        let llm = Arc::new(ScriptedLlm::per_agent(
            model(),
            [(
                PRIOR_LABEL.to_string(),
                vec![ScriptStep::ToolCalls(vec![(
                    "submit_prior_states".into(),
                    json!({"states": [
                        {"finding_id": 1, "state": "addressed",
                         "reason": "the new guard fixes it"},
                        {"finding_id": 2, "state": "still_present",
                         "reason": "still divides by zero",
                         "new_anchor": {"path": "src/a.rs", "line": 13}},
                        {"finding_id": 3, "state": "still_present",
                         "reason": "moved but off-diff",
                         "new_anchor": {"path": "src/a.rs", "line": 99}},
                    ]}),
                )])],
            )]
            .into_iter()
            .collect(),
        ));
        let inputs = stage_inputs(&dir, llm);
        let run = inputs.cx.run.clone();
        let (one, two, three) = (
            open_finding(&run, 11),
            open_finding(&run, 12),
            open_finding(&run, 14),
        );

        run_prior_check(&inputs, &prior_agent(), run.findings())
            .await
            .expect("prior check completes");

        let findings = run.findings();
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0].id, one);
        assert_eq!(findings[1].id, two);
        assert_eq!(findings[2].id, three);

        assert_eq!(findings[0].state, FindingState::Addressed);
        assert_eq!(
            findings[0].state_reason.as_deref(),
            Some("the new guard fixes it")
        );
        assert_eq!(
            findings[0].anchor,
            FindingAnchor::Line(test_support::line_anchor("src/a.rs", 11)),
            "addressed keeps its anchor"
        );

        // Line 13 is a commentable RIGHT line: the anchor moves there.
        assert_eq!(findings[1].state, FindingState::StillPresent);
        assert_eq!(
            findings[1].anchor,
            FindingAnchor::Line(Anchor {
                path: "src/a.rs".to_string(),
                side: crate::model::diff::Side::Right,
                line: 13,
                start_line: None,
                start_side: None,
            })
        );

        // Line 99 is not in the diff: the anchor degrades to a file summary.
        assert_eq!(findings[2].state, FindingState::StillPresent);
        assert_eq!(
            findings[2].anchor,
            FindingAnchor::Summary {
                path: "src/a.rs".to_string()
            }
        );

        // The store rows match the cache exactly: state, reason and anchor
        // were persisted, not just mirrored.
        let stored = run.store().finding_list(run.session_id()).expect("rows");
        assert_eq!(stored, findings);

        // The seed message carries every finding and the changed files.
        let transcript = run
            .store()
            .messages(run.run_id(), PRIOR_LABEL)
            .expect("seed");
        assert_eq!(transcript[0].0, "user");
        let seed: Vec<ContentBlock> = serde_json::from_str(&transcript[0].1).expect("seed json");
        let ContentBlock::Text { text } = &seed[0] else {
            panic!("the seed is a text block: {seed:?}")
        };
        assert!(
            text.contains("## Open findings from previous reviews"),
            "{text}"
        );
        assert!(
            text.contains("### #1 (draft, bug) — src/a.rs:11 (RIGHT)"),
            "{text}"
        );
        assert!(
            text.contains("evidence:\n- src/a.rs:11 — saw it here"),
            "{text}"
        );
        assert!(
            text.contains("## Changed files since last review"),
            "{text}"
        );
        assert!(text.contains("src/a.rs\n"), "{text}");
        assert!(text.contains("## Instructions"), "{text}");
        assert!(
            text.contains("submit_prior_states"),
            "the finish tool is named:\n{text}"
        );
    }

    #[tokio::test]
    async fn prior_check_without_submit_is_ok() {
        let dir = crate::test_support::tempdir();
        let llm = Arc::new(ScriptedLlm::per_agent(
            model(),
            [(
                PRIOR_LABEL.to_string(),
                vec![
                    ScriptStep::Text("nothing to say".into()),
                    ScriptStep::Text("still nothing".into()),
                    ScriptStep::Text("last word".into()),
                ],
            )]
            .into_iter()
            .collect(),
        ));
        let inputs = stage_inputs(&dir, llm);
        let run = inputs.cx.run.clone();
        open_finding(&run, 11);

        run_prior_check(&inputs, &prior_agent(), run.findings())
            .await
            .expect("no states is a normal completion");
        assert_eq!(run.findings()[0].state, FindingState::Draft, "untouched");
    }
}
