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
        url: format!("https://github.com/o/r/pull/{number}"),
    }
}
