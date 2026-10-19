//! The file-list filters (spec §10.4).
//!
//! Filters form a conjunction: a file is shown only if it passes every one of
//! them, in any order. An empty multi-select means "no constraint", so the
//! default [`FilterState`] keeps everything.

use std::collections::BTreeSet;

use crate::github::files::{FileStatus, PrFile};
use crate::model::tree::{build_tree, TreeNode};
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

/// The indices of the visible files in **display order**: the same
/// folders-first, alphabetical order the file tree shows, so `j`/`k`, the
/// initial selection, and "next unviewed" all follow what the reviewer sees.
pub fn file_order(files: &[PrFile], filters: &FilterState, viewed: &ViewedSet) -> Vec<usize> {
    let root = build_tree(files, &|f| matches(f, filters, viewed));
    let mut out = Vec::new();
    collect_file_indices(&root, &mut out);
    out
}

/// Depth-first collect of `node`'s file indices, in the tree's display order.
fn collect_file_indices(node: &TreeNode, out: &mut Vec<usize>) {
    for child in &node.children {
        match child.file_index {
            Some(index) => out.push(index),
            None => collect_file_indices(child, out),
        }
    }
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
    fn file_order_follows_the_tree_and_the_filters() {
        let files = files_at(&["src/b.rs", "src/a.rs", "README.md"]);
        let none = ViewedSet::default();
        // Folders precede root files; inside `src`, `a.rs` precedes `b.rs`.
        assert_eq!(
            file_order(&files, &FilterState::default(), &none),
            vec![1, 0, 2]
        );
        let f = FilterState {
            path_query: "src".into(),
            ..FilterState::default()
        };
        assert_eq!(file_order(&files, &f, &none), vec![1, 0]);
    }

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
