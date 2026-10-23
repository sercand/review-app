//! Test-only helpers shared by the unit tests in this crate.
//!
//! Later phases add more helpers here. When they do, read this file first and
//! add only the helpers that are missing — never redefine an existing one.

/// Load a JSON fixture from `tests/fixtures/` as a `&'static str`.
///
/// The file is embedded at compile time, so a missing fixture is a build error
/// rather than a panic in the middle of a test.
///
/// Because this is a `#[macro_export]` macro, call sites must bring it into
/// scope explicitly:
///
/// ```ignore
/// use crate::fixture;
///
/// let body = fixture!("pr_files_page.json");
/// ```
#[macro_export]
macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/",
            $name
        ))
    };
}

/// Create a fresh temporary directory for a test.
///
/// The returned [`tempfile::TempDir`] removes the directory when it is dropped,
/// so bind it to a variable that lives for as long as the test needs it.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("failed to create a temporary directory")
}

/// A [`crate::ai::review::RunState`] over a fresh in-memory store with one
/// session and one full run, for `ToolCx` test contexts. The event receiver
/// is dropped at once, so the state's sends fail and are ignored.
#[cfg(unix)]
pub fn ai_run_state() -> std::sync::Arc<crate::ai::review::RunState> {
    use crate::ai::review::RunState;
    use crate::ai::store::{PrKey, RunKind, Store};
    use std::sync::Arc;

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
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    Arc::new(RunState::new(store, session, run, tx))
}

/// A repository reference in the `o/r` every unit test uses.
pub fn repo() -> crate::model::RepoRef {
    crate::model::RepoRef::new("o", "r")
}

/// A [`crate::github::files::PrFile`] at `path`: modified, with a two-line
/// patch, a blob SHA derived from the path, and two changed lines.
pub fn file(path: &str) -> crate::github::files::PrFile {
    crate::github::files::PrFile {
        path: path.to_owned(),
        previous_path: None,
        status: crate::github::files::FileStatus::Modified,
        additions: 1,
        deletions: 1,
        changes: 2,
        patch: Some("@@ -1 +1 @@\n-a\n+b\n".to_owned()),
        patch_state: crate::github::files::PatchState::Present,
        blob_sha: format!("b-{path}"),
    }
}

/// One [`crate::github::files::PrFile`] per path, from [`file`].
pub fn files_at(paths: &[&str]) -> Vec<crate::github::files::PrFile> {
    paths.iter().map(|path| file(path)).collect()
}

/// A whole path tree over `paths`, with every file included.
pub fn tree_at(paths: &[&str]) -> crate::model::tree::TreeNode {
    crate::model::tree::build_tree(&files_at(paths), &|_| true)
}

/// The node at `path` anywhere in `root`'s subtree, or `None`.
pub fn find<'a>(
    root: &'a crate::model::tree::TreeNode,
    path: &str,
) -> Option<&'a crate::model::tree::TreeNode> {
    if root.path == path {
        return Some(root);
    }
    root.children.iter().find_map(|child| find(child, path))
}

/// A [`crate::model::pr::PrSummary`] in `o/r`: authored by `"a"`, not a draft,
/// with no checks and no review decision, all timestamps equal, and no URL.
pub fn pr_summary(number: u64, title: &str, comments: u32) -> crate::model::pr::PrSummary {
    crate::model::pr::PrSummary {
        repo: repo(),
        number,
        title: title.to_owned(),
        author: "a".to_owned(),
        draft: false,
        updated_at: "2026-10-01T00:00:00Z".to_owned(),
        created_at: "2026-09-01T00:00:00Z".to_owned(),
        additions: 0,
        deletions: 0,
        comments,
        ci: None,
        review: None,
        scopes: Vec::new(),
        url: format!("https://github.com/o/r/pull/{number}"),
    }
}

/// Every row of `patch`, flattened across its hunks.
pub fn rows_from(patch: &str) -> Vec<crate::model::diff::DiffRow> {
    parse(patch)
        .into_iter()
        .flat_map(|hunk| hunk.rows)
        .collect()
}

