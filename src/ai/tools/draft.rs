//! The assembler's `submit_review_draft` tool (spec §5.6): the structured
//! finish call that carries the review draft.
//!
//! This tool is **stage-local** and deliberately absent from [`all`]: the
//! assembler stage constructs it with the kept finding ids and the shared
//! [`SharedDraft`] cell its
//! [`run_assembler`](crate::ai::stages::assemble::run_assembler) reads, so
//! the draft travels by memory, not by tool output.
//!
//! Validation runs before anything is stored: `verdict` must be `approve`,
//! `comment` or `request_changes`; the summary and every rewrite body must
//! be non-empty; rewrites may only target **kept** findings (the rejection
//! names the offenders); and a finding id may not repeat. A valid
//! submission is stored verbatim, the rewrites as a `finding id → new body`
//! map; applying them to the findings is the app's job (Plan 3).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::stages::assemble::{ReviewDraft, Verdict};

/// The cell the stage and the tool hand the draft over through.
pub type SharedDraft = Arc<Mutex<Option<ReviewDraft>>>;

/// The `submit_review_draft` tool.
pub struct SubmitReviewDraft {
    /// The ids of the findings kept for the draft; rewrites may only
    /// target these.
    kept: Vec<i64>,
    submitted: SharedDraft,
}

impl SubmitReviewDraft {
    /// A submit tool that stores the validated draft into `submitted`;
    /// `kept` is the ids of the findings the draft carries.
    pub fn new(kept: Vec<i64>, submitted: SharedDraft) -> SubmitReviewDraft {
        SubmitReviewDraft { kept, submitted }
    }
}

#[derive(Deserialize)]
struct RawInput {
    summary: String,
    verdict: String,
    rationale: String,
    #[serde(default)]
    rewrites: Vec<RawRewrite>,
}

#[derive(Deserialize)]
struct RawRewrite {
    finding_id: i64,
    body: String,
}

#[async_trait::async_trait]
impl Tool for SubmitReviewDraft {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "submit_review_draft",
            "Submit the review draft and finish assembling. `verdict` is \
             `approve`, `comment` or `request_changes`. Rewrites are optional \
             body rewrites for tone and concision; each must target a kept \
             finding by id, at most once, with a non-empty body — anchors and \
             severities never change. The summary must state what was checked.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "What the PR does, the main issues, and \
                            what was checked and what was skipped."
                    },
                    "verdict": {
                        "type": "string",
                        "enum": ["approve", "comment", "request_changes"]
                    },
                    "rationale": {
                        "type": "string",
                        "description": "One line on why this verdict."
                    },
                    "rewrites": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "finding_id": {
                                    "type": "integer",
                                    "description": "A kept finding's id."
                                },
                                "body": {
                                    "type": "string",
                                    "description": "The finding's new body."
                                }
                            },
                            "required": ["finding_id", "body"]
                        }
                    }
                },
                "required": ["summary", "verdict", "rationale"]
            }),
        )
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolOutput {
        let raw: RawInput = match parse(input) {
            Ok(raw) => raw,
            Err(out) => return out,
        };
        if let Some(problem) = self.rejection(&raw) {
            return ToolOutput::error(problem);
        }
        let rewrites: BTreeMap<i64, String> = raw
            .rewrites
            .into_iter()
            .map(|rewrite| (rewrite.finding_id, rewrite.body))
            .collect();
        let count = rewrites.len();
        let draft = ReviewDraft {
            summary: raw.summary,
            // The rejection above proved the string parses.
            verdict: Verdict::parse(&raw.verdict).expect("validated verdict"),
            rationale: raw.rationale,
            rewrites,
        };
        *self.submitted.lock().unwrap_or_else(|e| e.into_inner()) = Some(draft);
        ToolOutput::text(format!("draft accepted: {count} rewrites"))
    }
}

