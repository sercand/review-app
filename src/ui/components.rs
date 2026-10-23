//! The mockup's shared controls (`docs/design/mockups/styles.css`): pills,
//! segmented controls, toggle chips, buttons, text fields and key hints.
//!
//! Every control paints itself from the [`Palette`] at a fixed mockup size,
//! instead of leaning on egui's default widget frames, so the same control
//! looks the same on every screen.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontId, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::ui::fonts::semibold;
use crate::ui::icons::key_hint;
use crate::ui::theme::{Palette, palette_of};

/// The height of a button, a field, and a toolbar chip.
pub const CONTROL_H: f32 = 28.0;

/// The colour role of a [`pill`] or a [`Btn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Grey: `muted` on `raised`.
    Neutral,
    /// The accent colour.
    Accent,
    /// Green.
    Ok,
    /// Amber.
    Warn,
    /// Red.
    Bad,
    /// Purple.
    Info,
    /// The pending-review amber.
    Pending,
}

/// `c` at `alpha` (0–255), unmultiplied: the mockup's `rgba(…, .14)` tints.
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// A tone's foreground and fill.
pub fn tone_colors(p: &Palette, tone: Tone) -> (Color32, Color32) {
    match tone {
        Tone::Neutral => (p.muted, p.raised),
        Tone::Accent => (p.accent, p.accent_soft),
        Tone::Ok => (p.ok, tint(p.ok, 36)),
        Tone::Warn => (p.warn, tint(p.warn, 36)),
        Tone::Bad => (p.bad, tint(p.bad, 36)),
        Tone::Info => (p.info, tint(p.info, 38)),
        Tone::Pending => (p.pending, p.pending_soft),
    }
}

/// A one-line galley that ends in "…" when it is wider than `max_width`.
pub fn elided_galley(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts_mut(|f| f.layout_job(job))
}

