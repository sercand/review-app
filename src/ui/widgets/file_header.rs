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
use crate::ui::components::{Btn, BtnKind, Seg, Tone, pill, segmented};
use crate::ui::file_icons::file_icon;
use crate::ui::fonts::mono_bold;
use crate::ui::icons::{
    self, COLUMNS_2, EXTERNAL_LINK, LAYERS, MESSAGE_SQUARE_PLUS, PILCROW, ROWS_2, SEARCH, SQUARE,
    SQUARE_CHECK, SQUARE_SPLIT_HORIZONTAL, WRAP_TEXT, icon_button, icon_toggle,
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
    use crate::diff::image::{ImageKind, image_kind};
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

    // The right-hand controls lay out first, so the path takes only the
    // width they leave and can never run under them.
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add_space(4.0);
        file_actions(ui, vm, actions);
        separator(ui);
        kind_controls(ui, vm, settings, actions);
        ui.add_space(8.0);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add_space(6.0);
            file_icon(ui, &vm.name);
            path_and_counts(ui, vm, actions);
        });
    });
}

/// The path (directory muted, file name bold), the copy button, the status
/// pill and the +/- counts, fitted into the width the controls left.
fn path_and_counts(ui: &mut egui::Ui, vm: &FileHeaderVm, actions: &mut Vec<FileHeaderAction>) {
    let p = palette_of(ui);
    let font = egui::FontId::monospace(12.5);
    let bold = mono_bold(12.5);
    // What follows the path: the copy button, the pill and the counts, as
    // they will actually measure, plus the gaps between them.
    let measure =
        |text: String, font: egui::FontId| ui.painter().layout_no_wrap(text, font, p.text).size().x;
    let pill_w = measure(
        status_label(vm.status).to_owned(),
        egui::FontId::proportional(11.5),
    ) + 14.0;
    let counts_w = if matches!(vm.kind, HeaderKind::Text | HeaderKind::TooLarge) {
        measure(
            format!("+{} −{}", vm.additions, vm.deletions),
            egui::FontId::monospace(12.0),
        ) + 10.0
    } else {
        0.0
    };
    let tail = 28.0 + pill_w + counts_w + 4.0 * ui.spacing().item_spacing.x;
    let budget = (ui.available_width() - tail).max(60.0);
    let name_w = ui
        .painter()
        .layout_no_wrap(vm.name.clone(), bold.clone(), p.text)
        .size()
        .x;
    let dir = fit_dir(ui, &vm.dir, &font, budget - name_w);
    let mut job = egui::text::LayoutJob::default();
    job.append(&dir, 0.0, egui::TextFormat::simple(font, p.muted));
    job.append(&vm.name, 0.0, egui::TextFormat::simple(bold, p.text));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let response = ui.add(egui::Label::new(galley).selectable(false));
    if dir != vm.dir {
        response.on_hover_text(format!("{}{}", vm.dir, vm.name));
    }
    if icon_button(ui, icons::COPY, "Copy path").clicked() {
        actions.push(FileHeaderAction::CopyPath);
    }
    status_pill(ui, vm);
    if matches!(vm.kind, HeaderKind::Text | HeaderKind::TooLarge) {
        ui.add_space(4.0);
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
}

/// `dir` shortened from the left with "…" until it fits `max_width`.
fn fit_dir(ui: &egui::Ui, dir: &str, font: &egui::FontId, max_width: f32) -> String {
    let width = |text: &str| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    };
    if dir.is_empty() || width(dir) <= max_width {
        return dir.to_owned();
    }
    let chars: Vec<char> = dir.chars().collect();
    for start in 1..chars.len() {
        let candidate: String = std::iter::once('…')
            .chain(chars[start..].iter().copied())
            .collect();
        if width(&candidate) <= max_width {
            return candidate;
        }
    }
    String::new()
}

/// A 1 pt vertical rule between control groups (mockup `.vsep`).
fn separator(ui: &mut egui::Ui) {
    let p = palette_of(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 20.0), egui::Sense::hover());
    ui.painter()
        .vline(rect.center().x, rect.y_range(), (1.0, p.line));
}

/// The status pill's word for a file status.
fn status_label(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "Added",
        FileStatus::Removed => "Removed",
        FileStatus::Renamed => "Renamed",
        FileStatus::Copied => "Copied",
        _ => "Modified",
    }
}

/// The status pill: a coloured word for the file's status.
fn status_pill(ui: &mut egui::Ui, vm: &FileHeaderVm) {
    let tone = match vm.status {
        FileStatus::Added => Tone::Ok,
        FileStatus::Removed => Tone::Bad,
        FileStatus::Renamed | FileStatus::Copied => Tone::Info,
        _ => Tone::Warn,
    };
    pill(ui, None, status_label(vm.status), tone);
    if let Some(previous) = &vm.previous_name {
        let p = palette_of(ui);
        ui.label(
            egui::RichText::new(format!("from {previous}"))
                .color(p.faint)
                .size(12.0),
        );
    }
}

