//! The scout's `submit_brief` tool (spec §5.2): the structured finish call
//! that carries the review brief.
//!
//! This tool is **stage-local** and deliberately absent from [`all`]: the
//! scout stage constructs it with the shared [`SharedBrief`] cell its
//! [`run_scout`](crate::ai::stages::scout::run_scout) reads, so the brief
//! travels by memory, not by tool output.
//!
//! Validation runs **before** any normalization: every PR file must appear
//! in exactly one group and no group may be empty (its leads and CI notes
//! would be silently lost), or the call is rejected listing the missing
//! paths, the paths listed more than once, and the empty groups so the model
//! can correct itself. A valid brief is stored into the cell verbatim; ids,
//! the Generated skim group and the size splits are the stage's to apply.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::snapshot::PrSnapshot;
use crate::ai::stages::scout::{Brief, Group, GroupFile, Risk};

/// The cell the stage and the tool hand the brief over through.
pub type SharedBrief = Arc<Mutex<Option<Brief>>>;

/// The `submit_brief` tool.
pub struct SubmitBrief {
    submitted: SharedBrief,
}

impl SubmitBrief {
    /// A submit tool that stores the validated brief into `submitted`.
    pub fn new(submitted: SharedBrief) -> SubmitBrief {
        SubmitBrief { submitted }
    }
}

/// The tool's input: like [`Brief`], but the groups carry no ids (the app
/// assigns them) and their files are plain paths.
#[derive(Deserialize)]
struct RawBrief {
    intent: String,
    description_matches: bool,
    #[serde(default)]
    unexplained: Vec<String>,
    #[serde(default)]
    suggested_commands: Vec<String>,
    #[serde(default)]
    suggested_prepare: Vec<String>,
    groups: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawGroup {
    title: String,
    risk: Risk,
    reason: String,
    files: Vec<String>,
    #[serde(default)]
    leads: Vec<String>,
    #[serde(default)]
    extra_checks: Vec<String>,
    #[serde(default)]
    ci_notes: Vec<String>,
    #[serde(default)]
    skim: bool,
}

#[async_trait::async_trait]
impl Tool for SubmitBrief {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "submit_brief",
            "Submit the review brief and finish scouting. Every file the PR changes must \
             appear in exactly one group and no group may be empty; the call is rejected \
             listing any missing or duplicated paths and any empty groups. Generated \
             files, lockfiles and vendored code belong in a \
             `skim` group.",
            json!({
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "What the PR is trying to do, in 2-4 sentences."
                    },
                    "description_matches": {
                        "type": "boolean",
                        "description": "Whether the PR description matches the diff."
                    },
                    "unexplained": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Changes the description does not explain."
                    },
                    "suggested_commands": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Build/test/lint commands, for a repo with none \
                            configured."
                    },
                    "suggested_prepare": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Prepare commands (outside the sandbox), for a repo \
                            with none configured."
                    },
                    "groups": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "risk": {
                                    "type": "string",
                                    "enum": ["high", "medium", "low"]
                                },
                                "reason": {
                                    "type": "string",
                                    "description": "One line on why this risk."
                                },
                                "files": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Repo-relative paths, each in exactly \
                                        one group."
                                },
                                "leads": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Things to look at; hints, not findings."
                                },
                                "extra_checks": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Checklist item ids to add to the group."
                                },
                                "ci_notes": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Which CI failures relate to this group."
                                },
                                "skim": {
                                    "type": "boolean",
                                    "description": "True for generated/lockfile/vendored \
                                        groups."
                                }
                            },
                            "required": ["title", "risk", "reason", "files"]
                        }
                    }
                },
                "required": ["intent", "description_matches", "groups"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let raw: RawBrief = match parse(input) {
            Ok(raw) => raw,
            Err(out) => return out,
        };
        let Some(snapshot) = cx.snapshot.as_deref() else {
            return ToolOutput::error("submit_brief: no PR snapshot is attached");
        };
        if let Some(problem) = coverage_problem(snapshot, &raw) {
            return ToolOutput::error(problem);
        }
        let groups = raw.groups.len();
        *self.submitted.lock().unwrap_or_else(|e| e.into_inner()) = Some(to_brief(raw));
        ToolOutput::text(format!("brief accepted: {groups} groups"))
    }
}

