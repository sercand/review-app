//! The Lucide icon font: one constant per glyph (codepoints from
//! `lucide-static@0.469.0`), plus the small icon widgets every screen uses.
//! Icons come only from these constants; every icon-only button carries a
//! tooltip naming the action and its shortcut.

use eframe::egui;

use crate::ui::theme::palette_of;

pub const ARROW_DOWN_WIDE_NARROW: char = '\u{e04b}';
pub const ARROW_LEFT: char = '\u{e04c}';
pub const ARROW_RIGHT: char = '\u{e04d}';
pub const AT_SIGN: char = '\u{e052}';
pub const BOLD: char = '\u{e061}';
pub const BOOK_MARKED: char = '\u{e3f5}';
pub const BUILDING_2: char = '\u{e28f}';
pub const CHECK: char = '\u{e070}';
pub const CHECK_CHECK: char = '\u{e391}';
pub const CHEVRON_DOWN: char = '\u{e071}';
pub const CHEVRON_LEFT: char = '\u{e072}';
pub const CHEVRON_RIGHT: char = '\u{e073}';
pub const CHEVRON_UP: char = '\u{e074}';
pub const CHEVRONS_DOWN: char = '\u{e075}';
pub const CHEVRONS_DOWN_UP: char = '\u{e227}';
pub const CHEVRONS_UP: char = '\u{e078}';
pub const CHEVRONS_UP_DOWN: char = '\u{e210}';
pub const CIRCLE: char = '\u{e07a}';
pub const CIRCLE_CHECK: char = '\u{e225}';
pub const CIRCLE_DOT: char = '\u{e348}';
pub const CIRCLE_DOT_DASHED: char = '\u{e4b5}';
pub const CIRCLE_X: char = '\u{e088}';
pub const CLOCK: char = '\u{e08b}';
pub const CLOUD: char = '\u{e08c}';
pub const CODE: char = '\u{e097}';
pub const COLUMNS_2: char = '\u{e09c}';
pub const COMMAND: char = '\u{e09e}';
pub const COPY: char = '\u{e0a2}';
pub const DOWNLOAD: char = '\u{e0b6}';
pub const ELLIPSIS: char = '\u{e0ba}';
pub const EXTERNAL_LINK: char = '\u{e0bd}';
pub const EYE: char = '\u{e0be}';
pub const EYE_OFF: char = '\u{e0bf}';
pub const FILE_DIFF: char = '\u{e319}';
pub const FILE_TEXT: char = '\u{e0d0}';
pub const GIT_COMMIT_HORIZONTAL: char = '\u{e0e6}';
pub const GIT_PULL_REQUEST: char = '\u{e0e8}';
pub const GIT_PULL_REQUEST_DRAFT: char = '\u{e35e}';
pub const GITHUB: char = '\u{e0e9}';
pub const GLOBE: char = '\u{e0eb}';
pub const HARD_DRIVE: char = '\u{e0f0}';
pub const INBOX: char = '\u{e0fa}';
pub const INFO: char = '\u{e0fe}';
pub const KEY_ROUND: char = '\u{e4a7}';
pub const LAYERS: char = '\u{e52d}';
pub const LINK: char = '\u{e107}';
pub const LIST_FILTER: char = '\u{e464}';
pub const LOG_IN: char = '\u{e110}';
pub const LOG_OUT: char = '\u{e111}';
pub const MESSAGE_SQUARE: char = '\u{e11a}';
pub const MESSAGE_SQUARE_DASHED: char = '\u{e40f}';
pub const MESSAGE_SQUARE_PLUS: char = '\u{e410}';
pub const MESSAGES_SQUARE: char = '\u{e411}';
pub const MINUS: char = '\u{e11f}';
pub const MOON: char = '\u{e121}';
pub const PANEL_RIGHT: char = '\u{e435}';
pub const PENCIL: char = '\u{e1f8}';
pub const PENCIL_LINE: char = '\u{e4f4}';
pub const PILCROW: char = '\u{e3a6}';
pub const PLUS: char = '\u{e140}';
pub const REFRESH_CW: char = '\u{e148}';
pub const ROTATE_CCW: char = '\u{e14b}';
pub const ROWS_2: char = '\u{e43d}';
pub const SEARCH: char = '\u{e154}';
pub const SEND: char = '\u{e155}';
pub const SETTINGS: char = '\u{e157}';
pub const SQUARE: char = '\u{e16a}';
pub const SQUARE_CHECK: char = '\u{e55d}';
pub const SQUARE_SPLIT_HORIZONTAL: char = '\u{e3b9}';
pub const SUN: char = '\u{e17b}';
pub const TRASH_2: char = '\u{e18d}';
pub const UNFOLD_VERTICAL: char = '\u{e442}';
pub const USER_CHECK: char = '\u{e19f}';
pub const USERS: char = '\u{e1a3}';
pub const WRAP_TEXT: char = '\u{e247}';
pub const X: char = '\u{e1b1}';

