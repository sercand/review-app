//! The file tree: mockup 02's left pane. Flattened path-tree rows with
//! icons, per-file comment/pending counts, churn and status letters, plus the
//! progress bar and tools row under the filter combobox
//! (spec §9.1, §9.2, §10.3; Task 11).
//!
//! [`flatten_visible`] turns the [`TreeNode`] tree into the flat rows the widget
//! draws, honoring the collapsed set and the order. Everything the rows carry
//! is a value, so the widget cannot borrow the session, and the test needs no
//! window.

use std::collections::HashMap;

use eframe::egui::{self, Align, Color32, Layout, Rect, RichText, ScrollArea, Sense, Ui, Vec2};

use crate::github::files::{FileStatus, PrFile};
use crate::model::tree::TreeNode;
use crate::state::filters::{FilterState, TreeOrder, ordered_children, status_group};
use crate::state::folder_viewed::{TriState, folder_state};
use crate::state::viewed::ViewedSet;
use crate::ui::{file_icons, fonts, icons, theme};

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
    /// Unresolved review threads on the file (Task 11).
    pub comments: u32,
    /// Local pending comments on the file (Task 11).
    pub pending: u32,
    /// Whether a directory is expanded (drives the chevron; Task 11).
    pub expanded: bool,
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
    /// Expand every directory, or — when any is collapsed — collapse them all.
    CollapseAll,
    /// Flip between path order and largest-first (Task 11).
    ToggleOrder,
}

/// The one-letter status marker on a tree row (Task 11).
pub fn status_letter(status: FileStatus) -> char {
    match status {
        FileStatus::Added => 'A',
        FileStatus::Removed => 'D',
        FileStatus::Renamed => 'R',
        _ => 'M',
    }
}

/// The progress line: `"5 of 26 viewed"`, or the split wording while a
/// filter hides files (Task 11).
pub fn progress_label(
    viewed_filtered: usize,
    total_filtered: usize,
    viewed: usize,
    total: usize,
    filtered: bool,
) -> String {
    if filtered {
        format!("{viewed_filtered} of {total_filtered} shown viewed, {viewed} of {total} total")
    } else {
        format!("{viewed} of {total} viewed")
    }
}

/// Flatten `tree` depth-first into the rows to draw.
///
/// The root is not rendered; top-level entries start at indent `0`. A directory
/// is expanded unless it is in `collapsed`, whose children are then skipped.
/// `counts` carries each file's `(threads, pending)`; `order` sorts children
/// within every directory.
#[allow(clippy::too_many_arguments)]
pub fn flatten_visible(
    tree: &TreeNode,
    collapsed: &std::collections::BTreeSet<String>,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
    current: Option<&str>,
    counts: &HashMap<String, (u32, u32)>,
    order: TreeOrder,
) -> Vec<TreeRowVm> {
    let mut rows = Vec::new();
    push_children(
        tree, 0, collapsed, files, filters, viewed, current, counts, order, &mut rows,
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
    counts: &HashMap<String, (u32, u32)>,
    order: TreeOrder,
    rows: &mut Vec<TreeRowVm>,
) {
    for child in ordered_children(node, order) {
        rows.push(row_for(
            child, indent, collapsed, files, filters, viewed, current, counts,
        ));
        if child.is_dir && !collapsed.contains(&child.path) {
            push_children(
                child,
                indent + 1,
                collapsed,
                files,
                filters,
                viewed,
                current,
                counts,
                order,
                rows,
            );
        }
    }
}

/// Build one row from a tree node.
#[allow(clippy::too_many_arguments)]
fn row_for(
    node: &TreeNode,
    indent: u32,
    collapsed: &std::collections::BTreeSet<String>,
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
    current: Option<&str>,
    counts: &HashMap<String, (u32, u32)>,
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
            comments: 0,
            pending: 0,
            expanded: !collapsed.contains(&node.path),
        };
    }

    let file = node.file_index.and_then(|index| files.get(index));
    let (comments, pending) = counts.get(&node.path).copied().unwrap_or((0, 0));
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
        comments,
        pending,
        expanded: false,
    }
}

/// The row height and the indent per level (mockup 02).
const ROW_H: f32 = 26.0;
const INDENT_W: f32 = 14.0;

