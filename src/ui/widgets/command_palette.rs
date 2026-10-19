//! The ⌘K command palette and the `?` keyboard-shortcut overlay (spec §12).
//!
//! Both are floating [`egui::Window`]s drawn over whatever screen is showing.
//! The palette owns its query text and selection; Enter (or a click) runs the
//! selected action and closes it, and `Esc` closes it through the app's
//! global `close` dispatch. The help overlay is a read-only listing of the
//! registry.

use eframe::egui;

use crate::ui::command::{self, ActionEntry};

/// The palette's state across frames, owned by the app.
#[derive(Clone, Debug, Default)]
pub struct PaletteState {
    /// Whether the palette is showing.
    pub open: bool,
    /// The fuzzy query typed so far.
    pub query: String,
    /// Index into the current match list.
    pub selected: usize,
}

impl PaletteState {
    /// Open the palette with a fresh query.
    pub fn open_now(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    /// Dismiss the palette.
    pub fn close(&mut self) {
        self.open = false;
    }
}

/// Draw the palette over the current screen, pushing the ids of the actions
/// the user ran onto `actions`.
///
/// The query box takes keyboard focus while the palette is open, so plain
/// keys type into it instead of reaching the global shortcuts; the arrows
/// move the selection and Enter runs it. `Esc` arrives as the app's `close`
/// action, which closes the palette first.
pub fn render(
    ctx: &egui::Context,
    state: &mut PaletteState,
    entries: &[ActionEntry],
    actions: &mut Vec<&'static str>,
) {
    if !state.open {
        return;
    }

    let matches = command::fuzzy_filter(entries, &state.query);
    if state.selected >= matches.len() {
        state.selected = matches.len().saturating_sub(1);
    }

    let mut run: Option<&'static str> = None;
    let mut keep_open = true;

    egui::Window::new("Command palette")
        .anchor(egui::Align2::CENTER_TOP, [0.0, 64.0])
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .show(ctx, |ui| {
            ui.set_width(360.0);
            let box_response = ui.add(
                egui::TextEdit::singleline(&mut state.query)
                    .hint_text("Type a command…")
                    .desired_width(344.0),
            );
            box_response.request_focus();

            // Arrow keys move the selection; Enter runs it. These are read
            // from the frame's events directly, so they work while the query
            // box holds focus.
            let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
            let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if down && !matches.is_empty() {
                state.selected = (state.selected + 1).min(matches.len() - 1);
            }
            if up {
                state.selected = state.selected.saturating_sub(1);
            }

            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("palette-matches")
                .max_height(280.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (index, entry) in matches.iter().enumerate() {
                        let row = ui.selectable_label(index == state.selected, entry.label);
                        if index == state.selected {
                            row.scroll_to_me(Some(egui::Align::Center));
                        }
                        if row.clicked() {
                            run = Some(entry.id);
                        }
                    }
                    if matches.is_empty() {
                        ui.weak("No matching commands");
                    }
                });

            if enter {
                if let Some(entry) = matches.get(state.selected) {
                    run = Some(entry.id);
                }
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                keep_open = false;
            }
        });

    if let Some(id) = run {
        actions.push(id);
        keep_open = false;
    }
    state.open = keep_open;
}

/// Draw the `?` overlay: every registry entry with its keybinding.
pub fn render_help(ctx: &egui::Context, open: &mut bool, entries: &[ActionEntry]) {
    if !*open {
        return;
    }
    egui::Window::new("Keyboard shortcuts")
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_width(340.0);
            egui::Grid::new("help-grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for entry in entries {
                        ui.label(entry.label);
                        match entry.keybinding {
                            Some(keys) => ui.monospace(keys),
                            None => ui.weak("—"),
                        };
                        ui.end_row();
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::command::registry;

    #[test]
    fn palette_paints_an_idle_frame_without_running_anything() {
        let entries = registry();
        let mut state = PaletteState::default();
        state.open_now();
        let mut actions: Vec<&'static str> = Vec::new();
        egui::__run_test_ui(|ui| {
            render(ui.ctx(), &mut state, &entries, &mut actions);
        });
        assert!(actions.is_empty(), "an untouched palette runs nothing");
        assert!(state.open, "painting alone keeps it open");
    }

    #[test]
    fn palette_filters_its_list_as_the_query_is_typed() {
        let entries = registry();
        let mut state = PaletteState::default();
        state.open_now();
        state.query = "togview".into();
        let mut actions: Vec<&'static str> = Vec::new();
        egui::__run_test_ui(|ui| {
            render(ui.ctx(), &mut state, &entries, &mut actions);
        });
        // The query survives the frame, and the match list is non-empty.
        assert_eq!(state.query, "togview");
        assert!(!command::fuzzy_filter(&entries, &state.query).is_empty());
        assert!(actions.is_empty());
    }

    #[test]
    fn help_overlay_paints_the_registry_and_stays_open_until_closed() {
        let mut open = true;
        egui::__run_test_ui(|ui| {
            render_help(ui.ctx(), &mut open, &registry());
        });
        assert!(open, "painting alone never closes the overlay");
    }
}
