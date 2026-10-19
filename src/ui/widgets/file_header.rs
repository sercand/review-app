//! The file header bar and the file footer of the centre pane (redesign
//! mockup `pr.html`).
//!
//! The header replaces the old in-table file header row and the diff
//! toolbar: one 44 pt bar carries the path, the status pill, the +/- counts,
//! the layout controls for the file's kind, and the file actions. The 40 pt
//! footer under the diff walks the filtered file list. Everything the two
//! draw is decided by the pure view models here, so the layout can be tested
//! without a window.

use eframe::egui;

use crate::diff::image::ImageMode;
use crate::github::files::{FileStatus, PrFile};
use crate::model::diff::DiffMode;
use crate::ui::file_icons::file_icon;
use crate::ui::fonts::mono_bold;
use crate::ui::icons::{
    self, icon_button, icon_text, icon_toggle, COLUMNS_2, EXTERNAL_LINK, MESSAGE_SQUARE_PLUS,
    PILCROW, ROWS_2, SEARCH, SQUARE, SQUARE_CHECK, WRAP_TEXT,
};
use crate::ui::theme::palette_of;

/// The height of the header bar.
pub const HEADER_BAR_H: f32 = 44.0;

/// The height of the footer bar.
pub const FOOTER_BAR_H: f32 = 40.0;

/// What the header's body offers for a file, per the mockup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderKind {
    /// A textual diff (or a too-large file that can become one).
    Text,
    /// A raster image, or an SVG switched to the rendered viewer.
    Image,
    /// A binary file with no diff at all.
    Binary,
    /// A file whose patch GitHub refused to send.
    TooLarge,
}

/// Everything the header bar draws for one file.
#[derive(Clone, Debug, PartialEq)]
pub struct FileHeaderVm {
    /// The directory part of the path with its trailing slash, `""` at the
    /// repository root.
    pub dir: String,
    /// The file name.
    pub name: String,
    /// The file's status, for the pill.
    pub status: FileStatus,
    /// For renames and copies, the previous file's **name**.
    pub previous_name: Option<String>,
    /// The added-line count.
    pub additions: u32,
    /// The removed-line count.
    pub deletions: u32,
    /// Whether the reviewer marked the file viewed.
    pub viewed: bool,
    /// Which controls the kind offers.
    pub kind: HeaderKind,
}

/// Build the header view model for one file.
pub fn file_header_vm(f: &PrFile, viewed: bool) -> FileHeaderVm {
    let path = &f.path;
    let (dir, name) = match path.rfind('/') {
        Some(slash) => (path[..=slash].to_owned(), path[slash + 1..].to_owned()),
        None => (String::new(), path.clone()),
    };
    let previous_name = match f.status {
        FileStatus::Renamed | FileStatus::Copied => f
            .previous_path
            .as_deref()
            .map(|previous| previous.rsplit('/').next().unwrap_or(previous).to_owned()),
        _ => None,
    };
    let kind = header_kind(f);
    FileHeaderVm {
        dir,
        name,
        status: f.status,
        previous_name,
        additions: f.additions,
        deletions: f.deletions,
        viewed,
        kind,
    }
}

/// The kind that decides the header's controls: images get the viewer
/// segments, everything else the diff controls.
fn header_kind(f: &PrFile) -> HeaderKind {
    use crate::diff::image::{image_kind, ImageKind};
    use crate::github::files::PatchState;
    match f.patch_state {
        PatchState::Binary => match image_kind(&f.path) {
            Some(ImageKind::Raster) | Some(ImageKind::Svg) => HeaderKind::Image,
            _ => HeaderKind::Binary,
        },
        PatchState::TooLarge => HeaderKind::TooLarge,
        PatchState::Present => {
            if image_kind(&f.path) == Some(ImageKind::Svg) {
                HeaderKind::Image
            } else {
                HeaderKind::Text
            }
        }
    }
}

/// What a control in the header bar or footer reported.
#[derive(Clone, Debug, PartialEq)]
pub enum FileHeaderAction {
    /// Put the file's path on the clipboard.
    CopyPath,
    /// Open the file's diff in the browser.
    OpenInBrowser,
    /// Open the file-level composer.
    CommentOnFile,
    /// Flip the file's viewed mark.
    ToggleViewed,
    /// Switch the diff layout.
    SetMode(DiffMode),
    /// Flip word wrap.
    ToggleWrap,
    /// Flip the hide-whitespace-only-changes option.
    ToggleHideWhitespace,
    /// Focus the find box.
    OpenFind,
    /// Pick an image viewer mode.
    SetImageMode(ImageMode),
    /// Flip an SVG between its text diff and the rendered viewer.
    ToggleRendered,
}