/// A label that never takes more than `max_width`: longer text ends in "…"
/// and shows in full on hover.
pub fn elided_label(
    ui: &mut Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Response {
    let galley = elided_galley(ui, text, font, color, max_width);
    let elided = galley.elided;
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    ui.painter().galley(rect.min, galley, color);
    if elided {
        response.on_hover_text(text)
    } else {
        response
    }
}

/// An icon followed by text, as one layout job: the icon at `icon_size`, the
/// text in `font`, separated by `gap` points.
fn icon_job(
    icon: Option<char>,
    text: &str,
    font: FontId,
    icon_size: f32,
    color: Color32,
    gap: f32,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    if let Some(icon) = icon {
        job.append(
            &icon.to_string(),
            0.0,
            egui::TextFormat {
                font_id: FontId::proportional(icon_size),
                color,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    if !text.is_empty() {
        job.append(
            text,
            if icon.is_some() { gap } else { 0.0 },
            egui::TextFormat {
                font_id: font,
                color,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    job
}

/// A 20 pt rounded pill: an optional icon and a short label in the tone's
/// colour on its tint (mockup `.pill`).
pub fn pill(ui: &mut Ui, icon: Option<char>, text: &str, tone: Tone) -> Response {
    let p = palette_of(ui);
    let (fg, bg) = tone_colors(p, tone);
    let job = icon_job(icon, text, FontId::proportional(11.5), 12.0, fg, 4.0);
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let size = Vec2::new(galley.size().x + 14.0, 20.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, 10.0, bg);
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, fg);
    response
}

/// One option of a [`segmented`] control.
pub struct Seg<'a, T> {
    /// The value this option selects.
    pub value: T,
    /// The option's icon, if any.
    pub icon: Option<char>,
    /// The option's label; empty for an icon-only option.
    pub label: &'a str,
    /// The hover text, e.g. `"Split  S"`; empty for none.
    pub tooltip: &'a str,
}

impl<'a, T> Seg<'a, T> {
    /// A text option.
    pub fn text(value: T, label: &'a str) -> Self {
        Seg {
            value,
            icon: None,
            label,
            tooltip: "",
        }
    }

    /// An icon-and-text option.
    pub fn icon_text(value: T, icon: char, label: &'a str) -> Self {
        Seg {
            value,
            icon: Some(icon),
            label,
            tooltip: "",
        }
    }

    /// An icon-only option with a tooltip.
    pub fn icon(value: T, icon: char, tooltip: &'a str) -> Self {
        Seg {
            value,
            icon: Some(icon),
            label: "",
            tooltip,
        }
    }
}

/// A segmented control (mockup `.seg`): options in a `bg` track with a
/// `line` border; the selected one is a `raised` chip. Returns the option
/// that was clicked this frame.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: T, items: &[Seg<'_, T>]) -> Option<T> {
    let p = palette_of(ui);
    let font = FontId::proportional(12.5);
    let jobs: Vec<_> = items
        .iter()
        .map(|item| {
            let color = if item.value == current {
                p.text
            } else {
                p.muted
            };
            ui.fonts_mut(|f| {
                f.layout_job(icon_job(
                    item.icon,
                    item.label,
                    font.clone(),
                    14.0,
                    color,
                    6.0,
                ))
            })
        })
        .collect();
    let widths: Vec<f32> = items
        .iter()
        .zip(&jobs)
        .map(|(item, g)| {
            if item.label.is_empty() {
                26.0
            } else {
                g.size().x + 16.0
            }
        })
        .collect();
    let total = widths.iter().sum::<f32>() + 2.0 * (items.len() as f32 - 1.0) + 6.0;
    let (track, _) = ui.allocate_exact_size(Vec2::new(total, 30.0), Sense::hover());
    ui.painter().rect(
        track,
        6.0,
        p.bg,
        Stroke::new(1.0, p.line),
        StrokeKind::Inside,
    );

    let mut clicked = None;
    let mut x = track.left() + 3.0;
    for ((item, galley), width) in items.iter().zip(jobs).zip(widths) {
        let rect = Rect::from_min_size(egui::pos2(x, track.top() + 3.0), Vec2::new(width, 24.0));
        x += width + 2.0;
        let id = ui.make_persistent_id(("seg", track.min.x as i32, track.min.y as i32, x as i32));
        let response = ui.interact(rect, id, Sense::click());
        let on = item.value == current;
        if on {
            ui.painter().rect(
                rect,
                4.0,
                p.raised,
                Stroke::new(1.0, p.line),
                StrokeKind::Inside,
            );
        } else if response.hovered() {
            ui.painter().rect_filled(rect, 4.0, p.hover);
        }
        let color = if on || response.hovered() {
            p.text
        } else {
            p.muted
        };
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
        let response = if item.tooltip.is_empty() {
            response
        } else {
            response.on_hover_text(key_hint(item.tooltip))
        };
        if response.clicked() {
            clicked = Some(item.value);
        }
    }
    clicked
}

/// A multi-select toggle chip (mockup `.scope`/`.toggle`): a pill outline
/// when off, `accent_soft` with `accent` text when on, an optional count.
pub fn toggle_chip(
    ui: &mut Ui,
    on: bool,
    icon: Option<char>,
    label: &str,
    count: Option<u32>,
) -> Response {
    let p = palette_of(ui);
    let height = CONTROL_H;
    let font = FontId::proportional(13.0);
    let mut job = icon_job(icon, label, font, 14.0, Color32::PLACEHOLDER, 6.0);
    if let Some(count) = count {
        job.append(
            &count.to_string(),
            6.0,
            egui::TextFormat {
                font_id: FontId::proportional(11.5),
                color: Color32::PLACEHOLDER,
                valign: egui::Align::Center,
                ..Default::default()
            },
        );
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let size = Vec2::new(galley.size().x + 20.0, height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let (fill, stroke, color) = if on {
        (p.accent_soft, Stroke::NONE, p.accent)
    } else if response.hovered() {
        (p.hover, Stroke::new(1.0, p.line), p.text)
    } else {
        (Color32::TRANSPARENT, Stroke::new(1.0, p.line), p.muted)
    };
    ui.painter()
        .rect(rect, height / 2.0, fill, stroke, StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    response
}

/// How a [`Btn`] is drawn (mockup `.btn` variants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnKind {
    /// `raised` fill with a `line` border.
    Default,
    /// `accent` fill, `accent_ink` semibold text.
    Primary,
    /// No fill until hovered.
    Ghost,
    /// Like `Ghost`, in `bad`.
    Danger,
}

/// A mockup button: 28 pt high, 6 pt radius, an optional icon and key hint.
pub struct Btn<'a> {
    kind: BtnKind,
    icon: Option<char>,
    label: &'a str,
    kbd: Option<&'a str>,
    trailing_icon: Option<char>,
    height: f32,
    width: Option<f32>,
    enabled: bool,
    selected: bool,
}

impl<'a> Btn<'a> {
    /// A button of `kind` labelled `label`.
    pub fn new(kind: BtnKind, label: &'a str) -> Self {
        Btn {
            kind,
            icon: None,
            label,
            kbd: None,
            trailing_icon: None,
            height: CONTROL_H,
            width: None,
            enabled: true,
            selected: false,
        }
    }

    /// A leading icon.
    pub fn icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }

    /// A trailing key hint, e.g. `"⇧V"`.
    pub fn kbd(mut self, keys: &'a str) -> Self {
        self.kbd = Some(keys);
        self
    }

    /// An icon after everything else, e.g. a "next" chevron.
    pub fn trailing_icon(mut self, icon: char) -> Self {
        self.trailing_icon = Some(icon);
        self
    }

    /// A taller button (36 pt for the login's primary action).
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    /// A fixed width; the content centres in it.
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// Whether the button can be clicked.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draw as switched on (`accent_soft` fill, `accent` text).
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Paint the button.
    pub fn show(self, ui: &mut Ui) -> Response {
        let p = palette_of(ui);
        let font = if self.kind == BtnKind::Primary {
            semibold(13.0)
        } else {
            FontId::proportional(13.0)
        };
        let mut job = icon_job(self.icon, self.label, font, 15.0, Color32::PLACEHOLDER, 6.0);
        if let Some(keys) = self.kbd {
            job.append(
                &key_hint(keys),
                8.0,
                egui::TextFormat {
                    font_id: FontId::proportional(11.0),
                    color: Color32::PLACEHOLDER,
                    valign: egui::Align::Center,
                    ..Default::default()
                },
            );
        }
        if let Some(icon) = self.trailing_icon {
            job.append(
                &icon.to_string(),
                6.0,
                egui::TextFormat {
                    font_id: FontId::proportional(15.0),
                    color: Color32::PLACEHOLDER,
                    valign: egui::Align::Center,
                    ..Default::default()
                },
            );
        }
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let width = self.width.unwrap_or(galley.size().x + 20.0);
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, self.height), sense);
        // Painted by hand, so name it for screen readers (and UI tests).
        let label = self.label.to_owned();
        let enabled = self.enabled;
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &label));
        let hovered = self.enabled && response.hovered();
        let (fill, stroke, color) = match self.kind {
            _ if self.selected => (p.accent_soft, Stroke::NONE, p.accent),
            BtnKind::Primary => (
                if hovered {
                    p.accent.gamma_multiply(0.9)
                } else {
                    p.accent
                },
                Stroke::NONE,
                p.accent_ink,
            ),
            BtnKind::Default => (
                if hovered { p.hover } else { p.raised },
                Stroke::new(1.0, p.line),
                p.text,
            ),
            BtnKind::Ghost => (
                if hovered {
                    p.hover
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
                if hovered { p.text } else { p.muted },
            ),
            BtnKind::Danger => (
                if hovered {
                    tint(p.bad, 30)
                } else {
                    Color32::TRANSPARENT
                },
                Stroke::NONE,
                p.bad,
            ),
        };
        let color = if self.enabled {
            color
        } else {
            color.gamma_multiply(0.45)
        };
        ui.painter()
            .rect(rect, 6.0, fill, stroke, StrokeKind::Inside);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response
    }
}

/// A framed single-line text field (mockup `.field`): an optional leading
/// icon, a `bg` fill, a `line` border that turns `accent` while focused.
pub struct Field<'a> {
    text: &'a mut String,
    icon: Option<char>,
    hint: &'a str,
    width: Option<f32>,
    password: bool,
    id: Option<egui::Id>,
    trailing: Option<&'a str>,
}

