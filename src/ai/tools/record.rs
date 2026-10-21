//! The review-output tools (spec §5.4, §5.6): `record_finding`,
//! `record_check` and `done`. They write to the run's [`RunState`] — never
//! to the repo or to GitHub — so they are safe to hand to every reviewing
//! agent.
//!
//! `record_finding` enforces the anchoring rules: the anchor must be a line
//! (or range) inside the PR diff, and the error for a line outside it names
//! the nearest valid lines so the model can correct itself. `record_check`
//! enforces the assignment: a reviewer may only log checks for the
//! `(file, item)` pairs the app gave it.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::review::{CheckResult, CoverageEntry, Evidence, FindingAnchor, NewFinding};
use crate::model::comment::Anchor;
use crate::model::diff::Side;

/// The `record_finding` tool.
pub(super) struct RecordFinding;

/// The `record_check` tool.
pub(super) struct RecordCheck;

/// The `done` tool.
pub(super) struct Done;

/// How many nearest diff lines a rejected anchor error names.
const NEAREST_LINES: usize = 10;

#[derive(Deserialize)]
struct FindingInput {
    path: String,
    line: u32,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    side: Option<String>,
    severity: crate::ai::review::Severity,
    title: String,
    body: String,
    evidence: Vec<Evidence>,
    #[serde(default)]
    checklist_item: Option<String>,
}

#[derive(Deserialize)]
struct CheckInput {
    file: String,
    item: String,
    result: String,
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct DoneInput {
    #[serde(default)]
    summary: Option<String>,
}

#[async_trait::async_trait]
impl Tool for RecordFinding {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "record_finding",
            "Record one review finding. The anchor must be a line (or range) \
             inside the PR diff on the given side; other anchors are rejected \
             with the nearest valid lines listed. Evidence is required: at \
             least one {path, line, note} pointing at what you compared \
             against or traced — a location outside the PR goes in the \
             evidence, not the anchor. A finding about a file with no diff \
             (binary or asset) is stored as a summary item.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The PR file the finding is anchored on." },
                    "line": { "type": "integer", "description": "The diff line the finding ends on." },
                    "start_line": { "type": "integer", "description": "For a range, the line it starts on (same side)." },
                    "side": { "type": "string", "enum": ["RIGHT", "LEFT"], "description": "Diff side of the anchor; default RIGHT. LEFT numbers refer to the pre-change file." },
                    "severity": { "type": "string", "enum": ["bug", "risk", "nit", "question"] },
                    "title": { "type": "string", "description": "One-line headline." },
                    "body": { "type": "string", "description": "The full explanation." },
                    "evidence": {
                        "type": "array",
                        "description": "What you checked against; at least one entry, each with a non-empty note.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": "integer" },
                                "note": { "type": "string", "description": "What this location shows." }
                            },
                            "required": ["path", "note"]
                        }
                    },
                    "checklist_item": { "type": "string", "description": "The checklist item this came out of, if any." }
                },
                "required": ["path", "line", "severity", "title", "body", "evidence"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: FindingInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };

        // Evidence is what makes a finding checkable (spec §5.4).
        if input.evidence.is_empty() {
            return ToolOutput::error(
                "evidence is required: at least one {path, line, note} entry \
                 pointing at what you compared against or traced",
            );
        }
        for evidence in &input.evidence {
            if evidence.note.trim().is_empty() {
                return ToolOutput::error(format!(
                    "the evidence for {} needs a non-empty note saying what it shows",
                    evidence.path
                ));
            }
        }

        let side = match resolve_side(input.side.as_deref()) {
            Ok(side) => side,
            Err(out) => return out,
        };
        let Some(snapshot) = cx.snapshot.as_ref() else {
            return ToolOutput::error("no PR snapshot attached");
        };

        // The anchor must be on a PR file; anything outside the PR belongs in
        // the evidence (spec §5.6).
        if !snapshot.files.iter().any(|f| f.file.path == input.path) {
            return ToolOutput::error(format!(
                "{} is not in the PR: anchor on the PR line that introduces \
                 it; put the other location in evidence",
                input.path
            ));
        }

        let diff_lines = snapshot.diff_lines(&input.path);
        let anchor = if diff_lines.is_empty() {
            // In the PR but with no commentable line (binary or asset): a
            // summary item, line numbers skipped (spec §5.6).
            FindingAnchor::Summary {
                path: input.path.clone(),
            }
        } else {
            if let Err(out) = check_diff_line(&diff_lines, side, input.line, "line", &input.path) {
                return out;
            }
            if let Some(start) = input.start_line
                && let Err(out) =
                    check_diff_line(&diff_lines, side, start, "start_line", &input.path)
            {
                return out;
            }
            FindingAnchor::Line(Anchor {
                path: input.path.clone(),
                side,
                line: input.line,
                start_line: input.start_line,
                start_side: input.start_line.map(|_| side),
            })
        };

        let new = NewFinding {
            agent: cx.agent.clone(),
            anchor,
            severity: input.severity,
            title: input.title,
            body: input.body,
            evidence: input.evidence,
            checklist_item: input.checklist_item,
        };
        match cx.run.add_finding(new) {
            Ok(id) => ToolOutput::text(format!("recorded finding #{id}")),
            Err(e) => ToolOutput::error(format!("could not store the finding: {e}")),
        }
    }
}