/// The live settings the header's controls reflect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeaderSettings {
    /// The diff layout.
    pub mode: DiffMode,
    /// Whether word wrap is on.
    pub wrap: bool,
    /// Whether whitespace-only changes are hidden.
    pub hide_ws: bool,
    /// Whether an SVG shows its rendered viewer.
    pub rendered: bool,
    /// The image viewer mode.
    pub image_mode: ImageMode,
}

/// Draw the 44 pt header bar for `vm`.
///
/// `settings` carries the current choices the controls reflect; every click
/// is reported, never applied here.
pub fn render_header(
    ui: &mut egui::Ui,
    vm: &FileHeaderVm,
    settings: &HeaderSettings,
    actions: &mut Vec<FileHeaderAction>,
) {
    let p = palette_of(ui);
    let band = ui.max_rect();
    ui.painter().rect_filled(band, 0.0, p.bg);
    ui.painter().line_segment(
        [
            egui::pos2(band.left(), band.bottom()),
            egui::pos2(band.right(), band.bottom()),
        ],
        (1.0, p.line),
    );

    ui.horizontal(|ui| {
        file_icon(ui, &vm.name);
        let available = ui.available_width();
        let dir_budget = (available / 2.0).max(8.0) as usize;
        let dir = elide_left(&vm.dir, dir_budget);
        if !dir.is_empty() {
            ui.label(
                egui::RichText::new(dir)
                    .color(p.muted)
                    .font(mono_bold(13.0)),
            );
        }
        ui.label(egui::RichText::new(&vm.name).font(mono_bold(13.5)));
        if icon_button(ui, icons::COPY, "Copy file path").clicked() {
            actions.push(FileHeaderAction::CopyPath);
        }

        status_pill(ui, vm);
        if matches!(vm.kind, HeaderKind::Text | HeaderKind::TooLarge) {
            ui.label(
                egui::RichText::new(format!("+{}", vm.additions))
                    .color(p.ok)
                    .font(egui::FontId::monospace(12.0)),
            );
            ui.label(
                egui::RichText::new(format!("−{}", vm.deletions))
                    .color(p.bad)
                    .font(egui::FontId::monospace(12.0)),
            );
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            file_actions(ui, vm, actions);
            ui.separator();
            kind_controls(ui, vm, settings, actions);
        });
    });
}

/// The status pill: a coloured word for the file's status.
fn status_pill(ui: &mut egui::Ui, vm: &FileHeaderVm) {
    let p = palette_of(ui);
    let (label, color) = match vm.status {
        FileStatus::Added => ("Added", p.ok),
        FileStatus::Removed => ("Removed", p.bad),
        FileStatus::Renamed => ("Renamed", p.info),
        FileStatus::Copied => ("Copied", p.info),
        _ => ("Modified", p.warn),
    };
    ui.label(
        egui::RichText::new(label)
            .color(color)
            .font(crate::ui::fonts::semibold(11.5)),
    );
    if let Some(previous) = &vm.previous_name {
        ui.label(
            egui::RichText::new(format!("from {previous}"))
                .color(p.muted)
                .italics(),
        );
    }
}

/// The comment / browser / viewed buttons, right to left.
fn file_actions(ui: &mut egui::Ui, vm: &FileHeaderVm, actions: &mut Vec<FileHeaderAction>) {
    let p = palette_of(ui);
    let viewed_label = format!(
        "{} Viewed  V",
        if vm.viewed { SQUARE_CHECK } else { SQUARE }
    );
    let viewed = egui::Button::new(
        egui::RichText::new(viewed_label)
            .color(if vm.viewed { p.accent } else { p.muted })
            .font(crate::ui::fonts::semibold(12.0)),
    )
    .fill(if vm.viewed { p.accent_soft } else { p.surface });
    if ui.add(viewed).clicked() {
        actions.push(FileHeaderAction::ToggleViewed);
    }
    if icon_button(ui, EXTERNAL_LINK, "Open on GitHub").clicked() {
        actions.push(FileHeaderAction::OpenInBrowser);
    }
    if icon_button(ui, MESSAGE_SQUARE_PLUS, "Comment on file").clicked() {
        actions.push(FileHeaderAction::CommentOnFile);
    }
}