impl<'a> Field<'a> {
    /// A field editing `text`.
    pub fn new(text: &'a mut String) -> Self {
        Field {
            text,
            icon: None,
            hint: "",
            width: None,
            password: false,
            id: None,
            trailing: None,
        }
    }

    /// A leading icon.
    pub fn icon(mut self, icon: char) -> Self {
        self.icon = Some(icon);
        self
    }

    /// The placeholder text.
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }

    /// A fixed width; the default fills the available width.
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// Mask the text.
    pub fn password(mut self, password: bool) -> Self {
        self.password = password;
        self
    }

    /// The text edit's id, for focusing it from a shortcut.
    pub fn id(mut self, id: egui::Id) -> Self {
        self.id = Some(id);
        self
    }

    /// A trailing key hint, e.g. `"/"`.
    pub fn trailing_kbd(mut self, keys: &'a str) -> Self {
        self.trailing = Some(keys);
        self
    }

    /// Paint the field; the response is the text edit's.
    pub fn show(self, ui: &mut Ui) -> Response {
        let p = palette_of(ui);
        let width = self.width.unwrap_or_else(|| ui.available_width());
        let (frame, _) = ui.allocate_exact_size(Vec2::new(width, CONTROL_H), Sense::hover());
        // The fill first, so the text edit paints over it.
        ui.painter().rect_filled(frame, 6.0, p.bg);
        let mut left = frame.left() + 8.0;
        if let Some(icon) = self.icon {
            ui.painter().text(
                egui::pos2(left, frame.center().y),
                egui::Align2::LEFT_CENTER,
                icon,
                FontId::proportional(15.0),
                p.muted,
            );
            left += 22.0;
        }
        let mut right = frame.right() - 8.0;
        if let Some(keys) = self.trailing {
            let kbd_rect = kbd_at(ui, egui::pos2(right, frame.center().y), keys);
            right = kbd_rect.left() - 6.0;
        }
        let inner = Rect::from_min_max(
            egui::pos2(left, frame.top() + 1.0),
            egui::pos2(right, frame.bottom() - 1.0),
        );
        let mut edit = egui::TextEdit::singleline(self.text)
            .frame(egui::Frame::NONE)
            .password(self.password)
            .hint_text(egui::RichText::new(self.hint).color(p.faint))
            .align(egui::Align2::LEFT_CENTER)
            .margin(Vec2::ZERO)
            .desired_width(inner.width());
        if let Some(id) = self.id {
            edit = edit.id(id);
        }
        let response = ui.put(inner, edit);
        let stroke = if response.has_focus() {
            p.accent
        } else {
            p.line
        };
        ui.painter().rect(
            frame,
            6.0,
            Color32::TRANSPARENT,
            Stroke::new(1.0, stroke),
            StrokeKind::Inside,
        );
        response
    }
}

