//! The theme preference: what the user picks, and how it resolves against
//! the operating system's light/dark setting (Task 53).

use eframe::egui;
use serde::{Deserialize, Serialize};

/// The user's theme choice, persisted in `settings.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    /// Follow the operating system.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl Theme {
    /// The concrete egui theme for this preference, given the OS's current
    /// dark-mode flag (`System` follows it, the others ignore it).
    pub fn resolve(&self, system_dark: bool) -> egui::Theme {
        match self {
            Theme::System => {
                if system_dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                }
            }
            Theme::Light => egui::Theme::Light,
            Theme::Dark => egui::Theme::Dark,
        }
    }
}

/// The design tokens: every colour the UI may use, copied from
/// `docs/design/mockups/styles.css` (`:root` for dark,
/// `:root[data-theme="light"]` for light). No widget may write a hex
/// literal; colours come from these palettes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: egui::Color32,
    pub surface: egui::Color32,
    pub raised: egui::Color32,
    pub hover: egui::Color32,
    pub line: egui::Color32,
    pub line_soft: egui::Color32,
    pub text: egui::Color32,
    pub muted: egui::Color32,
    pub faint: egui::Color32,
    pub accent: egui::Color32,
    pub accent_ink: egui::Color32,
    pub accent_soft: egui::Color32,
    pub pending: egui::Color32,
    pub pending_soft: egui::Color32,
    pub ok: egui::Color32,
    pub warn: egui::Color32,
    pub bad: egui::Color32,
    pub info: egui::Color32,
    pub add_bg: egui::Color32,
    pub add_gutter: egui::Color32,
    pub add_word: egui::Color32,
    pub del_bg: egui::Color32,
    pub del_gutter: egui::Color32,
    pub del_word: egui::Color32,
    pub hunk_bg: egui::Color32,
    pub filler: egui::Color32,
    pub syn_kw: egui::Color32,
    pub syn_str: egui::Color32,
    pub syn_fn: egui::Color32,
    pub syn_type: egui::Color32,
    pub syn_num: egui::Color32,
    pub syn_com: egui::Color32,
    pub syn_attr: egui::Color32,
}

/// The dark palette (`:root` in `styles.css`). `rgba(r,g,b,a)` becomes
/// `from_rgba_unmultiplied(r, g, b, round(a * 255))`; `filler` is a flat
/// fill because egui has no CSS hatch.
pub const DARK: Palette = Palette {
    bg: egui::Color32::from_rgb(0x1a, 0x1d, 0x23),
    surface: egui::Color32::from_rgb(0x20, 0x24, 0x2c),
    raised: egui::Color32::from_rgb(0x28, 0x2d, 0x37),
    hover: egui::Color32::from_rgb(0x2e, 0x34, 0x40),
    line: egui::Color32::from_rgb(0x33, 0x39, 0x46),
    line_soft: egui::Color32::from_rgb(0x2a, 0x2f, 0x39),
    text: egui::Color32::from_rgb(0xe4, 0xe7, 0xec),
    muted: egui::Color32::from_rgb(0x90, 0x99, 0xa8),
    faint: egui::Color32::from_rgb(0x64, 0x6c, 0x7a),
    accent: egui::Color32::from_rgb(0x7a, 0xa2, 0xf7),
    accent_ink: egui::Color32::from_rgb(0x0f, 0x18, 0x30),
    accent_soft: egui::Color32::from_rgba_unmultiplied_const(122, 162, 247, 41),
    pending: egui::Color32::from_rgb(0xe0, 0xa9, 0x40),
    pending_soft: egui::Color32::from_rgba_unmultiplied_const(224, 169, 64, 36),
    ok: egui::Color32::from_rgb(0x4a, 0xc2, 0x6b),
    warn: egui::Color32::from_rgb(0xd6, 0xa2, 0x3a),
    bad: egui::Color32::from_rgb(0xf0, 0x60, 0x5a),
    info: egui::Color32::from_rgb(0xa8, 0x8b, 0xfa),
    add_bg: egui::Color32::from_rgba_unmultiplied_const(63, 185, 80, 33),
    add_gutter: egui::Color32::from_rgba_unmultiplied_const(63, 185, 80, 56),
    add_word: egui::Color32::from_rgba_unmultiplied_const(63, 185, 80, 97),
    del_bg: egui::Color32::from_rgba_unmultiplied_const(248, 81, 73, 31),
    del_gutter: egui::Color32::from_rgba_unmultiplied_const(248, 81, 73, 51),
    del_word: egui::Color32::from_rgba_unmultiplied_const(248, 81, 73, 97),
    hunk_bg: egui::Color32::from_rgba_unmultiplied_const(122, 162, 247, 20),
    filler: egui::Color32::from_rgba_premultiplied(6, 6, 6, 6),
    syn_kw: egui::Color32::from_rgb(0xc4, 0x9c, 0xf5),
    syn_str: egui::Color32::from_rgb(0xa5, 0xd6, 0xa7),
    syn_fn: egui::Color32::from_rgb(0x82, 0xb1, 0xff),
    syn_type: egui::Color32::from_rgb(0xf0, 0xc6, 0x74),
    syn_num: egui::Color32::from_rgb(0xf5, 0xa9, 0x7f),
    syn_com: egui::Color32::from_rgb(0x6b, 0x73, 0x85),
    syn_attr: egui::Color32::from_rgb(0x7f, 0xd3, 0xe6),
};

