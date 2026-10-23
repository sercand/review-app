//! The image diff viewer (Task 49).
//!
//! Draws one file's base and head blobs on a checkerboard (so transparency
//! is visible), in one of three modes — **2-up** side by side, **Swipe**
//! with a draggable divider clipping the head, and **Onion** with the head
//! over the base at an adjustable opacity. Added and removed images have one
//! side only: a single image, no mode switch. An over-cap or undecodable
//! blob falls back to the placeholder text with "Open in browser".
//!
//! The layout math is the pure helpers of [`crate::diff::image`]; this file
//! only turns them into widgets.

use std::io::Cursor;
use std::sync::Arc;

use eframe::egui;

use crate::diff::image::{
    ImageBlob, ImageKind, ImageMode, fit_size, image_kind, image_uri, size_delta_label, swipe_split,
};
use crate::github::files::PrFile;

/// The tallest image area the viewer claims; wide screenshots shrink to fit.
const MAX_VIEW_H: f32 = 400.0;

/// The checkerboard cell size, in points.
const CHECKER_CELL: f32 = 16.0;

/// Draw one file's image diff and return the height it used, so the caller
/// can update the row's height once the images are laid out.
///
/// `avail_width` is the width the viewer may draw into — the diff table's
/// full width, since the row's own cell is only a gutter wide.
#[allow(clippy::too_many_arguments)]
pub fn render(
    ui: &mut egui::Ui,
    session: u64,
    file: u32,
    pr_file: &PrFile,
    base: &ImageBlob,
    head: &ImageBlob,
    mode: &mut ImageMode,
    swipe: &mut f32,
    opacity: &mut f32,
    browser_url: Option<&str>,
    avail_width: f32,
) -> f32 {
    let kind = image_kind(&pr_file.path);
    let base_dims = dimensions(base, kind);
    let head_dims = dimensions(head, kind);

    // Over-cap or undecodable blobs — and a file with neither side — fall
    // back to the placeholder text with the browser escape hatch.
    let broken = |blob: &ImageBlob, dims: Option<(f32, f32)>| match blob {
        ImageBlob::TooLarge => true,
        ImageBlob::Bytes(_) => kind == Some(ImageKind::Raster) && dims.is_none(),
        ImageBlob::Absent => false,
    };
    if (base.bytes().is_none() && head.bytes().is_none())
        || broken(base, base_dims)
        || broken(head, head_dims)
    {
        return render_fallback(ui, browser_url);
    }

    let one_sided = base.bytes().is_none() || head.bytes().is_none();
    let mut used = 0.0;

    if !one_sided {
        let switch = ui.horizontal(|ui| {
            for (label, chosen) in [
                ("2-up", ImageMode::TwoUp),
                ("Swipe", ImageMode::Swipe),
                ("Onion", ImageMode::Onion),
            ] {
                if ui.selectable_label(*mode == chosen, label).clicked() {
                    *mode = chosen;
                }
            }
            match *mode {
                ImageMode::TwoUp => {}
                ImageMode::Swipe => {
                    ui.add(egui::Slider::new(swipe, 0.0..=1.0).text("split"));
                }
                ImageMode::Onion => {
                    ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity"));
                }
            }
        });
        used += switch.response.rect.height();
    }

    let avail = (avail_width.max(16.0), MAX_VIEW_H);
    used += if one_sided {
        // Added and removed images: the one side that exists, full width.
        match (base.bytes(), head.bytes()) {
            (Some(bytes), _) => draw_side(
                ui,
                image_uri(session, file, "base", &pr_file.path),
                bytes.clone(),
                base_dims,
                avail,
            ),
            (None, Some(bytes)) => draw_side(
                ui,
                image_uri(session, file, "head", &pr_file.path),
                bytes.clone(),
                head_dims,
                avail,
            ),
            (None, None) => 0.0,
        }
    } else if *mode == ImageMode::TwoUp {
        let cell = ((avail.0 - ui.spacing().item_spacing.x) / 2.0).max(16.0);
        let row = ui.horizontal(|ui| {
            let mut height: f32 = 0.0;
            if let Some(bytes) = base.bytes() {
                height = height.max(draw_side(
                    ui,
                    image_uri(session, file, "base", &pr_file.path),
                    bytes.clone(),
                    base_dims,
                    (cell, avail.1),
                ));
            }
            if let Some(bytes) = head.bytes() {
                height = height.max(draw_side(
                    ui,
                    image_uri(session, file, "head", &pr_file.path),
                    bytes.clone(),
                    head_dims,
                    (cell, avail.1),
                ));
            }
            height
        });
        row.response.rect.height()
    } else {
        let natural = head_dims.or(base_dims).unwrap_or((avail.0, avail.0));
        let size = fit_size(natural, avail);
        let (rect, _response) =
            ui.allocate_exact_size(egui::vec2(size.0, size.1), egui::Sense::hover());
        checkerboard(ui.painter(), rect);
        if let (Some(base_bytes), Some(head_bytes)) = (base.bytes(), head.bytes()) {
            let base_uri = image_uri(session, file, "base", &pr_file.path);
            let head_uri = image_uri(session, file, "head", &pr_file.path);
            if *mode == ImageMode::Swipe {
                draw_swipe(
                    ui, rect, &base_uri, base_bytes, &head_uri, head_bytes, swipe,
                );
            } else {
                draw_onion(
                    ui, rect, &base_uri, base_bytes, &head_uri, head_bytes, *opacity,
                );
            }
        }
        size.1
    };

    let caption = caption(base, head, base_dims, head_dims);
    if !caption.is_empty() {
        used += ui
            .label(egui::RichText::new(caption).small().weak())
            .rect
            .height();
    }
    used.max(0.0)
}

