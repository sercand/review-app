//! The PR detail screen shell (spec §9.1).
//!
//! The screen is a frame around the diff surface: a top bar, a resizable left
//! pane (the file tree lands in Phase D), the virtualized diff, and a
//! collapsible right pane (the description and timeline land in Phase D). It
//! owns only its layout state; everything the diff draws comes from the
//! [`PrSession`].

use eframe::egui;

use std::time::Instant;

use crate::diff::highlight::HighlightCache;
use crate::diff::row_model::{display_text, RowRef, ROW_H};
use crate::github::client::endpoints_for;
use crate::github::files::PrFile;
use crate::model::tree::build_tree;
use crate::state::filters::{matches, without_hide_viewed, FilterState};
use crate::state::folder_viewed::toggle_folder_viewed;
use crate::state::pr_detail::{head_moved_since_pin, viewer_is_author, PrSession};
use crate::state::viewed::ViewedSet;
use crate::ui::widgets::diff_view::{self, DiffAction, DiffViewState};
use crate::ui::widgets::file_header::{self, FileHeaderAction, FooterAction as FileFooterAction};
use crate::ui::widgets::file_tree::{self, flatten_visible, TreeAction};
use crate::ui::widgets::filter_bar;
use crate::ui::widgets::metadata_panel;
use crate::ui::widgets::review_panel::{self, ReviewAction};
use crate::ui::widgets::thread_view::CommentDrafts;

/// The width the left pane starts at (mockup 02).
const LEFT_W: f32 = 300.0;

/// The width the right pane starts at, when open.
const RIGHT_W: f32 = 320.0;

/// The height the review panel starts at.
const REVIEW_H: f32 = 220.0;

/// What the user asked of the detail screen beyond the diff.
#[derive(Clone, Debug, PartialEq)]
pub enum PrDetailAction {
    /// Leave the PR and go back to the list.
    Back,
    /// Re-fetch the PR from scratch.
    Refresh,
    /// Answer the 422 file-level fallback dialog (Task 38).
    FileFallback {
        /// Post the file-level comment, or keep editing the line one.
        accept: bool,
    },
    /// A control the review panel reported (Task 44).
    Review(ReviewAction),
    /// Open the settings window (Task 53).
    OpenSettings,
    /// A control in the file header bar (Task 7).
    FileHeader(FileHeaderAction),
    /// A control in the file footer bar (Task 7).
    FileFooter(FileFooterAction),
}

/// The screen's own layout state, owned across frames by the app.
#[derive(Debug)]
pub struct PrDetailState {
    /// The diff surface's mode and wrap.
    pub diff: DiffViewState,
    /// Whether the right pane is open.
    pub right_open: bool,
    /// The row of the hunk `n`/`p` last moved to, so the pair walks the
    /// current file's hunks instead of jumping to one each time (Task 52).
    pub hunk_row: Option<usize>,
    /// The left pane's current outer width: seeded from the saved layout on
    /// open, fed back from the panel every frame (Task 53).
    pub left_pane: f32,
    /// The right pane's current outer width, same round trip; only captured
    /// while the pane is open, so the close animation never saves a shrunk
    /// width.
    pub right_pane: f32,
    /// The `(wrap, content width)` the line heights were measured for, so a
    /// width change only re-measures when it actually matters (spec §8.4).
    pub last_layout: Option<(bool, f32)>,
    /// Reply texts and manually expanded threads, by thread node id.
    pub drafts: CommentDrafts,
    /// The header bar's find button was clicked: the diff shows its find
    /// box on the next frame and clears this (Task 7).
    pub find_open: bool,
    /// The filter combobox's popover and file-type search text (Task 10).
    pub filter_bar: filter_bar::FilterBarState,
}

impl Default for PrDetailState {
    fn default() -> Self {
        PrDetailState {
            diff: DiffViewState::default(),
            right_open: false,
            hunk_row: None,
            left_pane: LEFT_W,
            right_pane: RIGHT_W,
            last_layout: None,
            drafts: CommentDrafts::default(),
            find_open: false,
            filter_bar: filter_bar::FilterBarState::default(),
        }
    }
}

