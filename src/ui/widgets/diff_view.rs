//! The virtualized diff surface (spec §8.4).
//!
//! One `egui_extras::TableBuilder` renders the **one selected file**: its
//! header row, its lines, a placeholder when GitHub sent no patch, and the
//! expandable gaps between hunks. Rows come from [`RowModel`], so the table
//! only ever touches the rows it actually draws — and highlighting is
//! computed for exactly those rows, through the bounded cache of
//! [`crate::diff::highlight`].
//!
//! Everything the widget decides is a pure function ([`row_height`],
//! [`split_pairs`], [`placeholder_text`], [`header_buttons`]) so it can be
//! tested without a window; [`render`] only turns those decisions into
//! widgets.

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::diff::annotate::rows_for_anchor;
use crate::diff::expand::Expand;
use crate::diff::highlight::{syntax_for_path, HighlightCache, HighlightSpan};
use crate::diff::image::{image_kind, ImageBlob, ImageKind, ImageMode};
use crate::diff::options::{find_matches, next_match, prev_match};
use crate::diff::row_model::{display_text, pair_indices, RowRef, HEADER_H, ROW_H};
use crate::diff::word::word_diff;
use crate::github::files::{anchor_path, display_path, LoadedFile, PatchState, PrFile};
use crate::model::comment::CommentTarget;
use crate::model::diff::{DiffRow, LineKind, Side};
use crate::state::composer::{anchor_warning, ComposerAction};
use crate::state::pr_detail::{LoadPhase, PrSession, GITHUB_FILE_CAP};
use crate::ui::theme::palette_of;
use crate::ui::widgets::comment_composer;
use crate::ui::widgets::image_diff;
use crate::ui::widgets::markdown;
use crate::ui::widgets::thread_view::{self, CommentDrafts, ThreadAction};

/// The width of one line-number gutter.
pub const GUTTER_W: f32 = 48.0;

/// The width of the sign column (`+`/`−`) in front of a line's text.
pub const SIGN_W: f32 = 14.0;

pub use crate::model::diff::DiffMode;

/// The diff surface's own UI state.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffViewState {
    /// Whether long lines wrap instead of running off the pane.
    pub wrap: bool,
    /// The find box's text (Task 48).
    pub find_query: String,
    /// The query [`Self::find_matches`] was computed for, so an edited query
    /// jumps to its first match instead of advancing past it.
    pub find_applied: String,
    /// Row indices whose text contains `find_query`, in display order.
    pub find_matches: Vec<usize>,
    /// The match currently selected, as a row index.
    pub find_current: Option<usize>,
    /// A row the surface should scroll to on the next frame — the selected
    /// find match, consumed by [`render`].
    pub scroll_to_row: Option<usize>,
    /// How image diffs are drawn (Task 49).
    pub image_mode: ImageMode,
    /// The swipe divider position, normalized 0..=1.
    pub image_swipe: f32,
    /// The onion skin's head opacity, 0..=1.
    pub image_opacity: f32,
}

impl Default for DiffViewState {
    fn default() -> Self {
        DiffViewState {
            wrap: false,
            find_query: String::new(),
            find_applied: String::new(),
            find_matches: Vec::new(),
            find_current: None,
            scroll_to_row: None,
            image_mode: ImageMode::default(),
            image_swipe: 0.5,
            image_opacity: 0.5,
        }
    }
}

impl DiffViewState {
    /// Forget the find results. Row indices change whenever the row model is
    /// rebuilt for an option change, so stale matches must not survive it
    /// (Task 48).
    pub fn clear_find(&mut self) {
        self.find_applied = String::new();
        self.find_matches = Vec::new();
        self.find_current = None;
        self.scroll_to_row = None;
    }
}