/// The placeholder shown in place of an image that cannot be drawn, with
/// the browser escape hatch (Task 49).
fn render_fallback(ui: &mut egui::Ui, browser_url: Option<&str>) -> f32 {
    let row = ui.horizontal(|ui| {
        ui.label("Binary file not shown");
        if let Some(url) = browser_url
            && ui.button("Open in browser").clicked()
        {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url.to_owned()));
        }
    });
    row.response.rect.height()
}

/// Draw one side into its own slot and return the height it used.
fn draw_side(
    ui: &mut egui::Ui,
    uri: String,
    bytes: Arc<[u8]>,
    dims: Option<(f32, f32)>,
    avail: (f32, f32),
) -> f32 {
    // An SVG has no cheap pixel size; a square at the available width is
    // the box it is fitted into, and the vector loader scales it.
    let natural = dims.unwrap_or((avail.0, avail.0));
    let size = fit_size(natural, avail);
    let (rect, _response) =
        ui.allocate_exact_size(egui::vec2(size.0, size.1), egui::Sense::hover());
    checkerboard(ui.painter(), rect);
    ui.put(
        rect,
        egui::Image::from_bytes(uri, bytes).fit_to_exact_size(egui::vec2(size.0, size.1)),
    );
    size.1
}

/// Swipe: both sides in one rect, the head clipped left of a draggable
/// divider.
fn draw_swipe(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    base_uri: &str,
    base_bytes: &Arc<[u8]>,
    head_uri: &str,
    head_bytes: &Arc<[u8]>,
    swipe: &mut f32,
) {
    ui.put(
        rect,
        egui::Image::from_bytes(base_uri.to_owned(), base_bytes.clone())
            .fit_to_exact_size(rect.size()),
    );
    let split = swipe_split(*swipe);
    let head_rect = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.min.x + rect.width() * split, rect.max.y),
    );
    let clip = ui.clip_rect();
    ui.set_clip_rect(clip.intersect(head_rect));
    ui.put(
        rect,
        egui::Image::from_bytes(head_uri.to_owned(), head_bytes.clone())
            .fit_to_exact_size(rect.size()),
    );
    ui.set_clip_rect(clip);

    let divider_x = rect.min.x + rect.width() * split;
    let handle = egui::Rect::from_min_max(
        egui::pos2(divider_x - 4.0, rect.min.y),
        egui::pos2(divider_x + 4.0, rect.max.y),
    );
    let response = ui.interact(
        handle,
        ui.scope_id().with("swipe-divider"),
        egui::Sense::drag(),
    );
    if response.dragged()
        && let Some(pointer) = ui.input(|i| i.pointer.interact_pos())
    {
        *swipe = swipe_split((pointer.x - rect.min.x) / rect.width().max(1.0));
    }
    ui.painter().line_segment(
        [
            egui::pos2(divider_x, rect.min.y),
            egui::pos2(divider_x, rect.max.y),
        ],
        egui::Stroke::new(2.0, egui::Color32::from_gray(120)),
    );
}

/// Onion: the head over the base, multiplied by a white tint whose alpha is
/// the opacity.
fn draw_onion(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    base_uri: &str,
    base_bytes: &Arc<[u8]>,
    head_uri: &str,
    head_bytes: &Arc<[u8]>,
    opacity: f32,
) {
    ui.put(
        rect,
        egui::Image::from_bytes(base_uri.to_owned(), base_bytes.clone())
            .fit_to_exact_size(rect.size()),
    );
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0) as u8;
    ui.put(
        rect,
        egui::Image::from_bytes(head_uri.to_owned(), head_bytes.clone())
            .fit_to_exact_size(rect.size())
            .tint(egui::Color32::from_white_alpha(alpha)),
    );
}