/// Every PR file in exactly one group and no empty groups, or the rejection
/// text listing the missing paths, the paths listed more than once, and the
/// empty groups (in snapshot / submission order).
fn coverage_problem(snapshot: &PrSnapshot, raw: &RawBrief) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for group in &raw.groups {
        for path in &group.files {
            *counts.entry(path.as_str()).or_insert(0) += 1;
        }
    }
    let missing: Vec<&str> = snapshot
        .files
        .iter()
        .map(|loaded| loaded.file.path.as_str())
        .filter(|path| !counts.contains_key(*path))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let duplicates: Vec<&str> = raw
        .groups
        .iter()
        .flat_map(|group| group.files.iter().map(String::as_str))
        .filter(|path| counts.get(path).copied().unwrap_or(0) > 1 && seen.insert(path))
        .collect();
    let empty: Vec<&str> = raw
        .groups
        .iter()
        .filter(|group| group.files.is_empty())
        .map(|group| group.title.as_str())
        .collect();
    if missing.is_empty() && duplicates.is_empty() && empty.is_empty() {
        return None;
    }
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!("missing from every group: {}", missing.join(", ")));
    }
    if !duplicates.is_empty() {
        problems.push(format!("listed more than once: {}", duplicates.join(", ")));
    }
    if !empty.is_empty() {
        problems.push(format!("groups with no files: {}", empty.join(", ")));
    }
    Some(format!("brief rejected — {}", problems.join("; ")))
}

