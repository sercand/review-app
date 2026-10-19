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
use crate::ui::icons::{icon_button, BOLD, CODE, FILE_DIFF, HARD_DRIVE, LINK};
use crate::ui::theme::palette_of;
use crate::ui::widgets::markdown;

/// A markdown wrap the composer's toolbar applies to the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    /// `**bold**`.
    Bold,
    /// `` `code` ``.
    Code,
    /// `[text](url)`, with the caret landing on `url`.
    Link,
}

/// Wrap `text[sel]` in the markers of `kind`, returning the new text and
/// the range that selects the inner text (for [`Wrap::Link`], the `url`).
/// An empty selection inserts the markers and puts the caret between them.
pub fn wrap_selection(
    text: &str,
    sel: std::ops::Range<usize>,
    kind: Wrap,
) -> (String, std::ops::Range<usize>) {
    let (start, end) = (sel.start.min(text.len()), sel.end.min(text.len()));
    let inner = &text[start..end];
    let (open, close) = match kind {
        Wrap::Bold => ("**", "**"),
        Wrap::Code => ("`", "`"),
        Wrap::Link => ("[", "](url)"),
    };
    let mut out = String::with_capacity(text.len() + open.len() + close.len());
    out.push_str(&text[..start]);
    out.push_str(open);
    out.push_str(inner);
    out.push_str(close);
    out.push_str(&text[end..]);
    let inner_start = start + open.len();
    match kind {
        Wrap::Link => {
            let url_start = inner_start + inner.len() + 2;
            (out, url_start..url_start + 3)
        }
        _ => (out, inner_start..inner_start + inner.len()),
    }
}

/// The per-frame context the composer draws with. Bundled so
/// [`render_composer`] stays within clippy's argument budget (the same
/// pattern as `diff_view`'s `RowCtx`).
pub struct ComposerInputs<'a> {
    /// Whether the draft review already has comments: it names the mode
    /// buttons (spec §7.1).
    pub has_open_review: bool,
    /// Whether the anchor is a context row, where the side toggle is
    /// meaningful (spec §7.3).
    pub show_side: bool,
    /// The anchor warning of [`anchor_warning`], when one applies
    /// (Task 47).
    pub warning: Option<&'static str>,
    /// The width the text edit spans — the diff table's width, so the
    /// composer crosses its gutter column.
    pub width: f32,
    /// The rate-limit tooltip (spec §13): `Some` while writes are disabled
    /// keeps the Submit (and Retry) buttons off.
    pub write_blocked: Option<&'a str>,
}