/// A checkerboard so transparent pixels are visible (Task 49).
fn checkerboard(painter: &egui::Painter, rect: egui::Rect) {
    let light = egui::Color32::from_gray(200);
    let dark = egui::Color32::from_gray(160);
    let mut y = rect.min.y;
    let mut row = 0;
    while y < rect.max.y {
        let mut x = rect.min.x;
        let mut column = row % 2;
        while x < rect.max.x {
            let cell = egui::Rect::from_min_max(
                egui::pos2(x, y),
                egui::pos2(
                    (x + CHECKER_CELL).min(rect.max.x),
                    (y + CHECKER_CELL).min(rect.max.y),
                ),
            );
            painter.rect_filled(cell, 0.0, if column % 2 == 0 { light } else { dark });
            x += CHECKER_CELL;
            column += 1;
        }
        y += CHECKER_CELL;
        row += 1;
    }
}

/// The caption: pixel dimensions where they are known, then the size delta.
fn caption(
    base: &ImageBlob,
    head: &ImageBlob,
    base_dims: Option<(f32, f32)>,
    head_dims: Option<(f32, f32)>,
) -> String {
    let text = |dims: Option<(f32, f32)>| {
        dims.map(|(width, height)| format!("{}×{}", width as u32, height as u32))
    };
    let dimensions = match (text(base_dims), text(head_dims)) {
        (Some(base), Some(head)) => format!("{base} → {head} · "),
        (Some(base), None) | (None, Some(base)) => format!("{base} · "),
        (None, None) => String::new(),
    };
    format!("{dimensions}{}", size_delta_label(base.size(), head.size()))
}

/// The pixel dimensions of a raster blob, read from its header only — the
/// same cheap check that decides whether the blob is decodable at all. SVG
/// has no cheap size and reports `None`.
fn dimensions(blob: &ImageBlob, kind: Option<ImageKind>) -> Option<(f32, f32)> {
    if kind != Some(ImageKind::Raster) {
        return None;
    }
    let bytes = blob.bytes()?;
    let (width, height) = image::ImageReader::new(Cursor::new(bytes.as_ref()))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some((width as f32, height as f32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::files::PatchState;
    use crate::test_support::file;

    /// The smallest valid PNG: one transparent pixel.
    fn tiny_png() -> Arc<[u8]> {
        [
            0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
        .into()
    }

    fn png_file() -> PrFile {
        PrFile {
            patch: None,
            patch_state: PatchState::Binary,
            changes: 0,
            ..file("assets/logo.png")
        }
    }

    /// The viewer must paint a real PNG in every mode — and fall back
    /// cleanly for an over-cap blob — without panicking.
    #[test]
    fn the_viewer_paints_a_png_in_every_mode() {
        let png = tiny_png();
        let file = png_file();
        let both = || (ImageBlob::Bytes(png.clone()), ImageBlob::Bytes(png.clone()));
        for chosen in [ImageMode::TwoUp, ImageMode::Swipe, ImageMode::Onion] {
            let (base, head) = both();
            let mut mode = chosen;
            let mut swipe = 0.5;
            let mut opacity = 0.5;
            egui::__run_test_ui(|ui| {
                let height = render(
                    ui,
                    1,
                    0,
                    &file,
                    &base,
                    &head,
                    &mut mode,
                    &mut swipe,
                    &mut opacity,
                    Some("https://github.com/o/r/pull/7/files"),
                    600.0,
                );
                assert!(height > 0.0, "{chosen:?} must lay out the images");
            });
        }

        // One side only (an added image): no mode switch, still drawn.
        let mut mode = ImageMode::TwoUp;
        let mut swipe = 0.5;
        let mut opacity = 0.5;
        egui::__run_test_ui(|ui| {
            let height = render(
                ui,
                1,
                0,
                &file,
                &ImageBlob::Absent,
                &ImageBlob::Bytes(png.clone()),
                &mut mode,
                &mut swipe,
                &mut opacity,
                None,
                600.0,
            );
            assert!(height > 0.0);
        });

        // Over the cap: the placeholder, not a panic.
        egui::__run_test_ui(|ui| {
            let height = render(
                ui,
                1,
                0,
                &file,
                &ImageBlob::TooLarge,
                &ImageBlob::Bytes(png.clone()),
                &mut mode,
                &mut swipe,
                &mut opacity,
                None,
                600.0,
            );
            assert!(height > 0.0);
        });
    }
}