/// What the user asked of the diff.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffAction {
    /// Toggle one file's viewed state (Phase D).
    ToggleViewed(u32),
    /// Start a comment on a line or a file.
    StartComment(CommentTarget),
    /// Show the file at this index in `PrSession::files` (one file at a
    /// time: the header's prev/next controls report through this).
    SelectFile(u32),
    /// Switch between the unified and split layouts (the toolbar reports
    /// the click; the session owns the mode because switching rebuilds
    /// the rows).
    SetMode(DiffMode),
    /// Fetch and locally diff a too-large file (Phase G).
    LoadLargeDiff(u32),
    /// Flip the hide-whitespace-only-hunks option (Task 48).
    ToggleHideWhitespace,
    /// A raster image's (or rendered SVG's) placeholder row became visible
    /// and its blobs have not been asked for yet (Task 49).
    LoadImages(u32),
    /// Switch an SVG file between its text diff and the rendered viewer
    /// (Task 49).
    ToggleRendered(u32),
    /// Expand the omitted context before one hunk (Task 45).
    ExpandGap {
        /// The index of the file.
        file: u32,
        /// The hunk the gap precedes.
        before_hunk: u32,
        /// How much of the gap to reveal.
        how: Expand,
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
    pair_indices(rows)
        .into_iter()
        .map(|(left, right)| {
            (
                left.map(|index| &rows[index as usize]),
                right.map(|index| &rows[index as usize]),
            )
        })
        .collect()
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
    write_blocked: Option<&str>,
) {
    let style = RowStyle {
        font_id: egui::TextStyle::Monospace.resolve(ui.style()),
        plain: ui.visuals().text_color(),
        strong: ui.visuals().strong_text_color(),
        weak: ui.visuals().weak_text_color(),
    };

    render_toolbar(ui, session, state, actions);

    // Spec §7.5: GitHub's files API returns at most 3000 files per PR and
    // says nothing when it truncates; the session flags the cap and this
    // banner keeps the reason on screen while any of the diff shows.
    if session.files_truncated {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!(
                "GitHub only provides the diff for the first {} files — use \
                 'Open in browser' to see the rest.",
                GITHUB_FILE_CAP
            ),
        );
    }

    if session.row_model.rows.is_empty() {
        render_status(ui, session);
        return;
    }

    let mode = session.mode;
    let wrap = state.wrap;
    let content = content_width(ui, mode);

    // The table's own area: as wide as its text area (the scroll bar keeps
    // its own column).
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

    // A find match the toolbar just selected: jump to that exact row
    // (Task 48).
    if let Some(row) = state.scroll_to_row.take() {
        table = table.vertical_scroll_offset(session.row_model.offset_of(row));
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
        state,
        style: &style,
        write_blocked,
    };
    // The heights are taken out first: the row closure reaches back into the
    // session mutably to draw composer and thread rows.
    let heights: Vec<f32> = ctx.session.row_model.heights().to_vec();
    table.body(|body| {
        body.heterogeneous_rows(heights.into_iter(), |mut row| {
            let index = row.index();
            let Some(reference) = ctx.session.row_model.rows.get(index).cloned() else {
                return;
            };

            render_row(&mut row, &reference, &mut ctx, cache, drafts, actions);
        });
    });
}

