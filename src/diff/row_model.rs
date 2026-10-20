//! The flat, index-based row model the diff view virtualizes (spec §8.4).
//!
//! The model is built from the loaded files and handed to the UI thread once;
//! rows reference their file, hunk, and row **by index**, so no path or line
//! text is ever cloned into the row list. Heights live beside the rows (a
//! header, a wrapped line, and — later — a thread row are all different) with
//! a prefix-sum array, so scroll-to-file and "which rows are visible" stay
//! cheap on a 50 000-line PR.

use std::borrow::Cow;
use std::ops::Range;

use crate::github::files::{FileStatus, LoadedFile};
use crate::model::diff::{DiffMode, DiffRow, LineKind};

/// The default height of a file header row.
pub const HEADER_H: f32 = 30.0;

/// The default height of an unwrapped diff line row.
pub const ROW_H: f32 = 20.0;

/// The height of a hunk's header bar (redesign Task 8).
pub const HUNK_H: f32 = 28.0;

/// A line's displayed text is truncated at this many characters (spec §8.4).
pub const MAX_DISPLAY_CHARS: usize = 10_000;

/// Appended to a line whose text was truncated for display.
const TRUNCATION_MARKER: &str = " … (line truncated)";

/// One row of the virtualized diff surface.
///
/// Every variant carries indices, never data: the renderer resolves them
/// against the session's `LoadedFile`s and their parsed hunks.
#[derive(Clone, Debug, PartialEq)]
pub enum RowRef {
    /// A file's header row.
    FileHeader {
        /// The index of the file in `PrSession::files`.
        file: u32,
    },
    /// The placeholder for a binary or too-large file.
    Placeholder {
        /// The index of the file in `PrSession::files`.
        file: u32,
    },
    /// One line of one hunk.
    Line {
        /// The index of the file in `PrSession::files`.
        file: u32,
        /// The index of the hunk in the file's diff.
        hunk: u32,
        /// The index of the row in the hunk.
        row: u32,
    },
    /// One table row of split mode: the old-side row and the new-side row
    /// that sit beside each other. Either side can be absent — the blank
    /// half of an uneven removed/added run.
    Pair {
        /// The index of the file in `PrSession::files`.
        file: u32,
        /// The index of the hunk in the file's diff.
        hunk: u32,
        /// The index of the old-side row in the hunk, when it has one.
        left: Option<u32>,
        /// The index of the new-side row in the hunk, when it has one.
        right: Option<u32>,
    },
    /// A hunk's header bar (redesign Task 8): the `@@` line, plus the
    /// context-expansion buttons for the gap above the hunk when there is
    /// one — the mockup's single `.hunk` bar.
    HunkHeader {
        /// The index of the file in `PrSession::files`.
        file: u32,
        /// The index of the hunk in the file's diff.
        hunk: u32,
    },
    /// An existing comment thread, inserted by Task 37.
    Thread {
        /// The index of the thread in the session's thread list.
        thread: u32,
    },
    /// A pending local comment, inserted by Task 37.
    Pending {
        /// The pending comment's local id.
        local_id: u64,
    },
    /// The inline comment composer, inserted by Task 37.
    Composer,
}

/// The whole PR as one flat, virtualizable list of rows.
pub struct RowModel {
    /// Every row of the surface, in display order.
    pub rows: Vec<RowRef>,
    /// The row range of each file, indexed like `PrSession::files`.
    pub file_ranges: Vec<Range<usize>>,
    /// The height of each row, parallel to `rows`.
    heights: Vec<f32>,
    /// `prefix[i]` is the total height of `rows[..i]`; one longer than `rows`.
    prefix: Vec<f32>,
}

impl RowModel {
    /// An empty model: no rows, no ranges, the one prefix sum.
    pub(crate) fn empty() -> RowModel {
        RowModel {
            rows: Vec::new(),
            file_ranges: Vec::new(),
            heights: Vec::new(),
            prefix: vec![0.0],
        }
    }

    /// The per-row heights, parallel to [`RowModel::rows`].
    pub fn heights(&self) -> &[f32] {
        &self.heights
    }

