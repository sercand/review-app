//! Local diffs for files GitHub sends no patch for (Task 46, spec §7.5).
//!
//! For a too-large file the app fetches both blobs and diffs them here with
//! `similar`, grouped into hunks the way GitHub's own unified diffs are.
//! Every row is marked [`RowOrigin::Local`]: the coordinates are ours, not
//! GitHub's, and the composer warns before anchoring a comment to one
//! (Task 47).

use similar::{DiffOp, TextDiff};

use crate::model::diff::{DiffRow, FileDiff, Hunk, LineKind, RowOrigin};

/// Diff `old` against `new` with `context` unchanged lines per hunk.
///
/// Numbering is 1-based like every other diff row, and no row claims the
/// `Patch` origin — this diff is the app's reconstruction, not GitHub's.
pub fn local_diff(old: &str, new: &str, context: usize) -> FileDiff {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let diff = TextDiff::from_lines(old, new);

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(context) {
        let Some(first) = group.first() else {
            continue;
        };
        let old_start = first.old_range().start as u32 + 1;
        let new_start = first.new_range().start as u32 + 1;
        let mut old_no = old_start;
        let mut new_no = new_start;
        let mut rows = Vec::new();

        for op in group {
            match op {
                DiffOp::Equal { .. } => {
                    // Both ranges have the same length; the text is the same
                    // on either side, so the old line is what we draw.
                    for old_i in op.old_range() {
                        rows.push(row(
                            LineKind::Context,
                            Some(old_no),
                            Some(new_no),
                            old_lines[old_i],
                        ));
                        old_no += 1;
                        new_no += 1;
                    }
                }
                DiffOp::Delete { .. } => {
                    for old_i in op.old_range() {
                        rows.push(row(LineKind::Removed, Some(old_no), None, old_lines[old_i]));
                        old_no += 1;
                    }
                }
                DiffOp::Insert { .. } => {
                    for new_i in op.new_range() {
                        rows.push(row(LineKind::Added, None, Some(new_no), new_lines[new_i]));
                        new_no += 1;
                    }
                }
                DiffOp::Replace { .. } => {
                    for old_i in op.old_range() {
                        rows.push(row(LineKind::Removed, Some(old_no), None, old_lines[old_i]));
                        old_no += 1;
                    }
                    for new_i in op.new_range() {
                        rows.push(row(LineKind::Added, None, Some(new_no), new_lines[new_i]));
                        new_no += 1;
                    }
                }
            }
        }

        hunks.push(Hunk {
            old_start,
            new_start,
            section: String::new(),
            rows,
        });
    }

    FileDiff { hunks }
}

/// One local row: `similar`'s line with its coordinates and `Local` origin.
fn row(kind: LineKind, old_no: Option<u32>, new_no: Option<u32>, text: &str) -> DiffRow {
    DiffRow {
        kind,
        old_no,
        new_no,
        text: text.to_owned(),
        no_newline_at_eof: false,
        origin: RowOrigin::Local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::diff::{LineKind, RowOrigin};

    #[test]
    fn local_diff_numbers_rows_and_marks_them_local() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let d = local_diff(old, new, 1);
        let rows: Vec<_> = d.hunks.iter().flat_map(|h| &h.rows).collect();
        assert!(rows.iter().all(|r| r.origin == RowOrigin::Local));
        let added_b = rows.iter().find(|r| r.text == "B").unwrap();
        assert_eq!(
            (added_b.kind.clone(), added_b.new_no),
            (LineKind::Added, Some(2))
        );
        let removed_b = rows.iter().find(|r| r.text == "b").unwrap();
        assert_eq!(removed_b.old_no, Some(2));
        assert_eq!(rows.iter().find(|r| r.text == "e").unwrap().new_no, Some(5));
    }
}