/// The theme- and font-derived values every row needs.
pub(crate) struct RowStyle {
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
    /// The surface's own state, which the image viewer drives (Task 49).
    state: &'a mut DiffViewState,
    style: &'a RowStyle,
    /// The rate-limit tooltip for the composer's Submit buttons (spec §13):
    /// `Some` while writes are disabled.
    write_blocked: Option<&'a str>,
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
            let row_index = row.index();
            match image_slot(ctx.session, index) {
                ImageSlot::Plain => {
                    row.col(|ui| {
                        if let Some(loaded) = ctx.session.files.get(index as usize) {
                            render_placeholder(ui, index, loaded, actions);
                        }
                    });
                    blank_rest(row, mode);
                }
                ImageSlot::Request => {
                    // The row is on screen for the first time: ask once
                    // (Task 49's lazy loading).
                    actions.push(DiffAction::LoadImages(index));
                    row.col(|ui| {
                        if let Some(loaded) = ctx.session.files.get(index as usize) {
                            render_placeholder(ui, index, loaded, actions);
                        }
                        ui.spinner();
                    });
                    blank_rest(row, mode);
                }
                ImageSlot::Loading => {
                    row.col(|ui| {
                        ui.spinner();
                        ui.label("loading image…");
                    });
                    blank_rest(row, mode);
                }
                ImageSlot::Ready(base, head) => {
                    // Resolve everything the viewer needs up front so the
                    // session is free for the height update afterwards.
                    let Some(loaded) = ctx.session.files.get(index as usize) else {
                        blank_rest(row, mode);
                        return;
                    };
                    let pr_file = loaded.file.clone();
                    let session_id = ctx.session.id.0;
                    let browser_url = ctx
                        .session
                        .metadata
                        .as_ref()
                        .map(|metadata| format!("{}/files", metadata.html_url));
                    let full_width = ctx.full_width;
                    let state = &mut *ctx.state;
                    let mut height = 0.0;
                    row.col(|ui| {
                        height = image_diff::render(
                            ui,
                            session_id,
                            index,
                            &pr_file,
                            &base,
                            &head,
                            &mut state.image_mode,
                            &mut state.image_swipe,
                            &mut state.image_opacity,
                            browser_url.as_deref(),
                            full_width,
                        );
                    });
                    // The row grows to the laid-out images; the table
                    // picks the new height up next frame (Task 49).
                    let current = ctx.session.row_model.heights()[row_index];
                    if (current - height).abs() > 1.0 {
                        ctx.session.row_model.set_height(row_index, height);
                    }
                    blank_rest(row, mode);
                }
            }
        }
        RowRef::Gap { file, before_hunk } => {
            let (file, before_hunk) = (*file, *before_hunk);
            row.col(|ui| {
                ui.horizontal(|ui| {
                    for (label, how) in [
                        ("↑ 20", Expand::Up(20)),
                        ("↓ 20", Expand::Down(20)),
                        ("all", Expand::All),
                    ] {
                        if ui
                            .button(label)
                            .on_hover_text("show the lines GitHub omitted here")
                            .clicked()
                        {
                            actions.push(DiffAction::ExpandGap {
                                file,
                                before_hunk,
                                how,
                            });
                        }
                    }
                });
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

            // Unified word marks: the row's partner in the removed/added run
            // it belongs to, paired the same way split mode pairs them.
            let mark = unified_word_mark(ctx.session, *file, *hunk, *row_index);
            render_line_cells(row, diff_row, &text, spans, mark, ctx);
        }
        RowRef::Pair {
            file,
            hunk,
            left,
            right,
        } => {
            let left_row = left.and_then(|index| line_at(ctx.session, *file, *hunk, index));
            let right_row = right.and_then(|index| line_at(ctx.session, *file, *hunk, index));
            let left_text = left_row.map(|row| display_text(&row.text));
            let right_text = right_row.map(|row| display_text(&row.text));

            let path = ctx
                .session
                .files
                .get(*file as usize)
                .map(|loaded| loaded.file.path.as_str())
                .unwrap_or_default();
            let syntax = syntax_for_path(path);
            // The cache hands out one borrow at a time, so the left spans
            // are copied before the right side asks for its own.
            let left_spans: Vec<HighlightSpan> = match &left_text {
                Some(text) => cache.spans(&syntax, text).to_vec(),
                None => Vec::new(),
            };
            let right_spans: Vec<HighlightSpan> = match &right_text {
                Some(text) => cache.spans(&syntax, text).to_vec(),
                None => Vec::new(),
            };

            // The changed bytes, bounded per visible pair by
            // `MAX_DISPLAY_CHARS` inside `word_diff`'s inputs.
            let marks = match (&left_text, &right_text) {
                (Some(left), Some(right)) => word_diff(left, right),
                _ => None,
            };

            row.col(|ui| {
                number_cell(
                    ui,
                    left_row.and_then(|row| row.old_no),
                    cell_kind(left_row),
                    None,
                    ctx,
                )
            });
            row.col(|ui| {
                code_cell(
                    ui,
                    left_text.as_deref(),
                    &left_spans,
                    marks.as_ref().map(|(left, _)| left.clone()),
                    cell_kind(left_row),
                    ctx,
                )
            });
            row.col(|ui| {
                number_cell(
                    ui,
                    right_row.and_then(|row| row.new_no),
                    cell_kind(right_row),
                    Some(true),
                    ctx,
                )
            });
            row.col(|ui| {
                code_cell(
                    ui,
                    right_text.as_deref(),
                    &right_spans,
                    marks.as_ref().map(|(_, right)| right.clone()),
                    cell_kind(right_row),
                    ctx,
                )
            });
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
            let warning = composer_warning(ctx.session, ctx.session.composer.target.as_ref());
            let width = ctx.full_width;
            row.col(|ui| {
                let mut composer_actions = Vec::new();
                comment_composer::render_composer(
                    ui,
                    &mut ctx.session.composer,
                    comment_composer::ComposerInputs {
                        has_open_review,
                        show_side,
                        warning,
                        width,
                        write_blocked: ctx.write_blocked,
                    },
                    &mut composer_actions,
                );
                actions.extend(composer_actions.into_iter().map(DiffAction::Composer));
            });
            blank_rest(row, mode);
        }
    }
}

