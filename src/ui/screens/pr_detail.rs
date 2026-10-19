//! The PR detail screen shell (spec §9.1).
//!
//! The screen is a frame around the diff surface: a top bar, a resizable left
//! pane (the file tree lands in Phase D), the virtualized diff, and a
//! collapsible right pane (the description and timeline land in Phase D). It
//! owns only its layout state; everything the diff draws comes from the
//! [`PrSession`].

use eframe::egui;

use crate::diff::highlight::HighlightCache;
use crate::diff::row_model::{display_text, RowRef, ROW_H};
use crate::state::pr_detail::PrSession;
use crate::ui::widgets::diff_view::{self, DiffAction, DiffViewState};

/// The width the left pane starts at.
const LEFT_W: f32 = 240.0;

/// The width the right pane starts at, when open.
const RIGHT_W: f32 = 320.0;

/// What the user asked of the detail screen beyond the diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrDetailAction {
    /// Leave the PR and go back to the list.
    Back,
    /// Re-fetch the PR from scratch.
    Refresh,
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
}

/// Paint the whole screen, returning what the user asked of it.
///
/// The diff's own actions are pushed onto `diff_actions` for the app to fold
/// in; the screen's actions are returned.
pub fn render(
    ui: &mut egui::Ui,
    session: &mut PrSession,
    state: &mut PrDetailState,
    cache: &mut HighlightCache,
    diff_actions: &mut Vec<DiffAction>,
) -> Vec<PrDetailAction> {
    let mut actions = Vec::new();

    egui::Panel::top("pr-detail-top").show(ui, |ui| {
        render_top_bar(ui, session, state, &mut actions);
    });

    egui::Panel::left("pr-detail-left")
        .resizable(true)
        .default_size(LEFT_W)
        .show(ui, |ui| {
            ui.heading("Files");
            ui.label("the file tree lands with the file list (Phase D)");
        });

    egui::Panel::right("pr-detail-right")
        .resizable(true)
        .default_size(RIGHT_W)
        .show_collapsible(ui, &mut state.right_open, |ui| {
            ui.heading("Conversation");
            ui.label("the description and timeline land in Phase D");
        });

    egui::CentralPanel::default().show(ui, |ui| {
        recompute_line_heights(ui, session, state);
        diff_view::render(ui, session, &mut state.diff, cache, diff_actions);
    });

    actions
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
            actions = render(ui, &mut session, &mut state, &mut cache, &mut diff_actions);
        });

        assert!(actions.is_empty());
        assert!(diff_actions.is_empty());
        assert!(state.last_layout.is_some());
    }
}
