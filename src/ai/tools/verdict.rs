//! The verifier's `submit_verdicts` tool (spec §5.5): the structured finish
//! call that carries one verdict per finding in the batch.
//!
//! This tool is **stage-local** and deliberately absent from [`all`]: the
//! verifier stage constructs it with the batch's finding ids and the shared
//! [`SharedVerdicts`] cell its
//! [`run_verifier`](crate::ai::stages::verify::run_verifier) reads, so the
//! verdicts travel by memory, not by tool output.
//!
//! Validation runs before anything is stored: every finding id in the batch
//! must appear **exactly once** (the rejection lists the missing ids, the
//! ids listed more than once, and the ids outside the batch), a `downgrade`
//! must carry a severity, and `duplicate_of` must name another finding of
//! this batch. A valid submission is stored verbatim; applying the verdicts
//! to the run state is the stage's job.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::review::Severity;

/// What the verifier decided about one finding (spec §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictKind {
    /// The finding stands as recorded.
    Keep,
    /// The finding is wrong, already known, or already in CI.
    Drop,
    /// The issue is real but smaller than recorded; `severity` says how much.
    Downgrade,
}

/// One verdict: the tool's input shape, stored once the whole batch is in.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// The finding this verdict is about; always a batch id.
    pub finding_id: i64,
    /// `keep`, `drop` or `downgrade`.
    pub verdict: VerdictKind,
    /// The severity to move to; present exactly for `downgrade`.
    pub severity: Option<Severity>,
    /// The one-line reason; shown as the finding's `state_reason`.
    pub reason: String,
    /// Another batch finding this one duplicates; it is then dropped as a
    /// duplicate rather than by its own verdict.
    pub duplicate_of: Option<i64>,
}

/// The cell the stage and the tool hand the verdicts over through.
pub type SharedVerdicts = Arc<Mutex<Option<Vec<Verdict>>>>;

/// The `submit_verdicts` tool.
pub struct SubmitVerdicts {
    /// The finding ids the batch under verification carries.
    batch: Vec<i64>,
    submitted: SharedVerdicts,
}

impl SubmitVerdicts {
    /// A submit tool that stores the validated verdicts into `submitted`;
    /// `batch` is the ids of the findings under verification.
    pub fn new(batch: Vec<i64>, submitted: SharedVerdicts) -> SubmitVerdicts {
        SubmitVerdicts { batch, submitted }
    }
}

#[derive(Deserialize)]
struct RawInput {
    verdicts: Vec<RawVerdict>,
}

#[derive(Deserialize)]
struct RawVerdict {
    finding_id: i64,
    verdict: VerdictKind,
    #[serde(default)]
    severity: Option<Severity>,
    reason: String,
    #[serde(default)]
    duplicate_of: Option<i64>,
}

#[async_trait::async_trait]
impl Tool for SubmitVerdicts {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "submit_verdicts",
            "Record your verdict for every finding in the batch and finish \
             verifying. Each finding id in the batch needs exactly one \
             verdict; missing, duplicated or outside ids reject the call. \
             `downgrade` requires a `severity`; `duplicate_of` names another \
             finding in this batch that this one repeats, and drops it as a \
             duplicate.",
            json!({
                "type": "object",
                "properties": {
                    "verdicts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "finding_id": { "type": "integer" },
                                "verdict": {
                                    "type": "string",
                                    "enum": ["keep", "drop", "downgrade"]
                                },
                                "severity": {
                                    "type": "string",
                                    "enum": ["bug", "risk", "nit", "question"],
                                    "description": "Required for `downgrade`: \
                                        the severity to move the finding to."
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "One line, concrete: what you \
                                        opened and what it showed."
                                },
                                "duplicate_of": {
                                    "type": "integer",
                                    "description": "Another finding in this batch \
                                        this one duplicates."
                                }
                            },
                            "required": ["finding_id", "verdict", "reason"]
                        }
                    }
                },
                "required": ["verdicts"]
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let raw: RawInput = match parse(input) {
            Ok(raw) => raw,
            Err(out) => return out,
        };
        if let Some(problem) = self.rejection(&raw.verdicts) {
            return ToolOutput::error(problem);
        }
        let count = raw.verdicts.len();
        let verdicts: Vec<Verdict> = raw
            .verdicts
            .into_iter()
            .map(|v| Verdict {
                finding_id: v.finding_id,
                verdict: v.verdict,
                severity: v.severity,
                reason: v.reason,
                duplicate_of: v.duplicate_of,
            })
            .collect();
        *self.submitted.lock().unwrap_or_else(|e| e.into_inner()) = Some(verdicts);
        ToolOutput::text(format!("verdicts accepted: {count} findings"))
    }
}

