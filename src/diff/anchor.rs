//! Comment coordinate mapping: rows to GitHub-accepted anchors (spec §7.3).
//!
//! GitHub numbers lines downward on both sides, so a range is
//! `start_line < line` regardless of side, must stay within one hunk, and its
//! `start_side` must equal its `side`.

use std::ops::RangeInclusive;

use crate::github::files::{PrFile, anchor_path};
use crate::model::comment::Anchor;
use crate::model::diff::{DiffRow, Hunk, Side};

/// Why a row range could not become an [`Anchor`].
#[derive(Debug, PartialEq)]
pub enum AnchorError {
    /// A row the anchor needs has no line number on the requested side (a
    /// removed row on the right, an added row on the left).
    NoCoordinateOnSide,
    /// The range contains no rows.
    EmptyRange,
    /// `start_line` is not before `line`.
    StartNotBeforeEnd,
    /// `start_side` differs from `side`.
    MixedSides,
}

/// Anchor a comment to one row, on `side`.
pub fn single(f: &PrFile, row: &DiffRow, side: Side) -> Result<Anchor, AnchorError> {
    let line = coordinate(row, side).ok_or(AnchorError::NoCoordinateOnSide)?;

    Ok(Anchor {
        path: anchor_path(f).to_owned(),
        side,
        line,
        start_line: None,
        start_side: None,
    })
}

/// Anchor a comment to a run of rows *within one hunk*.
///
/// The first and last rows must both have a line number on `side`; a one-row
/// range becomes a single-line anchor (no `start_*`), as GitHub expects.
pub fn range(
    f: &PrFile,
    hunk: &Hunk,
    rows: RangeInclusive<usize>,
    side: Side,
) -> Result<Anchor, AnchorError> {
    if rows.is_empty() {
        return Err(AnchorError::EmptyRange);
    }

    let first = hunk
        .rows
        .get(*rows.start())
        .ok_or(AnchorError::NoCoordinateOnSide)?;
    let last = hunk
        .rows
        .get(*rows.end())
        .ok_or(AnchorError::NoCoordinateOnSide)?;

    let start_line = coordinate(first, side).ok_or(AnchorError::NoCoordinateOnSide)?;
    let line = coordinate(last, side).ok_or(AnchorError::NoCoordinateOnSide)?;
    let is_range = start_line < line;

    let anchor = Anchor {
        path: anchor_path(f).to_owned(),
        side,
        line,
        start_line: is_range.then_some(start_line),
        start_side: is_range.then_some(side),
    };
    validate(&anchor)?;

    Ok(anchor)
}

/// Check an anchor against GitHub's constraints.
///
/// Only a range (`start_line` set) can be invalid: its `start_side` must equal
/// its `side` and its `start_line` must be before its `line`.
pub fn validate(a: &Anchor) -> Result<(), AnchorError> {
    if let Some(start_line) = a.start_line {
        if a.start_side != Some(a.side) {
            return Err(AnchorError::MixedSides);
        }
        if start_line >= a.line {
            return Err(AnchorError::StartNotBeforeEnd);
        }
    }

    Ok(())
}

/// The line number `row` occupies on `side`, when it has one.
///
/// This is [`row_anchor`](crate::diff::parse::row_anchor) — a row's default
/// side — generalized to either side, which is what the composer's side toggle
/// for context rows needs: a context row has a number on both sides, an added
/// row only on the right, and a removed row only on the left.
fn coordinate(row: &DiffRow, side: Side) -> Option<u32> {
    match side {
        Side::Left => row.old_no,
        Side::Right => row.new_no,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse::{parse_patch, row_anchor};
    use crate::github::files::{FileStatus, PrFile};
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;
    use crate::test_support::file;

    #[test]
    fn ranges_respect_github_constraints_and_paths() {
        let f = PrFile {
            previous_path: Some("src/old name.rs".into()),
            status: FileStatus::Renamed,
            ..file("src/new name.rs")
        };
        let h = &parse_patch("@@ -0,0 +1,3 @@\n+a\n+b\n+c\n").unwrap()[0];

        let a = range(&f, h, 0..=1, Side::Right).unwrap();
        assert_eq!(
            a,
            Anchor {
                path: "src/new name.rs".into(),
                side: Side::Right,
                line: 2,
                start_line: Some(1),
                start_side: Some(Side::Right)
            }
        );
        assert!(validate(&a).is_ok());

        let one = range(&f, h, 2..=2, Side::Right).unwrap();
        assert_eq!((one.line, one.start_line), (3, None)); // one row → single-line comment

        // a removed row has no RIGHT coordinate
        let h2 = &parse_patch("@@ -1,2 +1,1 @@\n-x\n+y\n-z\n").unwrap()[0];
        assert_eq!(
            range(&f, h2, 0..=1, Side::Right),
            Err(AnchorError::NoCoordinateOnSide)
        );
        assert_eq!(single(&f, &h2.rows[0], Side::Left).unwrap().line, 1);

        // start_line must be before line on BOTH sides
        let left = |start, line| Anchor {
            path: "x".into(),
            side: Side::Left,
            line,
            start_line: Some(start),
            start_side: Some(Side::Left),
        };
        assert!(validate(&left(1, 3)).is_ok());
        assert_eq!(validate(&left(3, 1)), Err(AnchorError::StartNotBeforeEnd));
        assert_eq!(validate(&left(2, 2)), Err(AnchorError::StartNotBeforeEnd));
        let mixed = Anchor {
            start_side: Some(Side::Right),
            ..left(1, 3)
        };
        assert_eq!(validate(&mixed), Err(AnchorError::MixedSides));
    }

    #[test]
    fn context_rows_anchor_on_both_sides() {
        let f = file("a.rs");
        let h = &parse_patch("@@ -3,3 +3,3 @@\n l3\n-l4\n+l4b\n l5\n").unwrap()[0];

        // a context row is on both sides; the side toggle picks the number
        assert_eq!(single(&f, &h.rows[0], Side::Right).unwrap().line, 3);
        assert_eq!(single(&f, &h.rows[0], Side::Left).unwrap().line, 3);
        assert_eq!(row_anchor(&h.rows[0]), Some((Side::Right, 3))); // default is RIGHT

        // an added row has no LEFT coordinate
        assert_eq!(
            single(&f, &h.rows[2], Side::Left),
            Err(AnchorError::NoCoordinateOnSide)
        );

        // only a range's endpoints need a coordinate, not the rows between them
        let across = range(&f, h, 0..=2, Side::Right).unwrap();
        assert_eq!((across.start_line, across.line), (Some(3), 4));

        // a range with no rows is rejected (built from variables, since a
        // literal `2..=1` reads as a reversed iterator)
        let (empty_start, empty_end) = (2, 1);
        assert_eq!(
            range(&f, h, empty_start..=empty_end, Side::Right),
            Err(AnchorError::EmptyRange)
        );
    }
}