impl SubmitReviewDraft {
    /// Every rejection text for this submission, or `None` when the verdict
    /// is one of the three, the summary and every rewrite body are non-empty
    /// and the rewrites are unique kept findings.
    fn rejection(&self, raw: &RawInput) -> Option<String> {
        let mut problems: Vec<String> = Vec::new();

        if Verdict::parse(&raw.verdict).is_err() {
            problems.push(format!(
                "verdict must be approve, comment or request_changes: {:?}",
                raw.verdict
            ));
        }
        if raw.summary.trim().is_empty() {
            problems.push("summary is empty".to_string());
        }

        let mut counts: HashMap<i64, usize> = HashMap::new();
        for rewrite in &raw.rewrites {
            *counts.entry(rewrite.finding_id).or_insert(0) += 1;
        }
        let mut seen: HashSet<i64> = HashSet::new();
        let outside: Vec<String> = raw
            .rewrites
            .iter()
            .map(|rewrite| rewrite.finding_id)
            .filter(|id| !self.kept.contains(id) && seen.insert(*id))
            .collect::<Vec<_>>()
            .iter()
            .map(i64::to_string)
            .collect();
        if !outside.is_empty() {
            problems.push(format!(
                "rewrites may only target kept findings: {}",
                outside.join(", ")
            ));
        }
        let mut seen: HashSet<i64> = HashSet::new();
        let duplicated: Vec<String> = raw
            .rewrites
            .iter()
            .map(|rewrite| rewrite.finding_id)
            .filter(|id| counts.get(id).copied().unwrap_or(0) > 1 && seen.insert(*id))
            .collect::<Vec<_>>()
            .iter()
            .map(i64::to_string)
            .collect();
        if !duplicated.is_empty() {
            problems.push(format!(
                "rewrite listed more than once: {}",
                duplicated.join(", ")
            ));
        }
        let empty_bodies: Vec<String> = raw
            .rewrites
            .iter()
            .filter(|rewrite| rewrite.body.trim().is_empty())
            .map(|rewrite| rewrite.finding_id.to_string())
            .collect();
        if !empty_bodies.is_empty() {
            problems.push(format!(
                "rewrite with an empty body: {}",
                empty_bodies.join(", ")
            ));
        }

        if problems.is_empty() {
            None
        } else {
            Some(format!("draft rejected — {}", problems.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use review_sandbox::LocalHost;

    use crate::ai::testing::ScriptedInteraction;
    use crate::ai::tools::{AgentRef, AgentRole, Tool, ToolCx, ToolOutput};
    use crate::test_support;

    use super::*;

    /// An assembler context; the tool itself reads none of it.
    fn cx(dir: &tempfile::TempDir) -> ToolCx {
        let host = LocalHost::new(dir.path()).expect("local host");
        ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: AgentRef {
                id: 7,
                role: AgentRole::Assembler,
                label: "assembler".to_string(),
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

    #[tokio::test]
    async fn submit_review_draft_rejects_bad_verdict_and_dup_rewrites() {
        let dir = crate::test_support::tempdir();
        let cx = cx(&dir);
        let submitted: SharedDraft = Arc::new(Mutex::new(None));
        let tool = SubmitReviewDraft::new(vec![1, 2], submitted.clone());

        // A verdict outside the three values.
        let out = tool
            .call(
                json!({"summary": "s", "verdict": "lgtm", "rationale": "r"}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(
            text.contains("approve, comment or request_changes"),
            "{text}"
        );
        assert!(
            submitted.lock().unwrap().is_none(),
            "nothing stored on rejection"
        );

        // A rewrite outside the kept set names the offender.
        let out = tool
            .call(
                json!({"summary": "s", "verdict": "approve", "rationale": "r",
                       "rewrites": [{"finding_id": 9, "body": "x"},
                                    {"finding_id": 1, "body": "y"}]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(
            text.contains("rewrites may only target kept findings: 9"),
            "{text}"
        );
        assert!(submitted.lock().unwrap().is_none());

        // A repeated rewrite id.
        let out = tool
            .call(
                json!({"summary": "s", "verdict": "approve", "rationale": "r",
                       "rewrites": [{"finding_id": 1, "body": "y"},
                                    {"finding_id": 1, "body": "z"}]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(text.contains("more than once: 1"), "{text}");

        // An empty summary, and a rewrite with an empty body.
        let out = tool
            .call(
                json!({"summary": "  ", "verdict": "approve", "rationale": "r"}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        assert!(text_of(&out).contains("summary is empty"));
        let out = tool
            .call(
                json!({"summary": "s", "verdict": "comment", "rationale": "r",
                       "rewrites": [{"finding_id": 2, "body": "  "}]}),
                &cx,
            )
            .await;
        assert!(out.is_error);
        assert!(text_of(&out).contains("empty body: 2"));
        assert!(submitted.lock().unwrap().is_none(), "still nothing stored");

        // A valid submission fills the cell; the rewrites become a map.
        let out = tool
            .call(
                json!({"summary": "adds retries", "verdict": "request_changes",
                       "rationale": "one bug",
                       "rewrites": [{"finding_id": 2, "body": "rewritten"},
                                    {"finding_id": 1, "body": "shorter"}]}),
                &cx,
            )
            .await;
        assert!(!out.is_error, "{}", text_of(&out));
        let stored = submitted.lock().unwrap();
        let draft = stored.as_ref().expect("draft stored");
        assert_eq!(draft.summary, "adds retries");
        assert_eq!(draft.verdict, Verdict::RequestChanges);
        assert_eq!(draft.rationale, "one bug");
        let expected: BTreeMap<i64, String> = [(1i64, "shorter"), (2, "rewritten")]
            .into_iter()
            .map(|(id, body)| (id, body.to_string()))
            .collect();
        assert_eq!(draft.rewrites, expected);
    }

    #[test]
    fn submit_review_draft_is_stage_local() {
        assert!(
            crate::ai::tools::all()
                .iter()
                .all(|tool| tool.definition().name != "submit_review_draft"),
            "the assembler constructs it with its kept ids; it is not in all()"
        );
    }
}
