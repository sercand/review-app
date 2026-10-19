//! The inline comment composer (spec §7.1 and §7.2).
//!
//! The widget draws what [`ComposerState`] says and reports every control as
//! a [`ComposerAction`]; the reducer in [`crate::state::composer`] decides
//! what each one does. The mode buttons are the submit buttons, as on
//! GitHub, and a failed post keeps its text so a retry — or the file-level
//! fallback of Task 38 — never loses the reviewer's words.

use eframe::egui;

use crate::model::comment::CommentTarget;
use crate::model::diff::Side;
use crate::state::composer::{can_submit, mode_buttons, ComposerAction, ComposerState};
use crate::ui::widgets::markdown;

/// Draw the composer row.
///
/// `show_side` gates the Left/Right toggle: only a single-line anchor on a
/// context row can pick a side (spec §7.3). `warning` is the anchor warning
/// of [`anchor_warning`]: when it applies the composer shows the banner and
/// offers the file-level route (Task 47). `width` is the diff table's width,
/// so the text edit spans the row instead of its gutter column.
pub fn render_composer(
    ui: &mut egui::Ui,
    state: &mut ComposerState,
    has_open_review: bool,
    show_side: bool,
    warning: Option<&'static str>,
    width: f32,
    actions: &mut Vec<ComposerAction>,
) {
    egui::Frame::new()
        .inner_margin(8.0)
        .corner_radius(6.0)
        .fill(ui.visuals().panel_fill)
        .stroke(egui::Stroke::new(1.0, ui.visuals().window_stroke.color))
        .show(ui, |ui| {
            if let Some(error) = &state.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }

            if let Some(warning) = warning {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                    if ui.button("Add as file-level comment instead").clicked() {
                        actions.push(ComposerAction::ToFileLevel);
                    }
                });
            }

            if show_side {
                ui.horizontal(|ui| {
                    ui.weak("Comment on:");
                    let current = side_of(state);
                    for side in [Side::Left, Side::Right] {
                        if ui
                            .selectable_label(current == Some(side), side_label(side))
                            .clicked()
                        {
                            actions.push(ComposerAction::SetSide(side));
                        }
                    }
                });
            }

            if state.preview {
                markdown::render(ui, &markdown::to_doc(&state.body));
            } else {
                ui.add(
                    egui::TextEdit::multiline(&mut state.body)
                        .hint_text("Leave a comment")
                        .desired_width((width - 32.0).max(160.0))
                        .desired_rows(4)
                        .font(egui::TextStyle::Monospace),
                );
            }

            ui.horizontal(|ui| {
                let toggle = if state.preview { "Write" } else { "Preview" };
                if ui.button(toggle).clicked() {
                    actions.push(ComposerAction::TogglePreview);
                }
                if matches!(state.target, Some(CommentTarget::Line(_)))
                    && ui.button("Insert suggestion").clicked()
                {
                    actions.push(ComposerAction::InsertSuggestion);
                }
                // A retryable failure (Task 38) leaves the text in place;
                // this is the manual retry button, never an automatic one.
                if state.error.is_some() && can_submit(state) && ui.button("Retry").clicked() {
                    actions.push(ComposerAction::Submit(state.mode));
                }
            });

            ui.horizontal(|ui| {
                if let Some(target) = &state.target {
                    ui.add_enabled_ui(can_submit(state), |ui| {
                        for (label, mode) in mode_buttons(target, has_open_review) {
                            if ui.button(label).clicked() {
                                actions.push(ComposerAction::Submit(mode));
                            }
                        }
                    });
                }
                if ui.button("Cancel").clicked() {
                    actions.push(ComposerAction::Cancel);
                }
            });
        });
}

/// The side a single-line anchor points at, if it has one.
fn side_of(state: &ComposerState) -> Option<Side> {
    match &state.target {
        Some(CommentTarget::Line(anchor)) => Some(anchor.side),
        _ => None,
    }
}

/// The label for a side toggle button.
fn side_label(side: Side) -> &'static str {
    match side {
        Side::Left => "Left",
        Side::Right => "Right",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::composer::{reduce, CommentMode, ComposerMsg};
    use crate::test_support::line_anchor;

    /// The widget must paint the composer for a line target — text edit,
    /// mode buttons, preview toggle, and the anchor-warning banner — without
    /// panicking and without inventing actions on its own.
    #[test]
    fn render_paints_the_composer_without_actions() {
        let mut state = ComposerState::default();
        reduce(
            &mut state,
            ComposerMsg::OpenFor {
                target: CommentTarget::Line(line_anchor("a.rs", 3)),
                has_open_review: false,
            },
        );
        state.error = Some("line must be part of the diff".into());

        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            render_composer(ui, &mut state, false, true, None, 600.0, &mut actions);
            state.preview = true;
            render_composer(
                ui,
                &mut state,
                true,
                false,
                Some("This line is outside the PR's diff; GitHub may reject the comment."),
                600.0,
                &mut actions,
            );
        });
        assert!(actions.is_empty(), "an untouched composer emits no actions");
        assert_eq!(state.mode, CommentMode::Immediate);
    }
}