/// The stored shape: empty group ids (the stage assigns them) and hunks-free
/// file entries.
fn to_brief(raw: RawBrief) -> Brief {
    Brief {
        intent: raw.intent,
        description_matches: raw.description_matches,
        unexplained: raw.unexplained,
        groups: raw
            .groups
            .into_iter()
            .map(|group| Group {
                id: String::new(),
                title: group.title,
                risk: group.risk,
                reason: group.reason,
                files: group
                    .files
                    .into_iter()
                    .map(|path| GroupFile { path, hunks: None })
                    .collect(),
                leads: group.leads,
                extra_checks: group.extra_checks,
                ci_notes: group.ci_notes,
                skim: group.skim,
            })
            .collect(),
        suggested_commands: raw.suggested_commands,
        suggested_prepare: raw.suggested_prepare,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    use review_llm::ToolResultPart;
    use review_sandbox::LocalHost;

    use super::*;
    use crate::ai::snapshot::PrSnapshot;
    use crate::ai::testing::ScriptedInteraction;
    use crate::ai::tools::{AgentRef, AgentRole, Tool, ToolCx, ToolOutput};
    use crate::github::files::LoadedFile;
    use crate::test_support;

    /// A scout context whose snapshot covers one [`test_support::file`] per
    /// path.
    fn cx_with(paths: &[&str], dir: &tempfile::TempDir) -> ToolCx {
        let host = LocalHost::new(dir.path()).expect("local host");
        ToolCx {
            sandbox: Arc::new(host.clone()),
            repo_root: host.root().path().to_path_buf(),
            agent: AgentRef {
                id: 1,
                role: AgentRole::Scout,
                label: "scout".to_string(),
            },
            interaction: Arc::new(ScriptedInteraction::new()),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: Some(Arc::new(PrSnapshot {
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
            })),
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
                ToolResultPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect()
    }

    #[tokio::test]
    async fn submit_brief_rejects_missing_and_duplicate_files() {
        let dir = crate::test_support::tempdir();
        let cx = cx_with(&["src/a.rs", "src/b.rs", "src/c.rs"], &dir);
        let submitted: SharedBrief = Arc::new(Mutex::new(None));
        let tool = SubmitBrief::new(submitted.clone());

        // src/b.rs is in two groups; src/c.rs is in none.
        let out = tool
            .call(
                json!({
                    "intent": "does things",
                    "description_matches": true,
                    "groups": [
                        { "title": "one", "risk": "medium", "reason": "r",
                          "files": ["src/a.rs", "src/b.rs"] },
                        { "title": "two", "risk": "low", "reason": "r",
                          "files": ["src/b.rs"] },
                    ],
                }),
                &cx,
            )
            .await;

        assert!(out.is_error);
        let text = text_of(&out);
        assert!(
            text.contains("src/c.rs"),
            "the missing path is listed: {text}"
        );
        assert!(
            text.contains("src/b.rs"),
            "the duplicated path is listed: {text}"
        );
        assert!(
            submitted.lock().unwrap().is_none(),
            "nothing stored on rejection"
        );

        // A complete, non-duplicate submission is accepted and stored.
        let out = tool
            .call(
                json!({
                    "intent": "does things",
                    "description_matches": false,
                    "unexplained": ["a stray rename"],
                    "groups": [
                        { "title": "one", "risk": "medium", "reason": "r",
                          "files": ["src/a.rs", "src/b.rs", "src/c.rs"] },
                    ],
                }),
                &cx,
            )
            .await;
        assert!(!out.is_error, "{}", text_of(&out));
        let stored = submitted.lock().unwrap();
        let brief = stored.as_ref().expect("brief stored");
        assert_eq!(brief.groups.len(), 1);
        assert_eq!(brief.groups[0].id, "", "ids are the stage's to assign");
        assert_eq!(brief.unexplained, vec!["a stray rename".to_string()]);
    }

    #[tokio::test]
    async fn submit_brief_rejects_empty_groups() {
        let dir = crate::test_support::tempdir();
        let cx = cx_with(&["src/a.rs"], &dir);
        let submitted: SharedBrief = Arc::new(Mutex::new(None));
        let tool = SubmitBrief::new(submitted.clone());

        // A group with no files: its leads and ci_notes would be lost.
        let out = tool
            .call(
                json!({
                    "intent": "x",
                    "description_matches": true,
                    "groups": [
                        { "title": "notes only", "risk": "low", "reason": "r",
                          "files": [], "leads": ["look here"] },
                        { "title": "real", "risk": "low", "reason": "r",
                          "files": ["src/a.rs"] },
                    ],
                }),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(
            text.contains("notes only"),
            "the empty group's title is named: {text}"
        );
        assert!(
            submitted.lock().unwrap().is_none(),
            "nothing stored on rejection"
        );

        // Folded into the same rejection when other problems exist, and a
        // path listed twice inside one group is caught too.
        let out = tool
            .call(
                json!({
                    "intent": "x",
                    "description_matches": true,
                    "groups": [
                        { "title": "empty one", "risk": "low", "reason": "r", "files": [] },
                        { "title": "twice", "risk": "low", "reason": "r",
                          "files": ["src/a.rs", "src/a.rs"] },
                    ],
                }),
                &cx,
            )
            .await;
        assert!(out.is_error);
        let text = text_of(&out);
        assert!(
            text.contains("empty one"),
            "the empty group is named: {text}"
        );
        assert!(text.contains("src/a.rs"), "the duplicate is named: {text}");
        assert!(
            text.contains("listed more than once"),
            "same-group duplicates are worded accurately: {text}"
        );
    }

    #[tokio::test]
    async fn submit_brief_without_a_snapshot_is_an_error() {
        let dir = crate::test_support::tempdir();
        let mut cx = cx_with(&["src/a.rs"], &dir);
        cx.snapshot = None;
        let tool = SubmitBrief::new(Arc::new(Mutex::new(None)));

        let out = tool
            .call(
                json!({
                    "intent": "x",
                    "description_matches": true,
                    "groups": [],
                }),
                &cx,
            )
            .await;
        assert!(out.is_error, "{}", text_of(&out));
    }
}