/// Maps the tool's `side` string; absent or empty means RIGHT (spec §5.4).
pub(crate) fn resolve_side(raw: Option<&str>) -> Result<Side, ToolOutput> {
    match raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .as_deref()
    {
        None | Some("RIGHT") => Ok(Side::Right),
        Some("LEFT") => Ok(Side::Left),
        Some(other) => Err(ToolOutput::error(format!(
            "side must be \"RIGHT\" or \"LEFT\", got {other:?}"
        ))),
    }
}

/// Whether `(side, line)` is a commentable diff line of `path`; the error
/// names the nearest ones on that side.
fn check_diff_line(
    diff_lines: &[(Side, u32)],
    side: Side,
    line: u32,
    field: &str,
    path: &str,
) -> Result<(), ToolOutput> {
    if diff_lines.iter().any(|(s, l)| *s == side && *l == line) {
        return Ok(());
    }
    let side_name = side_name(side);
    let nearest = nearest_lines(diff_lines, side, line);
    if nearest.is_empty() {
        return Err(ToolOutput::error(format!(
            "{field} {line} is not a commentable {side_name} line in the diff for {path}; \
             no {side_name}-side lines exist in this file's diff — \
             anchor on the side that has lines"
        )));
    }
    let nearest: Vec<String> = nearest.iter().map(u32::to_string).collect();
    Err(ToolOutput::error(format!(
        "{field} {line} is not a commentable {side_name} line in the diff for {path}; \
         nearest {side_name} diff lines: {} — anchor on one of them",
        nearest.join(", ")
    )))
}

/// Up to [`NEAREST_LINES`] commentable line numbers on `side`, the closest
/// to `wanted` first by absolute distance, then shown ascending.
fn nearest_lines(diff_lines: &[(Side, u32)], side: Side, wanted: u32) -> Vec<u32> {
    let mut valid: Vec<u32> = diff_lines
        .iter()
        .filter(|(s, _)| *s == side)
        .map(|(_, l)| *l)
        .collect();
    valid.sort_unstable();
    valid.dedup();
    let mut by_distance = valid;
    by_distance.sort_by_key(|l| (l.abs_diff(wanted), *l));
    by_distance.truncate(NEAREST_LINES);
    by_distance.sort_unstable();
    by_distance
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Right => "RIGHT",
        Side::Left => "LEFT",
    }
}

