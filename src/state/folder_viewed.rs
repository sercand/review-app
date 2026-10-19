//! Folder-level viewed marking and tri-state (spec §10.3).
//!
//! A folder's checkbox reflects its **filtered** descendants, computed with
//! hide-viewed *ignored* — otherwise hiding viewed files would make every
//! visible folder look unviewed. Toggling snapshots those descendants **before**
//! mutating, so a shrinking list (from hide-viewed re-filtering) cannot drop the
//! files still to be marked.

use crate::github::files::PrFile;
use crate::state::filters::{matches, without_hide_viewed, FilterState};
use crate::state::viewed::ViewedSet;

/// The three states a folder's viewed checkbox can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriState {
    /// No descendant is viewed.
    None,
    /// Some descendants are viewed.
    Some,
    /// Every descendant is viewed.
    All,
}

/// What a folder toggle did.
#[derive(Debug, PartialEq)]
pub struct ToggleResult {
    /// Whether the snapshot was marked (`true`) or unmarked (`false`).
    pub marked: bool,
    /// The paths the toggle touched.
    pub paths: Vec<String>,
}

/// The files under `folder` that pass every filter **except** hide-viewed.
///
/// `folder` is `""` for the whole PR. The prefix is exact at a `/` boundary, so
/// `"d"` never picks up `"dx/y.rs"`.
pub fn files_under<'a>(
    folder: &str,
    files: &'a [PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
) -> Vec<&'a PrFile> {
    let relaxed = without_hide_viewed(filters);
    files
        .iter()
        .filter(|f| under(folder, &f.path) && matches(f, &relaxed, viewed))
        .collect()
}

/// The tri-state of `folder` over its filtered descendants (hide-viewed ignored).
pub fn folder_state(
    folder: &str,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
) -> TriState {
    let descendants = files_under(folder, files, filters, viewed);
    if descendants.is_empty() {
        return TriState::None;
    }

    let viewed_count = descendants.iter().filter(|f| viewed.is_viewed(f)).count();
    match viewed_count {
        0 => TriState::None,
        count if count == descendants.len() => TriState::All,
        _ => TriState::Some,
    }
}

/// Toggle `folder`'s filtered descendants, snapshotting them first.
///
/// If every file in the snapshot is viewed it unmarks them; otherwise it marks
/// them all (spec §10.3). The returned [`ToggleResult::paths`] are the snapshot's
/// paths, so a caller can tell exactly what changed.
pub fn toggle_folder_viewed(
    folder: &str,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &mut ViewedSet,
) -> ToggleResult {
    // Snapshot before mutating: re-filtering after the mark (e.g. hide-viewed)
    // would otherwise shrink the set mid-toggle.
    let snapshot = files_under(folder, files, filters, viewed);
    let all_viewed = !snapshot.is_empty() && snapshot.iter().all(|f| viewed.is_viewed(f));
    let marked = !all_viewed;

    viewed.set_many(&snapshot, marked);

    ToggleResult {
        marked,
        paths: snapshot.iter().map(|f| f.path.clone()).collect(),
    }
}

/// Whether `path` is directly under `folder` (`""` matches every file).
fn under(folder: &str, path: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::filters::FilterState;
    use crate::state::viewed::ViewedSet;
    use crate::test_support::files_at;
    use std::collections::BTreeSet;

    #[test]
    fn tri_state_ignores_hide_viewed_and_prefix_is_exact() {
        let files = files_at(&["d/a.rs", "d/b.rs", "dx/y.rs"]);
        let mut v = ViewedSet::default();
        let mut f = FilterState::default();
        assert_eq!(folder_state("d", &files, &f, &v), TriState::None);
        v.set(&files[0], true);
        assert_eq!(folder_state("d", &files, &f, &v), TriState::Some);
        f.hide_viewed = true; // must not hide the viewed half
        assert_eq!(folder_state("d", &files, &f, &v), TriState::Some);
        v.set(&files[1], true);
        assert_eq!(folder_state("d", &files, &f, &v), TriState::All);
        assert_eq!(folder_state("dx", &files, &f, &v), TriState::None); // "d" did not match "dx/"
    }

    #[test]
    fn toggle_marks_filtered_snapshot_then_unmarks() {
        let files = files_at(&["d/a.rs", "d/b.rs", "d/c.md", "e/x.rs"]);
        let mut v = ViewedSet::default();
        let f = FilterState {
            extensions: BTreeSet::from(["rs".to_string()]),
            hide_viewed: true,
            ..Default::default()
        };

        let r = toggle_folder_viewed("d", &files, &f, &mut v);
        assert_eq!(
            r,
            ToggleResult {
                marked: true,
                paths: vec!["d/a.rs".into(), "d/b.rs".into()]
            }
        );
        assert!(!v.is_viewed(&files[2]) && !v.is_viewed(&files[3]));

        let r = toggle_folder_viewed("d", &files, &f, &mut v);
        assert!(!r.marked);
        assert_eq!(folder_state("d", &files, &f, &v), TriState::None);
    }
}