/// The anchor warning for the open composer (Task 47): the rows its anchor
/// covers, run through [`anchor_warning`]. A file target has no rows and so
/// never warns.
fn composer_warning(session: &PrSession, target: Option<&CommentTarget>) -> Option<&'static str> {
    let Some(CommentTarget::Line(anchor)) = target else {
        return None;
    };
    let rows = rows_for_anchor(&session.files, anchor);
    anchor_warning(&rows.iter().collect::<Vec<_>>())
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

/// The colour role of one side of a pair, or one unified line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CellKind {
    /// Present on both sides: no background.
    Context,
    /// Old side only: red.
    Removed,
    /// New side only: green.
    Added,
    /// The blank half of an uneven run: the flat filler.
    Missing,
}

/// The role a resolved side of a pair plays.
fn cell_kind(row: Option<&DiffRow>) -> CellKind {
    match row.map(|row| &row.kind) {
        None => CellKind::Missing,
        Some(LineKind::Context) => CellKind::Context,
        Some(LineKind::Removed) => CellKind::Removed,
        Some(LineKind::Added) => CellKind::Added,
    }
}

/// The word-diff mark for one unified line, if it has a partner: unified
/// pairs removed/added runs by index within the block, exactly the way
/// [`pair_indices`] does, and the partner is what `word_diff` runs against.
fn unified_word_mark(
    session: &PrSession,
    file: u32,
    hunk: u32,
    row_index: u32,
) -> Option<std::ops::Range<usize>> {
    let rows = session
        .files
        .get(file as usize)?
        .diff
        .as_ref()?
        .hunks
        .get(hunk as usize)?
        .rows
        .as_slice();
    let me = usize::try_from(row_index).ok()?;
    for (left, right) in pair_indices(rows) {
        let (left, right) = (left? as usize, right? as usize);
        if left == right || left == me || right == me {
            continue;
        }
        let (other_index, mark_side) = if left == me {
            (right, true)
        } else {
            (left, false)
        };
        let other = &rows[other_index];
        let mine = &rows[me];
        let mark = if mark_side {
            word_diff(&mine.text, &other.text).map(|(mark, _)| mark)
        } else {
            word_diff(&other.text, &mine.text).map(|(_, mark)| mark)
        }?;
        return Some(mark);
    }
    None
}

