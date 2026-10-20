//! The file-list filter bar: extension chips, status chips, and the path box
//! (spec §9.1, §10.4).
//!
//! Every control writes straight into the [`FilterState`], so the widget has no
//! actions of its own: the file tree and the summary re-read the state next
//! frame. An empty multi-select is "no constraint", which is why a chip is only
//! highlighted while its value is in the set.

use eframe::egui::{
    self, Id, Layout, Popup, PopupCloseBehavior, RichText, ScrollArea, TextEdit, Ui,
};

use crate::github::files::FileStatus;
use crate::state::filters::FilterState;
use crate::ui::{file_icons, icons, theme};

/// The status groups the filter offers, in display order (spec §10.4).
pub const STATUS_CHOICES: [FileStatus; 4] = [
    FileStatus::Added,
    FileStatus::Modified,
    FileStatus::Removed,
    FileStatus::Renamed,
];

/// The label a status group shows under.
pub fn status_label(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "added",
        FileStatus::Modified => "modified",
        FileStatus::Removed => "removed",
        FileStatus::Renamed => "renamed",
        _ => "changed",
    }
}

/// The label an extension chip shows under (`""` is the "(none)" chip).
pub fn extension_label(extension: &str) -> String {
    if extension.is_empty() {
        "(none)".to_owned()
    } else {
        format!(".{extension}")
    }
}

/// The widget's own transient state, owned across frames by the screen.
#[derive(Debug, Default)]
pub struct FilterBarState {
    /// Whether the popover is open.
    pub popover_open: bool,
    /// The popover's file-type search text; a display filter only, never a
    /// constraint on the file list itself.
    pub ext_query: String,
}

/// Draw the filter combobox (mockup 02): a path box with a search glyph, a
/// funnel button carrying the active-filter badge, and removable chips under.
pub fn render(
    ui: &mut Ui,
    filters: &mut FilterState,
    options: &[ExtOption],
    state: &mut FilterBarState,
) {
    let search_id = ui.scope_id().with("filter-ext-search");
    let state_id = ui.scope_id().with("filter-bar-popover");
    let was_open = ui.data(|d| d.get_temp::<bool>(state_id)).unwrap_or(false);

    ui.horizontal(|ui| {
        let field_w = ui.available_width() - 34.0;
        crate::ui::components::Field::new(&mut filters.path_query)
            .icon(icons::SEARCH)
            .hint("Filter paths, * matches any")
            .width(field_w.max(60.0))
            .show(ui);
        let button = icons::icon_toggle(
            ui,
            icons::LIST_FILTER,
            "Filter files",
            &mut state.popover_open,
        );
        icons::badge(ui, &button, active_filter_count(filters));
        // The button click above already flipped `popover_open`; the popup
        // reads and writes back the same bool, so outside clicks and Escape
        // close it for real.
        Popup::from_response(&button)
            .open_bool(&mut state.popover_open)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| popover(ui, filters, options, &mut state.ext_query, search_id));
    });

    if state.popover_open && !was_open {
        ui.memory_mut(|mem| mem.request_focus(search_id));
    }
    ui.data_mut(|d| d.insert_temp(state_id, state.popover_open));

    let chips = chips(filters);
    if !chips.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for chip in chips {
                let removed = crate::ui::components::Btn::new(
                    crate::ui::components::BtnKind::Default,
                    &chip.label,
                )
                .selected(true)
                .height(22.0)
                .trailing_icon(icons::X)
                .show(ui)
                .on_hover_text("Remove filter")
                .clicked();
                if removed {
                    match &chip.kind {
                        ChipKind::Ext(extension) => {
                            filters.extensions.remove(extension);
                        }
                        ChipKind::Status(status) => {
                            filters.statuses.remove(status);
                        }
                    }
                }
            }
        });
    }
}

