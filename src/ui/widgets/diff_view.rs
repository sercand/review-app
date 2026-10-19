//! The virtualized diff surface (spec §8.4).
//!
//! One `egui_extras::TableBuilder` renders the whole PR: a header row per file,
//! that file's lines, placeholders for files GitHub sent no patch for, and the
//! expandable gaps between hunks. Rows come from [`RowModel`], so the table only
//! ever touches the rows it actually draws — and highlighting is computed for
//! exactly those rows, through the bounded cache of
//! [`crate::diff::highlight`].
//!
//! The file whose rows are at the top of the viewport has its header pinned over
//! the table ([`sticky_file`]), so a long file's header stays on screen while its
//! lines scroll under it.
//!
//! Everything the widget decides is a pure function ([`row_height`],
//! [`split_pairs`], [`placeholder_text`], [`header_buttons`], [`sticky_file`]) so
//! it can be tested without a window; [`render`] only turns those decisions into
//! widgets.

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::diff::highlight::{syntax_for_path, HighlightCache, HighlightSpan};
use crate::diff::row_model::{display_text, RowRef, HEADER_H, ROW_H};
use crate::github::files::{anchor_path, display_path, LoadedFile, PatchState, PrFile};
use crate::model::comment::CommentTarget;
use crate::model::diff::{DiffRow, LineKind, Side};
use crate::state::composer::ComposerAction;
use crate::state::pr_detail::{LoadPhase, PrSession};
use crate::ui::widgets::comment_composer;
use crate::ui::widgets::markdown;
use crate::ui::widgets::thread_view::{self, CommentDrafts, ThreadAction};

/// The width of one line-number gutter.
pub const GUTTER_W: f32 = 48.0;

/// How the diff is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMode {
    /// One column of lines, with the old and new numbers in the gutters.
    Unified,
    /// Removed and added lines side by side.
    Split,
}

/// The diff surface's own UI state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffViewState {
    /// The layout to draw.
    pub mode: DiffMode,
    /// Whether long lines wrap instead of running off the pane.
    pub wrap: bool,
    /// A file whose header the diff should scroll to on the next frame.
    ///
    /// The file tree sets this when a file is selected; [`render`] consumes it
    /// (takes it) so the surface scrolls once and then leaves the user alone.
    pub scroll_to: Option<u32>,
}

impl Default for DiffViewState {
    fn default() -> Self {
        DiffViewState {
            mode: DiffMode::Unified,
            wrap: false,
            scroll_to: None,
        }
    }
}

/// What the user asked of the diff.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffAction {
    /// Toggle one file's viewed state (Phase D).
    ToggleViewed(u32),
    /// Start a comment on a line or a file.
    StartComment(CommentTarget),
    /// Scroll one file's header into view (wired by the file tree).
    ScrollToFile(u32),
    /// Fetch and locally diff a too-large file (Phase G).
    LoadLargeDiff(u32),
    /// Expand the omitted context before one hunk (Task 45).
    ExpandGap {
        /// The index of the file.
        file: u32,
        /// The hunk the gap precedes.
        before_hunk: u32,
    },
    /// A composer control was used (Task 37).
    Composer(ComposerAction),
    /// A thread control was used (Task 37).
    Thread(ThreadAction),
    /// Remove a locally pending comment (Task 37).
    DeletePending {
        /// The pending comment's local id.
        local_id: u64,
    },
}

/// The height one line needs, in points.
///
/// Unwrapped, every line is [`ROW_H`]; wrapped, the display text is measured at
/// `char_width` per character against `width`. `char_width` comes from the
/// monospace font's real metrics (see [`char_width`]), and `text` is measured
/// as it will be *drawn*, i.e. through [`display_text`].
pub fn row_height(wrap: bool, text: &str, char_width: f32, width: f32) -> f32 {
    if !wrap || width <= 0.0 {
        return ROW_H;
    }

    let characters = display_text(text).chars().count() as f32;
    let lines = (characters * char_width / width).ceil().max(1.0);
    ROW_H * lines
}

/// The monospace character width of `ui`'s current style.
pub fn char_width(ui: &egui::Ui) -> f32 {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    ui.fonts_mut(|fonts| fonts.glyph_width(&font_id, ' '))
}