/// The light palette (`:root[data-theme="light"]` in `styles.css`).
pub const LIGHT: Palette = Palette {
    bg: egui::Color32::from_rgb(0xf6, 0xf7, 0xf9),
    surface: egui::Color32::from_rgb(0xff, 0xff, 0xff),
    raised: egui::Color32::from_rgb(0xee, 0xf0, 0xf4),
    hover: egui::Color32::from_rgb(0xe6, 0xe9, 0xef),
    line: egui::Color32::from_rgb(0xd8, 0xdc, 0xe4),
    line_soft: egui::Color32::from_rgb(0xe7, 0xea, 0xf0),
    text: egui::Color32::from_rgb(0x1d, 0x23, 0x30),
    muted: egui::Color32::from_rgb(0x5d, 0x66, 0x76),
    faint: egui::Color32::from_rgb(0x8d, 0x95, 0xa3),
    accent: egui::Color32::from_rgb(0x3b, 0x63, 0xd6),
    accent_ink: egui::Color32::from_rgb(0xff, 0xff, 0xff),
    accent_soft: egui::Color32::from_rgba_unmultiplied_const(59, 99, 214, 31),
    pending: egui::Color32::from_rgb(0xb7, 0x79, 0x1f),
    pending_soft: egui::Color32::from_rgba_unmultiplied_const(183, 121, 31, 31),
    ok: egui::Color32::from_rgb(0x1f, 0x8f, 0x3f),
    warn: egui::Color32::from_rgb(0x9a, 0x6b, 0x00),
    bad: egui::Color32::from_rgb(0xcf, 0x2f, 0x2a),
    info: egui::Color32::from_rgb(0x7c, 0x4d, 0xdb),
    add_bg: egui::Color32::from_rgb(0xe9, 0xf8, 0xec),
    add_gutter: egui::Color32::from_rgb(0xd2, 0xf0, 0xd9),
    add_word: egui::Color32::from_rgb(0xab, 0xe2, 0xb8),
    del_bg: egui::Color32::from_rgb(0xfd, 0xec, 0xeb),
    del_gutter: egui::Color32::from_rgb(0xfa, 0xd4, 0xd2),
    del_word: egui::Color32::from_rgb(0xf5, 0xb3, 0xae),
    hunk_bg: egui::Color32::from_rgb(0xee, 0xf3, 0xfd),
    filler: egui::Color32::from_black_alpha(9),
    syn_kw: egui::Color32::from_rgb(0x8a, 0x3f, 0xd1),
    syn_str: egui::Color32::from_rgb(0x2e, 0x7d, 0x32),
    syn_fn: egui::Color32::from_rgb(0x23, 0x57, 0xc5),
    syn_type: egui::Color32::from_rgb(0xa0, 0x5a, 0x00),
    syn_num: egui::Color32::from_rgb(0xc2, 0x41, 0x0c),
    syn_com: egui::Color32::from_rgb(0x8b, 0x93, 0xa1),
    syn_attr: egui::Color32::from_rgb(0x0e, 0x74, 0x90),
};

/// The palette for a dark-mode flag.
pub fn palette(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// The eight avatar fills (redesign mockup `.avatar`), indexed by login.
/// They are identity colours, not theme-dependent surfaces, so they live
/// beside the palette as fixed design tokens (Task 17 review).
pub const AVATAR_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(0x5b, 0x7e, 0xe5),
    egui::Color32::from_rgb(0xc0, 0x62, 0x9b),
    egui::Color32::from_rgb(0x3f, 0x9d, 0x8f),
    egui::Color32::from_rgb(0xc4, 0x7a, 0x35),
    egui::Color32::from_rgb(0x8a, 0x6b, 0xd1),
    egui::Color32::from_rgb(0x4f, 0x97, 0xc9),
    egui::Color32::from_rgb(0xb5, 0x53, 0x4f),
    egui::Color32::from_rgb(0x6c, 0x9a, 0x3c),
];