    /// The first surface row that shows `line` on `side` (Task 12: jumping to
    /// a pending comment). `files` resolves the row references; a line no row
    /// carries (collapsed away, or never in the diff) is `None`.
    pub fn row_of_line(
        &self,
        files: &[LoadedFile],
        side: crate::model::diff::Side,
        line: u32,
    ) -> Option<usize> {
        let carries = |file: u32, hunk: u32, row: u32| -> bool {
            files
                .get(file as usize)
                .and_then(|loaded| loaded.diff.as_ref())
                .and_then(|diff| diff.hunks.get(hunk as usize))
                .and_then(|hunk| hunk.rows.get(row as usize))
                .and_then(|diff_row| match side {
                    crate::model::diff::Side::Left => diff_row.old_no,
                    crate::model::diff::Side::Right => diff_row.new_no,
                })
                == Some(line)
        };
        self.rows.iter().enumerate().find_map(|(index, row)| {
            let holds = match row {
                RowRef::Line { file, hunk, row } => carries(*file, *hunk, *row),
                RowRef::Pair {
                    file,
                    hunk,
                    left,
                    right,
                } => match side {
                    crate::model::diff::Side::Left => {
                        left.is_some_and(|row| carries(*file, *hunk, row))
                    }
                    crate::model::diff::Side::Right => {
                        right.is_some_and(|row| carries(*file, *hunk, row))
                    }
                },
                _ => false,
            };
            holds.then_some(index)
        })
    }

    /// Record a measured height for one row.
    ///
    /// Called when word wrap is on and the pane width changes; out-of-range
    /// indices are ignored so a stale measurement can never panic a frame.
    pub fn set_height(&mut self, idx: usize, h: f32) {
        let Some(current) = self.heights.get_mut(idx) else {
            return;
        };
        let delta = h - *current;
        *current = h;
        for total in &mut self.prefix[idx + 1..] {
            *total += delta;
        }
    }

    /// The y offset of row `idx`: the total height of every row before it.
    pub fn offset_of(&self, idx: usize) -> f32 {
        self.prefix
            .get(idx)
            .copied()
            .unwrap_or_else(|| *self.prefix.last().unwrap_or(&0.0))
    }

    /// The rows overlapping `scroll_y .. scroll_y + viewport_h`.
    ///
    /// Binary search over the prefix sums (spec §8.4), so the range costs
    /// `O(log n)` no matter how many rows the PR has.
    pub fn visible_range(&self, scroll_y: f32, viewport_h: f32) -> Range<usize> {
        let start = self
            .prefix
            .partition_point(|top| *top <= scroll_y)
            .saturating_sub(1);
        let bottom = scroll_y + viewport_h.max(0.0);
        let end = self
            .prefix
            .partition_point(|top| *top < bottom)
            .max(start + 1)
            .min(self.rows.len());
        start..end
    }

    /// Append one row with its default height, extending the prefix sums.
    pub(crate) fn push(&mut self, row: RowRef, height: f32) {
        self.rows.push(row);
        self.heights.push(height);
        let total = self.prefix.last().copied().unwrap_or(0.0) + height;
        self.prefix.push(total);
    }
}

/// Build the row list for `files`, in unified layout.
///
/// Per file: a header, then either the placeholder (GitHub sent no diff) or,
/// per hunk, a [`RowRef::HunkHeader`] bar followed by the hunk's lines.
pub fn build(files: &[LoadedFile]) -> RowModel {
    let mut model = RowModel {
        rows: Vec::new(),
        file_ranges: Vec::new(),
        heights: Vec::new(),
        prefix: vec![0.0],
    };

    for (file_idx, loaded) in files.iter().enumerate() {
        let start = model.rows.len();
        push_file(&mut model, file_idx as u32, loaded, DiffMode::Unified);
        model.file_ranges.push(start..model.rows.len());
    }

    model
}

/// Build the row list for the **one** file the diff shows (redesign spec:
/// one file at a time).
///
/// Rows are emitted only for `selected`; [`RowModel::file_ranges`] stays
/// indexed by file — empty for every other file — so row references and
/// per-file lookups keep working unchanged. `None`, or an index past the
/// end of `files`, produces a model with no rows. `mode` decides whether
/// the hunk lines become [`RowRef::Line`] rows or paired
/// [`RowRef::Pair`] rows.
pub fn build_one(files: &[LoadedFile], selected: Option<usize>, mode: DiffMode) -> RowModel {
    let mut model = RowModel {
        rows: Vec::new(),
        file_ranges: vec![0..0; files.len()],
        heights: Vec::new(),
        prefix: vec![0.0],
    };

    if let Some(file_idx) = selected.filter(|i| *i < files.len()) {
        let start = model.rows.len();
        push_file(&mut model, file_idx as u32, &files[file_idx], mode);
        // The file header bar above the table names the file; its row stays
        // only as the annotate anchor, so it takes no height.
        model.set_height(start, 0.0);
        model.file_ranges[file_idx] = start..model.rows.len();
    }

    model
}