/// The width available to line text in `mode`, from the diff pane's [`egui::Ui`].
///
/// Mirrors the width `egui_extras` gives the text column: the pane minus the
/// vertical scroll bar, the fixed gutters, and the space between columns. Split
/// mode halves what is left, because two text columns share the pane.
///
/// [`add_line`] wraps each line at this width and the screen measures the wrapped
/// height at the same one, so a wrapped row is never a line short.
pub fn content_width(ui: &egui::Ui, mode: DiffMode) -> f32 {
    let table = ui.available_width() - ui.spacing().scroll.allocated_width();
    let spacing = ui.spacing().item_spacing.x;
    match mode {
        DiffMode::Unified => (table - 2.0 * GUTTER_W - 2.0 * spacing).max(1.0),
        DiffMode::Split => ((table - 2.0 * GUTTER_W - 3.0 * spacing) / 2.0).max(1.0),
    }
}

/// Pair each run of removed lines with the run of added lines that follows it.
///
/// Context rows pair with themselves, exactly as GitHub's split view shows
/// them. Runs of unequal length leave the shorter side blank, which is how a
/// deletion with no matching insertion (or vice versa) is rendered.
pub fn split_pairs(rows: &[DiffRow]) -> Vec<(Option<&DiffRow>, Option<&DiffRow>)> {
    let mut pairs = Vec::new();
    let mut index = 0;

    while index < rows.len() {
        match rows[index].kind {
            LineKind::Context => {
                pairs.push((Some(&rows[index]), Some(&rows[index])));
                index += 1;
            }
            LineKind::Removed => {
                let removed_start = index;
                while index < rows.len() && rows[index].kind == LineKind::Removed {
                    index += 1;
                }
                let removed = &rows[removed_start..index];

                let added_start = index;
                while index < rows.len() && rows[index].kind == LineKind::Added {
                    index += 1;
                }
                let added = &rows[added_start..index];

                for offset in 0..removed.len().max(added.len()) {
                    pairs.push((removed.get(offset), added.get(offset)));
                }
            }
            LineKind::Added => {
                // An insertion with no deletion to sit beside.
                pairs.push((None, Some(&rows[index])));
                index += 1;
            }
        }
    }

    pairs
}

/// The placeholder text shown in place of a file GitHub sent no patch for.
pub fn placeholder_text(f: &PrFile) -> &'static str {
    match f.patch_state {
        PatchState::Binary => "Binary file not shown",
        PatchState::TooLarge => "Large diff not shown — Load diff",
        PatchState::Present => "",
    }
}

/// One control in a file's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderButton {
    /// Copy the file's path.
    CopyPath,
    /// Open the file in the browser.
    OpenInBrowser,
    /// Start a file-level comment.
    CommentOnFile,
    /// The viewed checkbox.
    Viewed,
}

/// The header controls a file offers.
///
/// Every file — binary and too-large included — offers `CommentOnFile`; for a
/// binary file it is the only way to comment at all (spec §7.5). The viewed
/// checkbox is keyed by the blob SHA (spec §10.3), so a file GitHub gave no SHA
/// for cannot be marked viewed.
pub fn header_buttons(f: &PrFile) -> Vec<HeaderButton> {
    let mut buttons = vec![
        HeaderButton::CopyPath,
        HeaderButton::OpenInBrowser,
        HeaderButton::CommentOnFile,
    ];
    if !f.blob_sha.is_empty() {
        buttons.push(HeaderButton::Viewed);
    }
    buttons
}

/// Resolve a line row to its parsed [`DiffRow`].
pub fn line_at(session: &PrSession, file: u32, hunk: u32, row: u32) -> Option<&DiffRow> {
    session
        .files
        .get(file as usize)?
        .diff
        .as_ref()?
        .hunks
        .get(hunk as usize)?
        .rows
        .get(row as usize)
}

/// The file whose header is pinned to the top of the viewport.
///
/// The file that owns the row at `scroll_y`, so [`render`] can keep its header
/// on screen while the file's lines scroll under it (the brief's sticky file
/// headers). `None` only when there is no row to own it.
pub fn sticky_file(session: &PrSession, scroll_y: f32) -> Option<u32> {
    // Only the start of the visible range is needed, and that depends only on
    // `scroll_y`; the viewport height does not change which row is on top.
    let start = session.row_model.visible_range(scroll_y, 0.0).start;
    let reference = session.row_model.rows.get(start)?;
    if let Some(file) = file_of(reference) {
        return Some(file);
    }
    // Thread, pending, and composer rows live inside their file's range, so
    // the header that stays pinned over them is that file's.
    session
        .row_model
        .file_ranges
        .iter()
        .position(|range| range.contains(&start))
        .map(|file| file as u32)
}