/// A key hint box (mockup `kbd`) whose right edge is at `right_center`.
fn kbd_at(ui: &Ui, right_center: egui::Pos2, keys: &str) -> Rect {
    let p = palette_of(ui);
    let galley = ui
        .painter()
        .layout_no_wrap(key_hint(keys), FontId::proportional(10.5), p.muted);
    let size = Vec2::new((galley.size().x + 8.0).max(18.0), 17.0);
    let rect = Rect::from_min_size(
        egui::pos2(right_center.x - size.x, right_center.y - size.y / 2.0),
        size,
    );
    ui.painter().rect(
        rect,
        4.0,
        p.surface,
        Stroke::new(1.0, p.line),
        StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, p.muted);
    rect
}

/// A key hint box (mockup `kbd`), laid out inline.
pub fn kbd(ui: &mut Ui, keys: &str) -> Response {
    let p = palette_of(ui);
    let galley = ui
        .painter()
        .layout_no_wrap(key_hint(keys), FontId::proportional(10.5), p.muted);
    let size = Vec2::new((galley.size().x + 8.0).max(18.0), 17.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    kbd_at(ui, egui::pos2(rect.right(), rect.center().y), keys);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_tint_their_foreground() {
        let p = &crate::ui::theme::DARK;
        let (fg, bg) = tone_colors(p, Tone::Ok);
        assert_eq!(fg, p.ok);
        assert_eq!(
            bg,
            Color32::from_rgba_unmultiplied(p.ok.r(), p.ok.g(), p.ok.b(), 36)
        );
        assert_eq!(tone_colors(p, Tone::Accent), (p.accent, p.accent_soft));
    }
}