/// Every icon constant, used by the glyph coverage test.
pub const ALL: &[char] = &[
    ARROW_DOWN_WIDE_NARROW,
    ARROW_LEFT,
    ARROW_RIGHT,
    AT_SIGN,
    BOLD,
    BOOK_MARKED,
    BUILDING_2,
    CHECK,
    CHECK_CHECK,
    CHEVRON_DOWN,
    CHEVRON_LEFT,
    CHEVRON_RIGHT,
    CHEVRON_UP,
    CHEVRONS_DOWN,
    CHEVRONS_DOWN_UP,
    CHEVRONS_UP,
    CHEVRONS_UP_DOWN,
    CIRCLE,
    CIRCLE_CHECK,
    CIRCLE_DOT,
    CIRCLE_DOT_DASHED,
    CIRCLE_X,
    CLOCK,
    CLOUD,
    CODE,
    COLUMNS_2,
    COMMAND,
    COPY,
    DOWNLOAD,
    ELLIPSIS,
    EXTERNAL_LINK,
    EYE,
    EYE_OFF,
    FILE_DIFF,
    FILE_TEXT,
    GIT_COMMIT_HORIZONTAL,
    GIT_PULL_REQUEST,
    GIT_PULL_REQUEST_DRAFT,
    GITHUB,
    GLOBE,
    HARD_DRIVE,
    INBOX,
    INFO,
    KEY_ROUND,
    LAYERS,
    LINK,
    LIST_FILTER,
    LOG_IN,
    LOG_OUT,
    MESSAGE_SQUARE,
    MESSAGE_SQUARE_DASHED,
    MESSAGE_SQUARE_PLUS,
    MESSAGES_SQUARE,
    MINUS,
    MOON,
    PANEL_RIGHT,
    PENCIL,
    PENCIL_LINE,
    PILCROW,
    PLUS,
    REFRESH_CW,
    ROTATE_CCW,
    ROWS_2,
    SEARCH,
    SEND,
    SETTINGS,
    SQUARE,
    SQUARE_CHECK,
    SQUARE_SPLIT_HORIZONTAL,
    SUN,
    TRASH_2,
    UNFOLD_VERTICAL,
    USER_CHECK,
    USERS,
    WRAP_TEXT,
    X,
];

/// The size of an icon glyph, per the mockups.
const GLYPH_PT: f32 = 16.0;
/// The hit area of an icon button, per the mockups.
const BUTTON_PT: f32 = 28.0;

/// A 28×28 frameless icon button: the glyph at 16 pt in `muted`, a `hover`
/// fill and `text` glyph while hovered. Every icon-only button names its
/// action (and shortcut) in `tooltip`.
pub fn icon_button(ui: &mut egui::Ui, icon: char, tooltip: &str) -> egui::Response {
    icon_at(ui, icon, tooltip, false)
}

/// Like [`icon_button`], drawn with an `accent_soft` fill and an `accent`
/// glyph while `*on`; clicking flips `*on`.
pub fn icon_toggle(ui: &mut egui::Ui, icon: char, tooltip: &str, on: &mut bool) -> egui::Response {
    let resp = icon_at(ui, icon, tooltip, *on);
    if resp.clicked() {
        *on = !*on;
    }
    resp
}

/// The label of a button that shows both an icon and text.
pub fn icon_text(icon: char, text: &str) -> String {
    format!("{icon} {text}")
}

/// Paints the count dot (accent, 14 pt high) on the top-right corner of an
/// icon button while `n > 0`.
pub fn badge(ui: &mut egui::Ui, response: &egui::Response, n: usize) {
    if n == 0 {
        return;
    }
    let p = palette_of(ui);
    let rect = response.rect;
    let dot = egui::Rect::from_min_size(
        rect.right_center() - egui::vec2(7.0, 20.0),
        egui::vec2(14.0, 14.0),
    );
    let galley =
        ui.painter()
            .layout_no_wrap(n.to_string(), egui::FontId::proportional(9.0), p.accent_ink);
    let size = dot.size().max(galley.size() + egui::vec2(4.0, 2.0));
    let dot = egui::Rect::from_center_size(dot.center(), size);
    ui.painter().rect_filled(dot, 7.0, p.accent);
    ui.painter().galley(
        dot.center() - galley.size() / 2.0,
        galley,
        egui::Color32::PLACEHOLDER,
    );
}

/// Shared drawing for the icon buttons; `on` keeps the accent "on" look.
fn icon_at(ui: &mut egui::Ui, icon: char, tooltip: &str, on: bool) -> egui::Response {
    let p = palette_of(ui);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(BUTTON_PT, BUTTON_PT), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let fill = if on {
            p.accent_soft
        } else if hovered {
            p.hover
        } else {
            egui::Color32::TRANSPARENT
        };
        if fill != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 6.0, fill);
        }
        let glyph = if on {
            p.accent
        } else if hovered {
            p.text
        } else {
            p.muted
        };
        let galley = ui.painter().layout_no_wrap(
            icon.to_string(),
            egui::FontId::proportional(GLYPH_PT),
            glyph,
        );
        ui.painter().galley(
            rect.center() - galley.size() / 2.0,
            galley,
            egui::Color32::PLACEHOLDER,
        );
    }
    resp.on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_codepoints_are_unique_private_use() {
        let mut seen = std::collections::HashSet::new();
        for &c in ALL {
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&c),
                "{c:?} outside the private use area"
            );
            assert!(seen.insert(c), "{c:?} listed twice");
        }
        assert_eq!(ALL.len(), 76); // every constant above
    }
}
