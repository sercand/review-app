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

use crate::github::files::LoadedFile;

/// The default height of a file header row.
pub const HEADER_H: f32 = 30.0;

/// The default height of an unwrapped diff line row.
pub const ROW_H: f32 = 20.0;

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
    /// Expandable context before one hunk (Task 45).
    Gap {
        /// The index of the file in `PrSession::files`.
        file: u32,
        /// The hunk the omitted context precedes.
        before_hunk: u32,
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
    /// The per-row heights, parallel to [`RowModel::rows`].
    pub fn heights(&self) -> &[f32] {
        &self.heights
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
    fn push(&mut self, row: RowRef, height: f32) {
        self.rows.push(row);
        self.heights.push(height);
        let total = self.prefix.last().copied().unwrap_or(0.0) + height;
        self.prefix.push(total);
    }
}

/// Build the row list for `files`.
///
/// Per file: a header, then either the placeholder (GitHub sent no diff) or the
/// hunks' lines, each preceded by a [`RowRef::Gap`] when the hunk does not
/// start at line 1.
pub fn build(files: &[LoadedFile]) -> RowModel {
    let mut model = RowModel {
        rows: Vec::new(),
        file_ranges: Vec::new(),
        heights: Vec::new(),
        prefix: vec![0.0],
    };

    for (file_idx, loaded) in files.iter().enumerate() {
        let file = file_idx as u32;
        let start = model.rows.len();
        model.push(RowRef::FileHeader { file }, HEADER_H);

        match &loaded.diff {
            None => model.push(RowRef::Placeholder { file }, ROW_H),
            Some(diff) => {
                for (hunk_idx, hunk) in diff.hunks.iter().enumerate() {
                    let hunk_idx = hunk_idx as u32;
                    if hunk.old_start != 1 {
                        model.push(
                            RowRef::Gap {
                                file,
                                before_hunk: hunk_idx,
                            },
                            ROW_H,
                        );
                    }
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
            }
        }

        model.file_ranges.push(start..model.rows.len());
    }

    model
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
    use crate::test_support::loaded;

    #[test]
    fn builds_ranges_placeholders_and_gaps() {
        let mut bin = loaded("logo.png", "@@ -1 +1 @@\n-a\n+b\n");
        bin.diff = None;
        let m = build(&[loaded("src/a.rs", "@@ -4,2 +4,2 @@\n-a\n+b\n c\n"), bin]);
        assert_eq!(
            m.rows[..2],
            [
                RowRef::FileHeader { file: 0 },
                RowRef::Gap {
                    file: 0,
                    before_hunk: 0
                }
            ]
        );
        assert_eq!(m.file_ranges[0], 0..5); // header + gap + 3 lines
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