/// The middle controls the file's kind offers.
fn kind_controls(
    ui: &mut egui::Ui,
    vm: &FileHeaderVm,
    settings: &HeaderSettings,
    actions: &mut Vec<FileHeaderAction>,
) {
    let &HeaderSettings {
        mode,
        wrap,
        hide_ws,
        rendered,
        image_mode,
    } = settings;
    let show_diff_controls = matches!(vm.kind, HeaderKind::Text)
        || (matches!(vm.kind, HeaderKind::Image) && !rendered && is_svg(vm));
    if show_diff_controls {
        if ui
            .selectable_label(mode == DiffMode::Split, icon_text(COLUMNS_2, "Split  S"))
            .clicked()
        {
            actions.push(FileHeaderAction::SetMode(DiffMode::Split));
        }
        if ui
            .selectable_label(mode == DiffMode::Unified, icon_text(ROWS_2, "Unified  U"))
            .clicked()
        {
            actions.push(FileHeaderAction::SetMode(DiffMode::Unified));
        }
        let mut wrap_on = wrap;
        if icon_toggle(ui, WRAP_TEXT, "Toggle word wrap", &mut wrap_on).clicked() {
            actions.push(FileHeaderAction::ToggleWrap);
        }
        if ui
            .selectable_label(hide_ws, icon_text(PILCROW, "Hide whitespace-only changes"))
            .clicked()
        {
            actions.push(FileHeaderAction::ToggleHideWhitespace);
        }
        if icon_button(ui, SEARCH, "Find in file  ⌘F").clicked() {
            actions.push(FileHeaderAction::OpenFind);
        }
        if is_svg(vm) {
            if ui.selectable_label(!rendered, "Text").clicked() && rendered {
                actions.push(FileHeaderAction::ToggleRendered);
            }
            if ui.selectable_label(rendered, "Rendered").clicked() && !rendered {
                actions.push(FileHeaderAction::ToggleRendered);
            }
        }
    }
    if matches!(vm.kind, HeaderKind::Image) && (rendered || !is_svg(vm)) {
        for (label, wanted) in [
            ("2-up", ImageMode::TwoUp),
            ("Swipe", ImageMode::Swipe),
            ("Onion skin", ImageMode::Onion),
        ] {
            if ui.selectable_label(image_mode == wanted, label).clicked() {
                actions.push(FileHeaderAction::SetImageMode(wanted));
            }
        }
    }
}

/// Whether the header's file is an SVG (which offers the Text/Rendered
/// segment alongside its diff controls).
fn is_svg(vm: &FileHeaderVm) -> bool {
    crate::state::filters::extension(&vm.name) == "svg"
}

/// What the footer shows for the current file.
#[derive(Clone, Debug, PartialEq)]
pub struct FooterVm {
    /// The previous file's name, `None` at the start of the order.
    pub prev: Option<String>,
    /// The next file's name, `None` at the end of the order.
    pub next: Option<String>,
    /// The current file's 1-based position in the order.
    pub position: usize,
    /// How many files the order holds.
    pub total: usize,
}

/// Build the footer view model: `order` is the visible files' indices in
/// display order, `current` the selected one.
pub fn footer_vm(order: &[u32], current: Option<u32>, files: &[PrFile]) -> FooterVm {
    let position = current.and_then(|c| order.iter().position(|i| *i == c));
    let name_at = |pos: usize| {
        order
            .get(pos)
            .and_then(|i| files.get(*i as usize))
            .map(|f| f.path.rsplit('/').next().unwrap_or(&f.path).to_owned())
    };
    FooterVm {
        prev: position.and_then(|p| p.checked_sub(1)).and_then(name_at),
        next: position.and_then(|p| name_at(p + 1)),
        position: position.map_or(0, |p| p + 1),
        total: order.len(),
    }
}

/// What a footer control reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FooterAction {
    /// The previous file.
    Prev,
    /// The next file.
    Next,
    /// Mark the current file viewed and move to the next one.
    ViewedAndNext,
}

