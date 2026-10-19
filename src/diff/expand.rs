//! Context expansion from the head blob (Task 45, Review Focus #4).
//!
//! GitHub's patches carry only a few lines of context; everything else is a
//! gap the reviewer can open. [`gaps`] finds those unchanged ranges, and
//! [`expand`] inserts real `Context` rows into one of them from the file's
//! head blob — the same text GitHub renders, so the inserted rows carry exact
//! coordinates and a comment anchored to one anchors where it looks.

use crate::model::diff::{DiffRow, FileDiff, Hunk, LineKind, RowOrigin};

/// A run of unchanged lines the patch omitted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gap {
    /// The hunk this gap precedes; `hunks.len()` for the tail gap.
    pub before_hunk: u32,
    /// The first hidden line's new-side number.
    pub new_start: u32,
    /// The last hidden line's new-side number, inclusive.
    pub new_end: u32,
    /// `new − old` across this gap, so `old_no = new_no − offset`.
    pub offset: i64,
}

/// How much of a gap to reveal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Expand {
    /// The `n` lines at the gap's bottom, adjacent to the hunk that follows.
    Up(u32),
    /// The `n` lines at the gap's top, adjacent to the hunk that precedes.
    Down(u32),
    /// Everything the gap hides.
    All,
}

/// The unchanged ranges before, between, and after `diff`'s hunks.
///
/// `head_line_count` is the head blob's line count, which bounds the tail
/// gap; a caller that has not fetched the blob yet can pass `u32::MAX` —
/// [`expand`] clamps to the blob it is given either way.
pub fn gaps(diff: &FileDiff, head_line_count: u32) -> Vec<Gap> {
    let mut out = Vec::new();
    let mut next_new: u32 = 1;
    let mut next_old: i64 = 1;

    for (index, hunk) in diff.hunks.iter().enumerate() {
        if next_new < hunk.new_start {
            out.push(Gap {
                before_hunk: index as u32,
                new_start: next_new,
                new_end: hunk.new_start - 1,
                offset: hunk.new_start as i64 - hunk.old_start as i64,
            });
        }
        let (mut old_cursor, mut new_cursor) = (hunk.old_start, hunk.new_start);
        for row in &hunk.rows {
            match row.kind {
                LineKind::Context => {
                    old_cursor += 1;
                    new_cursor += 1;
                }
                LineKind::Added => new_cursor += 1,
                LineKind::Removed => old_cursor += 1,
            }
        }
        next_new = new_cursor;
        next_old = old_cursor as i64;
    }

    if next_new <= head_line_count {
        out.push(Gap {
            before_hunk: diff.hunks.len() as u32,
            new_start: next_new,
            new_end: head_line_count,
            offset: next_new as i64 - next_old,
        });
    }

    out
}