/// The initials drawn on an [`AVATAR_COLORS`] circle.
pub const AVATAR_INK: egui::Color32 = egui::Color32::WHITE;

/// The palette that matches what the ui is currently painting on.
pub fn palette_of(ui: &egui::Ui) -> &'static Palette {
    palette(ui.visuals().dark_mode)
}

/// `egui::Visuals` built from a palette, per the mockup's fills, strokes,
/// radii and shadows.
pub fn visuals(p: &Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.surface;
    v.widgets.inactive.weak_bg_fill = p.raised;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.active.weak_bg_fill = p.hover;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.line);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = egui::Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.pending;
    v.error_fg_color = p.bad;
    v.override_text_color = None;
    v.widgets.noninteractive.fg_stroke.color = p.text;
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.window_shadow = egui::Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: egui::Color32::from_black_alpha(115),
    };
    v
}

/// The shared `egui::Style`: mockup spacing and text sizes.
pub fn style(mut base: egui::Style, code_font_size: f32) -> egui::Style {
    base.spacing.item_spacing = egui::vec2(6.0, 4.0);
    base.spacing.button_padding = egui::vec2(10.0, 4.0);
    base.spacing.interact_size.y = 28.0;
    base.text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
    base.text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    base.text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(11.5));
    base.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::monospace(code_font_size),
    );
    base.text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(15.0));
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::search::Scope;
    use crate::persist::settings::Settings;
    use crate::ui::widgets::settings_window::{settings_vm, SettingsVm};

    #[test]
    fn theme_resolves_system_and_vm_round_trips() {
        assert_eq!(Theme::System.resolve(true), egui::Theme::Dark);
        assert_eq!(Theme::System.resolve(false), egui::Theme::Light);
        assert_eq!(Theme::Light.resolve(true), egui::Theme::Light);

        let mut s = Settings::default();
        let mut vm = settings_vm(&s);
        vm.host = "ghe.example.com".into();
        vm.default_scopes = vec![Scope::Assigned];
        assert!(vm.apply(&mut s)); // host changed
        assert_eq!(s.host, "ghe.example.com");
        assert_eq!(s.default_scopes, vec![Scope::Assigned]);
    }

    #[test]
    fn vm_flattens_the_client_id_and_reports_no_change_when_the_host_stays() {
        let mut s = Settings {
            client_id: Some("cid".into()),
            ..Settings::default()
        };
        let mut vm = settings_vm(&s);
        assert_eq!(vm.client_id, "cid");
        vm.client_id = String::new();
        assert!(!vm.apply(&mut s), "the host did not change");
        assert_eq!(s.client_id, None, "an empty client id flattens to None");
    }

    #[test]
    fn theme_defaults_to_system() {
        assert_eq!(Theme::default(), Theme::System);
        assert_eq!(Settings::default().theme, Theme::System);
        let _ = SettingsVm::default();
    }

    fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
        fn lum(c: egui::Color32) -> f32 {
            let f = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn tokens_match_the_mockup() {
        assert_eq!(DARK.bg, egui::Color32::from_hex("#1a1d23").unwrap());
        assert_eq!(DARK.accent, egui::Color32::from_hex("#7aa2f7").unwrap());
        assert_eq!(LIGHT.bg, egui::Color32::from_hex("#f6f7f9").unwrap());
        assert_eq!(LIGHT.accent, egui::Color32::from_hex("#3b63d6").unwrap());
        assert_eq!(
            DARK.add_bg,
            egui::Color32::from_rgba_unmultiplied(63, 185, 80, 33)
        );
    }

    #[test]
    fn text_tokens_are_readable_in_both_themes() {
        for p in [&DARK, &LIGHT] {
            assert!(contrast(p.text, p.bg) >= 7.0);
            assert!(contrast(p.muted, p.bg) >= 4.5);
            assert!(contrast(p.muted, p.surface) >= 4.5);
            assert!(contrast(p.accent_ink, p.accent) >= 4.5);
        }
    }

    #[test]
    fn visuals_follow_the_palette() {
        let v = visuals(&DARK, true);
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, DARK.bg);
        assert_eq!(v.selection.bg_fill, DARK.accent_soft);
        assert_eq!(v.hyperlink_color, DARK.accent);
        assert!(!visuals(&LIGHT, false).dark_mode);
    }

    #[test]
    fn style_uses_the_code_font_size() {
        let s = style(egui::Style::default(), 14.0);
        assert_eq!(s.text_styles[&egui::TextStyle::Monospace].size, 14.0);
        assert_eq!(s.text_styles[&egui::TextStyle::Body].size, 13.0);
    }
}