#[async_trait::async_trait]
impl Tool for RecordCheck {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "record_check",
            "Log the result of one checklist item for one file. Every \
             (file, assigned item) pair needs exactly one record before you \
             are done. `ok` and `n/a` require a note saying what you checked \
             (e.g. \"grepped parseDate, 0 other definitions\"); `issue` needs \
             none but should come with a record_finding.",
            json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "The file the check ran against." },
                    "item": { "type": "string", "description": "The checklist item, exactly as assigned." },
                    "result": { "type": "string", "enum": ["issue", "ok", "n/a"] },
                    "note": { "type": "string", "description": "What you checked and how; required for ok and n/a." }
                },
                "required": ["file", "item", "result", "note"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: CheckInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };

        let result = match input.result.trim().to_ascii_lowercase().as_str() {
            "issue" => CheckResult::Issue,
            "ok" => CheckResult::Ok,
            "n/a" => CheckResult::NotApplicable,
            "skipped" => {
                return ToolOutput::error(
                    "\"skipped\" is recorded by the app after two coverage rounds, not by you",
                );
            }
            other => {
                return ToolOutput::error(format!(
                    "result must be one of \"issue\", \"ok\", \"n/a\" (got {other:?})"
                ));
            }
        };

        // Anti-rubber-stamp: a clean result has to say what was checked.
        if result != CheckResult::Issue && input.note.trim().is_empty() {
            return ToolOutput::error(format!(
                "result \"{}\" needs a note saying what you checked",
                result.as_str()
            ));
        }

        // Only the pairs the app assigned to this agent (empty = anything).
        if !cx.assigned.is_empty()
            && !cx
                .assigned
                .iter()
                .any(|(f, i)| *f == input.file && *i == input.item)
        {
            let items: Vec<&str> = cx
                .assigned
                .iter()
                .filter(|(f, _)| *f == input.file)
                .map(|(_, i)| i.as_str())
                .collect();
            let assigned_there = if items.is_empty() {
                "none".to_owned()
            } else {
                items.join(", ")
            };
            return ToolOutput::error(format!(
                "\"{}\" is not assigned to you for {} (assigned there: {assigned_there})",
                input.item, input.file
            ));
        }

        let entry = CoverageEntry {
            file: input.file.clone(),
            item: input.item.clone(),
            result,
            note: input.note,
        };
        match cx.run.record_check(entry) {
            Ok(()) => ToolOutput::text(format!(
                "recorded check {} · {}: {}",
                input.file,
                input.item,
                result.as_str()
            )),
            Err(e) => ToolOutput::error(format!("could not store the check: {e}")),
        }
    }
}