/// Paint the whole screen, returning what the user asked of it.
///
/// The diff's own actions are pushed onto `diff_actions` for the app to fold
/// in; the screen's actions are returned. `login` is the signed-in user, so
/// the review panel can apply the author rule. `write_blocked` is the
/// rate-limit tooltip (spec §13): `Some` while writes are disabled, gating
/// the review-panel and composer Submit buttons.
pub fn render(
    ui: &mut egui::Ui,
    session: &mut PrSession,
    state: &mut PrDetailState,
    cache: &mut HighlightCache,
    diff_actions: &mut Vec<DiffAction>,
    login: &str,
    write_blocked: Option<&str>,
) -> Vec<PrDetailAction> {
    let mut actions = Vec::new();
    let mut tree_actions = Vec::new();

    // The file tree and the summary work from `PrFile`s; the session keeps the
    // loaded files. Built once per frame and shared by the tree build, the row
    // flatten, and the action handling below.
    let pr_files: Vec<PrFile> = session.files.iter().map(|l| l.file.clone()).collect();

    egui::Panel::top("pr-detail-top").show(ui, |ui| {
        render_top_bar(ui, session, state, &mut actions);
    });

    let left = egui::Panel::left("pr-detail-left")
        .resizable(true)
        .default_size(state.left_pane)
        .show(ui, |ui| {
            render_files_pane(ui, session, state, &pr_files, &mut tree_actions);
        });
    // The panel's response carries its outer rect — the same measure
    // `default_size` consumes — so the saved width restores exactly (Task 53).
    state.left_pane = left.response.rect.width().clamp(220.0, 560.0);

    // Fold the tree's actions in before the diff draws, so selecting a file
    // rebuilds the rows for this very frame.
    for action in tree_actions {
        apply_tree_action(session, &pr_files, action, Instant::now());
    }

    let right = egui::Panel::right("pr-detail-right")
        .resizable(true)
        .default_size(state.right_pane)
        .show_collapsible(ui, &mut state.right_open, |ui| {
            // The whole right pane is the metadata panel: description,
            // chips, referenced issues, commits, checks, and the read-only
            // timeline (Tasks 50 and 51).
            let web = endpoints_for(&session.host).web;
            metadata_panel::render(
                ui,
                metadata_panel::PanelInputs {
                    metadata: session.metadata.as_ref(),
                    commits: &session.commits,
                    check_runs: &session.check_runs,
                    combined: session.combined_status.as_ref(),
                    reviews: &session.reviews,
                    conversation: &session.conversation,
                    web: &web,
                    repo: &session.repo,
                },
            );
        });
    // Only capture while open: the close animation slides the panel through
    // every smaller width on its way out, and none of those are the size the
    // user chose.
    if state.right_open {
        if let Some(right) = right {
            state.right_pane = right.response.rect.width();
        }
    }

    // The review panel sits under the diff, always visible: it is where the
    // draft is edited and submitted (spec §7.2). Its actions ride out as
    // screen actions for the app, which owns the session mutations.
    let mut review_actions = Vec::new();
    egui::Panel::bottom("pr-detail-review")
        .resizable(true)
        .default_size(REVIEW_H)
        .show(ui, |ui| {
            let inputs = review_panel::PanelInputs {
                pending: &session.pending,
                viewer_is_author: viewer_is_author(session, login),
                server_pending: session.server_pending.is_some(),
                head_moved_since_pin: head_moved_since_pin(session),
                write_blocked: write_blocked.map(str::to_owned),
            };
            let vm = review_panel::review_panel_vm(&inputs);
            let past = review_panel::reviews_vm(&session.reviews);
            review_panel::render(ui, &vm, &mut session.pending, &past, &mut review_actions);
        });
    actions.extend(review_actions.into_iter().map(PrDetailAction::Review));

    egui::CentralPanel::default().show(ui, |ui| {
        // The centre column: the file header bar on top, the diff between,
        // and the footer navigation under it (redesign mockup).
        let current = session.current_file().map(|(index, _)| index);
        if let Some(index) = current {
            let vm = file_header::file_header_vm(
                &pr_files[index],
                session.viewed.is_viewed(&pr_files[index]),
            );
            let rendered = session.rendered_svg.contains(&(index as u32));
            let mut header_actions = Vec::new();
            egui::Panel::top("file-header-bar")
                .exact_size(file_header::HEADER_BAR_H)
                .show(ui, |ui| {
                    file_header::render_header(
                        ui,
                        &vm,
                        &file_header::HeaderSettings {
                            mode: session.mode,
                            wrap: state.diff.wrap,
                            hide_ws: session.hide_whitespace,
                            rendered,
                            image_mode: state.diff.image_mode,
                        },
                        &mut header_actions,
                    );
                });
            actions.extend(header_actions.into_iter().map(PrDetailAction::FileHeader));

            let order: Vec<u32> = session
                .file_order()
                .into_iter()
                .map(|index| index as u32)
                .collect();
            let footer = file_header::footer_vm(&order, Some(index as u32), &pr_files);
            let mut footer_actions = Vec::new();
            egui::Panel::bottom("file-footer-bar")
                .exact_size(file_header::FOOTER_BAR_H)
                .show(ui, |ui| {
                    file_header::render_footer(ui, &footer, &mut footer_actions);
                });
            actions.extend(footer_actions.into_iter().map(PrDetailAction::FileFooter));
        }

        recompute_line_heights(ui, session, state);
        // The header bar's search button opens the find box for one frame's
        // hand-off: the surface owns it from there.
        if state.find_open {
            state.find_open = false;
            state.diff.find_open = true;
        }
        diff_view::render(
            ui,
            session,
            &mut state.diff,
            cache,
            &mut state.drafts,
            diff_actions,
            diff_view::SurfaceInputs {
                login,
                write_blocked,
            },
        );
    });

    // A line comment GitHub rejected with a 422: offer the same text as a
    // file-level comment, with GitHub's message as the reason (Task 38).
    if let Some(prompt) = &session.fallback {
        egui::Window::new("Could not post the line comment").show(ui, |ui| {
            ui.label(&prompt.message);
            ui.label(
                "The line may have moved out of the diff. You can post the \
                 same comment on the file instead.",
            );
            ui.horizontal(|ui| {
                if ui.button("Add as a file-level comment").clicked() {
                    actions.push(PrDetailAction::FileFallback { accept: true });
                }
                if ui.button("Keep editing").clicked() {
                    actions.push(PrDetailAction::FileFallback { accept: false });
                }
            });
        });
    }

    actions
}

