//! The `gh_*` tools: the PR snapshot, read back to the model.
//!
//! These never touch the network — everything comes from [`ToolCx::snapshot`],
//! which [`crate::ai::snapshot::gather_snapshot`] filled before the run
//! started. With no snapshot attached (tool tests, chat before a PR loads)
//! every tool says so instead of guessing.
//!
//! [`gh_pr_patch`] renders from the *parsed* hunks, not GitHub's raw patch
//! string: every row carries its new-side line number in a 6-wide gutter, so
//! a finding can cite the exact line the model is looking at. Removed rows
//! have no new-side number and get a blank gutter.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};
use crate::ai::snapshot::PrSnapshot;
use crate::github::files::FileStatus;
use crate::model::diff::LineKind;
use crate::model::thread::{ReviewThread, SubjectType};

/// The snapshot every `gh_*` tool reads, or the error to return without one.
fn snapshot(cx: &ToolCx) -> Result<&PrSnapshot, ToolOutput> {
    cx.snapshot
        .as_deref()
        .ok_or_else(|| ToolOutput::error("no PR snapshot attached"))
}

/// No-input tools still parse their input, so a model sending a non-object
/// gets a correction instead of silent success.
#[derive(Deserialize)]
struct NoInput {}

/// The `gh_pr` tool: the PR's identity and description.
pub(super) struct GhPr;

#[async_trait::async_trait]
impl Tool for GhPr {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "gh_pr",
            "The pull request under review: title, author, state, base and head refs with \
             their SHAs, labels, and the body. Reads the run's snapshot; no network.",
            json!({"type": "object", "properties": {}}),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        if let Err(out) = parse::<NoInput>(input) {
            return out;
        }
        let snapshot = match snapshot(cx) {
            Ok(snapshot) => snapshot,
            Err(out) => return out,
        };
        let meta = &snapshot.metadata;
        let labels = if meta.labels.is_empty() {
            "(none)".to_owned()
        } else {
            meta.labels.join(", ")
        };
        let body = if meta.body.trim().is_empty() {
            "(empty)".to_owned()
        } else {
            meta.body.clone()
        };
        ToolOutput::text(format!(
            "title: {}\nauthor: {}\nstate: {}{}\nbase: {} @ {}\nhead: {} @ {}\nlabels: {labels}\nbody:\n{body}",
            meta.title,
            meta.author,
            meta.state,
            if meta.draft { " (draft)" } else { "" },
            meta.base_ref,
            meta.base_sha,
            meta.head_ref,
            meta.head_sha,
        ))
    }
}

/// The `gh_pr_files` tool: the changed-file listing.
pub(super) struct GhPrFiles;

#[async_trait::async_trait]
impl Tool for GhPrFiles {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "gh_pr_files",
            "Every file the pull request changes, one per line, as \
             '<path> <status> +<additions> -<deletions>'. Read the run's snapshot; no network.",
            json!({"type": "object", "properties": {}}),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        if let Err(out) = parse::<NoInput>(input) {
            return out;
        }
        let snapshot = match snapshot(cx) {
            Ok(snapshot) => snapshot,
            Err(out) => return out,
        };
        let lines: Vec<String> = snapshot
            .files
            .iter()
            .map(|f| {
                format!(
                    "{} {} +{} -{}",
                    f.file.path,
                    status_name(f.file.status),
                    f.file.additions,
                    f.file.deletions
                )
            })
            .collect();
        if lines.is_empty() {
            return ToolOutput::text("(no files)");
        }
        ToolOutput::text(lines.join("\n"))
    }
}

/// The `gh_pr_patch` tool: one file's diff with new-side line numbers.
pub(super) struct GhPrPatch;

#[derive(Deserialize)]
struct PatchInput {
    path: String,
}

#[async_trait::async_trait]
impl Tool for GhPrPatch {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "gh_pr_patch",
            "One file's diff from the pull request, with every row prefixed by its new-side \
             line number in a 6-wide gutter (removed rows have a blank gutter), each hunk \
             preceded by its @@ header. Cite those numbers when reporting a finding. The path \
             must be one from gh_pr_files. Read the run's snapshot; no network.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The file to show, exactly as gh_pr_files lists it."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: PatchInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let snapshot = match snapshot(cx) {
            Ok(snapshot) => snapshot,
            Err(out) => return out,
        };
        let Some(file) = snapshot.files.iter().find(|f| f.file.path == input.path) else {
            return ToolOutput::error(format!("unknown path: {}", input.path));
        };
        let Some(diff) = file.diff.as_ref() else {
            return ToolOutput::error(format!(
                "no patch for {} (GitHub sent none: a binary file or a diff too large)",
                input.path
            ));
        };
        ToolOutput::text(render_patch(diff))
    }
}

