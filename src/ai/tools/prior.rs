//! The prior-check stage's `submit_prior_states` tool (spec §12): the
//! structured finish call that carries one state per open finding.
//!
//! Like [`SubmitVerdicts`](super::verdict::SubmitVerdicts), this tool is
//! **stage-local** and deliberately absent from [`all`]: the prior-check
//! stage constructs it with the open finding ids and the shared
//! [`SharedPriorStates`] cell [`run_prior_check`](crate::ai::stages::prior::run_prior_check)
//! reads.
//!
//! Validation runs before anything is stored: every open finding id must
//! appear **exactly once** (the rejection lists the missing, extra and
//! duplicated ids), every state needs a non-empty reason, and `new_anchor`
//! is only meaningful for `still_present` — the stage validates the anchor
//! itself against the snapshot, since the tool has no diff to check.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};

/// What the prior check concluded about one open finding (spec §12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorStateKind {
    /// The changed code fixed the issue.
    Addressed,
    /// The issue is still there; `new_anchor` may point at its new line.
    StillPresent,
    /// The code the finding sits on is gone (file deleted or rewritten).
    Outdated,
}

/// A corrected anchor for a still-present finding; mirrors the
/// `record_finding` anchor shape (`side` defaults to RIGHT).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewAnchor {
    /// The path the finding now belongs to.
    pub path: String,
    /// The line in the new head the finding now points at.
    pub line: u32,
    /// `LEFT` or `RIGHT`; absent means `RIGHT`.
    #[serde(default)]
    pub side: Option<String>,
}

/// One state: the tool's input shape, stored once the whole set is in.
#[derive(Clone, Debug, PartialEq)]
pub struct PriorState {
    /// The finding this state is about; always an open finding id.
    pub finding_id: i64,
    /// `addressed`, `still_present` or `outdated`.
    pub state: PriorStateKind,
    /// The one-line reason; shown as the finding's `state_reason`.
    pub reason: String,
    /// Where the finding lives now; only for `still_present`.
    pub new_anchor: Option<NewAnchor>,
}

/// The cell the stage and the tool hand the states over through.
pub type SharedPriorStates = Arc<Mutex<Option<Vec<PriorState>>>>;

/// The `submit_prior_states` tool.
pub struct SubmitPriorStates {
    /// The ids of the findings open when the run started.
    open: Vec<i64>,
    submitted: SharedPriorStates,
}

impl SubmitPriorStates {
    /// A submit tool that stores the validated states into `submitted`;
    /// `open` is the ids of the findings under check.
    pub fn new(open: Vec<i64>, submitted: SharedPriorStates) -> SubmitPriorStates {
        SubmitPriorStates { open, submitted }
    }
}

#[derive(Deserialize)]
struct RawInput {
    states: Vec<RawPriorState>,
}

#[derive(Deserialize)]
struct RawPriorState {
    finding_id: i64,
    state: PriorStateKind,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    new_anchor: Option<NewAnchor>,
}

#[async_trait::async_trait]
impl Tool for SubmitPriorStates {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "submit_prior_states",
            "Record your state for every open finding from previous reviews \
             and finish. Each finding id needs exactly one state; missing, \
             duplicated or outside ids reject the call. `addressed` means the \
             changes fixed it, `outdated` means the code it pointed at is \
             gone, `still_present` means the issue remains — only then may \
             `new_anchor` ({path, line, side}) correct where it points. \
             Every state needs a one-line `reason`.",
            json!({
                "type": "object",
                "properties": {
                    "states": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "finding_id": { "type": "integer" },
                                "state": {
                                    "type": "string",
                                    "enum": ["addressed", "still_present", "outdated"]
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "One line, concrete: what you \
                                        looked at and what it showed."
                                },
                                "new_anchor": {
                                    "type": "object",
                                    "properties": {
                                        "path": { "type": "string" },
                                        "line": { "type": "integer" },
                                        "side": {
                                            "type": "string",
                                            "enum": ["LEFT", "RIGHT"],
                                            "description": "Defaults to RIGHT."
                                        }
                                    },
                                    "required": ["path", "line"]
                                }
                            },
                            "required": ["finding_id", "state", "reason"]
                        }
                    }
                },
                "required": ["states"]
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let raw: RawInput = match parse(input) {
            Ok(raw) => raw,
            Err(out) => return out,
        };
        if let Some(problem) = self.rejection(&raw.states) {
            return ToolOutput::error(problem);
        }
        let count = raw.states.len();
        let states: Vec<PriorState> = raw
            .states
            .into_iter()
            .map(|s| PriorState {
                finding_id: s.finding_id,
                state: s.state,
                reason: s.reason,
                new_anchor: s.new_anchor,
            })
            .collect();
        *self.submitted.lock().unwrap_or_else(|e| e.into_inner()) = Some(states);
        ToolOutput::text(format!("prior states accepted: {count} findings"))
    }
}