/// The file a row belongs to, when it belongs to one.
fn file_of(reference: &RowRef) -> Option<u32> {
    match reference {
        RowRef::FileHeader { file }
        | RowRef::Placeholder { file }
        | RowRef::Line { file, .. }
        | RowRef::Gap { file, .. } => Some(*file),
        RowRef::Thread { .. } | RowRef::Pending { .. } | RowRef::Composer => None,
    }
}

/// Draw the whole diff surface.
///
/// Only the rows the table actually paints are resolved and highlighted, so a
/// 50 000-line PR costs the same per frame as a small one.
pub fn render(
    ui: &mut egui::Ui,
    session: &mut PrSession,
    state: &mut DiffViewState,
    cache: &mut HighlightCache,
    drafts: &mut CommentDrafts,
    actions: &mut Vec<DiffAction>,
) {
    let style = RowStyle {
        font_id: egui::TextStyle::Monospace.resolve(ui.style()),
        plain: ui.visuals().text_color(),
        strong: ui.visuals().strong_text_color(),
        weak: ui.visuals().weak_text_color(),
    };

    render_toolbar(ui, state);

    if session.row_model.rows.is_empty() {
        render_status(ui, session);
        return;
    }

    let mode = state.mode;
    let wrap = state.wrap;
    let content = content_width(ui, mode);

    // Where a sticky header is pinned: the top of the table, as wide as its text
    // area (the scroll bar keeps its own column).
    let band_area = ui.available_rect_before_wrap();
    let band = egui::Rect::from_min_size(
        band_area.min,
        egui::vec2(
            band_area.width() - ui.spacing().scroll.allocated_width(),
            HEADER_H,
        ),
    );
    // The width a full-row widget (composer, thread) lays out at: the table
    // itself, since those rows overflow their gutter column unclipped.
    let full_width = band.width();

    let mut table = TableBuilder::new(ui)
        .id_salt("diff-view")
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));

    // A file picked in the tree: jump to its header this frame. `offset_of`
    // gives the pixel offset of the file's first row (spec §9.2).
    if let Some(file) = state.scroll_to.take() {
        if let Some(range) = session.row_model.file_ranges.get(file as usize) {
            table = table.vertical_scroll_offset(session.row_model.offset_of(range.start));
        }
    }

    // The gutters must not clip: a file header is drawn in the first one and
    // spans the row visually, the way a header does in GitHub's diff.
    table = match mode {
        DiffMode::Unified => table
            .column(Column::exact(GUTTER_W).clip(false))
            .column(Column::exact(GUTTER_W).clip(false))
            .column(Column::remainder().at_least(160.0)),
        DiffMode::Split => table
            .column(Column::exact(GUTTER_W).clip(false))
            .column(Column::remainder().at_least(100.0))
            .column(Column::exact(GUTTER_W).clip(false))
            .column(Column::remainder().at_least(100.0)),
    };

    let mut ctx = RowCtx {
        session,
        mode,
        wrap,
        content,
        full_width,
        style: &style,
    };
    // The heights are taken out first: the row closure reaches back into the
    // session mutably to draw composer and thread rows.
    let heights: Vec<f32> = ctx.session.row_model.heights().to_vec();
    let scroll_area = table.body(|body| {
        body.heterogeneous_rows(heights.into_iter(), |mut row| {
            let index = row.index();
            let Some(reference) = ctx.session.row_model.rows.get(index).cloned() else {
                return;
            };

            render_row(&mut row, &reference, &mut ctx, cache, drafts, actions);
        });
    });

    // Pin the header of the file whose rows are at the top of the viewport, so a
    // long file's header stays on screen as its lines scroll under it. Painting
    // after the table draws it over the top row, exactly the way CSS
    // `position: sticky` overlaps the content behind it.
    let scroll_y = scroll_area.state.clamped_offset().y;
    if let Some(file) = sticky_file(ctx.session, scroll_y) {
        if let Some(loaded) = ctx.session.files.get(file as usize) {
            ui.painter()
                .rect_filled(band, 0.0, ui.visuals().faint_bg_color);
            ui.scope_builder(egui::UiBuilder::new().max_rect(band), |ui| {
                render_file_header(ui, file, loaded, ctx.session, &style, actions);
            });
        }
    }
}