#[async_trait::async_trait]
impl Tool for Done {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "done",
            "Signal that you are finished with your assignment, with a \
             one-paragraph summary of what you reviewed and found. The app \
             then checks your coverage log before accepting it.",
            json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "What you reviewed and how it ended." }
                }
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: DoneInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        cx.run.mark_done(&cx.agent);
        match input.summary.filter(|s| !s.trim().is_empty()) {
            Some(summary) => ToolOutput::text(summary),
            None => ToolOutput::text("done"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ai::interaction::NoInteraction;
    use crate::ai::review::{CheckResult, FindingAnchor, RunState};
    use crate::ai::snapshot::PrSnapshot;
    use crate::ai::store::{PrKey, RunKind, Store};
    use crate::ai::tools::{AgentRef, AgentRole, Tool, ToolCx, ToolOutput, all};
    use crate::github::files::{LoadedFile, PatchState};
    use crate::test_support;
    use review_llm::ToolResultPart;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    fn tool(name: &str) -> Arc<dyn Tool> {
        all()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    fn text(out: &ToolOutput) -> &str {
        assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
        match &out.parts[0] {
            ToolResultPart::Text { text } => text,
            other => panic!("not text: {other:?}"),
        }
    }

    struct Ctx {
        _dir: tempfile::TempDir,
        cx: ToolCx,
        run: Arc<RunState>,
    }

    /// A reviewer's context over `files` with `assigned` as its assignment
    /// list, backed by an in-memory store.
    fn ctx(files: Vec<LoadedFile>, assigned: &[(&str, &str)]) -> Ctx {
        let dir = test_support::tempdir();
        let host = review_sandbox::LocalHost::new(dir.path()).unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = store
            .session_id(&PrKey {
                host: "github.com".to_string(),
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 7,
            })
            .unwrap();
        let run_id = store
            .run_begin(session, RunKind::Full, None, "sha_head")
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let run = Arc::new(RunState::new(store, session, run_id, tx));
        let snapshot = Arc::new(PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files,
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        });
        let cx = ToolCx {
            sandbox: Arc::new(host),
            repo_root: dir.path().to_path_buf(),
            agent: AgentRef {
                id: 1,
                role: AgentRole::Reviewer,
                label: "reviewer 1".to_string(),
            },
            interaction: Arc::new(NoInteraction),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot: Some(snapshot),
            context: None,
            run: run.clone(),
            assigned: Arc::new(
                assigned
                    .iter()
                    .map(|(f, i)| (f.to_string(), i.to_string()))
                    .collect(),
            ),
            spawner: None,
            prepare: None,
        };
        Ctx { _dir: dir, cx, run }
    }

    /// A file whose RIGHT side covers lines 10..=14 (five context rows).
    fn patched(path: &str) -> LoadedFile {
        test_support::loaded(path, "@@ -10,5 +10,5 @@\n c1\n c2\n c3\n c4\n c5\n")
    }

    fn binary(path: &str) -> LoadedFile {
        LoadedFile::parse(crate::github::files::PrFile {
            patch: None,
            patch_state: PatchState::Binary,
            ..test_support::file(path)
        })
    }

    fn finding_input(path: &str, line: u32) -> Value {
        json!({
            "path": path,
            "line": line,
            "severity": "bug",
            "title": "t",
            "body": "b",
            "evidence": [{"path": path, "line": line, "note": "because"}],
        })
    }

    async fn call(ctx: &Ctx, name: &str, input: Value) -> ToolOutput {
        tool(name).call(input, &ctx.cx).await
    }

    #[tokio::test]
    async fn finding_without_evidence_is_rejected() {
        let ctx = ctx(vec![patched("a.rs")], &[]);

        let mut input = finding_input("a.rs", 12);
        input["evidence"] = json!([]);
        let out = call(&ctx, "record_finding", input).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("evidence"), "{}", text(&out));

        // A blank note is no evidence either.
        let mut input = finding_input("a.rs", 12);
        input["evidence"] = json!([{"path": "a.rs", "note": "   "}]);
        let out = call(&ctx, "record_finding", input).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("evidence"), "{}", text(&out));
        assert!(ctx.run.findings().is_empty());
    }

    #[tokio::test]
    async fn finding_outside_the_diff_lists_nearest_lines() {
        let ctx = ctx(vec![patched("a.rs")], &[]);

        // The diff covers RIGHT 10..=14; line 30 is not in it.
        let out = call(&ctx, "record_finding", finding_input("a.rs", 30)).await;
        assert!(out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("30"), "{t}");
        assert!(t.contains("10") && t.contains("14"), "{t}");

        // A line inside the diff succeeds and reports the new id.
        let out = call(&ctx, "record_finding", finding_input("a.rs", 13)).await;
        assert!(!out.is_error, "{:?}", out.parts);
        let id = ctx.run.findings()[0].id;
        assert!(text(&out).contains(&id.to_string()), "{}", text(&out));

        // A range's start_line is validated against the same side.
        let mut input = finding_input("a.rs", 13);
        input["start_line"] = json!(31);
        let out = call(&ctx, "record_finding", input).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(ctx.run.findings().len() == 1, "the range was not stored");
    }

    #[tokio::test]
    async fn left_side_anchor_validates_against_old_lines() {
        // LEFT lines 10..=12 (removed rows); RIGHT only line 10 (the added row).
        let file = test_support::loaded("a.rs", "@@ -10,3 +10,1 @@\n-x\n-y\n-z\n+new\n");
        let ctx = ctx(vec![file], &[]);

        let mut input = finding_input("a.rs", 11);
        input["side"] = json!("left");
        let out = call(&ctx, "record_finding", input).await;
        assert!(!out.is_error, "{}", text(&out));

        // 11 is not a RIGHT line (RIGHT has only 10)…
        let out = call(&ctx, "record_finding", finding_input("a.rs", 11)).await;
        assert!(out.is_error, "RIGHT has only line 10");

        // …and 30 is not a LEFT line; the error lists the nearest ones.
        let mut input = finding_input("a.rs", 30);
        input["side"] = json!("LEFT");
        let out = call(&ctx, "record_finding", input).await;
        assert!(out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("10") && t.contains("12"), "{t}");
    }

    #[tokio::test]
    async fn anchor_on_a_side_without_lines_says_so() {
        // A pure deletion: the diff has LEFT lines only.
        let file = test_support::loaded("a.rs", "@@ -5,2 +5,0 @@\n-gone5\n-gone6\n");
        let ctx = ctx(vec![file], &[]);

        // The default RIGHT side has no lines: the error says so rather than
        // listing an empty nearest set.
        let out = call(&ctx, "record_finding", finding_input("a.rs", 5)).await;
        assert!(out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("no RIGHT-side lines exist"), "{t}");
        assert!(!t.contains("nearest"), "{t}");

        // The side that has lines still anchors fine.
        let mut input = finding_input("a.rs", 5);
        input["side"] = json!("LEFT");
        let out = call(&ctx, "record_finding", input).await;
        assert!(!out.is_error, "{}", text(&out));
    }

    #[tokio::test]
    async fn binary_file_finding_becomes_a_summary_item() {
        let ctx = ctx(vec![patched("a.rs"), binary("logo.png")], &[]);

        let out = call(&ctx, "record_finding", finding_input("logo.png", 3)).await;
        assert!(!out.is_error, "{}", text(&out));
        assert_eq!(
            ctx.run.findings()[0].anchor,
            FindingAnchor::Summary {
                path: "logo.png".to_string()
            }
        );

        // A path the PR does not touch is an error with the anchoring advice.
        let out = call(&ctx, "record_finding", finding_input("nope.rs", 3)).await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(
            text(&out).contains(
                "anchor on the PR line that introduces it; put the other location in evidence"
            ),
            "{}",
            text(&out)
        );
    }

    #[tokio::test]
    async fn ok_check_needs_a_note_and_an_assigned_item() {
        let ctx = ctx(vec![patched("a.rs")], &[("a.rs", "errors")]);

        // `ok` without a note.
        let out = call(
            &ctx,
            "record_check",
            json!({"file": "a.rs", "item": "errors", "result": "ok", "note": "  "}),
        )
        .await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("note"), "{}", text(&out));

        // An item that is not assigned: the error names what is.
        let out = call(
            &ctx,
            "record_check",
            json!({"file": "a.rs", "item": "naming", "result": "ok", "note": "checked"}),
        )
        .await;
        assert!(out.is_error, "{:?}", out.parts);
        assert!(text(&out).contains("errors"), "{}", text(&out));

        // `issue` needs no note; `ok` and `n/a` with one are recorded.
        let out = call(
            &ctx,
            "record_check",
            json!({"file": "a.rs", "item": "errors", "result": "issue", "note": ""}),
        )
        .await;
        assert!(!out.is_error, "{}", text(&out));
        let out = call(
            &ctx,
            "record_check",
            json!({"file": "a.rs", "item": "errors", "result": "n/a", "note": "no dates here"}),
        )
        .await;
        assert!(!out.is_error, "{}", text(&out));

        let cov = ctx.run.coverage();
        assert_eq!(cov.len(), 2);
        assert_eq!(cov[0].result, CheckResult::Issue);
        assert_eq!(cov[1].result, CheckResult::NotApplicable);

        // `skipped` is the app's, not the model's.
        let out = call(
            &ctx,
            "record_check",
            json!({"file": "a.rs", "item": "errors", "result": "skipped", "note": "x"}),
        )
        .await;
        assert!(out.is_error, "{:?}", out.parts);
    }

    #[tokio::test]
    async fn done_marks_the_calling_agent_and_echoes_the_summary() {
        let ctx = ctx(vec![patched("a.rs")], &[]);
        let out = call(&ctx, "done", json!({"summary": "reviewed a.rs, 1 finding"})).await;
        assert!(!out.is_error, "{:?}", out.parts);
        assert!(
            text(&out).contains("reviewed a.rs, 1 finding"),
            "{}",
            text(&out)
        );
        assert!(
            ctx.run.take_done(&ctx.cx.agent),
            "the tool set the flag for its caller"
        );

        let out = call(&ctx, "done", json!({})).await;
        assert!(!out.is_error, "{:?}", out.parts);
        assert!(ctx.run.take_done(&ctx.cx.agent));
    }
}