/// Draw the composer row.
///
/// See [`ComposerInputs`] for what the context fields mean.
pub fn render_composer(
    ui: &mut egui::Ui,
    state: &mut ComposerState,
    inputs: ComposerInputs<'_>,
    actions: &mut Vec<ComposerAction>,
) {
    let ComposerInputs {
        has_open_review,
        show_side,
        warning,
        width,
        write_blocked,
    } = inputs;
    let palette = palette_of(ui);
    egui::Frame::new()
        .inner_margin(10.0)
        .corner_radius(10.0)
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.accent))
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

            // The toolbar. The text edit below remembers its cursor in
            // temporary data so a wrap button clicked this frame can use
            // the selection from the last one.
            let text_id = ui.scope_id().with("gh-review-composer-text");
            let saved: Option<std::ops::Range<usize>> = ui.data(|d| d.get_temp(text_id));
            let mut rewrap: Option<(String, std::ops::Range<usize>)> = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                segment(ui, "Write", !state.preview, || {
                    if state.preview {
                        actions.push(ComposerAction::TogglePreview);
                    }
                });
                segment(ui, "Preview", state.preview, || {
                    if !state.preview {
                        actions.push(ComposerAction::TogglePreview);
                    }
                });
                ui.add_space(6.0);
                for (icon, tooltip, kind) in [
                    (BOLD, "Bold", Wrap::Bold),
                    (CODE, "Code", Wrap::Code),
                    (LINK, "Link", Wrap::Link),
                ] {
                    if icon_button(ui, icon, tooltip).clicked() {
                        let sel = saved.clone().unwrap_or(state.body.len()..state.body.len());
                        rewrap = Some(wrap_selection(&state.body, sel, kind));
                    }
                }
                // Suggestions replace right-side lines; the mockup shows the
                // button only where GitHub does.
                let right_line = matches!(
                    &state.target,
                    Some(CommentTarget::Line(anchor)) if anchor.side == Side::Right
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(anchor_label(state.target.as_ref()))
                            .small()
                            .color(palette.faint),
                    );
                    if show_side {
                        let current = side_of(state);
                        for side in [Side::Right, Side::Left] {
                            segment(ui, side_label(side), current == Some(side), || {
                                actions.push(ComposerAction::SetSide(side));
                            });
                        }
                    }
                    if right_line && icon_button(ui, FILE_DIFF, "Insert suggestion").clicked() {
                        actions.push(ComposerAction::InsertSuggestion);
                    }
                });
            });

            if state.preview {
                markdown::render(ui, &markdown::to_doc(&state.body));
            } else {
                let output = egui::TextEdit::multiline(&mut state.body)
                    .hint_text("Leave a comment. Markdown works.")
                    .desired_width((width - 32.0).max(160.0))
                    .desired_rows(4)
                    .font(egui::TextStyle::Monospace)
                    .show(ui);
                let id = output.response.id;
                let mut state_out = output.state;
                if let Some((body, sel)) = rewrap {
                    state.body = body;
                    // The wrap range is in bytes; the cursor is in chars.
                    let start = state.body[..sel.start].chars().count();
                    let end = state.body[..sel.end].chars().count();
                    state_out
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(start),
                            egui::text::CCursor::new(end),
                        )));
                    state_out.store(ui.ctx(), id);
                }
                if let Some(range) = output.cursor_range {
                    let byte = |c: egui::text::CharIndex| {
                        state
                            .body
                            .chars()
                            .take(c.0)
                            .map(char::len_utf8)
                            .sum::<usize>()
                    };
                    let chars = range.as_sorted_char_range();
                    let bytes = byte(chars.start)..byte(chars.end);
                    ui.data_mut(|d| d.insert_temp(text_id, bytes));
                }
            }

            // The footer.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    egui::RichText::new(format!("{HARD_DRIVE} Saved as you type"))
                        .small()
                        .color(palette.faint),
                );
                // A retryable failure (Task 38) leaves the text in place;
                // this is the manual retry button, never an automatic one.
                // A retry is a write, so the rate-limit gate holds it too.
                if state.error.is_some() && can_submit(state) {
                    let retry = ui.add_enabled(write_blocked.is_none(), egui::Button::new("Retry"));
                    let clicked = retry.clicked();
                    if let Some(reason) = write_blocked {
                        retry.on_disabled_hover_text(reason);
                    }
                    if clicked {
                        actions.push(ComposerAction::Submit(state.mode));
                    }
                }
                if ui.button("Cancel").clicked() {
                    actions.push(ComposerAction::Cancel);
                }
                if let Some(target) = &state.target {
                    // The mode buttons are the submit buttons (spec §7.1);
                    // a rate limit disables them until its reset (spec §13).
                    let enabled = can_submit(state) && write_blocked.is_none();
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(enabled, |ui| {
                            let buttons = mode_buttons(target, has_open_review);
                            let is_line = matches!(target, CommentTarget::Line(_));
                            // Right-to-left: the primary button first so it
                            // lands rightmost.
                            for (label, mode) in buttons.iter().rev() {
                                let primary = !is_line
                                    || *mode == crate::state::composer::CommentMode::Pending;
                                let text = if primary {
                                    format!("{label}  ⌘↵")
                                } else {
                                    (*label).to_owned()
                                };
                                let button = if primary {
                                    ui.add(
                                        egui::Button::new(
                                            egui::RichText::new(text)
                                                .strong()
                                                .color(palette.accent_ink),
                                        )
                                        .fill(palette.accent)
                                        .corner_radius(6.0),
                                    )
                                } else {
                                    ui.button(text)
                                };
                                let clicked = button.clicked();
                                if let Some(reason) = write_blocked {
                                    button.on_disabled_hover_text(reason);
                                }
                                if clicked {
                                    actions.push(ComposerAction::Submit(*mode));
                                }
                            }
                        });
                    });
                }
            });
        });
}

/// A two-state segment control cell ("Write" | "Preview", "Left" | "Right").
fn segment(ui: &mut egui::Ui, label: &str, on: bool, on_click: impl FnOnce()) {
    let palette = palette_of(ui);
    let button = egui::Button::new(egui::RichText::new(label).small().color(if on {
        palette.text
    } else {
        palette.muted
    }))
    .fill(if on {
        palette.hover
    } else {
        egui::Color32::TRANSPARENT
    })
    .corner_radius(6.0)
    .min_size(egui::vec2(0.0, 22.0));
    if ui.add(button).clicked() {
        on_click();
    }
}

/// The composer's anchor label: "Line R75", "Line L12" or "Whole file".
fn anchor_label(target: Option<&CommentTarget>) -> String {
    match target {
        Some(CommentTarget::Line(anchor)) => format!(
            "Line {}{}",
            if anchor.side == Side::Right { "R" } else { "L" },
            anchor.line
        ),
        Some(CommentTarget::File { .. }) | None => "Whole file".to_owned(),
    }
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
            render_composer(
                ui,
                &mut state,
                ComposerInputs {
                    has_open_review: false,
                    show_side: true,
                    warning: None,
                    width: 600.0,
                    write_blocked: None,
                },
                &mut actions,
            );
            state.preview = true;
            render_composer(
                ui,
                &mut state,
                ComposerInputs {
                    has_open_review: true,
                    show_side: false,
                    warning: Some(
                        "This line is outside the PR's diff; GitHub may reject the comment.",
                    ),
                    width: 600.0,
                    write_blocked: Some("GitHub is rate-limiting writes — try again in ~30s"),
                },
                &mut actions,
            );
        });
        assert!(actions.is_empty(), "an untouched composer emits no actions");
        assert_eq!(state.mode, CommentMode::Immediate);
    }

    #[test]
    fn wrap_selection_inserts_markdown() {
        assert_eq!(
            wrap_selection("use foo here", 4..7, Wrap::Bold),
            ("use **foo** here".into(), 6..9)
        );
        assert_eq!(
            wrap_selection("call x", 5..6, Wrap::Code),
            ("call `x`".into(), 6..7)
        );
        assert_eq!(
            wrap_selection("see docs", 4..8, Wrap::Link),
            ("see [docs](url)".into(), 11..14)
        );
        assert_eq!(wrap_selection("", 0..0, Wrap::Bold), ("****".into(), 2..2));
    }
}