/// The theme- and font-derived values every row needs.
struct RowStyle {
    font_id: egui::FontId,
    plain: egui::Color32,
    strong: egui::Color32,
    weak: egui::Color32,
}

/// The per-frame context a row is drawn with.
///
/// Bundled so [`render_row`] stays within clippy's argument budget and every
/// row is resolved against exactly the same session and layout.
struct RowCtx<'a> {
    session: &'a mut PrSession,
    mode: DiffMode,
    wrap: bool,
    /// The width line text wraps at, from [`content_width`].
    content: f32,
    /// The table's full width, for rows that span every column.
    full_width: f32,
    style: &'a RowStyle,
}

/// How many columns `mode` lays out.
fn column_count(mode: DiffMode) -> usize {
    match mode {
        DiffMode::Unified => 3,
        DiffMode::Split => 4,
    }
}

/// Render one row of the surface.
fn render_row(
    row: &mut egui_extras::TableRow<'_, '_>,
    reference: &RowRef,
    ctx: &mut RowCtx<'_>,
    cache: &mut HighlightCache,
    drafts: &mut CommentDrafts,
    actions: &mut Vec<DiffAction>,
) {
    let mode = ctx.mode;
    match reference {
        RowRef::FileHeader { file } => {
            let index = *file;
            row.col(|ui| {
                if let Some(loaded) = ctx.session.files.get(index as usize) {
                    render_file_header(ui, index, loaded, ctx.session, ctx.style, actions);
                }
            });
            blank_rest(row, mode);
        }
        RowRef::Placeholder { file } => {
            let index = *file;
            row.col(|ui| {
                if let Some(loaded) = ctx.session.files.get(index as usize) {
                    render_placeholder(ui, index, loaded, actions);
                }
            });
            blank_rest(row, mode);
        }
        RowRef::Gap { file, before_hunk } => {
            let (file, before_hunk) = (*file, *before_hunk);
            row.col(|ui| {
                if ui
                    .button("Expand context")
                    .on_hover_text("show the lines GitHub omitted here")
                    .clicked()
                {
                    actions.push(DiffAction::ExpandGap { file, before_hunk });
                }
            });
            blank_rest(row, mode);
        }
        RowRef::Line {
            file,
            hunk,
            row: row_index,
        } => {
            let Some(diff_row) = line_at(ctx.session, *file, *hunk, *row_index) else {
                blank_all(row, mode);
                return;
            };

            let text = display_text(&diff_row.text);
            let path = ctx
                .session
                .files
                .get(*file as usize)
                .map(|loaded| loaded.file.path.as_str())
                .unwrap_or_default();
            let syntax = syntax_for_path(path);
            let spans = cache.spans(&syntax, &text);

            render_line_cells(row, diff_row, &text, spans, ctx);
        }
        RowRef::Thread { thread } => {
            let index = *thread;
            row.col(|ui| {
                let Some(thread) = ctx.session.threads.get(index as usize) else {
                    return;
                };
                let vm = thread_view::thread_vm(thread);
                // Outdated and resolved threads arrive collapsed (spec §7.4);
                // the reviewer expands them from the summary line.
                let collapsed = (thread.is_outdated || thread.is_resolved)
                    && !drafts.expanded.contains(&thread.id);
                let mut thread_actions = Vec::new();
                thread_view::render_thread(ui, thread, &vm, collapsed, drafts, &mut thread_actions);
                actions.extend(thread_actions.into_iter().map(DiffAction::Thread));
            });
            blank_rest(row, mode);
        }
        RowRef::Pending { local_id } => {
            let id = *local_id;
            row.col(|ui| {
                let Some(pending) = ctx
                    .session
                    .pending
                    .comments
                    .iter()
                    .find(|p| p.local_id == id)
                else {
                    return;
                };
                egui::Frame::new()
                    .inner_margin(8.0)
                    .corner_radius(6.0)
                    .fill(ui.visuals().faint_bg_color)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Pending comment").small().weak());
                            if ui.button("Remove").clicked() {
                                actions.push(DiffAction::DeletePending { local_id: id });
                            }
                        });
                        markdown::render(ui, &markdown::to_doc(&pending.body));
                    });
            });
            blank_rest(row, mode);
        }
        RowRef::Composer => {
            let has_open_review = !ctx.session.pending.is_empty();
            let show_side = anchor_is_context(ctx.session, ctx.session.composer.target.as_ref());
            let width = ctx.full_width;
            row.col(|ui| {
                let mut composer_actions = Vec::new();
                comment_composer::render_composer(
                    ui,
                    &mut ctx.session.composer,
                    has_open_review,
                    show_side,
                    width,
                    &mut composer_actions,
                );
                actions.extend(composer_actions.into_iter().map(DiffAction::Composer));
            });
            blank_rest(row, mode);
        }
    }
}