/// Draw the filter bar, the invalidated-viewed hint, and the file tree.
fn render_files_pane(
    ui: &mut egui::Ui,
    session: &mut PrSession,
    state: &mut PrDetailState,
    pr_files: &[PrFile],
    tree_actions: &mut Vec<TreeAction>,
) {
    filter_bar::render(
        ui,
        &mut session.filters,
        &filter_bar::ext_options(pr_files),
        &mut state.filter_bar,
    );

    // The tree covers exactly the files the filters keep, but its rows are
    // resolved against the full list so a row's `file_index` stays valid.
    let include = |f: &PrFile| matches(f, &session.filters, &session.viewed);
    let tree = build_tree(pr_files, &include);
    let counts = crate::state::pr_detail::comment_counts(&session.threads, &session.pending);
    let rows = flatten_visible(
        &tree,
        &session.collapsed_dirs,
        pr_files,
        &session.filters,
        &session.viewed,
        session.last_opened_file.as_deref(),
        &counts,
        session.tree_order,
    );

    let (viewed_filtered, total_filtered, _additions, _deletions) =
        filtered_progress(pr_files, &session.filters, &session.viewed);

    let progress = (
        viewed_filtered,
        total_filtered,
        session.viewed.viewed_count(pr_files),
        pr_files.len(),
    );
    file_tree::render(
        ui,
        &rows,
        &mut session.filters,
        session.tree_order,
        progress,
        session.viewed.invalidated_count(pr_files),
        tree_actions,
    );
}

