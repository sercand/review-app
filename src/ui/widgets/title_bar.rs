//! The app's own title bar (mockup `.titlebar`): a 40 pt strip at the top of
//! every screen that the window's controls share.
//!
//! On macOS the window draws its content under a transparent title bar
//! (`ReviewApp::run`), so this strip sits beside the traffic lights: it keeps
//! their width free, and its empty space drags the window and zooms it on a
//! double click, the way a native title bar does. Elsewhere the native title
//! bar stays and the strip is just the screen's top bar.

use eframe::egui;

use crate::ui::theme::palette_of;

/// The strip's height, which also centres the macOS traffic lights.
pub const TITLE_BAR_H: f32 = 40.0;

/// Whether the window's content runs under a transparent title bar.
pub const UNDER_TITLE_BAR: bool = cfg!(target_os = "macos");

/// The width kept free at the strip's left for the traffic lights.
pub fn leading_inset() -> f32 {
    if UNDER_TITLE_BAR {
        78.0
    } else {
        8.0
    }
}

/// Draw the title bar as a top panel and lay `add` out in it, left to
/// right, vertically centred.
pub fn show(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = palette_of(ui);
    egui::Panel::top(id.to_owned())
        .exact_size(TITLE_BAR_H)
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .inner_margin(egui::Margin {
                    left: leading_inset() as i8,
                    right: 10,
                    top: 0,
                    bottom: 0,
                })
                .stroke(egui::Stroke::NONE),
        )
        .show(ui, |ui| {
            let strip = ui.max_rect().expand2(egui::vec2(leading_inset(), 0.0));
            ui.painter()
                .hline(strip.x_range(), strip.bottom() - 0.5, (1.0, p.line));
            // Registered before the controls, so they win the pointer; the
            // strip only gets what falls between them.
            let drag = ui.interact(
                strip,
                ui.make_persistent_id((id, "drag")),
                egui::Sense::click_and_drag(),
            );
            if UNDER_TITLE_BAR {
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
            }
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                add(ui);
            });
        });
}