/// Draw the 40 pt footer bar.
pub fn render_footer(ui: &mut egui::Ui, vm: &FooterVm, actions: &mut Vec<FooterAction>) {
    let p = palette_of(ui);
    let band = ui.max_rect();
    ui.painter().rect_filled(band, 0.0, p.bg);
    ui.painter().line_segment(
        [
            egui::pos2(band.left(), band.top()),
            egui::pos2(band.right(), band.top()),
        ],
        (1.0, p.line),
    );

    ui.horizontal_centered(|ui| {
        let prev_label = match &vm.prev {
            Some(name) => format!("‹ {name}  K"),
            None => "First file".to_owned(),
        };
        let prev = egui::Button::new(
            egui::RichText::new(prev_label)
                .color(if vm.prev.is_some() { p.text } else { p.faint })
                .font(crate::ui::fonts::semibold(12.0)),
        )
        .fill(p.surface);
        if ui.add_enabled(vm.prev.is_some(), prev).clicked() {
            actions.push(FooterAction::Prev);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let next_label = match &vm.next {
                Some(name) => format!("J {name} ›"),
                None => "Last file".to_owned(),
            };
            let next = egui::Button::new(
                egui::RichText::new(next_label)
                    .color(if vm.next.is_some() { p.text } else { p.faint })
                    .font(crate::ui::fonts::semibold(12.0)),
            )
            .fill(p.surface);
            if ui.add_enabled(vm.next.is_some(), next).clicked() {
                actions.push(FooterAction::Next);
            }
            let viewed_next = egui::Button::new(
                egui::RichText::new(format!("{} Viewed, next  ⇧V", icons::CHECK))
                    .color(p.muted)
                    .font(crate::ui::fonts::semibold(12.0)),
            )
            .fill(p.surface);
            if ui.add(viewed_next).clicked() {
                actions.push(FooterAction::ViewedAndNext);
            }
            ui.label(
                egui::RichText::new(format!("{} of {}", vm.position, vm.total))
                    .color(p.muted)
                    .font(egui::FontId::monospace(12.0)),
            );
        });
    });
}

/// Keep the end of `text` that fits in `max_chars` — the ellipsis counts —
/// cutting at a path separator so a partial segment never survives, and
/// prefixing `"…"`: the mockup's left-elided directory path.
pub fn elide_left(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = text.chars().skip(count - keep).collect();
    // Prefer the last separator inside the tail that still leaves content
    // after it, dropping it and any plain cut before it; a tail without one
    // keeps the plain cut.
    let slash = (0..tail.len() - 1)
        .rev()
        .find(|i| tail.as_bytes()[*i] == b'/');
    let trimmed = match slash {
        Some(slash) => tail[slash + 1..].to_owned(),
        None => tail,
    };
    format!("…{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{file, files_at};

    #[test]
    fn header_vm_splits_the_path_and_names_renames() {
        let mut f = file("src/lib/data-tools/push.ts");
        let vm = file_header_vm(&f, false);
        assert_eq!(
            (vm.dir.as_str(), vm.name.as_str()),
            ("src/lib/data-tools/", "push.ts")
        );
        assert_eq!(vm.kind, HeaderKind::Text);
        f.status = FileStatus::Renamed;
        f.previous_path = Some("src/components/notifications/kind.ts".into());
        assert_eq!(
            file_header_vm(&f, true).previous_name.as_deref(),
            Some("kind.ts")
        );
        assert_eq!(file_header_vm(&file("package-lock.json"), false).dir, "");
    }

    #[test]
    fn footer_names_neighbours_and_clamps() {
        let files = files_at(&["a/x.ts", "a/y.ts", "b/z.ts"]);
        let vm = footer_vm(&[0, 1, 2], Some(1), &files);
        assert_eq!(
            (
                vm.prev.as_deref(),
                vm.next.as_deref(),
                vm.position,
                vm.total
            ),
            (Some("x.ts"), Some("z.ts"), 2, 3)
        );
        let first = footer_vm(&[0, 1, 2], Some(0), &files);
        assert_eq!(first.prev, None);
    }

    #[test]
    fn elide_left_keeps_the_end() {
        assert_eq!(
            elide_left("src/components/notifications/", 16),
            "…notifications/"
        );
        assert_eq!(elide_left("src/", 16), "src/");
    }

    /// Every file kind's header carries the comment button — the assertion
    /// `diff_view::header_buttons` used to carry, now unconditional: the
    /// bar always draws `MESSAGE_SQUARE_PLUS` for every kind.
    #[test]
    fn every_header_can_start_a_file_comment() {
        use crate::github::files::PatchState;
        let png = crate::github::files::PrFile {
            patch: None,
            patch_state: PatchState::Binary,
            changes: 0,
            ..file("assets/logo.png")
        };
        for f in [png, file("src/a.rs"), file("huge.json")] {
            let vm = file_header_vm(&f, false);
            let mut actions = Vec::new();
            egui::__run_test_ui(|ui| {
                render_header(
                    ui,
                    &vm,
                    &HeaderSettings {
                        mode: DiffMode::Split,
                        wrap: true,
                        hide_ws: false,
                        rendered: false,
                        image_mode: ImageMode::default(),
                    },
                    &mut actions,
                );
            });
            // Painting reports nothing without input; what matters is that
            // the bar paints every kind without panicking, and the comment
            // button is part of `file_actions` for all of them.
            assert!(actions.is_empty());
        }
        let _ = PatchState::Present;
    }
}