/// Append one file's rows: header, then placeholder or hunk bars and lines.
fn push_file(model: &mut RowModel, file: u32, loaded: &LoadedFile, mode: DiffMode) {
    model.push(RowRef::FileHeader { file }, HEADER_H);
    match &loaded.diff {
        None => model.push(RowRef::Placeholder { file }, ROW_H),
        Some(diff) => {
            for (hunk_idx, hunk) in diff.hunks.iter().enumerate() {
                let hunk_idx = hunk_idx as u32;
                model.push(
                    RowRef::HunkHeader {
                        file,
                        hunk: hunk_idx,
                    },
                    HUNK_H,
                );
                match mode {
                    DiffMode::Unified => {
                        for row_idx in 0..hunk.rows.len() {
                            model.push(
                                RowRef::Line {
                                    file,
                                    hunk: hunk_idx,
                                    row: row_idx as u32,
                                },
                                ROW_H,
                            );
                        }
                    }
                    DiffMode::Split => {
                        for (left, right) in pair_indices(&hunk.rows) {
                            model.push(
                                RowRef::Pair {
                                    file,
                                    hunk: hunk_idx,
                                    left,
                                    right,
                                },
                                ROW_H,
                            );
                        }
                    }
                }
            }
            // The tail bar: expands the rest of the file after the last
            // hunk. An added or removed file has no rest to show.
            let whole_file = matches!(loaded.file.status, FileStatus::Added | FileStatus::Removed);
            if !diff.hunks.is_empty() && !whole_file {
                model.push(
                    RowRef::HunkHeader {
                        file,
                        hunk: diff.hunks.len() as u32,
                    },
                    HUNK_H,
                );
            }
        }
    }
}

/// Pair a hunk's rows the way split mode draws them: each run of removed
/// rows followed by a run of added rows is zipped, the shorter side padded
/// with `None`; context rows pair with themselves.
///
/// The index form of the same rule [`split_pairs`](crate::ui::widgets::diff_view::split_pairs)
/// applies to row references.
pub fn pair_indices(rows: &[DiffRow]) -> Vec<(Option<u32>, Option<u32>)> {
    let mut pairs = Vec::new();
    let mut index = 0usize;

    while index < rows.len() {
        match rows[index].kind {
            LineKind::Context => {
                pairs.push((Some(index as u32), Some(index as u32)));
                index += 1;
            }
            LineKind::Removed => {
                let removed_start = index;
                while index < rows.len() && rows[index].kind == LineKind::Removed {
                    index += 1;
                }
                let added_start = index;
                while index < rows.len() && rows[index].kind == LineKind::Added {
                    index += 1;
                }
                let removed_len = added_start - removed_start;
                let added_len = index - added_start;
                for offset in 0..removed_len.max(added_len) {
                    pairs.push((
                        (offset < removed_len).then(|| (removed_start + offset) as u32),
                        (offset < added_len).then(|| (added_start + offset) as u32),
                    ));
                }
            }
            LineKind::Added => {
                // An insertion with no deletion to sit beside.
                pairs.push((None, Some(index as u32)));
                index += 1;
            }
        }
    }

    pairs
}