/// Draw one line row, placing it in the columns `mode` expects.
fn render_line_cells(
    row: &mut egui_extras::TableRow<'_, '_>,
    diff_row: &DiffRow,
    text: &str,
    spans: &[HighlightSpan],
    mark: Option<std::ops::Range<usize>>,
    ctx: &RowCtx<'_>,
) {
    let mode = ctx.mode;
    let kind = cell_kind(Some(diff_row));
    match mode {
        DiffMode::Unified => {
            row.col(|ui| number_cell(ui, diff_row.old_no, kind, None, ctx));
            row.col(|ui| number_cell(ui, diff_row.new_no, kind, None, ctx));
            row.col(|ui| code_cell(ui, Some(text), spans, mark, kind, ctx));
        }
        DiffMode::Split => {
            // Split rows are `RowRef::Pair`s; a `Line` row only reaches split
            // when the model was built in the other mode, one frame late.
            // Draw it on the side its kind belongs to.
            let on_left = diff_row.kind != LineKind::Added;
            let on_right = diff_row.kind != LineKind::Removed;
            row.col(|ui| {
                if on_left {
                    number_cell(ui, diff_row.old_no, kind, None, ctx);
                }
            });
            row.col(|ui| {
                if on_left {
                    code_cell(ui, Some(text), spans, mark.clone(), kind, ctx);
                }
            });
            row.col(|ui| {
                if on_right {
                    number_cell(ui, diff_row.new_no, kind, Some(true), ctx);
                }
            });
            row.col(|ui| {
                if on_right {
                    code_cell(ui, Some(text), spans, mark.clone(), kind, ctx);
                }
            });
        }
    }
}

/// One gutter cell: the line number over the kind's background.
fn number_cell(
    ui: &mut egui::Ui,
    number: Option<u32>,
    kind: CellKind,
    divider: Option<bool>,
    ctx: &RowCtx<'_>,
) {
    let palette = palette_of(ui);
    let bg = match kind {
        CellKind::Context => None,
        CellKind::Removed => Some(palette.del_gutter),
        CellKind::Added => Some(palette.add_gutter),
        CellKind::Missing => Some(palette.filler),
    };
    let rect = ui.max_rect();
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    if divider == Some(true) {
        ui.painter()
            .line_segment([rect.left_top(), rect.left_bottom()], (1.0, palette.line));
    }
    let text = number.map(|number| number.to_string()).unwrap_or_default();
    let color = match kind {
        CellKind::Missing => palette.faint,
        _ => style_weak(ctx),
    };
    ui.label(
        egui::RichText::new(text)
            .font(ctx.style.font_id.clone())
            .color(color),
    );
}

/// One code cell: background fill, sign glyph, and the highlighted text with
/// its word-diff mark. A missing side is just the filler.
fn code_cell(
    ui: &mut egui::Ui,
    text: Option<&str>,
    spans: &[HighlightSpan],
    mark: Option<std::ops::Range<usize>>,
    kind: CellKind,
    ctx: &RowCtx<'_>,
) {
    let palette = palette_of(ui);
    let bg = match kind {
        CellKind::Context => None,
        CellKind::Removed => Some(palette.del_bg),
        CellKind::Added => Some(palette.add_bg),
        CellKind::Missing => Some(palette.filler),
    };
    let rect = ui.max_rect();
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    let Some(text) = text else {
        return;
    };

    let (sign, sign_color) = match kind {
        CellKind::Removed => ("−", palette.bad),
        CellKind::Added => ("+", palette.ok),
        _ => (" ", style_weak(ctx)),
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_sized(
            [SIGN_W, 0.0],
            egui::Label::new(
                egui::RichText::new(sign)
                    .font(ctx.style.font_id.clone())
                    .color(sign_color),
            )
            .selectable(false),
        );
        let mark_bg = match kind {
            CellKind::Added => palette.add_word,
            _ => palette.del_word,
        };
        add_line_marked(ui, text, spans, mark, mark_bg, ctx);
    });
}