/// Insert the chosen part of `gap` into `diff` as `Expanded` context rows.
///
/// Row text and `new_no` come from `head_lines` (the head blob's lines, in
/// order) and `old_no = new_no − gap.offset`, so expanded rows anchor exactly
/// like patch rows. `Up` and `All` prepend to the following hunk (and move
/// its start, so the UI's gap row shrinks or disappears); `Down` appends to
/// the preceding hunk, keeping both hunks contiguous, or forms a hunk of its
/// own above the first one. The tail gap appends to the last hunk, or becomes
/// a hunk of its own when there is none.
pub fn expand(diff: &FileDiff, head_lines: &[&str], gap: Gap, how: Expand) -> FileDiff {
    let mut out = diff.clone();
    let start = gap.new_start;
    // A caller without the blob may pass an open-ended gap; the blob bounds it.
    let end = gap.new_end.min(head_lines.len() as u32);
    if start == 0 || start > end {
        return out;
    }
    let (from, to) = match how {
        Expand::All => (start, end),
        Expand::Up(0) | Expand::Down(0) => return out,
        Expand::Up(n) => (end.saturating_sub(n - 1).max(start), end),
        Expand::Down(n) => (start, end.min(start.saturating_add(n - 1))),
    };

    let rows: Vec<DiffRow> = (from..=to)
        .map(|new_no| DiffRow {
            kind: LineKind::Context,
            old_no: u32::try_from(new_no as i64 - gap.offset).ok(),
            new_no: Some(new_no),
            text: head_lines[(new_no - 1) as usize].to_owned(),
            no_newline_at_eof: false,
            origin: RowOrigin::Expanded,
        })
        .collect();

    let first_old = rows[0].old_no;
    let before = gap.before_hunk as usize;
    match how {
        // ↓ reveals the gap's top, adjacent to the PRECEDING hunk, so append
        // the rows there: that hunk now runs up to `to` and stays contiguous,
        // and the following hunk is left untouched. For the tail gap the
        // "preceding" hunk is the last one, so this also covers expanding
        // below everything.
        Expand::Down(_) if before > 0 => out.hunks[before - 1].rows.extend(rows),
        // ↓ above the first hunk (or a file with no patch rows) has no
        // preceding hunk to grow, so the revealed block becomes its own,
        // exactly as GitHub renders a down-expansion.
        Expand::Down(_) => out.hunks.insert(
            0,
            Hunk {
                old_start: first_old.unwrap_or(1),
                new_start: from,
                rows,
            },
        ),
        // ↑ and "all" reveal the gap's bottom, adjacent to the FOLLOWING
        // hunk: prepend there and move its start so the UI's gap row shrinks
        // — and disappears once line 1 is reached.
        _ if before < out.hunks.len() => {
            let hunk = &mut out.hunks[before];
            hunk.rows.splice(0..0, rows);
            hunk.old_start = first_old.unwrap_or(hunk.old_start);
            hunk.new_start = from;
        }
        // Tail gap: grow the last hunk, or start one when there is none.
        _ => {
            if let Some(hunk) = out.hunks.last_mut() {
                hunk.rows.extend(rows);
            } else {
                out.hunks.push(Hunk {
                    old_start: first_old.unwrap_or(1),
                    new_start: from,
                    rows,
                });
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse::{parse_patch, row_anchor};
    use crate::model::diff::{FileDiff, RowOrigin, Side};

    #[test]
    fn gaps_follow_hunk_offsets() {
        // old: l1..l9; new: l1..l5, ins, l6..l9 (one inserted line)
        let diff = FileDiff {
            hunks: parse_patch("@@ -4,3 +4,4 @@\n l4\n l5\n+ins\n l6\n").unwrap(),
        };
        let g = gaps(&diff, 10);
        assert_eq!(
            g,
            vec![
                Gap {
                    before_hunk: 0,
                    new_start: 1,
                    new_end: 3,
                    offset: 0
                },
                Gap {
                    before_hunk: 1,
                    new_start: 8,
                    new_end: 10,
                    offset: 1
                },
            ]
        );
    }

    #[test]
    fn expanded_rows_have_exact_coordinates() {
        let head = ["l1", "l2", "l3", "l4", "l5", "ins", "l6", "l7", "l8", "l9"];
        let diff = FileDiff {
            hunks: parse_patch("@@ -4,3 +4,4 @@\n l4\n l5\n+ins\n l6\n").unwrap(),
        };
        let tail = gaps(&diff, 10)[1];
        let out = expand(&diff, &head, tail, Expand::All);
        let rows: Vec<_> = out
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .filter(|r| r.origin == RowOrigin::Expanded)
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (rows[0].old_no, rows[0].new_no, rows[0].text.as_str()),
            (Some(7), Some(8), "l7")
        );
        assert_eq!(row_anchor(rows[0]), Some((Side::Right, 8)));

        let head_gap = gaps(&diff, 10)[0];
        let up = expand(&diff, &head, head_gap, Expand::Up(2)); // last 2 lines above the hunk
        let first = &up.hunks[0].rows[0];
        assert_eq!((first.old_no, first.new_no), (Some(2), Some(2)));
    }

    const HEAD: [&str; 12] = [
        "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10", "l11", "l12",
    ];

    /// One-line hunks at new 4 and new 10, leaving gaps 1..3, 5..9 and 11..12.
    fn two_hunks() -> FileDiff {
        FileDiff {
            hunks: parse_patch("@@ -4,1 +4,1 @@\n-l4\n+l4x\n@@ -10,1 +10,1 @@\n-l10\n+l10x\n")
                .unwrap(),
        }
    }

    /// Every hunk's `new_no`s run without a jump (Removed rows carry none).
    fn assert_contiguous(diff: &FileDiff) {
        for (index, hunk) in diff.hunks.iter().enumerate() {
            let mut prev: Option<u32> = None;
            for row in &hunk.rows {
                if let Some(new_no) = row.new_no {
                    if let Some(p) = prev {
                        assert_eq!(
                            new_no,
                            p + 1,
                            "hunk {index} jumps from new line {p} to {new_no}"
                        );
                    }
                    prev = Some(new_no);
                }
            }
        }
    }

    fn assert_unique_new_nos(diff: &FileDiff) {
        let mut seen = std::collections::HashSet::new();
        for row in diff.hunks.iter().flat_map(|h| &h.rows) {
            if let Some(new_no) = row.new_no {
                assert!(seen.insert(new_no), "new line {new_no} appears twice");
            }
        }
    }

    #[test]
    fn partial_down_expand_keeps_hunks_contiguous() {
        let diff = two_hunks();
        let mid = gaps(&diff, 12)
            .into_iter()
            .find(|gap| gap.before_hunk == 1)
            .unwrap();
        let out = expand(&diff, &HEAD, mid, Expand::Down(1));
        assert_contiguous(&out);
        assert_unique_new_nos(&out);

        // The rest of the gap stays reachable: `gaps` still reports 6..9,
        // so a second ↓ reveals the next chunk instead of duplicating rows.
        let remaining = gaps(&out, 12)
            .into_iter()
            .find(|gap| gap.before_hunk == 1)
            .expect("the hidden remainder is still a gap");
        assert_eq!((remaining.new_start, remaining.new_end), (6, 9));

        let out2 = expand(&out, &HEAD, remaining, Expand::Down(1));
        assert_contiguous(&out2);
        assert_unique_new_nos(&out2);
        let revealed: Vec<u32> = out2
            .hunks
            .iter()
            .flat_map(|h| &h.rows)
            .filter(|r| r.origin == RowOrigin::Expanded)
            .map(|r| r.new_no.unwrap())
            .collect();
        assert_eq!(
            revealed,
            vec![5, 6],
            "the two ↓ clicks reveal lines 5 and 6"
        );
    }

    #[test]
    fn down_expand_before_the_first_hunk_stands_alone() {
        let diff = two_hunks();
        let top = gaps(&diff, 12)[0]; // 1..3, above every hunk
        let out = expand(&diff, &HEAD, top, Expand::Down(1));
        assert_contiguous(&out);
        assert_unique_new_nos(&out);

        // The revealed line sits in its own block; the patch hunk keeps its
        // rows, and the remaining 2..3 gap stays reachable before it.
        let remaining = gaps(&out, 12)
            .into_iter()
            .find(|gap| gap.before_hunk == 1)
            .expect("the hidden remainder is still a gap");
        assert_eq!((remaining.new_start, remaining.new_end), (2, 3));
    }
}
