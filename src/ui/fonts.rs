//! Bundled fonts: IBM Plex Sans for UI text, JetBrains Mono for code, and
//! the Lucide icon font as a fallback in every family. egui has no font
//! weights, so bold text uses the named `plex-semibold` and `mono-bold`
//! families.

use eframe::egui::{FontData, FontDefinitions, FontFamily, FontId};

const PLEX: &[u8] = include_bytes!("../../assets/fonts/IBMPlexSans-Regular.ttf");
const PLEX_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/IBMPlexSans-SemiBold.ttf");
const JETBRAINS: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
const JETBRAINS_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf");
const LUCIDE: &[u8] = include_bytes!("../../assets/icons/lucide.ttf");

/// The app's font setup. Lucide sits behind every family so private-use
/// icon codepoints resolve everywhere text is drawn.
pub fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("plex".into(), FontData::from_static(PLEX).into());
    fonts.font_data.insert(
        "plex-semibold".into(),
        FontData::from_static(PLEX_SEMIBOLD).into(),
    );
    fonts
        .font_data
        .insert("jetbrains".into(), FontData::from_static(JETBRAINS).into());
    fonts.font_data.insert(
        "jetbrains-bold".into(),
        FontData::from_static(JETBRAINS_BOLD).into(),
    );
    fonts
        .font_data
        .insert("lucide".into(), FontData::from_static(LUCIDE).into());
    fonts.families.insert(
        FontFamily::Proportional,
        vec!["plex".into(), "lucide".into()],
    );
    fonts.families.insert(
        FontFamily::Monospace,
        vec!["jetbrains".into(), "lucide".into()],
    );
    fonts.families.insert(
        FontFamily::Name("plex-semibold".into()),
        vec!["plex-semibold".into(), "lucide".into()],
    );
    fonts.families.insert(
        FontFamily::Name("mono-bold".into()),
        vec!["jetbrains-bold".into(), "lucide".into()],
    );
    fonts
}

/// Semi-bold UI text (Plex SemiBold).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("plex-semibold".into()))
}

/// Bold code (JetBrains Mono Bold).
pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-bold".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    #[test]
    fn every_icon_and_text_family_has_glyphs() {
        let ctx = egui::Context::default();
        ctx.set_fonts(font_definitions());
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear(); // the font atlas delta has no renderer here
        ctx.fonts_mut(|f| {
            for &c in crate::ui::icons::ALL {
                assert!(
                    f.has_glyph(&egui::FontId::proportional(16.0), c),
                    "{c:?} missing"
                );
                assert!(f.has_glyph(&egui::FontId::monospace(12.5), c));
            }
            assert!(f.has_glyph(&semibold(13.0), 'ï'));
            assert!(f.has_glyph(&egui::FontId::monospace(12.5), '−')); // U+2212 used in −N counts
        });
    }
}