/// Draw the progress bar, the tools row, and the tree rows.
///
/// `progress` is `(viewed_filtered, total_filtered, viewed_all, total_all)`.
pub fn render(
    ui: &mut Ui,
    rows: &[TreeRowVm],
    filters: &mut FilterState,
    order: TreeOrder,
    progress: (usize, usize, usize, usize),
    invalidated: usize,
    actions: &mut Vec<TreeAction>,
) {
    let p = theme::palette_of(ui);
    let (viewed_filtered, total_filtered, viewed_all, total_all) = progress;
    let filtered = total_filtered != total_all;

    ui.label(
        RichText::new(progress_label(
            viewed_filtered,
            total_filtered,
            viewed_all,
            total_all,
            filtered,
        ))
        .color(p.muted),
    );
    let bar = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(bar.0, 2.0, p.line);
    if total_all > 0 {
        let frac = viewed_all as f32 / total_all as f32;
        ui.painter().rect_filled(
            Rect::from_min_size(bar.0.min, Vec2::new(bar.0.width() * frac, bar.0.height())),
            2.0,
            p.accent,
        );
    }

    ui.horizontal(|ui| {
        let files = rows.iter().filter(|row| !row.is_dir);
        let count = files.clone().count();
        let additions: u32 = files.clone().map(|row| row.additions).sum();
        let deletions: u32 = files.map(|row| row.deletions).sum();
        ui.label(
            RichText::new(format!("{count} files +{additions} −{deletions}"))
                .color(p.faint)
                .font(egui::FontId::monospace(11.0)),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icons::icon_button(ui, icons::CHECK_CHECK, "Mark all viewed").clicked() {
                actions.push(TreeAction::MarkAllViewed);
            }
            if icons::icon_button(ui, icons::CHEVRONS_DOWN_UP, "Collapse folders").clicked() {
                actions.push(TreeAction::CollapseAll);
            }
            let mut largest = order == TreeOrder::LargestFirst;
            if icons::icon_toggle(
                ui,
                icons::ARROW_DOWN_WIDE_NARROW,
                "Largest first",
                &mut largest,
            )
            .clicked()
            {
                actions.push(TreeAction::ToggleOrder);
            }
            icons::icon_toggle(ui, icons::EYE_OFF, "Hide viewed", &mut filters.hide_viewed);
        });
    });
    if invalidated > 0 {
        ui.label(
            RichText::new(format!("{invalidated} files you had viewed changed since"))
                .color(p.pending),
        );
    }
    ui.separator();

    ScrollArea::vertical()
        .id_salt("file-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in rows {
                render_row(ui, row, actions);
            }
        });
}

/// Draw one 26 pt tree row: chevron/icon/name on the left; comment counts,
/// churn, status letter and the hover-revealed viewed box on the right.
fn render_row(ui: &mut Ui, row: &TreeRowVm, actions: &mut Vec<TreeAction>) {
    let p = theme::palette_of(ui);
    let full = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(full, ROW_H), Sense::click());
    // Whether the pointer is over the row at all. Not `response.hovered()`:
    // once the checkbox sits under the pointer egui gives *it* the hover, the
    // row would report "not hovered", the checkbox would vanish next frame,
    // and the click would land on the row instead.
    let hovered = ui.rect_contains_pointer(rect);

    let fill = if row.current {
        p.accent_soft
    } else if hovered {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    if row.current {
        ui.painter().rect_filled(
            Rect::from_min_size(rect.left_top(), Vec2::new(2.0, rect.height())),
            0.0,
            p.accent,
        );
    }
    let cy = rect.center().y;

    // The right cluster paints right to left first, so the name gets
    // exactly the width that is left and ends in "…" instead of running
    // under the counts.
    let mut right = rect.right() - 8.0;
    let mut check_clicked = false;
    let check = Rect::from_center_size(egui::pos2(right - 8.0, cy), Vec2::splat(16.0));
    right -= 16.0 + 6.0;
    if hovered || row.tri != TriState::None {
        let id = ui.make_persistent_id(("tree-viewed", row.path.as_str()));
        let check_response = ui.interact(check, id, Sense::click());
        paint_check(ui, check, row.tri, check_response.hovered());
        let tip = match (row.is_dir, row.tri) {
            (true, TriState::All) => "Unmark folder viewed",
            (true, _) => "Mark folder viewed",
            (false, TriState::All) => "Mark not viewed",
            (false, _) => "Mark viewed",
        };
        if check_response.on_hover_text(tip).clicked() {
            check_clicked = true;
            actions.push(if row.is_dir {
                TreeAction::ToggleFolderViewed(row.path.clone())
            } else {
                TreeAction::ToggleViewed(row.path.clone())
            });
        }
    }
    let mono = egui::FontId::monospace(11.0);
    let mut paint_right = |text: String, font: egui::FontId, color: Color32, gap: f32| {
        let galley = ui.painter().layout_no_wrap(text, font, color);
        let pos = egui::pos2(right - galley.size().x, cy - galley.size().y / 2.0);
        right -= galley.size().x + gap;
        ui.painter().galley(pos, galley, color);
    };
    if let Some(status) = row.status {
        let color = match status_group(status) {
            FileStatus::Added => p.ok,
            FileStatus::Removed => p.bad,
            FileStatus::Renamed => p.info,
            _ => p.warn,
        };
        paint_right(
            status_letter(status).to_string(),
            fonts::mono_bold(10.5),
            color,
            8.0,
        );
    }
    if row.additions + row.deletions > 0 {
        paint_right(format!("−{}", row.deletions), mono.clone(), p.bad, 4.0);
        paint_right(format!("+{}", row.additions), mono.clone(), p.ok, 8.0);
    }
    if row.pending > 0 {
        paint_right(
            format!("{} {}", icons::MESSAGE_SQUARE_DASHED, row.pending),
            egui::FontId::proportional(11.0),
            p.pending,
            6.0,
        );
    }
    if row.comments > 0 {
        paint_right(
            format!("{} {}", icons::MESSAGE_SQUARE, row.comments),
            egui::FontId::proportional(11.0),
            p.muted,
            6.0,
        );
    }

    // Left: indent, chevron, icon, name.
    let mut x = rect.left() + 8.0 + row.indent as f32 * INDENT_W;
    if row.is_dir {
        let chevron = if row.expanded {
            icons::CHEVRON_DOWN
        } else {
            icons::CHEVRON_RIGHT
        };
        ui.painter().text(
            egui::pos2(x + 7.0, cy),
            egui::Align2::CENTER_CENTER,
            chevron,
            egui::FontId::proportional(14.0),
            p.faint,
        );
    }
    x += 16.0;
    let icon = if row.is_dir {
        file_icons::folder_icon_name(&row.label, row.expanded)
    } else {
        file_icons::file_icon_name(&row.path)
    };
    if let Some(source) = file_icons::icon_source(icon) {
        egui::Image::new(source).paint_at(
            ui,
            Rect::from_center_size(egui::pos2(x + 8.0, cy), Vec2::splat(16.0)),
        );
    }
    x += 22.0;
    let name_color = if row.is_dir {
        p.muted
    } else if row.tri == TriState::All {
        p.faint
    } else {
        p.text
    };
    let name = crate::ui::components::elided_galley(
        ui,
        &row.label,
        egui::FontId::proportional(13.0),
        name_color,
        right - x - 4.0,
    );
    let elided = name.elided;
    ui.painter()
        .galley(egui::pos2(x, cy - name.size().y / 2.0), name, name_color);

    let response = if elided || row.is_dir {
        response.on_hover_text(&row.path)
    } else {
        response
    };
    if response.clicked() && !check_clicked {
        actions.push(if row.is_dir {
            TreeAction::ToggleCollapsed(row.path.clone())
        } else {
            TreeAction::SelectFile(row.path.clone())
        });
    }
}

