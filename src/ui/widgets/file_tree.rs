//! The file tree: a flattened view of the path tree with per-file and per-folder
//! viewed checkboxes, the progress counter, and "mark all viewed"
//! (spec §9.1, §9.2, §10.3).
//!
//! [`flatten_visible`] turns the [`TreeNode`] tree into the flat rows the widget
//! draws, honoring the collapsed set. Everything the rows carry is a value, so
//! the widget cannot borrow the session, and the test needs no window.

use eframe::egui;

use crate::github::files::{FileStatus, PrFile};
use crate::model::tree::TreeNode;
use crate::state::filters::{status_group, FilterState};
use crate::state::folder_viewed::{folder_state, TriState};
use crate::state::viewed::ViewedSet;
use crate::ui::widgets::filter_bar::status_label;

/// One flattened tree row, ready to draw.
///
/// For a file, `tri` is its binary viewed state expressed as [`TriState::All`]
/// (viewed) or [`TriState::None`] (not viewed); for a directory it is the
/// folder's tri-state.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRowVm {
    /// How deeply nested the row is; the top level is `0`.
    pub indent: u32,
    /// The label to draw (a directory chain keeps its slashes, `"a/b/c"`).
    pub label: String,
    /// The full path, which actions carry.
    pub path: String,
    /// Whether the row is a directory.
    pub is_dir: bool,
    /// The viewed tri-state.
    pub tri: TriState,
    /// A file's status group, or `None` for a directory.
    pub status: Option<FileStatus>,
    /// Lines added (rolled up for a directory).
    pub additions: u32,
    /// Lines deleted (rolled up for a directory).
    pub deletions: u32,
    /// Whether this is the file the diff is scrolled to.
    pub current: bool,
}

/// What the user asked of the file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    /// Collapse or expand a directory, by path.
    ToggleCollapsed(String),
    /// Toggle one file's viewed state, by path.
    ToggleViewed(String),
    /// Toggle a directory's viewed state, by path.
    ToggleFolderViewed(String),
    /// Scroll the diff to a file, by path.
    SelectFile(String),
    /// Mark every filtered file viewed.
    MarkAllViewed,
}

/// Flatten `tree` depth-first into the rows to draw.
///
/// The root is not rendered; top-level entries start at indent `0`. A directory
/// is expanded unless it is in `collapsed`, whose children are then skipped.
pub fn flatten_visible(
    tree: &TreeNode,
    collapsed: &std::collections::BTreeSet<String>,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
    current: Option<&str>,
) -> Vec<TreeRowVm> {
    let mut rows = Vec::new();
    push_children(
        tree, 0, collapsed, files, filters, viewed, current, &mut rows,
    );
    rows
}

/// Append `node`'s children at `indent`, recursing into expanded directories.
#[allow(clippy::too_many_arguments)]
fn push_children(
    node: &TreeNode,
    indent: u32,
    collapsed: &std::collections::BTreeSet<String>,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
    current: Option<&str>,
    rows: &mut Vec<TreeRowVm>,
) {
    for child in &node.children {
        rows.push(row_for(child, indent, files, filters, viewed, current));
        if child.is_dir && !collapsed.contains(&child.path) {
            push_children(
                child,
                indent + 1,
                collapsed,
                files,
                filters,
                viewed,
                current,
                rows,
            );
        }
    }
}

/// Build one row from a tree node.
fn row_for(
    node: &TreeNode,
    indent: u32,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
    current: Option<&str>,
) -> TreeRowVm {
    if node.is_dir {
        return TreeRowVm {
            indent,
            label: node.name.clone(),
            path: node.path.clone(),
            is_dir: true,
            tri: folder_state(&node.path, files, filters, viewed),
            status: None,
            additions: node.additions,
            deletions: node.deletions,
            current: false,
        };
    }

    let file = node.file_index.and_then(|index| files.get(index));
    TreeRowVm {
        indent,
        label: node.name.clone(),
        path: node.path.clone(),
        is_dir: false,
        tri: match file {
            Some(f) if viewed.is_viewed(f) => TriState::All,
            _ => TriState::None,
        },
        status: file.map(|f| status_group(f.status)),
        additions: node.additions,
        deletions: node.deletions,
        current: current == Some(node.path.as_str()),
    }
}