/// Whether the composer's anchor sits on a context row, where the side
/// toggle is meaningful (spec §7.3): the row exists on both sides, and the
/// anchor is a single line.
pub fn anchor_is_context(session: &PrSession, target: Option<&CommentTarget>) -> bool {
    let Some(CommentTarget::Line(anchor)) = target else {
        return false;
    };
    if anchor.start_line.is_some() {
        return false;
    }
    let Some(loaded) = session.files.iter().find(|l| l.file.path == anchor.path) else {
        return false;
    };
    let Some(diff) = &loaded.diff else {
        return false;
    };
    diff.hunks
        .iter()
        .flat_map(|hunk| hunk.rows.iter())
        .any(|row| {
            let on_side = match anchor.side {
                Side::Right => row.new_no,
                Side::Left => row.old_no,
            } == Some(anchor.line);
            on_side && row.old_no.is_some() && row.new_no.is_some()
        })
}

/// Draw one line row, placing it in the columns `mode` expects.
fn render_line_cells(
    row: &mut egui_extras::TableRow<'_, '_>,
    diff_row: &DiffRow,
    text: &str,
    spans: &[HighlightSpan],
    ctx: &RowCtx<'_>,
) {
    let mode = ctx.mode;
    let style = ctx.style;
    match mode {
        DiffMode::Unified => {
            row.col(|ui| render_number(ui, diff_row.old_no, style));
            row.col(|ui| render_number(ui, diff_row.new_no, style));
            row.col(|ui| add_line(ui, text, spans, ctx));
        }
        DiffMode::Split => {
            // Removed lines belong to the left (old) side, added lines to the
            // right (new) side, and context lines to both.
            let on_left = diff_row.kind != LineKind::Added;
            let on_right = diff_row.kind != LineKind::Removed;

            row.col(|ui| {
                if on_left {
                    render_number(ui, diff_row.old_no, style);
                }
            });
            row.col(|ui| {
                if on_left {
                    add_line(ui, text, spans, ctx);
                }
            });
            row.col(|ui| {
                if on_right {
                    render_number(ui, diff_row.new_no, style);
                }
            });
            row.col(|ui| {
                if on_right {
                    add_line(ui, text, spans, ctx);
                }
            });
        }
    }
}

/// A gutter line number, or nothing when the row has no number on that side.
fn render_number(ui: &mut egui::Ui, number: Option<u32>, style: &RowStyle) {
    let text = number.map(|number| number.to_string()).unwrap_or_default();
    ui.label(
        egui::RichText::new(text)
            .font(style.font_id.clone())
            .color(style.weak),
    );
}

/// Add one line of highlighted text to a cell.
///
/// The line wraps at [`RowCtx::content`] — the same width the row's height was
/// measured at — so a wrapped line is never drawn taller than the row holding it.
fn add_line(ui: &mut egui::Ui, text: &str, spans: &[HighlightSpan], ctx: &RowCtx<'_>) {
    let mut job = layout_job(text, spans, ctx.style);
    if ctx.wrap {
        job.wrap.max_width = ctx.content;
        job.wrap.break_anywhere = true;
    }

    // Laying the job out ourselves (rather than handing the job to a `Label`)
    // keeps the wrap settings above: a `Label` would replace them with the
    // enclosing layout's.
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.add(egui::Label::new(galley).selectable(false));
}