/// The popover body: file-type search + checklist, a 2×2 state grid, and a
/// clear button. Clicks inside keep it open (`CloseOnClickOutside`).
fn popover(
    ui: &mut Ui,
    filters: &mut FilterState,
    options: &[ExtOption],
    query: &mut String,
    search_id: Id,
) {
    let p = theme::palette_of(ui);
    ui.set_width(280.0);
    ui.spacing_mut().item_spacing.y = 6.0;

    ui.add(
        TextEdit::singleline(query)
            .id(search_id)
            .hint_text("Search file type"),
    );

    let visible: Vec<&ExtOption> = options
        .iter()
        .filter(|option| ext_matches(&option.extension, query))
        .collect();
    ScrollArea::vertical()
        .max_height(180.0)
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            for option in visible {
                let selected = filters.extensions.contains(&option.extension);
                let clicked = ui
                    .horizontal(|ui| {
                        let mut checked = selected;
                        let check = ui.checkbox(&mut checked, "");
                        file_icons::file_icon(ui, &extension_label(&option.extension));
                        let label = ui.label(extension_label(&option.extension));
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.colored_label(p.faint, option.count.to_string());
                        });
                        check.changed() || label.clicked()
                    })
                    .inner;
                if clicked {
                    if selected {
                        filters.extensions.remove(&option.extension);
                    } else {
                        filters.extensions.insert(option.extension.clone());
                    }
                }
            }
        });

    ui.separator();
    ui.colored_label(p.muted, "State");
    egui::Grid::new("filter-status-grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            for (i, status) in STATUS_CHOICES.iter().enumerate() {
                let selected = filters.statuses.contains(status);
                let (letter, color) = match status {
                    FileStatus::Added => ("A", p.ok),
                    FileStatus::Modified => ("M", p.warn),
                    FileStatus::Removed => ("D", p.bad),
                    _ => ("R", p.info),
                };
                let button = egui::Button::new(
                    RichText::new(format!("{letter} {}", status_label(*status))).color(color),
                )
                .selected(selected);
                if ui.add(button).clicked() {
                    if selected {
                        filters.statuses.remove(status);
                    } else {
                        filters.statuses.insert(*status);
                    }
                }
                if i % 2 == 1 {
                    ui.end_row();
                }
            }
        });

    if active_filter_count(filters) > 0 {
        ui.separator();
        if ui
            .add(egui::Button::new(RichText::new("Clear filters").color(p.accent)).frame(false))
            .clicked()
        {
            filters.extensions.clear();
            filters.statuses.clear();
        }
    }
}

/// One file-type row in the popover: the extension and how many files have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtOption {
    /// The bare extension (`""` for files without one).
    pub extension: String,
    /// How many files in the PR carry it.
    pub count: usize,
}

/// The popover's file-type list: most common first, ties alphabetical.
pub fn ext_options(files: &[crate::github::files::PrFile]) -> Vec<ExtOption> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for file in files {
        *counts
            .entry(crate::state::filters::extension(&file.path))
            .or_default() += 1;
    }
    let mut options: Vec<ExtOption> = counts
        .into_iter()
        .map(|(extension, count)| ExtOption { extension, count })
        .collect();
    options.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.extension.cmp(&b.extension))
    });
    options
}

/// Whether a file-type option matches the popover's search query. The query
/// works with or without a leading dot, and `"(none)"` matches files without
/// an extension.
pub fn ext_matches(extension: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let label = extension_label(extension).to_lowercase();
    label.contains(&query) || label.contains(query.trim_start_matches('.'))
}

/// The badge count: selected file types plus selected states. The path query
/// and "hide viewed" are deliberately not counted — they show elsewhere.
pub fn active_filter_count(filters: &FilterState) -> usize {
    filters.extensions.len() + filters.statuses.len()
}

/// What a removable chip stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipKind {
    /// A selected file type.
    Ext(String),
    /// A selected state.
    Status(FileStatus),
}

/// A removable chip under the combobox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// What clicking the `x` removes.
    pub kind: ChipKind,
    /// The chip's text.
    pub label: String,
}

/// The chips for everything selected, file types first.
pub fn chips(filters: &FilterState) -> Vec<Chip> {
    let mut chips: Vec<Chip> = filters
        .extensions
        .iter()
        .map(|extension| Chip {
            kind: ChipKind::Ext(extension.clone()),
            label: extension_label(extension),
        })
        .collect();
    chips.extend(filters.statuses.iter().map(|status| Chip {
        kind: ChipKind::Status(*status),
        label: status_label(*status).to_owned(),
    }));
    chips
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files_at;

    #[test]
    fn ext_options_count_sort_and_search() {
        let files = files_at(&["a.ts", "b.ts", "c.d.ts", "README", "x.rs"]);
        let options = ext_options(&files);
        assert_eq!(
            options,
            vec![
                ExtOption {
                    extension: "ts".into(),
                    count: 3
                },
                ExtOption {
                    extension: "".into(),
                    count: 1
                },
                ExtOption {
                    extension: "rs".into(),
                    count: 1
                },
            ]
        );
        assert!(ext_matches("ts", "t"));
        assert!(ext_matches("ts", ".TS"));
        assert!(ext_matches("", "none"));
        assert!(!ext_matches("rs", "ts"));
        assert!(ext_matches("rs", ""));
    }

    #[test]
    fn chips_and_count_ignore_path_and_viewed() {
        let mut filters = FilterState::default();
        filters.extensions.insert("ts".into());
        filters.statuses.insert(FileStatus::Added);
        filters.path_query = "src".into();
        filters.hide_viewed = true;
        assert_eq!(active_filter_count(&filters), 2);
        assert_eq!(
            chips(&filters),
            vec![
                Chip {
                    kind: ChipKind::Ext("ts".into()),
                    label: ".ts".into()
                },
                Chip {
                    kind: ChipKind::Status(FileStatus::Added),
                    label: "added".into()
                },
            ]
        );
    }

    #[test]
    fn labels_cover_extensions_and_statuses() {
        assert_eq!(extension_label("rs"), ".rs");
        assert_eq!(extension_label(""), "(none)");
        assert_eq!(status_label(FileStatus::Added), "added");
        assert_eq!(status_label(FileStatus::Changed), "changed");
    }
}