/// The weak text colour, for gutters and context signs.
fn style_weak(ctx: &RowCtx<'_>) -> egui::Color32 {
    ctx.style.weak
}

/// Add one line of highlighted text to a cell.
///
/// The line wraps at [`RowCtx::content`] — the same width the row's height was
/// measured at — so a wrapped line is never drawn taller than the row holding it.
fn add_line_marked(
    ui: &mut egui::Ui,
    text: &str,
    spans: &[HighlightSpan],
    mark: Option<std::ops::Range<usize>>,
    mark_bg: egui::Color32,
    ctx: &RowCtx<'_>,
) {
    let mut job = layout_job(text, spans, mark, mark_bg, ctx.style);
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
/// highlight spans' and the word-diff mark's byte ranges.
///
/// Sections are cut at every span and mark boundary, so exactly the bytes
/// inside `mark` carry `mark_bg` as their background — the mockup's strong
/// word highlight — while the rest keeps the syntax colours.
pub(crate) fn layout_job(
    text: &str,
    spans: &[HighlightSpan],
    mark: Option<std::ops::Range<usize>>,
    mark_bg: egui::Color32,
    style: &RowStyle,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();

    // Every boundary that must start a new section, clamped and deduplicated.
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for span in spans {
        cuts.push(span.range.start.min(text.len()));
        cuts.push(span.range.end.min(text.len()));
    }
    if let Some(mark) = &mark {
        cuts.push(mark.start.min(text.len()));
        cuts.push(mark.end.min(text.len()));
    }
    cuts.sort_unstable();
    cuts.dedup();

    for window in cuts.windows(2) {
        let (start, end) = (window[0], window[1]);
        if start >= end {
            continue;
        }
        // `get` rather than indexing: a range that is not on a `char`
        // boundary is skipped instead of panicking.
        let Some(piece) = text.get(start..end) else {
            continue;
        };
        let color = spans
            .iter()
            .find(|span| span.range.start <= start && end <= span.range.end)
            .map(|span| egui::Color32::from_rgb(span.color[0], span.color[1], span.color[2]))
            .unwrap_or(style.plain);
        let bold = spans
            .iter()
            .any(|span| span.bold && span.range.start <= start && end <= span.range.end);
        let italic = spans
            .iter()
            .any(|span| span.italic && span.range.start <= start && end <= span.range.end);
        let marked = mark
            .as_ref()
            .is_some_and(|mark| mark.start <= start && end <= mark.end);

        let mut format = egui::TextFormat {
            font_id: style.font_id.clone(),
            color: if bold { style.strong } else { color },
            italics: italic,
            ..Default::default()
        };
        if marked {
            format.background = mark_bg;
        }
        job.append(piece, 0.0, format);
    }

    job
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

        // An SVG has a text patch, so the reviewer picks what the body
        // shows: the diff lines or the rendered viewer (Task 49).
        if image_kind(&loaded.file.path) == Some(ImageKind::Svg) {
            let rendered = session.rendered_svg.contains(&file);
            if ui.selectable_label(!rendered, "Text").clicked() && rendered {
                actions.push(DiffAction::ToggleRendered(file));
            }
            if ui.selectable_label(rendered, "Rendered").clicked() && !rendered {
                actions.push(DiffAction::ToggleRendered(file));
            }
        }
    });
}

/// What a placeholder row should draw for an image file (Task 49).
enum ImageSlot {
    /// Not an image viewer: the plain placeholder.
    Plain,
    /// The viewer belongs here and the blobs have not been asked for yet.
    Request,
    /// The blobs are in flight.
    Loading,
    /// The blobs are cached: draw the viewer.
    Ready(ImageBlob, ImageBlob),
}