/// Draw the hide-viewed toggle, the progress counter, "mark all viewed", and the
/// tree rows.
///
/// `progress` is `(viewed_filtered, total_filtered, viewed_all, total_all)`.
pub fn render(
    ui: &mut egui::Ui,
    rows: &[TreeRowVm],
    filters: &mut FilterState,
    progress: (usize, usize, usize, usize),
    actions: &mut Vec<TreeAction>,
) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut filters.hide_viewed, "Hide viewed");
        if ui.button("Mark all viewed").clicked() {
            actions.push(TreeAction::MarkAllViewed);
        }
    });

    let (viewed_filtered, total_filtered, viewed_all, total_all) = progress;
    ui.weak(format!(
        "{viewed_filtered}/{total_filtered} viewed · {viewed_all}/{total_all} total"
    ));
    ui.separator();

    egui::ScrollArea::vertical()
        .id_salt("file-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in rows {
                render_row(ui, row, actions);
            }
        });
}

/// Draw one tree row: its checkbox, label, and counts.
fn render_row(ui: &mut egui::Ui, row: &TreeRowVm, actions: &mut Vec<TreeAction>) {
    ui.horizontal(|ui| {
        ui.add_space(row.indent as f32 * 12.0);

        let mut checked = row.tri == TriState::All;
        let checkbox = if row.is_dir {
            ui.add(
                egui::Checkbox::without_text(&mut checked).indeterminate(row.tri == TriState::Some),
            )
        } else {
            ui.checkbox(&mut checked, "")
        };
        if checkbox.clicked() {
            let action = if row.is_dir {
                TreeAction::ToggleFolderViewed(row.path.clone())
            } else {
                TreeAction::ToggleViewed(row.path.clone())
            };
            actions.push(action);
        }

        if row.is_dir {
            if ui.selectable_label(false, &row.label).clicked() {
                actions.push(TreeAction::ToggleCollapsed(row.path.clone()));
            }
        } else if ui.selectable_label(row.current, &row.label).clicked() {
            actions.push(TreeAction::SelectFile(row.path.clone()));
        }

        ui.add_space(4.0);
        ui.weak(format!("+{} −{}", row.additions, row.deletions));
        if let Some(status) = row.status {
            ui.weak(status_label(status));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tree::build_tree;
    use crate::state::filters::{available_extensions, FilterState};
    use crate::state::viewed::ViewedSet;
    use crate::test_support::files_at;
    use crate::ui::widgets::filter_bar;
    use std::collections::BTreeSet;

    #[test]
    fn flatten_honors_collapse_and_indent() {
        let files = files_at(&["src/a.rs", "src/state/b.rs"]);
        let root = build_tree(&files, &|_| true);
        let f = FilterState::default();
        let v = ViewedSet::default();
        let rows = flatten_visible(&root, &BTreeSet::new(), &files, &f, &v, Some("src/a.rs"));
        let shape: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.indent)).collect();
        assert_eq!(
            shape,
            vec![("src", 0), ("state", 1), ("b.rs", 2), ("a.rs", 1)]
        );
        assert!(rows[3].current && !rows[3].is_dir);

        let collapsed = BTreeSet::from(["src".to_string()]);
        assert_eq!(
            flatten_visible(&root, &collapsed, &files, &f, &v, None).len(),
            1
        );
    }

    #[test]
    fn render_paints_the_filter_bar_and_rows_without_actions() {
        let files = files_at(&["src/a.rs", "src/dir/b.rs"]);
        let root = build_tree(&files, &|_| true);
        let mut filters = FilterState::default();
        let viewed = ViewedSet::default();
        let rows = flatten_visible(&root, &BTreeSet::new(), &files, &filters, &viewed, None);
        assert!(!rows.is_empty());

        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            filter_bar::render(ui, &mut filters, &available_extensions(&files));
            render(
                ui,
                &rows,
                &mut filters,
                (0, rows.len(), 0, files.len()),
                &mut actions,
            );
        });

        assert!(actions.is_empty(), "an unclicked tree emits no actions");
        assert!(!filters.hide_viewed, "an unclicked toggle stays off");
    }
}
