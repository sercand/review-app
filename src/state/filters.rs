//! The file-list filters (spec §10.4).
//!
//! Filters form a conjunction: a file is shown only if it passes every one of
//! them, in any order. An empty multi-select means "no constraint", so the
//! default [`FilterState`] keeps everything.

use std::collections::BTreeSet;

use crate::github::files::{FileStatus, PrFile};
use crate::state::viewed::ViewedSet;

/// What the left pane is filtering the file list by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterState {
    /// Selected extensions (the text after the last `.`). Empty = no constraint.
    pub extensions: BTreeSet<String>,
    /// Selected status groups. Empty = no constraint.
    pub statuses: BTreeSet<FileStatus>,
    /// A case-insensitive path substring; `*` matches any run of characters.
    pub path_query: String,
    /// Whether files already viewed should be hidden.
    pub hide_viewed: bool,
}

/// The extension of `path`: the text after the last `.` of the file **name**.
///
/// A name with no `.` has no extension, and a dot in a directory does not count
/// (`"a.d/b"` → `""`).
pub fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, extension)) => extension.to_owned(),
        None => String::new(),
    }
}

/// The status group a file's status filters (and is shown) under.
///
/// `Changed`, `Copied`, and `Unchanged` all group under `Modified` (spec §10.4).
pub fn status_group(status: FileStatus) -> FileStatus {
    match status {
        FileStatus::Changed | FileStatus::Copied | FileStatus::Unchanged => FileStatus::Modified,
        other => other,
    }
}

/// Whether `f` passes every filter in `filters`.
pub fn matches(f: &PrFile, filters: &FilterState, viewed: &ViewedSet) -> bool {
    if !filters.extensions.is_empty() && !filters.extensions.contains(&extension(&f.path)) {
        return false;
    }
    if !filters.statuses.is_empty() && !filters.statuses.contains(&status_group(f.status)) {
        return false;
    }
    if !filters.path_query.is_empty() && !path_matches(&f.path, &filters.path_query) {
        return false;
    }
    if filters.hide_viewed && viewed.is_viewed(f) {
        return false;
    }
    true
}

/// `filters` with hide-viewed cleared, so folder tri-state can ignore it.
pub fn without_hide_viewed(filters: &FilterState) -> FilterState {
    FilterState {
        hide_viewed: false,
        ..filters.clone()
    }
}

/// The files `filters` keeps, in order.
pub fn apply<'a>(
    files: &'a [PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
) -> Vec<&'a PrFile> {
    files
        .iter()
        .filter(|f| matches(f, filters, viewed))
        .collect()
}

/// Every extension present in `files`, sorted and deduplicated.
///
/// Includes `""` for files with no extension.
pub fn available_extensions(files: &[PrFile]) -> Vec<String> {
    files
        .iter()
        .map(|f| extension(&f.path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Whether `path` matches a case-insensitive substring query with `*` wildcards.
///
/// `*` matches any run of characters, including none: the query is split on `*`
/// and each non-empty piece must appear in order.
fn path_matches(path: &str, query: &str) -> bool {
    let haystack = path.to_lowercase();
    let mut rest = haystack.as_str();
    for piece in query.to_lowercase().split('*') {
        if piece.is_empty() {
            continue;
        }
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::files::FileStatus;
    use crate::state::viewed::ViewedSet;
    use crate::test_support::files_at;

    #[test]
    fn filters_form_a_conjunction() {
        let files = files_at(&["src/a.rs", "src/b.ts", "docs/c.md", "Makefile"]);
        let none = ViewedSet::default();
        let mut f = FilterState::default();
        assert_eq!(apply(&files, &f, &none).len(), 4); // empty filters keep everything

        f.extensions.insert("rs".into());
        f.statuses.insert(FileStatus::Modified);
        assert_eq!(names(apply(&files, &f, &none)), vec!["src/a.rs"]);

        f.path_query = "SRC/*".into();
        assert_eq!(names(apply(&files, &f, &none)), vec!["src/a.rs"]);
        f.path_query = "docs".into();
        assert!(apply(&files, &f, &none).is_empty());

        let mut v = ViewedSet::default();
        v.set(&files[0], true);
        let f = FilterState {
            hide_viewed: true,
            ..Default::default()
        };
        assert_eq!(apply(&files, &f, &v).len(), 3);
        assert_eq!(available_extensions(&files), vec!["", "md", "rs", "ts"]);
    }

    fn names(fs: Vec<&PrFile>) -> Vec<&str> {
        fs.into_iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn extensions_come_from_the_file_name_and_statuses_group() {
        assert_eq!(extension("Makefile"), "");
        assert_eq!(extension("a.d/b"), "");
        assert_eq!(extension("src/a.rs"), "rs");
        assert_eq!(status_group(FileStatus::Changed), FileStatus::Modified);
        assert_eq!(status_group(FileStatus::Copied), FileStatus::Modified);
        assert_eq!(status_group(FileStatus::Unchanged), FileStatus::Modified);
        assert_eq!(status_group(FileStatus::Added), FileStatus::Added);
    }
}