/// A [`crate::github::files::LoadedFile`] at `path` carrying `patch`, with the
/// patch already parsed into its diff.
pub fn loaded(path: &str, patch: &str) -> crate::github::files::LoadedFile {
    crate::github::files::LoadedFile {
        file: crate::github::files::PrFile {
            patch: Some(patch.to_owned()),
            ..file(path)
        },
        diff: Some(crate::model::diff::FileDiff {
            hunks: parse(patch),
        }),
    }
}

/// Parse a test patch, panicking on a malformed one.
fn parse(patch: &str) -> Vec<crate::model::diff::Hunk> {
    crate::diff::parse::parse_patch(patch).expect("the test patch should parse")
}

/// An [`crate::ai::review::Finding`] in `state`, anchored to `anchor`, from
/// the run's first reviewer.
#[cfg(unix)]
pub fn ai_finding(
    id: i64,
    anchor: crate::ai::review::FindingAnchor,
    severity: crate::ai::review::Severity,
    state: crate::ai::review::FindingState,
    title: &str,
    body: &str,
) -> crate::ai::review::Finding {
    crate::ai::review::Finding {
        id,
        run_id: 1,
        agent: crate::ai::tools::AgentRef {
            id: 1,
            role: crate::ai::tools::AgentRole::Reviewer,
            label: "reviewer:g1".to_owned(),
        },
        anchor,
        severity,
        title: title.to_owned(),
        body: body.to_owned(),
        evidence: Vec::new(),
        checklist_item: None,
        state,
        state_reason: None,
        pending_local_id: None,
    }
}

/// A single-line [`crate::model::comment::Anchor`] on the RIGHT side of `path`
/// at `line`, with no range.
pub fn line_anchor(path: &str, line: u32) -> crate::model::comment::Anchor {
    crate::model::comment::Anchor {
        path: path.to_owned(),
        side: crate::model::diff::Side::Right,
        line,
        start_line: None,
        start_side: None,
    }
}

/// A range [`crate::model::comment::Anchor`] on the RIGHT side of `path`,
/// from `start` to `end`.
pub fn range_anchor(path: &str, start: u32, end: u32) -> crate::model::comment::Anchor {
    crate::model::comment::Anchor {
        path: path.to_owned(),
        side: crate::model::diff::Side::Right,
        line: end,
        start_line: Some(start),
        start_side: Some(crate::model::diff::Side::Right),
    }
}

/// A [`crate::github::metadata::PrMetadata`] authored by `"author"` with the
/// given head SHA, in `o/r` as PR #7.
pub fn meta(head_sha: &str) -> crate::github::metadata::PrMetadata {
    meta_by("author", head_sha)
}

/// A [`crate::github::metadata::PrMetadata`] authored by `author` with the
/// given head SHA, in `o/r` as PR #7.
pub fn meta_by(author: &str, head_sha: &str) -> crate::github::metadata::PrMetadata {
    crate::github::metadata::PrMetadata {
        node_id: "node".to_owned(),
        number: 7,
        title: "a pull request".to_owned(),
        body: String::new(),
        author: author.to_owned(),
        state: "open".to_owned(),
        draft: false,
        base_ref: "main".to_owned(),
        base_sha: "sha_base".to_owned(),
        head_ref: "feature".to_owned(),
        head_sha: head_sha.to_owned(),
        additions: 1,
        deletions: 1,
        changed_files: 1,
        requested_reviewers: Vec::new(),
        labels: Vec::new(),
        milestone: None,
        html_url: "https://github.com/o/r/pull/7".to_owned(),
        mergeable: Some(true),
        mergeable_state: "clean".to_owned(),
        merge_state_status: "clean".to_owned(),
        merged: false,
    }
}

/// A [`crate::github::reviews::Review`] with id 1, author `"me"`, the given
/// state, an empty body, and no submit time.
pub fn review(state: crate::github::reviews::ReviewState) -> crate::github::reviews::Review {
    crate::github::reviews::Review {
        id: 1,
        node_id: "PRR_1".to_owned(),
        author: "me".to_owned(),
        state,
        body: String::new(),
        submitted_at: None,
    }
}