impl SubmitPriorStates {
    /// Every rejection text for this submission, or `None` when it covers
    /// the open findings exactly and every state is well-formed.
    fn rejection(&self, raw: &[RawPriorState]) -> Option<String> {
        let mut problems: Vec<String> = Vec::new();

        let mut counts: HashMap<i64, usize> = HashMap::new();
        for state in raw {
            *counts.entry(state.finding_id).or_insert(0) += 1;
        }
        let missing: Vec<String> = self
            .open
            .iter()
            .filter(|id| !counts.contains_key(id))
            .map(i64::to_string)
            .collect();
        if !missing.is_empty() {
            problems.push(format!("no state for: {}", missing.join(", ")));
        }
        let mut seen: HashSet<i64> = HashSet::new();
        let duplicated: Vec<String> = raw
            .iter()
            .map(|s| s.finding_id)
            .filter(|id| counts.get(id).copied().unwrap_or(0) > 1 && seen.insert(*id))
            .collect::<Vec<_>>()
            .iter()
            .map(i64::to_string)
            .collect();
        if !duplicated.is_empty() {
            problems.push(format!("listed more than once: {}", duplicated.join(", ")));
        }
        let mut seen: HashSet<i64> = HashSet::new();
        let extra: Vec<String> = raw
            .iter()
            .map(|s| s.finding_id)
            .filter(|id| !self.open.contains(id) && seen.insert(*id))
            .collect::<Vec<_>>()
            .iter()
            .map(i64::to_string)
            .collect();
        if !extra.is_empty() {
            problems.push(format!("not open findings: {}", extra.join(", ")));
        }

        let no_reason: Vec<String> = raw
            .iter()
            .filter(|s| s.reason.trim().is_empty())
            .map(|s| s.finding_id.to_string())
            .collect();
        if !no_reason.is_empty() {
            problems.push(format!("state without a reason: {}", no_reason.join(", ")));
        }
        let stray_anchor: Vec<String> = raw
            .iter()
            .filter(|s| s.state != PriorStateKind::StillPresent && s.new_anchor.is_some())
            .map(|s| s.finding_id.to_string())
            .collect();
        if !stray_anchor.is_empty() {
            problems.push(format!(
                "new_anchor is only for still_present: {}",
                stray_anchor.join(", ")
            ));
        }

        if problems.is_empty() {
            None
        } else {
            Some(format!("prior states rejected — {}", problems.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use review_sandbox::LocalHost;

    use super::*;
    use crate::ai::testing::ScriptedInteraction;
    use crate::ai::tools::{AgentRef, AgentRole, Tool, ToolCx, ToolOutput};
    use crate::test_support;

    /// A prior-check context; the tool itself reads none of it.
    fn cx(dir: &tempfile::TempDir) -> ToolCx {
        let host = LocalHost::new(dir.path()).expect("local host");
        ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: AgentRef {
                id: 1,
                role: AgentRole::PriorCheck,
                label: "prior_check".to_string(),
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
            approval: Default::default(),
        }
    }

    fn text_of(out: &ToolOutput) -> String {
        out.parts
            .iter()
            .map(|part| match part {
                review_llm::ToolResultPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect()
    }

    fn state(finding_id: i64, state: &str) -> Value {
        json!({"finding_id": finding_id, "state": state, "reason": "checked"})
    }

    #[tokio::test]
    async fn prior_states_must_cover_the_open_findings() {
        let dir = crate::test_support::tempdir();
        let cx = cx(&dir);
        let submitted: SharedPriorStates = Arc::new(Mutex::new(None));
        let tool = SubmitPriorStates::new(vec![1, 2, 3], submitted.clone());

        // Finding 3 has no state.
        let out = tool
            .call(
                json!({"states": [state(1, "addressed"), state(2, "outdated")]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("no state for: 3"), "{text}");
        assert!(
            submitted.lock().unwrap().is_none(),
            "nothing stored on rejection"
        );

        // Id 1 twice, id 9 not open, id 3 still missing: all listed.
        let out = tool
            .call(
                json!({"states": [
                    state(1, "addressed"), state(1, "addressed"),
                    state(2, "addressed"), state(9, "addressed"),
                ]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("no state for: 3"), "{text}");
        assert!(text.contains("listed more than once: 1"), "{text}");
        assert!(text.contains("not open findings: 9"), "{text}");

        // A state without a reason, and a new_anchor outside still_present.
        let out = tool
            .call(
                json!({"states": [
                    {"finding_id": 1, "state": "addressed", "reason": "  "},
                    state(2, "addressed"),
                    {"finding_id": 3, "state": "outdated", "reason": "gone",
                     "new_anchor": {"path": "src/a.rs", "line": 3}},
                ]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("state without a reason: 1"), "{text}");
        assert!(
            text.contains("new_anchor is only for still_present: 3"),
            "{text}"
        );

        // A complete, well-formed set is accepted and stored verbatim.
        let out = tool
            .call(
                json!({"states": [
                    state(1, "addressed"),
                    {"finding_id": 2, "state": "still_present",
                     "reason": "still divides by zero",
                     "new_anchor": {"path": "src/a.rs", "line": 13, "side": "RIGHT"}},
                    state(3, "outdated"),
                ]}),
                &cx,
            )
            .await;
        assert!(!out.is_error, "{}", text_of(&out));
        let stored = submitted.lock().unwrap();
        let states = stored.as_ref().expect("states stored");
        assert_eq!(states.len(), 3);
        assert_eq!(states[0].state, PriorStateKind::Addressed);
        assert_eq!(states[1].state, PriorStateKind::StillPresent);
        assert_eq!(states[1].reason, "still divides by zero");
        let anchor = states[1].new_anchor.as_ref().expect("anchor stored");
        assert_eq!(anchor.path, "src/a.rs");
        assert_eq!(anchor.line, 13);
        assert_eq!(anchor.side.as_deref(), Some("RIGHT"));
        assert_eq!(states[2].state, PriorStateKind::Outdated);
        assert_eq!(states[2].new_anchor, None);
    }

    #[test]
    fn submit_prior_states_is_stage_local() {
        assert!(
            crate::ai::tools::all()
                .iter()
                .all(|tool| tool.definition().name != "submit_prior_states"),
            "the prior-check stage constructs it with its findings; it is not in all()"
        );
    }
}
