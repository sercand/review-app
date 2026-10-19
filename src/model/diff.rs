//! The diff model: hunks and rows as the diff engine and the UI see them.
//!
//! Every row carries both coordinates (its old and new line numbers), which is
//! what lets a comment be anchored to either side (spec §7.3).

use serde::{Deserialize, Serialize};

/// Which side of a diff a line number or a comment belongs to.
///
/// Serializes as GitHub's wire values, `LEFT` and `RIGHT` (spec §7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    /// The base side — pre-change line numbers.
    Left,
    /// The head side — post-change line numbers.
    Right,
}

/// What a row is, relative to the two sides.
#[derive(Clone, Debug, PartialEq)]
pub enum LineKind {
    /// Present on both sides.
    Context,
    /// Present on the new side only.
    Added,
    /// Present on the old side only.
    Removed,
}

/// Where a row came from (spec §8.3 and §7.5).
#[derive(Clone, Debug, PartialEq)]
pub enum RowOrigin {
    /// Part of the patch GitHub sent.
    Patch,
    /// Inserted by context expansion from the head blob.
    Expanded,
    /// Computed locally with `similar` for a file GitHub sent no patch for.
    Local,
}

/// One line of a diff.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffRow {
    /// Whether the line is context, added, or removed.
    pub kind: LineKind,
    /// The line number on the old side, when the row has one.
    pub old_no: Option<u32>,
    /// The line number on the new side, when the row has one.
    pub new_no: Option<u32>,
    /// The line's text, with its leading `+`/`-`/` ` marker stripped.
    pub text: String,
    /// Set when the row is followed by `\ No newline at end of file`.
    pub no_newline_at_eof: bool,
    /// Where the row came from.
    pub origin: RowOrigin,
}

/// A contiguous run of changes, plus the context around them.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    /// The old-side line number the hunk starts at.
    pub old_start: u32,
    /// The new-side line number the hunk starts at.
    pub new_start: u32,
    /// The hunk's rows, in order.
    pub rows: Vec<DiffRow>,
}

/// One file's diff: its hunks.
#[derive(Clone, Debug, PartialEq)]
pub struct FileDiff {
    /// The file's hunks, in order.
    pub hunks: Vec<Hunk>,
}
