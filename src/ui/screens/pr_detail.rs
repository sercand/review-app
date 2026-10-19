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
use crate::github::files::PrFile;
use crate::model::tree::build_tree;
use crate::state::filters::{available_extensions, matches, without_hide_viewed, FilterState};
use crate::state::folder_viewed::toggle_folder_viewed;
use crate::state::pr_detail::{head_moved_since_pin, viewer_is_author, PrSession};
use crate::state::viewed::ViewedSet;
use crate::ui::widgets::diff_view::{self, DiffAction, DiffViewState};
use crate::ui::widgets::file_tree::{self, flatten_visible, TreeAction};
use crate::ui::widgets::filter_bar;
use crate::ui::widgets::review_panel::{self, ReviewAction};
use crate::ui::widgets::thread_view::CommentDrafts;

/// The width the left pane starts at.
const LEFT_W: f32 = 240.0;

/// The width the right pane starts at, when open.
const RIGHT_W: f32 = 320.0;

/// The height the review panel starts at.
const REVIEW_H: f32 = 220.0;

/// What the user asked of the detail screen beyond the diff.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

/// The screen's own layout state, owned across frames by the app.
#[derive(Debug, Default)]
pub struct PrDetailState {
    /// The diff surface's mode and wrap.
    pub diff: DiffViewState,
    /// Whether the right pane is open.
    pub right_open: bool,
    /// The `(wrap, content width)` the line heights were measured for, so a
    /// width change only re-measures when it actually matters (spec §8.4).
    pub last_layout: Option<(bool, f32)>,
    /// Reply texts and manually expanded threads, by thread node id.
    pub drafts: CommentDrafts,
}

/// Paint the whole screen, returning what the user asked of it.
///
/// The diff's own actions are pushed onto `diff_actions` for the app to fold
/// in; the screen's actions are returned. `login` is the signed-in user, so
/// the review panel can apply the author rule.
pub fn render(
    ui: &mut egui::Ui,
    session: &mut PrSession,
    state: &mut PrDetailState,
    cache: &mut HighlightCache,
    diff_actions: &mut Vec<DiffAction>,
    login: &str,
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

    egui::Panel::left("pr-detail-left")
        .resizable(true)
        .default_size(LEFT_W)
        .show(ui, |ui| {
            render_files_pane(ui, session, &pr_files, &mut tree_actions);
        });

    // Fold the tree's actions in before the diff draws, so selecting a file
    // scrolls it into view on this very frame.
    for action in tree_actions {
        apply_tree_action(session, &pr_files, state, action, Instant::now());
    }

    egui::Panel::right("pr-detail-right")
        .resizable(true)
        .default_size(RIGHT_W)
        .show_collapsible(ui, &mut state.right_open, |ui| {
            ui.heading("Conversation");
            ui.label("the description and timeline land in Phase D");
        });

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
            };
            let vm = review_panel::review_panel_vm(&inputs);
            let past = review_panel::reviews_vm(&session.reviews);
            review_panel::render(ui, &vm, &mut session.pending, &past, &mut review_actions);
        });
    actions.extend(review_actions.into_iter().map(PrDetailAction::Review));

    egui::CentralPanel::default().show(ui, |ui| {
        recompute_line_heights(ui, session, state);
        diff_view::render(
            ui,
            session,
            &mut state.diff,
            cache,
            &mut state.drafts,
            diff_actions,
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
    pr_files: &[PrFile],
    tree_actions: &mut Vec<TreeAction>,
) {
    filter_bar::render(ui, &mut session.filters, &available_extensions(pr_files));

    // The tree covers exactly the files the filters keep, but its rows are
    // resolved against the full list so a row's `file_index` stays valid.
    let include = |f: &PrFile| matches(f, &session.filters, &session.viewed);
    let tree = build_tree(pr_files, &include);
    let rows = flatten_visible(
        &tree,
        &session.collapsed_dirs,
        pr_files,
        &session.filters,
        &session.viewed,
        session.last_opened_file.as_deref(),
    );

    let (viewed_filtered, total_filtered, additions, deletions) =
        filtered_progress(pr_files, &session.filters, &session.viewed);
    render_file_summary(ui, pr_files.len(), total_filtered, additions, deletions);
    render_invalidated_hint(ui, session.viewed.invalidated_count(pr_files));

    let progress = (
        viewed_filtered,
        total_filtered,
        session.viewed.viewed_count(pr_files),
        pr_files.len(),
    );
    file_tree::render(ui, &rows, &mut session.filters, progress, tree_actions);
}

/// The `N files, +X −Y` summary for the filtered set (spec §9.1).
fn render_file_summary(
    ui: &mut egui::Ui,
    total: usize,
    filtered: usize,
    additions: u32,
    deletions: u32,
) {
    let suffix = if filtered == total {
        String::new()
    } else {
        format!(" ({filtered} shown)")
    };
    ui.weak(format!("{total} files, +{additions} −{deletions}{suffix}"));
}

/// The "N files you had viewed changed since" hint, when any mark was
/// invalidated by a push (spec §10.3).
fn render_invalidated_hint(ui: &mut egui::Ui, invalidated: usize) {
    if invalidated == 0 {
        return;
    }
    let color = ui.visuals().warn_fg_color;
    ui.colored_label(
        color,
        format!("{invalidated} files you had viewed changed since"),
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
    state: &mut PrDetailState,
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
                state.diff.scroll_to = Some(index as u32);
                session.last_opened_file = Some(path);
            }
        }
    }
    session.mark_session_dirty(now);
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
    let width = diff_view::content_width(ui, state.diff.mode);
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

        let mut state = PrDetailState::default();
        let mut cache = HighlightCache::new(8, Box::new(SyntectHighlighter::new()));
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
        let mut cache = HighlightCache::new(8, Box::new(SyntectHighlighter::new()));
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
            );
        });
        assert!(
            actions.is_empty(),
            "an unanswered dialog emits no actions, and the text is kept"
        );
        assert!(session.fallback.is_some());
    }
}