/// Build the [`egui::text::LayoutJob`] for one line: plain text split by the
/// highlight spans' byte ranges.
fn layout_job(text: &str, spans: &[HighlightSpan], style: &RowStyle) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0;

    for span in spans {
        let start = span.range.start.min(text.len());
        let end = span.range.end.min(text.len());

        // `get` rather than indexing: a range that is not on a `char` boundary
        // is skipped instead of panicking.
        if start > cursor {
            if let Some(plain) = text.get(cursor..start) {
                push_section(&mut job, plain, style.plain, style, false, false);
            }
        }
        if end > start {
            if let Some(colored) = text.get(start..end) {
                let color = egui::Color32::from_rgb(span.color[0], span.color[1], span.color[2]);
                push_section(&mut job, colored, color, style, span.bold, span.italic);
            }
        }
        cursor = end.max(cursor);
    }

    if let Some(rest) = text.get(cursor..) {
        if !rest.is_empty() {
            push_section(&mut job, rest, style.plain, style, false, false);
        }
    }

    job
}

/// Append one styled section to a layout job.
///
/// egui has no bold font weight, so a bold span is drawn with the strong text
/// colour — the same approximation `RichText::strong` makes.
fn push_section(
    job: &mut egui::text::LayoutJob,
    text: &str,
    color: egui::Color32,
    style: &RowStyle,
    bold: bool,
    italic: bool,
) {
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: style.font_id.clone(),
            color: if bold { style.strong } else { color },
            italics: italic,
            ..Default::default()
        },
    );
}

/// The file header row: path, counts, and [`header_buttons`].
fn render_file_header(
    ui: &mut egui::Ui,
    file: u32,
    loaded: &LoadedFile,
    session: &PrSession,
    style: &RowStyle,
    actions: &mut Vec<DiffAction>,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(display_path(&loaded.file))
                .strong()
                .font(style.font_id.clone()),
        );
        ui.label(
            egui::RichText::new(format!(
                "+{} −{}",
                loaded.file.additions, loaded.file.deletions
            ))
            .color(style.weak),
        );

        for button in header_buttons(&loaded.file) {
            match button {
                HeaderButton::CopyPath => {
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(loaded.file.path.clone());
                    }
                }
                HeaderButton::OpenInBrowser => {
                    let url = session
                        .metadata
                        .as_ref()
                        .map(|metadata| format!("{}/files", metadata.html_url));
                    if let Some(url) = url {
                        if ui.button("Open in browser").clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                }
                HeaderButton::CommentOnFile => {
                    if ui.button("Comment on file").clicked() {
                        actions.push(DiffAction::StartComment(CommentTarget::File {
                            path: anchor_path(&loaded.file).to_owned(),
                        }));
                    }
                }
                HeaderButton::Viewed => {
                    // The real viewed state lands in Phase D; the checkbox
                    // reports the toggle as an action either way.
                    let mut viewed = false;
                    if ui.checkbox(&mut viewed, "Viewed").clicked() {
                        actions.push(DiffAction::ToggleViewed(file));
                    }
                }
            }
        }
    });
}

/// The placeholder that stands in for a file with no textual diff.
fn render_placeholder(
    ui: &mut egui::Ui,
    file: u32,
    loaded: &LoadedFile,
    actions: &mut Vec<DiffAction>,
) {
    ui.horizontal(|ui| {
        ui.label(placeholder_text(&loaded.file));

        if loaded.file.patch_state == PatchState::TooLarge && ui.button("Load diff").clicked() {
            actions.push(DiffAction::LoadLargeDiff(file));
        }

        // The header's "Comment on file" is the only route on binary files, so
        // the placeholder repeats it where the file's body would be.
        if ui.button("Comment on file").clicked() {
            actions.push(DiffAction::StartComment(CommentTarget::File {
                path: anchor_path(&loaded.file).to_owned(),
            }));
        }
    });
}

/// The unified/split and wrap controls above the surface.
fn render_toolbar(ui: &mut egui::Ui, state: &mut DiffViewState) {
    ui.horizontal(|ui| {
        if ui
            .selectable_label(state.mode == DiffMode::Unified, "Unified")
            .clicked()
        {
            state.mode = DiffMode::Unified;
        }
        if ui
            .selectable_label(state.mode == DiffMode::Split, "Split")
            .clicked()
        {
            state.mode = DiffMode::Split;
        }
        ui.checkbox(&mut state.wrap, "Wrap");
    });
}

