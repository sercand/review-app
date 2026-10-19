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