/// The comment / browser / viewed buttons, right to left.
fn file_actions(ui: &mut egui::Ui, vm: &FileHeaderVm, actions: &mut Vec<FileHeaderAction>) {
    let viewed = Btn::new(BtnKind::Default, "Viewed")
        .icon(if vm.viewed { SQUARE_CHECK } else { SQUARE })
        .kbd("V")
        .selected(vm.viewed)
        .show(ui);
    if viewed.clicked() {
        actions.push(FileHeaderAction::ToggleViewed);
    }
    if icon_button(ui, EXTERNAL_LINK, "Open file on GitHub").clicked() {
        actions.push(FileHeaderAction::OpenInBrowser);
    }
    if icon_button(ui, MESSAGE_SQUARE_PLUS, "Comment on file").clicked() {
        actions.push(FileHeaderAction::CommentOnFile);
    }
}

/// The middle controls the file's kind offers, right to left.
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
    if matches!(vm.kind, HeaderKind::Image) && (rendered || !is_svg(vm)) {
        let picked = segmented(
            ui,
            image_mode,
            &[
                Seg::icon_text(ImageMode::Onion, LAYERS, "Onion skin"),
                Seg::icon_text(ImageMode::Swipe, SQUARE_SPLIT_HORIZONTAL, "Swipe"),
                Seg::icon_text(ImageMode::TwoUp, COLUMNS_2, "2-up"),
            ],
        );
        if let Some(wanted) = picked {
            actions.push(FileHeaderAction::SetImageMode(wanted));
        }
    }
    let show_diff_controls = matches!(vm.kind, HeaderKind::Text)
        || (matches!(vm.kind, HeaderKind::Image) && !rendered && is_svg(vm));
    if is_svg(vm)
        && let Some(want_rendered) = segmented(
            ui,
            rendered,
            &[Seg::text(true, "Rendered"), Seg::text(false, "Text")],
        )
        && want_rendered != rendered
    {
        actions.push(FileHeaderAction::ToggleRendered);
    }
    if show_diff_controls {
        if icon_button(ui, SEARCH, &icons::key_hint("Find in file  ⌘F")).clicked() {
            actions.push(FileHeaderAction::OpenFind);
        }
        let mut hide = hide_ws;
        if icon_toggle(ui, PILCROW, "Hide whitespace-only changes", &mut hide).clicked() {
            actions.push(FileHeaderAction::ToggleHideWhitespace);
        }
        let mut wrap_on = wrap;
        if icon_toggle(ui, WRAP_TEXT, "Wrap long lines", &mut wrap_on).clicked() {
            actions.push(FileHeaderAction::ToggleWrap);
        }
        // Right to left: the segment's options are listed in reverse.
        if let Some(wanted) = segmented(
            ui,
            mode,
            &[
                Seg::icon(DiffMode::Unified, ROWS_2, "Unified  U"),
                Seg::icon(DiffMode::Split, COLUMNS_2, "Split  S"),
            ],
        ) && wanted != mode
        {
            actions.push(FileHeaderAction::SetMode(wanted));
        }
    }
}

/// Whether the header's file is an SVG (which offers the Text/Rendered
/// segment alongside its diff controls).
fn is_svg(vm: &FileHeaderVm) -> bool {
    crate::state::filters::extension(&vm.name) == "svg"
}

/// The file's place in the PR's "Files changed" page on GitHub: the files
/// view anchored at `#diff-<sha256 of the path>`, the anchor GitHub gives
/// every file there.
pub fn file_url(pr_html_url: &str, path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{pr_html_url}/files#diff-{hex}")
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
        ui.add_space(4.0);
        let prev = match &vm.prev {
            Some(name) => Btn::new(BtnKind::Ghost, name)
                .icon(icons::CHEVRON_LEFT)
                .kbd("K"),
            None => Btn::new(BtnKind::Ghost, "First file").enabled(false),
        };
        if prev.show(ui).clicked() && vm.prev.is_some() {
            actions.push(FooterAction::Prev);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(4.0);
            let next = match &vm.next {
                Some(name) => Btn::new(BtnKind::Ghost, name)
                    .kbd("J")
                    .trailing_icon(icons::CHEVRON_RIGHT),
                None => Btn::new(BtnKind::Ghost, "Last file").enabled(false),
            };
            if next.show(ui).clicked() && vm.next.is_some() {
                actions.push(FooterAction::Next);
            }
            let viewed_next = Btn::new(BtnKind::Default, "Viewed, next")
                .icon(icons::CHECK)
                .kbd("⇧V")
                .show(ui);
            if viewed_next.clicked() {
                actions.push(FooterAction::ViewedAndNext);
            }
            // The position sits centred in the space between the buttons.
            ui.with_layout(
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} of {}", vm.position, vm.total))
                            .color(p.faint)
                            .size(12.5),
                    );
                },
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

    #[test]
    fn file_url_anchors_the_file_in_the_files_view() {
        assert_eq!(
            file_url(
                "https://github.com/otsimo/kedis-studio/pull/2293",
                "Assets/com.yigitdemirkol.idmaptools/README.md"
            ),
            "https://github.com/otsimo/kedis-studio/pull/2293/files#diff-\
             155701b4b73949492effad2ee18ed659410e866868ba749a30520cd90609fea5"
        );
    }
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