/// The viewed checkbox (mockup `.check`): a filled `accent` box with a check
/// when every file is viewed, an `accent` outline with a dash for some, a
/// `faint` outline for none.
fn paint_check(ui: &Ui, rect: Rect, tri: TriState, hovered: bool) {
    let p = theme::palette_of(ui);
    match tri {
        TriState::All => {
            ui.painter().rect_filled(rect, 4.0, p.accent);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                icons::CHECK,
                egui::FontId::proportional(12.0),
                p.accent_ink,
            );
        }
        TriState::Some => {
            ui.painter().rect_stroke(
                rect.shrink(0.75),
                4.0,
                egui::Stroke::new(1.5, p.accent),
                egui::StrokeKind::Inside,
            );
            ui.painter().rect_filled(
                Rect::from_center_size(rect.center(), Vec2::new(8.0, 2.0)),
                1.0,
                p.accent,
            );
        }
        TriState::None => {
            let color = if hovered { p.muted } else { p.faint };
            ui.painter().rect_stroke(
                rect.shrink(0.75),
                4.0,
                egui::Stroke::new(1.5, color),
                egui::StrokeKind::Inside,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tree::build_tree;
    use crate::state::filters::{FilterState, TreeOrder};
    use crate::state::viewed::ViewedSet;
    use crate::test_support::files_at;
    use crate::ui::widgets::filter_bar;
    use std::collections::{BTreeSet, HashMap};

    /// Click the viewed checkbox of one file row with the real pointer
    /// sequence (move, press, release) and return what the row reported.
    fn click_viewed_box(row: TreeRowVm) -> Vec<TreeAction> {
        let state = (Vec::new(), egui::Rect::NOTHING);
        let mut harness = egui_kittest::Harness::builder()
            .with_context_setup(|ctx| {
                ctx.set_fonts(crate::ui::fonts::font_definitions());
                egui_extras::install_image_loaders(ctx);
            })
            .build_ui_state(
                move |ui, state: &mut (Vec<TreeAction>, egui::Rect)| {
                    state.1 = ui.available_rect_before_wrap();
                    render_row(ui, &row, &mut state.0);
                },
                state,
            );
        let area = harness.state().1;
        // The box's centre: 16 pt in from the row's right edge, mid-row.
        let pos = egui::pos2(area.right() - 16.0, area.top() + ROW_H / 2.0);
        harness.hover_at(pos);
        harness.run();
        harness.drag_at(pos);
        harness.run();
        harness.drop_at(pos);
        harness.run();
        std::mem::take(&mut harness.state_mut().0)
    }

    #[test]
    fn the_viewed_box_takes_the_click_from_its_row() {
        let row = TreeRowVm {
            indent: 0,
            label: "a.rs".into(),
            path: "src/a.rs".into(),
            is_dir: false,
            tri: TriState::None,
            status: Some(FileStatus::Modified),
            additions: 1,
            deletions: 1,
            current: false,
            comments: 0,
            pending: 0,
            expanded: false,
        };
        assert_eq!(
            click_viewed_box(row.clone()),
            vec![TreeAction::ToggleViewed("src/a.rs".into())]
        );
        let folder = TreeRowVm {
            is_dir: true,
            status: None,
            path: "src".into(),
            label: "src".into(),
            expanded: true,
            ..row
        };
        assert_eq!(
            click_viewed_box(folder),
            vec![TreeAction::ToggleFolderViewed("src".into())]
        );
    }

    #[test]
    fn status_letters_and_progress_labels() {
        assert_eq!(status_letter(FileStatus::Added), 'A');
        assert_eq!(status_letter(FileStatus::Copied), 'M');
        assert_eq!(progress_label(5, 26, 5, 26, false), "5 of 26 viewed");
        assert_eq!(
            progress_label(3, 9, 5, 26, true),
            "3 of 9 shown viewed, 5 of 26 total"
        );
    }

    #[test]
    fn largest_first_orders_files_within_a_folder() {
        let mut files = files_at(&["d/small.rs", "d/big.rs"]);
        files[1].additions = 40;
        let tree = build_tree(&files, &|_| true);
        let rows = flatten_visible(
            &tree,
            &Default::default(),
            &files,
            &FilterState::default(),
            &ViewedSet::default(),
            None,
            &HashMap::new(),
            TreeOrder::LargestFirst,
        );
        let names: Vec<_> = rows
            .iter()
            .filter(|r| !r.is_dir)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(names, vec!["big.rs", "small.rs"]);
    }

    #[test]
    fn rows_carry_comment_and_pending_counts() {
        let files = files_at(&["a.rs"]);
        let counts = HashMap::from([("a.rs".to_string(), (2, 1))]);
        let rows = flatten_visible(
            &build_tree(&files, &|_| true),
            &Default::default(),
            &files,
            &FilterState::default(),
            &ViewedSet::default(),
            None,
            &counts,
            TreeOrder::Path,
        );
        assert_eq!((rows[0].comments, rows[0].pending), (2, 1));
    }

    #[test]
    fn flatten_honors_collapse_and_indent() {
        let files = files_at(&["src/a.rs", "src/state/b.rs"]);
        let root = build_tree(&files, &|_| true);
        let f = FilterState::default();
        let v = ViewedSet::default();
        let rows = flatten_visible(
            &root,
            &BTreeSet::new(),
            &files,
            &f,
            &v,
            Some("src/a.rs"),
            &HashMap::new(),
            TreeOrder::Path,
        );
        let shape: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.indent)).collect();
        assert_eq!(
            shape,
            vec![("src", 0), ("state", 1), ("b.rs", 2), ("a.rs", 1)]
        );
        assert!(rows[3].current && !rows[3].is_dir);

        let collapsed = BTreeSet::from(["src".to_string()]);
        assert_eq!(
            flatten_visible(
                &root,
                &collapsed,
                &files,
                &f,
                &v,
                None,
                &HashMap::new(),
                TreeOrder::Path
            )
            .len(),
            1
        );
    }

    #[test]
    fn render_paints_the_filter_bar_and_rows_without_actions() {
        let files = files_at(&["src/a.rs", "src/dir/b.rs"]);
        let root = build_tree(&files, &|_| true);
        let mut filters = FilterState::default();
        let viewed = ViewedSet::default();
        let rows = flatten_visible(
            &root,
            &BTreeSet::new(),
            &files,
            &filters,
            &viewed,
            None,
            &HashMap::new(),
            TreeOrder::Path,
        );
        assert!(!rows.is_empty());

        let mut actions = Vec::new();
        let mut bar = filter_bar::FilterBarState::default();
        egui::__run_test_ui(|ui| {
            filter_bar::render(ui, &mut filters, &filter_bar::ext_options(&files), &mut bar);
            render(
                ui,
                &rows,
                &mut filters,
                TreeOrder::Path,
                (0, rows.len(), 0, files.len()),
                0,
                &mut actions,
            );
        });

        assert!(actions.is_empty(), "an unclicked tree emits no actions");
        assert!(!filters.hide_viewed, "an unclicked toggle stays off");
    }
}