/// The text to draw for one line: `text`, truncated at
/// [`MAX_DISPLAY_CHARS`] characters on a `char` boundary (spec §8.4).
///
/// The borrowed fast path means the common case clones nothing; a minified
/// bundle still cannot force a giant text layout.
pub fn display_text(text: &str) -> Cow<'_, str> {
    // `len()` is in bytes and is therefore an upper bound on the character
    // count, so this early return can only skip the counting, never the check.
    if text.len() <= MAX_DISPLAY_CHARS {
        return Cow::Borrowed(text);
    }

    let mut truncated = String::with_capacity(MAX_DISPLAY_CHARS * 4 + TRUNCATION_MARKER.len());
    for character in text.chars().take(MAX_DISPLAY_CHARS) {
        truncated.push(character);
    }
    truncated.push_str(TRUNCATION_MARKER);
    Cow::Owned(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::diff::Side;
    use crate::test_support::loaded;

    #[test]
    fn row_of_line_finds_pairs_and_lines() {
        let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-a\n+b\n c\n")];
        // Rows: 0 FileHeader, 1 HunkHeader, 2 Pair(-a,+b), 3 Pair(c,c).
        let split = build_one(&files, Some(0), DiffMode::Split);
        assert_eq!(split.row_of_line(&files, Side::Right, 2), Some(3));
        assert_eq!(split.row_of_line(&files, Side::Left, 1), Some(2));
        assert_eq!(split.row_of_line(&files, Side::Right, 99), None);
        // Unified: 2 is `-a`, 3 is `+b`, 4 is ` c`.
        let unified = build_one(&files, Some(0), DiffMode::Unified);
        assert_eq!(unified.row_of_line(&files, Side::Right, 2), Some(4));
        assert_eq!(unified.row_of_line(&files, Side::Left, 1), Some(2));
    }

    #[test]
    fn build_one_holds_only_the_selected_file() {
        let files = vec![
            loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n"),
            loaded("b.rs", "@@ -1 +1 @@\n-a\n+b\n"),
        ];
        let m = build_one(&files, Some(1), crate::model::diff::DiffMode::Unified);
        assert_eq!(m.file_ranges.len(), 2);
        assert!(m.file_ranges[0].is_empty());
        assert_eq!(
            m.rows
                .iter()
                .filter(|r| matches!(r, RowRef::FileHeader { .. }))
                .count(),
            1
        );
        assert!(m.rows[m.file_ranges[1].clone()].iter().all(|r| match r {
            RowRef::FileHeader { file } | RowRef::Line { file, .. } => *file == 1,
            RowRef::HunkHeader { file, .. } => *file == 1,
            other => panic!("unexpected row {other:?}"),
        }));
    }

    #[test]
    fn build_one_without_a_selection_is_empty() {
        let files = vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")];
        let m = build_one(&files, None, crate::model::diff::DiffMode::Unified);
        assert!(m.rows.is_empty());
        assert_eq!(m.file_ranges, vec![0..0]);
    }

    #[test]
    fn split_mode_pairs_removed_and_added_runs() {
        use crate::model::diff::DiffMode;
        let files = vec![loaded("a.rs", "@@ -1,4 +1,4 @@\n c\n-a\n-b\n+x\n d\n")];
        let m = build_one(&files, Some(0), DiffMode::Split);
        let pairs: Vec<_> = m
            .rows
            .iter()
            .filter_map(|r| match r {
                RowRef::Pair { left, right, .. } => Some((*left, *right)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(3)),
                (Some(2), None),
                (Some(4), Some(4))
            ]
        );
        assert!(build_one(&files, Some(0), DiffMode::Unified)
            .rows
            .iter()
            .all(|r| !matches!(r, RowRef::Pair { .. })));
    }

    #[test]
    fn builds_ranges_placeholders_and_hunk_headers() {
        let mut bin = loaded("logo.png", "@@ -1 +1 @@\n-a\n+b\n");
        bin.diff = None;
        let m = build(&[loaded("src/a.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n"), bin]);
        assert_eq!(
            m.rows[..2],
            [
                RowRef::FileHeader { file: 0 },
                RowRef::HunkHeader { file: 0, hunk: 0 }
            ]
        );
        assert_eq!(m.file_ranges[0], 0..6); // header + hunk bar + 3 lines + tail bar
        assert_eq!(m.rows[5], RowRef::HunkHeader { file: 0, hunk: 1 });
        assert_eq!(
            m.rows[m.file_ranges[1].start + 1],
            RowRef::Placeholder { file: 1 }
        );
    }

    #[test]
    fn visible_range_uses_prefix_sums() {
        let files: Vec<_> = (0..100)
            .map(|i| loaded(&format!("f{i}.rs"), "@@ -1 +1 @@\n-a\n+b\n"))
            .collect();
        let mut m = build(&files);
        assert_eq!(m.visible_range(0.0, 1.0), 0..1);
        let idx = m.file_ranges[50].start;
        assert_eq!(m.visible_range(m.offset_of(idx), 1.0).start, idx);
        let before = m.offset_of(idx);
        m.set_height(0, HEADER_H + 100.0);
        assert_eq!(m.offset_of(idx), before + 100.0);
    }

    #[test]
    fn display_text_truncates_on_char_boundaries() {
        assert!(display_text(&"a".repeat(500_000)).chars().count() <= MAX_DISPLAY_CHARS + 32);
        let wide = "é🎉".repeat(10_000); // multi-byte; must not panic
        assert!(display_text(&wide).ends_with("(line truncated)"));
        assert_eq!(display_text("short"), "short");
    }
}