/// Decide what this file's placeholder row shows, from the session's image
/// state (Task 49).
fn image_slot(session: &PrSession, file: u32) -> ImageSlot {
    if let Some(entry) = session.image_for(file) {
        return ImageSlot::Ready(entry.base.clone(), entry.head.clone());
    }
    if session.wants_images(file) {
        ImageSlot::Request
    } else if is_image_view(session, file) {
        ImageSlot::Loading
    } else {
        ImageSlot::Plain
    }
}

/// Whether this file's placeholder row is an image viewer rather than the
/// plain placeholder text: a binary raster, or an SVG switched to rendered.
fn is_image_view(session: &PrSession, file: u32) -> bool {
    let Some(loaded) = session.files.get(file as usize) else {
        return false;
    };
    match image_kind(&loaded.file.path) {
        Some(ImageKind::Raster) => loaded.file.patch_state == PatchState::Binary,
        Some(ImageKind::Svg) => session.rendered_svg.contains(&file),
        None => false,
    }
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

/// The unified/split, wrap, hide-whitespace, and find controls above the
/// surface (Task 48).
fn render_toolbar(
    ui: &mut egui::Ui,
    session: &PrSession,
    state: &mut DiffViewState,
    actions: &mut Vec<DiffAction>,
) {
    ui.horizontal(|ui| {
        if ui
            .selectable_label(session.mode == DiffMode::Unified, "Unified")
            .clicked()
        {
            actions.push(DiffAction::SetMode(DiffMode::Unified));
        }
        if ui
            .selectable_label(session.mode == DiffMode::Split, "Split")
            .clicked()
        {
            actions.push(DiffAction::SetMode(DiffMode::Split));
        }
        ui.checkbox(&mut state.wrap, "Wrap");

        // The option lives on the session because hiding rebuilds the rows;
        // the checkbox only reports the click.
        let mut hide = session.hide_whitespace;
        if ui
            .checkbox(&mut hide, "Hide whitespace")
            .on_hover_text("hide hunks whose changes are only whitespace")
            .clicked()
        {
            actions.push(DiffAction::ToggleHideWhitespace);
        }

        ui.separator();
        let find = egui::TextEdit::singleline(&mut state.find_query)
            .hint_text("Find in diff")
            .desired_width(160.0);
        let response = ui.add(find);
        let returned = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if returned {
            let backwards = ui.input(|i| i.modifiers.shift);
            search_diff(session, state, backwards);
        }
        if !state.find_query.is_empty() && state.find_applied == state.find_query {
            if state.find_matches.is_empty() {
                ui.weak("no matches");
            } else if let Some(current) = state.find_current {
                let position = state
                    .find_matches
                    .iter()
                    .position(|m| *m == current)
                    .unwrap_or(0);
                ui.weak(format!("{} of {}", position + 1, state.find_matches.len()));
            }
        }
    });
}

/// Re-run the find over the diff's line rows and select the next (or, with
/// `backwards`, previous) match, scrolling the surface to it (Task 48).
///
/// An edited query starts at its first (or last) match; repeating Enter on
/// the same query walks the list, wrapping.
pub fn search_diff(session: &PrSession, state: &mut DiffViewState, backwards: bool) {
    let changed = state.find_applied != state.find_query;
    state.find_applied = state.find_query.clone();
    state.find_matches = find_matches(line_rows(session).into_iter(), &state.find_query);
    state.find_current = if changed {
        if backwards {
            state.find_matches.last().copied()
        } else {
            state.find_matches.first().copied()
        }
    } else if backwards {
        prev_match(&state.find_matches, state.find_current)
    } else {
        next_match(&state.find_matches, state.find_current)
    };
    state.scroll_to_row = state.find_current;
}

/// Every line row of the session's row model as `(row_index, text)` pairs in
/// display order — the haystack [`find_matches`] searches (Task 48).
pub fn line_rows(session: &PrSession) -> Vec<(usize, &str)> {
    session
        .row_model
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, reference)| match reference {
            RowRef::Line { file, hunk, row } => {
                line_at(session, *file, *hunk, *row).map(|line| (index, line.text.as_str()))
            }
            _ => None,
        })
        .collect()
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

    if session.phase == LoadPhase::Ready {
        // One file at a time: with nothing selected there are no rows, and
        // the reason belongs on screen.
        if session.files.is_empty() {
            ui.label("This pull request has no files.");
        } else {
            ui.label("No files match the current filters.");
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
    fn layout_job_marks_exactly_the_changed_bytes() {
        let style = test_row_style();
        let job = layout_job(
            "import { apps, content }",
            &[],
            Some(9..15),
            crate::ui::theme::DARK.add_word,
            &style,
        );
        let marked: String = job
            .sections
            .iter()
            .filter(|s| s.format.background == crate::ui::theme::DARK.add_word)
            .map(|s| {
                let range = s.byte_range.start.0..s.byte_range.end.0;
                &job.text[range]
            })
            .collect();
        assert_eq!(marked, "apps, ");
    }

    /// A [`RowStyle`] built the way [`render`] builds one, from the default
    /// style — enough for the pure layout functions to be tested.
    fn test_row_style() -> RowStyle {
        let style = egui::Style::default();
        RowStyle {
            font_id: egui::TextStyle::Monospace.resolve(&style),
            plain: style.visuals.text_color(),
            strong: style.visuals.strong_text_color(),
            weak: style.visuals.weak_text_color(),
        }
    }

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
        // One file at a time: every row this test exercises lives on
        // `src/a.rs`, which is the file the surface shows.
        session.select_file(0);
        session.open_comment(CommentTarget::Line(line_anchor("src/a.rs", 1)));
        crate::state::composer::reduce(
            &mut session.composer,
            crate::state::composer::ComposerMsg::SetBody("hold for the review".into()),
        );
        session.submit_comment(CommentMode::Pending, "me", Instant::now());
        // And leave a file-level composer open on the same file.
        session.open_comment(CommentTarget::File {
            path: "src/a.rs".into(),
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

        let mut cache = HighlightCache::new(16, Box::new(SyntectHighlighter::new(true)));
        // Every file in turn: one file at a time, so the loop is what puts
        // the placeholders and the second file on screen.
        for file in 0..session.files.len() {
            session.select_file(file);
            for mode in [DiffMode::Unified, DiffMode::Split] {
                session.set_mode(mode);
                for wrap in [false, true] {
                    let mut state = DiffViewState {
                        wrap,
                        ..Default::default()
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
                            None,
                        );
                    });
                    assert!(
                        actions
                            .iter()
                            .all(|action| matches!(action, DiffAction::LoadImages(_))),
                        "painting without input emits nothing but lazy image loads"
                    );
                }
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
    fn find_walks_the_line_rows_and_scrolls() {
        use crate::runtime::{Event, SessionId};
        use crate::test_support::{loaded, meta, repo};

        // One file, two matching lines: the find walks the rows of the file
        // the surface shows.
        let (mut session, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        session.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        session.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("src/a.rs", "@@ -1,3 +1,3 @@\n-a\n+c\n+c2\n")],
            done: true,
        });
        let session = session;
        let mut state = DiffViewState {
            find_query: "c".into(),
            ..Default::default()
        };
        search_diff(&session, &mut state, false);
        assert!(!state.find_matches.is_empty());
        assert_eq!(state.find_current, state.find_matches.first().copied());
        assert_eq!(
            state.scroll_to_row, state.find_current,
            "the match is scrolled to"
        );
        let first = state.find_current;
        search_diff(&session, &mut state, false);
        assert_ne!(
            state.find_current, first,
            "Enter on the same query advances"
        );
        search_diff(&session, &mut state, true);
        assert_eq!(state.find_current, first, "Shift+Enter goes back");
        state.find_query = "zzz-nothing".into();
        search_diff(&session, &mut state, false);
        assert!(state.find_matches.is_empty());
        assert_eq!(state.find_current, None);
    }
}