impl SubmitVerdicts {
    /// Every rejection text for this submission, or `None` when it covers
    /// the batch exactly and every verdict is well-formed.
    fn rejection(&self, raw: &[RawVerdict]) -> Option<String> {
        let mut problems: Vec<String> = Vec::new();

        let mut counts: HashMap<i64, usize> = HashMap::new();
        for verdict in raw {
            *counts.entry(verdict.finding_id).or_insert(0) += 1;
        }
        let missing: Vec<String> = self
            .batch
            .iter()
            .filter(|id| !counts.contains_key(id))
            .map(i64::to_string)
            .collect();
        if !missing.is_empty() {
            problems.push(format!("no verdict for: {}", missing.join(", ")));
        }
        let mut seen: HashSet<i64> = HashSet::new();
        let duplicated: Vec<String> = raw
            .iter()
            .map(|v| v.finding_id)
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
            .map(|v| v.finding_id)
            .filter(|id| !self.batch.contains(id) && seen.insert(*id))
            .collect::<Vec<_>>()
            .iter()
            .map(i64::to_string)
            .collect();
        if !extra.is_empty() {
            problems.push(format!("not in this batch: {}", extra.join(", ")));
        }

        let no_severity: Vec<String> = raw
            .iter()
            .filter(|v| v.verdict == VerdictKind::Downgrade && v.severity.is_none())
            .map(|v| v.finding_id.to_string())
            .collect();
        if !no_severity.is_empty() {
            problems.push(format!(
                "downgrade without a severity: {}",
                no_severity.join(", ")
            ));
        }
        let bad_duplicate: Vec<String> = raw
            .iter()
            .filter(|v| {
                v.duplicate_of
                    .is_some_and(|other| other == v.finding_id || !self.batch.contains(&other))
            })
            .map(|v| format!("#{} -> #{:?}", v.finding_id, v.duplicate_of))
            .collect();
        if !bad_duplicate.is_empty() {
            problems.push(format!(
                "duplicate_of must name another finding in this batch: {}",
                bad_duplicate.join(", ")
            ));
        }

        if problems.is_empty() {
            None
        } else {
            Some(format!("verdicts rejected — {}", problems.join("; ")))
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

    /// A verifier context; the tool itself reads none of it.
    fn cx(dir: &tempfile::TempDir) -> ToolCx {
        let host = LocalHost::new(dir.path()).expect("local host");
        ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: AgentRef {
                id: 3,
                role: AgentRole::Verifier,
                label: "verifier:src-a.rs".to_string(),
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

    fn verdict(finding_id: i64, verdict: &str) -> Value {
        json!({"finding_id": finding_id, "verdict": verdict, "reason": "checked"})
    }

    #[tokio::test]
    async fn verdicts_must_cover_the_batch() {
        let dir = crate::test_support::tempdir();
        let cx = cx(&dir);
        let submitted: SharedVerdicts = Arc::new(Mutex::new(None));
        let tool = SubmitVerdicts::new(vec![1, 2, 3], submitted.clone());

        // Finding 3 has no verdict.
        let out = tool
            .call(
                json!({"verdicts": [verdict(1, "keep"), verdict(2, "drop")]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("no verdict for: 3"), "{text}");
        assert!(
            submitted.lock().unwrap().is_none(),
            "nothing stored on rejection"
        );

        // Id 1 twice, id 9 outside the batch, id 3 still missing: all listed.
        let out = tool
            .call(
                json!({"verdicts": [
                    verdict(1, "keep"), verdict(1, "keep"),
                    verdict(2, "keep"), verdict(9, "drop"),
                ]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("no verdict for: 3"), "{text}");
        assert!(text.contains("listed more than once: 1"), "{text}");
        assert!(text.contains("not in this batch: 9"), "{text}");

        // A downgrade without a severity.
        let out = tool
            .call(
                json!({"verdicts": [
                    verdict(1, "keep"), verdict(2, "downgrade"), verdict(3, "drop"),
                ]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("downgrade without a severity: 2"), "{text}");

        // duplicate_of must name another finding of this batch: neither a
        // self-pointer nor an outside id is accepted.
        for other in [1, 9] {
            let mut pointing = verdict(1, "drop");
            pointing["duplicate_of"] = json!(other);
            let out = tool
                .call(
                    json!({"verdicts": [
                        pointing, verdict(2, "keep"), verdict(3, "keep"),
                    ]}),
                    &cx,
                )
                .await;
            assert!(out.is_error, "duplicate_of {other}: {}", text_of(&out));
            let text = text_of(&out);
            assert!(
                text.contains("duplicate_of must name another finding in this batch"),
                "{text}"
            );
        }

        // A complete, well-formed batch is accepted and stored verbatim.
        let out = tool
            .call(
                json!({"verdicts": [
                    verdict(1, "keep"),
                    {"finding_id": 2, "verdict": "downgrade", "severity": "nit",
                     "reason": "real, but a style point"},
                    {"finding_id": 3, "verdict": "drop", "reason": "same issue",
                     "duplicate_of": 1},
                ]}),
                &cx,
            )
            .await;
        assert!(!out.is_error, "{}", text_of(&out));
        let stored = submitted.lock().unwrap();
        let verdicts = stored.as_ref().expect("verdicts stored");
        assert_eq!(verdicts.len(), 3);
        assert_eq!(verdicts[0].verdict, VerdictKind::Keep);
        assert_eq!(verdicts[1].verdict, VerdictKind::Downgrade);
        assert_eq!(verdicts[1].severity, Some(Severity::Nit));
        assert_eq!(verdicts[1].reason, "real, but a style point");
        assert_eq!(verdicts[2].verdict, VerdictKind::Drop);
        assert_eq!(verdicts[2].duplicate_of, Some(1));
    }

    #[test]
    fn submit_verdicts_is_stage_local() {
        assert!(
            crate::ai::tools::all()
                .iter()
                .all(|tool| tool.definition().name != "submit_verdicts"),
            "the verifier constructs it with its batch; it is not in all()"
        );
    }
}