/// The `gh_threads` tool: the existing review conversations.
pub(super) struct GhThreads;

#[derive(Deserialize)]
struct ThreadsInput {
    #[serde(default)]
    path: Option<String>,
}

#[async_trait::async_trait]
impl Tool for GhThreads {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "gh_threads",
            "The pull request's existing review threads: '<path>:<line> <author>: <body>' per \
             thread with replies indented, marked [resolved] or [outdated]. With a path, only \
             threads on that file. Check these before reporting anything — the point may \
             already be raised. Read the run's snapshot; no network.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Only threads on this file."
                    }
                }
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: ThreadsInput = match parse(input) {
            Ok(input) => input,
            Err(out) => return out,
        };
        let snapshot = match snapshot(cx) {
            Ok(snapshot) => snapshot,
            Err(out) => return out,
        };
        let path = input
            .path
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty());
        let threads: Vec<&ReviewThread> = snapshot
            .threads
            .iter()
            .filter(|t| path.as_deref().is_none_or(|p| t.path == p))
            .collect();
        if threads.is_empty() {
            return ToolOutput::text(match &path {
                Some(p) => format!("(no threads on {p})"),
                None => "(no review threads)".to_owned(),
            });
        }
        let out: Vec<String> = threads.iter().map(|thread| render_thread(thread)).collect();
        ToolOutput::text(out.join("\n"))
    }
}

/// The `gh_checks` tool: CI for the head commit.
pub(super) struct GhChecks;

#[async_trait::async_trait]
impl Tool for GhChecks {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "gh_checks",
            "The head commit's check runs: '<name> <conclusion>' per run, its summary and \
             text indented under it, and any annotations as '<path>:<line> <level>: \
             <message>'. Read the run's snapshot; no network.",
            json!({"type": "object", "properties": {}}),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        if let Err(out) = parse::<NoInput>(input) {
            return out;
        }
        let snapshot = match snapshot(cx) {
            Ok(snapshot) => snapshot,
            Err(out) => return out,
        };
        if snapshot.checks.is_empty() {
            return ToolOutput::text("(no check runs)");
        }
        let out: Vec<String> = snapshot
            .checks
            .iter()
            .flat_map(|check| {
                let mut lines = vec![format!(
                    "{} {}",
                    check.name,
                    check.conclusion.as_deref().unwrap_or("in_progress")
                )];
                if let Some(summary) = &check.summary {
                    lines.push(format!("  summary: {summary}"));
                }
                if let Some(text) = &check.text {
                    lines.push(format!("  text: {text}"));
                }
                for annotation in &check.annotations {
                    let title = annotation
                        .title
                        .as_deref()
                        .map(|t| format!("{t}: "))
                        .unwrap_or_default();
                    lines.push(format!(
                        "  {}:{} {}: {title}{}",
                        annotation.path,
                        annotation.start_line,
                        annotation.level,
                        annotation.message
                    ));
                }
                lines
            })
            .collect();
        ToolOutput::text(out.join("\n"))
    }
}

/// The wire-style name of a file status, as GitHub's `status` field spells it.
fn status_name(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "added",
        FileStatus::Removed => "removed",
        FileStatus::Modified => "modified",
        FileStatus::Renamed => "renamed",
        FileStatus::Copied => "copied",
        FileStatus::Changed => "changed",
        FileStatus::Unchanged => "unchanged",
    }
}

/// Render one file's parsed diff with new-side line numbers in the gutter.
///
/// Each hunk starts with its `@@` header (start lines and section; GitHub's
/// counts are dropped), then one line per row: the new-side number right-aligned
/// in 6 columns (blank for a removed row, which has none), a space, and the
/// row's patch marker and text.
fn render_patch(diff: &crate::model::diff::FileDiff) -> String {
    let mut lines: Vec<String> = Vec::new();
    for hunk in &diff.hunks {
        if hunk.section.is_empty() {
            lines.push(format!("@@ -{} +{} @@", hunk.old_start, hunk.new_start));
        } else {
            lines.push(format!(
                "@@ -{} +{} @@ {}",
                hunk.old_start, hunk.new_start, hunk.section
            ));
        }
        for row in &hunk.rows {
            let (marker, gutter) = match row.kind {
                LineKind::Removed => ("-", "      ".to_owned()),
                LineKind::Added => ("+", gutter(row.new_no)),
                LineKind::Context => (" ", gutter(row.new_no)),
            };
            lines.push(format!("{gutter} {marker}{}", row.text));
        }
    }
    lines.join("\n")
}