/// What to show when there is no row to draw yet.
fn render_status(ui: &mut egui::Ui, session: &PrSession) {
    if session.phase == LoadPhase::Failed {
        let color = ui.visuals().error_fg_color;
        match &session.error {
            Some(error) => {
                ui.colored_label(color, error.to_string());
            }
            None => {
                ui.colored_label(color, "the diff could not be loaded");
            }
        }
        return;
    }

    ui.horizontal(|ui| {
        ui.spinner();
        ui.label("loading the diff…");
    });
}

/// Fill the columns after the first.
fn blank_rest(row: &mut egui_extras::TableRow<'_, '_>, mode: DiffMode) {
    for _ in 1..column_count(mode) {
        row.col(|_ui| {});
    }
}

/// Fill every column.
fn blank_all(row: &mut egui_extras::TableRow<'_, '_>, mode: DiffMode) {
    for _ in 0..column_count(mode) {
        row.col(|_ui| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::files::{PatchState, PrFile};
    use crate::test_support::{file, line_anchor, rows_from};

    #[test]
    fn split_pairs_align_changed_runs() {
        let rows = rows_from("@@ -1,3 +1,3 @@\n-a\n-b\n+a2\n+b2\n c\n");
        let pairs = split_pairs(&rows);
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            (
                pairs[0].0.unwrap().text.as_str(),
                pairs[0].1.unwrap().text.as_str()
            ),
            ("a", "a2")
        );
        assert_eq!(pairs[2].0.unwrap().text, "c");
    }

    #[test]
    fn uneven_runs_leave_blanks() {
        let rows = rows_from("@@ -1,1 +1,2 @@\n-a\n+a2\n+extra\n");
        let pairs = split_pairs(&rows);
        assert!(pairs[1].0.is_none() && pairs[1].1.unwrap().text == "extra");
    }

    #[test]
    fn wrapped_rows_measure_taller() {
        let long = "x".repeat(400);
        assert!(row_height(true, &long, 8.0, 600.0) > row_height(false, &long, 8.0, 600.0));
    }

    #[test]
    fn every_header_can_start_a_file_comment() {
        let png = PrFile {
            patch: None,
            patch_state: PatchState::Binary,
            changes: 0,
            ..file("assets/logo.png")
        };
        let text = file("src/a.rs");
        assert!(header_buttons(&png).contains(&HeaderButton::CommentOnFile));
        assert!(header_buttons(&text).contains(&HeaderButton::CommentOnFile));
    }

    /// A loaded session with two text files and both placeholder kinds.
    fn surface_session() -> PrSession {
        use crate::runtime::{Event, SessionId};
        use crate::test_support::{loaded, meta, repo};

        let (mut session, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        session.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });

        let mut binary = loaded("assets/logo.png", "@@ -1 +1 @@\n-a\n+b\n");
        binary.file.patch = None;
        binary.file.patch_state = PatchState::Binary;
        binary.diff = None;

        let mut huge = loaded("data/huge.json", "@@ -1 +1 @@\n-a\n+b\n");
        huge.file.patch = None;
        huge.file.patch_state = PatchState::TooLarge;
        huge.diff = None;

        session.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![
                // `old_start != 1` inserts a gap row before the hunk.
                loaded("src/a.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n"),
                loaded("src/b.rs", "@@ -1,2 +1,2 @@\n-c\n+d\n"),
                binary,
                huge,
            ],
            done: true,
        });
        session
    }

    /// The widget must paint a real session in every layout without panicking —
    /// and must actually resolve its rows, not draw an empty frame.
    ///
    /// A lay-out smoke test standing in for the phase's manual QA: a gap, a
    /// line run, both placeholder kinds, a second file, an anchored thread,
    /// a pending comment, and the open composer are drawn in both modes,
    /// wrapped and unwrapped.
    #[test]
    fn the_surface_paints_every_mode_without_panicking() {
        use crate::diff::highlight::SyntectHighlighter;
        use crate::model::diff::Side;
        use crate::model::thread::{ReviewThread, SubjectType};
        use crate::state::composer::CommentMode;
        use std::time::Instant;

        let mut session = surface_session();
        session.threads.push(ReviewThread {
            id: "PRRT_1".into(),
            is_resolved: false,
            is_outdated: false,
            path: "src/a.rs".into(),
            line: Some(4),
            start_line: None,
            side: Some(Side::Right),
            subject_type: SubjectType::Line,
            comments: vec![crate::model::thread::ThreadComment {
                database_id: 1,
                author: "octocat".into(),
                avatar_url: None,
                body: "why here?".into(),
                created_at: "2026-10-01T00:00:00Z".into(),
            }],
        });
        session.rebuild_rows();
        session.open_comment(CommentTarget::Line(line_anchor("src/b.rs", 1)));
        crate::state::composer::reduce(
            &mut session.composer,
            crate::state::composer::ComposerMsg::SetBody("hold for the review".into()),
        );
        session.submit_comment(CommentMode::Pending, "me", Instant::now());
        // And leave a file-level composer open, on a placeholder file.
        session.open_comment(CommentTarget::File {
            path: "assets/logo.png".into(),
        });
        assert!(session
            .row_model
            .rows
            .contains(&RowRef::Thread { thread: 0 }));
        assert!(session
            .row_model
            .rows
            .iter()
            .any(|r| matches!(r, RowRef::Pending { .. })));
        assert!(session.row_model.rows.contains(&RowRef::Composer));

        let mut cache = HighlightCache::new(16, Box::new(SyntectHighlighter::new()));
        for mode in [DiffMode::Unified, DiffMode::Split] {
            for wrap in [false, true] {
                let mut state = DiffViewState {
                    mode,
                    wrap,
                    scroll_to: None,
                };
                let mut drafts = CommentDrafts::default();
                let mut actions = Vec::new();
                egui::__run_test_ui(|ui| {
                    render(
                        ui,
                        &mut session,
                        &mut state,
                        &mut cache,
                        &mut drafts,
                        &mut actions,
                    );
                });
                assert!(
                    actions.is_empty(),
                    "painting without input emits no actions"
                );
            }
        }

        // Rows were drawn and highlighted: an empty frame would leave the cache
        // untouched.
        assert!(
            cache.misses() > 0,
            "the surface must resolve and highlight the rows it draws"
        );
    }

    #[test]
    fn the_side_toggle_shows_only_on_context_rows() {
        let session = surface_session();
        // "src/a.rs" is "@@ -4,2 +4,2 @@\n-a\n+b\n c\n": "-a" is old 4,
        // "+b" is new 4, " c" is old 5 / new 5.
        assert!(
            !anchor_is_context(
                &session,
                Some(&CommentTarget::Line(line_anchor("src/a.rs", 4)))
            ),
            "an added row has no left side to toggle to"
        );
        assert!(
            anchor_is_context(
                &session,
                Some(&CommentTarget::Line(line_anchor("src/a.rs", 5)))
            ),
            "a context row exists on both sides"
        );
        assert!(
            !anchor_is_context(&session, None),
            "a closed composer has no side"
        );
        assert!(
            !anchor_is_context(
                &session,
                Some(&CommentTarget::File {
                    path: "src/a.rs".into()
                })
            ),
            "a file target has no side"
        );
    }

    #[test]
    fn sticky_file_follows_the_top_row() {
        use crate::runtime::{Event, SessionId};
        use crate::test_support::{loaded, meta, repo};

        let (mut session, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        session.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        session.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![
                loaded("src/a.rs", "@@ -1,2 +1,2 @@\n-a\n+b\n"),
                loaded("src/b.rs", "@@ -1,2 +1,2 @@\n-c\n+d\n"),
            ],
            done: true,
        });

        // The first row is the first file's header, so that file is pinned.
        assert_eq!(sticky_file(&session, 0.0), Some(0));
        // Scrolling past the header pins the file whose lines are now on top.
        assert_eq!(sticky_file(&session, HEADER_H + 1.0), Some(0));

        // At the second file's header offset, the second file is pinned.
        let second = session.row_model.file_ranges[1].start;
        let second_y = session.row_model.offset_of(second);
        assert_eq!(sticky_file(&session, second_y), Some(1));
    }
}