/// `(viewed, total, additions, deletions)` over the files the filters keep,
/// hide-viewed ignored.
fn filtered_progress(
    files: &[PrFile],
    filters: &FilterState,
    viewed: &ViewedSet,
) -> (usize, usize, u32, u32) {
    let relaxed = without_hide_viewed(filters);
    let kept: Vec<&PrFile> = files
        .iter()
        .filter(|f| matches(f, &relaxed, viewed))
        .collect();
    let viewed_count = kept.iter().filter(|f| viewed.is_viewed(f)).count();
    let additions = kept.iter().map(|f| f.additions).sum();
    let deletions = kept.iter().map(|f| f.deletions).sum();
    (viewed_count, kept.len(), additions, deletions)
}

/// Fold one file-tree action into the session (and the diff's scroll target).
///
/// Every action changes state the session file persists, so it marks the
/// session dirty and the debounced writer picks the change up next frame.
fn apply_tree_action(
    session: &mut PrSession,
    pr_files: &[PrFile],
    action: TreeAction,
    now: Instant,
) {
    match action {
        TreeAction::ToggleCollapsed(path) => {
            if !session.collapsed_dirs.remove(&path) {
                session.collapsed_dirs.insert(path);
            }
        }
        TreeAction::ToggleViewed(path) => {
            if let Some(file) = pr_files.iter().find(|f| f.path == path) {
                let viewed = session.viewed.is_viewed(file);
                session.viewed.set(file, !viewed);
            }
        }
        TreeAction::ToggleFolderViewed(folder) => {
            toggle_folder_viewed(&folder, pr_files, &session.filters, &mut session.viewed);
        }
        TreeAction::MarkAllViewed => {
            toggle_folder_viewed("", pr_files, &session.filters, &mut session.viewed);
        }
        TreeAction::SelectFile(path) => {
            if let Some(index) = pr_files.iter().position(|f| f.path == path) {
                session.select_file(index);
            }
        }
        TreeAction::CollapseAll => {
            if session.collapsed_dirs.is_empty() {
                let mut paths = Vec::new();
                collect_dir_paths(&build_tree(pr_files, &|_| true), &mut paths);
                session.collapsed_dirs.extend(paths);
            } else {
                session.collapsed_dirs.clear();
            }
        }
        TreeAction::ToggleOrder => {
            session.tree_order = match session.tree_order {
                crate::state::filters::TreeOrder::Path => {
                    crate::state::filters::TreeOrder::LargestFirst
                }
                crate::state::filters::TreeOrder::LargestFirst => {
                    crate::state::filters::TreeOrder::Path
                }
            };
        }
    }
    session.mark_session_dirty(now);
}

/// Every directory path in `node`'s subtree, root excluded.
fn collect_dir_paths(node: &crate::model::tree::TreeNode, out: &mut Vec<String>) {
    for child in &node.children {
        if child.is_dir {
            out.push(child.path.clone());
            collect_dir_paths(child, out);
        }
    }
}

/// The top bar: back, `repo#number`, title, refresh, and the right-pane toggle.
fn render_top_bar(
    ui: &mut egui::Ui,
    session: &PrSession,
    state: &mut PrDetailState,
    actions: &mut Vec<PrDetailAction>,
) {
    ui.horizontal(|ui| {
        if ui.button("← Back").clicked() {
            actions.push(PrDetailAction::Back);
        }
        ui.separator();

        ui.strong(format!("{}#{}", session.repo, session.number));

        match &session.metadata {
            Some(metadata) => {
                ui.label(&metadata.title);
                if metadata.draft {
                    ui.label("draft");
                }
            }
            // Nothing to title the PR with yet.
            None => {
                ui.spinner();
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Settings").clicked() {
                actions.push(PrDetailAction::OpenSettings);
            }
            let toggle = if state.right_open {
                "Hide details"
            } else {
                "Show details"
            };
            if ui.button(toggle).clicked() {
                state.right_open = !state.right_open;
            }
            if ui.button("Refresh").clicked() {
                actions.push(PrDetailAction::Refresh);
            }
        });
    });
}