/// The 6-wide, right-aligned new-side line number, or blanks when there is none.
fn gutter(new_no: Option<u32>) -> String {
    match new_no {
        Some(no) => format!("{no:>6}"),
        None => "      ".to_owned(),
    }
}

/// Render one thread: its anchor and state on the root comment, replies indented.
fn render_thread(thread: &ReviewThread) -> String {
    let anchor = match thread.subject_type {
        SubjectType::File => format!("{} (file)", thread.path),
        SubjectType::Line => match thread.line {
            Some(line) => format!("{}:{}", thread.path, line),
            None => format!("{} (line moved)", thread.path),
        },
    };
    let mut flags = String::new();
    if thread.is_resolved {
        flags.push_str(" [resolved]");
    }
    if thread.is_outdated {
        flags.push_str(" [outdated]");
    }
    let mut lines: Vec<String> = thread
        .comments
        .iter()
        .map(|comment| format!("{}: {}", comment.author, comment.body))
        .collect();
    match lines.first_mut() {
        Some(first) => *first = format!("{anchor} {first}{flags}"),
        None => lines.push(format!("{anchor}{flags}")),
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line.clone()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use crate::ai::interaction::NoInteraction;
    use crate::ai::snapshot::{CheckSummary, PrSnapshot};
    use crate::ai::tools::{AgentRef, AgentRole, ToolCx, ToolOutput};
    use crate::github::checks::CheckAnnotation;
    use crate::github::conversation::ConversationComment;
    use crate::github::files::{LoadedFile, PatchState};
    use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
    use crate::test_support;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    /// A context with `snapshot` attached, over a throwaway checkout.
    fn gh_ctx(snapshot: Option<Arc<PrSnapshot>>) -> (tempfile::TempDir, ToolCx) {
        let dir = test_support::tempdir();
        let host = review_sandbox::LocalHost::new(dir.path()).unwrap();
        let cx = ToolCx {
            sandbox: Arc::new(host),
            repo_root: dir.path().to_path_buf(),
            agent: AgentRef {
                id: 1,
                role: AgentRole::Reviewer,
                label: "reviewer".to_owned(),
            },
            interaction: Arc::new(NoInteraction),
            allowed: Arc::new(Mutex::new(Vec::new())),
            for_review: Arc::new(Mutex::new(Vec::new())),
            snapshot,
            context: None,
            run: crate::test_support::ai_run_state(),
            assigned: Arc::new(Vec::new()),
            spawner: None,
            prepare: None,
        };
        (dir, cx)
    }

    async fn call(snapshot: Option<Arc<PrSnapshot>>, name: &str, input: Value) -> ToolOutput {
        let (_dir, cx) = gh_ctx(snapshot);
        super::super::all()
            .into_iter()
            .find(|t| t.definition().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
            .call(input, &cx)
            .await
    }

    fn snapshot_with(
        files: Vec<LoadedFile>,
        threads: Vec<ReviewThread>,
        checks: Vec<CheckSummary>,
    ) -> Arc<PrSnapshot> {
        Arc::new(PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files,
            threads,
            conversation: vec![ConversationComment {
                author: "carol".to_owned(),
                body: "looks good".to_owned(),
                created_at: "2026-10-01T00:00:00Z".to_owned(),
            }],
            checks,
        })
    }

    fn thread(
        path: &str,
        line: Option<u32>,
        author: &str,
        body: &str,
        resolved: bool,
    ) -> ReviewThread {
        ReviewThread {
            id: "PRRT_1".to_owned(),
            is_resolved: resolved,
            is_outdated: false,
            path: path.to_owned(),
            line,
            start_line: None,
            side: None,
            subject_type: SubjectType::Line,
            comments: vec![ThreadComment {
                pending: false,
                database_id: 1,
                author: author.to_owned(),
                avatar_url: None,
                body: body.to_owned(),
                created_at: "2026-10-01T00:00:00Z".to_owned(),
            }],
        }
    }

    #[tokio::test]
    async fn gh_tools_without_a_snapshot_say_so() {
        for name in [
            "gh_pr",
            "gh_pr_files",
            "gh_pr_patch",
            "gh_threads",
            "gh_checks",
        ] {
            let out = call(None, name, json!({"path": "a.rs"})).await;
            assert!(out.is_error, "{name}: {:?}", out.parts);
            assert_eq!(text(&out), "no PR snapshot attached", "{name}");
        }
    }

    #[tokio::test]
    async fn gh_pr_reports_the_pr() {
        let snapshot = snapshot_with(Vec::new(), Vec::new(), Vec::new());
        let out = call(Some(snapshot), "gh_pr", json!({})).await;
        assert!(!out.is_error);
        let t = text(&out);
        assert!(t.contains("title: a pull request"), "{t}");
        assert!(t.contains("author: author"), "{t}");
        assert!(t.contains("base: main @ sha_base"), "{t}");
        assert!(t.contains("head: feature @ sha_head"), "{t}");
        assert!(t.contains("labels: (none)"), "{t}");
    }

    #[tokio::test]
    async fn gh_pr_files_lists_one_line_per_file() {
        let snapshot = snapshot_with(
            vec![test_support::loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            Vec::new(),
            Vec::new(),
        );
        let out = call(Some(snapshot), "gh_pr_files", json!({})).await;
        assert!(!out.is_error);
        assert_eq!(text(&out), "a.rs modified +1 -1");
    }

    #[tokio::test]
    async fn gh_pr_patch_unknown_path_is_an_error() {
        let snapshot = snapshot_with(
            vec![test_support::loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            Vec::new(),
            Vec::new(),
        );
        let out = call(Some(snapshot), "gh_pr_patch", json!({"path": "missing.rs"})).await;
        assert!(out.is_error);
        assert!(text(&out).contains("unknown path"), "{}", text(&out));
    }

    #[tokio::test]
    async fn gh_pr_patch_without_a_patch_explains_it() {
        let binary = crate::github::files::LoadedFile {
            file: crate::github::files::PrFile {
                patch: None,
                patch_state: PatchState::Binary,
                ..test_support::file("logo.png")
            },
            diff: None,
        };
        let snapshot = snapshot_with(vec![binary], Vec::new(), Vec::new());
        let out = call(Some(snapshot), "gh_pr_patch", json!({"path": "logo.png"})).await;
        assert!(out.is_error);
        assert!(text(&out).contains("no patch"), "{}", text(&out));
    }

    #[tokio::test]
    async fn gh_pr_patch_numbers_new_side_lines() {
        let patch = "@@ -10,4 +12,4 @@ fn main()\n ctx\n+added\n-removed\n ctx2\n";
        let snapshot = snapshot_with(
            vec![test_support::loaded("a.rs", patch)],
            Vec::new(),
            Vec::new(),
        );
        let out = call(Some(snapshot), "gh_pr_patch", json!({"path": "a.rs"})).await;
        assert!(!out.is_error, "{:?}", out.parts);
        assert_eq!(
            text(&out),
            [
                "@@ -10 +12 @@ fn main()",
                "    12  ctx",
                "    13 +added",
                "       -removed",
                "    14  ctx2",
            ]
            .join("\n")
        );
    }

    #[tokio::test]
    async fn gh_threads_filters_by_path() {
        let snapshot = snapshot_with(
            Vec::new(),
            vec![
                thread("a.rs", Some(12), "alice", "fix this", true),
                thread("b.rs", Some(3), "bob", "why?", false),
            ],
            Vec::new(),
        );
        let out = call(Some(snapshot.clone()), "gh_threads", json!({})).await;
        assert!(!out.is_error);
        let t = text(&out);
        assert!(t.contains("a.rs:12 alice: fix this [resolved]"), "{t}");
        assert!(t.contains("b.rs:3 bob: why?"), "{t}");

        let out = call(Some(snapshot), "gh_threads", json!({"path": "b.rs"})).await;
        assert!(!out.is_error);
        let t = text(&out);
        assert!(t.contains("b.rs:3 bob: why?"), "{t}");
        assert!(!t.contains("alice"), "{t}");
    }

    #[tokio::test]
    async fn gh_checks_lists_failed_annotations() {
        let snapshot = snapshot_with(
            Vec::new(),
            Vec::new(),
            vec![CheckSummary {
                name: "build".to_owned(),
                conclusion: Some("failure".to_owned()),
                summary: Some("cargo build exited 101".to_owned()),
                text: None,
                annotations: vec![CheckAnnotation {
                    path: "src/a.rs".to_owned(),
                    start_line: 12,
                    end_line: 12,
                    level: "failure".to_owned(),
                    title: None,
                    message: "used unwrap()".to_owned(),
                }],
            }],
        );
        let out = call(Some(snapshot), "gh_checks", json!({})).await;
        assert!(!out.is_error, "{:?}", out.parts);
        let t = text(&out);
        assert!(t.contains("build failure"), "{t}");
        assert!(t.contains("cargo build exited 101"), "{t}");
        assert!(t.contains("src/a.rs:12 failure: used unwrap()"), "{t}");
    }

    fn text(out: &ToolOutput) -> &str {
        assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
        match &out.parts[0] {
            review_llm::ToolResultPart::Text { text } => text,
            other => panic!("not text: {other:?}"),
        }
    }
}
