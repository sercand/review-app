//! The file-list filter bar: extension chips, status chips, and the path box
//! (spec §9.1, §10.4).
//!
//! Every control writes straight into the [`FilterState`], so the widget has no
//! actions of its own: the file tree and the summary re-read the state next
//! frame. An empty multi-select is "no constraint", which is why a chip is only
//! highlighted while its value is in the set.

use eframe::egui;

use crate::github::files::FileStatus;
use crate::state::filters::FilterState;

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

/// Draw the filter bar, mutating `filters` in place.
pub fn render(ui: &mut egui::Ui, filters: &mut FilterState, extensions: &[String]) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Type:");
        for extension in extensions {
            let selected = filters.extensions.contains(extension);
            if ui
                .selectable_label(selected, extension_label(extension))
                .clicked()
            {
                if selected {
                    filters.extensions.remove(extension);
                } else {
                    filters.extensions.insert(extension.clone());
                }
            }
        }
    });

    ui.horizontal_wrapped(|ui| {
        ui.label("Status:");
        for status in STATUS_CHOICES {
            let selected = filters.statuses.contains(&status);
            if ui
                .selectable_label(selected, status_label(status))
                .clicked()
            {
                if selected {
                    filters.statuses.remove(&status);
                } else {
                    filters.statuses.insert(status);
                }
            }
        }
    });

    ui.add(
        egui::TextEdit::singleline(&mut filters.path_query)
            .hint_text("filter paths (case-insensitive, * wildcards)"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_extensions_and_statuses() {
        assert_eq!(extension_label("rs"), ".rs");
        assert_eq!(extension_label(""), "(none)");
        assert_eq!(status_label(FileStatus::Added), "added");
        assert_eq!(status_label(FileStatus::Changed), "changed");
    }
}