/// Recompute every line row's height when wrap or the content width changed.
///
/// Unwrapped rows always measure [`ROW_H`], so this is a no-op unless wrap is
/// on — but it still runs on the first frame to record the layout, and it
/// restores the default heights when wrap is turned back off. Non-line rows
/// keep whatever height their own renderer measured (spec §8.4).
fn recompute_line_heights(ui: &egui::Ui, session: &mut PrSession, state: &mut PrDetailState) {
    let wrap = state.diff.wrap;
    let width = diff_view::content_width(ui, session.mode);
    if state.last_layout == Some((wrap, width)) {
        return;
    }

    let char_width = diff_view::char_width(ui);
    for index in 0..session.row_model.rows.len() {
        let height = {
            match &session.row_model.rows[index] {
                RowRef::Line { file, hunk, row } => {
                    match diff_view::line_at(session, *file, *hunk, *row) {
                        Some(diff_row) => diff_view::row_height(
                            wrap,
                            &display_text(&diff_row.text),
                            char_width,
                            width,
                        ),
                        None => ROW_H,
                    }
                }
                RowRef::Pair {
                    file,
                    hunk,
                    left,
                    right,
                } => {
                    // A wrapped pair is as tall as its taller side.
                    let side = |index: Option<u32>| {
                        index
                            .and_then(|index| diff_view::line_at(session, *file, *hunk, index))
                            .map(|diff_row| {
                                diff_view::row_height(
                                    wrap,
                                    &display_text(&diff_row.text),
                                    char_width,
                                    width,
                                )
                            })
                            .unwrap_or(ROW_H)
                    };
                    side(*left).max(side(*right))
                }
                _ => continue,
            }
        };
        session.row_model.set_height(index, height);
    }

    state.last_layout = Some((wrap, width));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::highlight::SyntectHighlighter;
    use crate::runtime::{Event, SessionId};
    use crate::test_support::{loaded, meta, repo};

    /// The shell must paint a loaded session — top bar, both panes, and the
    /// diff — and record the layout it measured the line heights for.
    #[test]
    fn the_screen_paints_a_loaded_session() {
        let (mut session, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        session.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        session.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded(
                "src/a.rs",
                "@@ -1,3 +1,3 @@\n-a\n-b\n+a2\n+b2\n c\n",
            )],
            done: true,
        });

        // Open the right pane so the metadata panel (Tasks 50 and 51) is
        // painted by this smoke test too.
        let mut state = PrDetailState {
            right_open: true,
            ..PrDetailState::default()
        };
        let mut cache = HighlightCache::new(8, Box::new(SyntectHighlighter::new(true)));
        let mut diff_actions = Vec::new();
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            actions = render(
                ui,
                &mut session,
                &mut state,
                &mut cache,
                &mut diff_actions,
                "me",
                None,
            );
        });

        assert!(actions.is_empty());
        assert!(diff_actions.is_empty());
        assert!(state.last_layout.is_some());
    }

    /// The 422 fallback dialog must paint over a loaded session without
    /// panicking, and emit nothing until the reviewer answers it.
    #[test]
    fn the_fallback_dialog_paints_without_answering_itself() {
        use crate::github::comments::NewComment;
        use crate::model::comment::CommentTarget;
        use crate::state::pr_detail::FallbackPrompt;

        let (mut session, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        session.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        session.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("src/a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        session.fallback = Some(FallbackPrompt {
            message: "Validation Failed: line must be part of the diff".into(),
            comment: NewComment {
                commit_id: "sha1".into(),
                body: "keep me".into(),
                target: CommentTarget::File {
                    path: "src/a.rs".into(),
                },
            },
        });

        let mut state = PrDetailState::default();
        let mut cache = HighlightCache::new(8, Box::new(SyntectHighlighter::new(true)));
        let mut diff_actions = Vec::new();
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            actions = render(
                ui,
                &mut session,
                &mut state,
                &mut cache,
                &mut diff_actions,
                "me",
                None,
            );
        });
        assert!(
            actions.is_empty(),
            "an unanswered dialog emits no actions, and the text is kept"
        );
        assert!(session.fallback.is_some());
    }
}
