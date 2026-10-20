//! Placing threads, pending comments, and the composer into the row model.
//!
//! Everything the diff surface shows beyond the patch itself — existing
//! review threads, locally pending comments, and the open composer — is
//! decided here as pure functions: [`place_thread`] turns a thread's anchor
//! into a [`Placement`], and [`annotate`] splices those placements into a
//! [`RowModel`]. The widget only draws the rows this produces (spec §7.4,
//! §8.4).

use std::ops::Range;

use crate::diff::row_model::{RowModel, RowRef};
use crate::github::files::LoadedFile;
use crate::model::comment::Anchor;
use crate::model::diff::{DiffRow, Side};
use crate::model::thread::{ReviewThread, SubjectType};

/// The default height of a thread row. Threads scroll inside their row, so
/// one height fits every thread.
pub const THREAD_H: f32 = 150.0;

/// The default height of a pending-comment row.
pub const PENDING_H: f32 = 110.0;

/// The default height of the composer row.
pub const COMPOSER_H: f32 = 220.0;

/// Where an inline item sits within its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Right after the file's header row: a file-level thread.
    AfterHeader,
    /// After the row whose coordinate on `side` equals `line`.
    AfterLine {
        /// The side the anchor's line number counts on.
        side: Side,
        /// The anchored line number.
        line: u32,
    },
    /// At the end of the file's rows: outdated threads whose line is gone,
    /// and anchors that no longer point into the diff.
    EndOfFile,
}

/// One row to splice into the model: a thread, a pending comment, or the
/// composer, with where it belongs.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineItem {
    /// The index of the file in `PrSession::files`.
    pub file: u32,
    /// Where in that file the row sits.
    pub placement: Placement,
    /// The row to insert: [`RowRef::Thread`], [`RowRef::Pending`], or
    /// [`RowRef::Composer`].
    pub row: RowRef,
}

/// Decide where `t` sits, or `None` when its file is not in `files` at all
/// (a thread on a file this page of the PR does not carry).
///
/// File-level threads sit under the header. Outdated threads and threads
/// with no line render collapsed at the end of the file; a line thread whose
/// line is no longer in the diff is treated the same way, since there is no
/// row to anchor it under (spec §7.4).
pub fn place_thread(t: &ReviewThread, idx: u32, files: &[LoadedFile]) -> Option<InlineItem> {
    let file = files.iter().position(|l| l.file.path == t.path)? as u32;
    let placement = match t.subject_type {
        SubjectType::File => Placement::AfterHeader,
        SubjectType::Line => match (t.line, t.is_outdated) {
            (Some(line), false) => Placement::AfterLine {
                side: t.side.unwrap_or(Side::Right),
                line,
            },
            _ => Placement::EndOfFile,
        },
    };
    Some(InlineItem {
        file,
        placement,
        row: RowRef::Thread { thread: idx },
    })
}

/// Insert `items` into `model`, rebuilding `file_ranges` and the heights.
///
/// Rows already inserted by a previous call are dropped first, so annotating
/// an annotated model replaces the comment rows instead of duplicating them.
/// Items sharing an insertion point keep a stable order: threads, then
/// pending comments, then the composer.
pub fn annotate(model: &RowModel, files: &[LoadedFile], items: &[InlineItem]) -> RowModel {
    let base: Vec<(&RowRef, f32)> = model
        .rows
        .iter()
        .zip(model.heights())
        .filter(|(row, _)| !is_comment_row(row))
        .map(|(row, height)| (row, *height))
        .collect();
    let spans = file_spans(base.iter().map(|(row, _)| *row), files.len(), base.len());

    // Where each item goes, as an index into `base`.
    let mut plan: Vec<(usize, u8, &InlineItem)> = Vec::new();
    for item in items {
        let file = item.file as usize;
        if file >= files.len() {
            continue;
        }
        let range = spans[file].clone();
        if range.is_empty() {
            // The file is not in the model (one file at a time: the other
            // files' threads wait until their file is selected).
            continue;
        }
        let at = match item.placement {
            Placement::AfterHeader => range.start + 1,
            Placement::AfterLine { side, line } => find_line(files, &base, &range, side, line)
                .map(|index| index + 1)
                .unwrap_or(range.end),
            Placement::EndOfFile => range.end,
        };
        plan.push((at, kind_rank(&item.row), item));
    }
    // Absolute indices keep files in order; the stable sort keeps items at
    // one point in their thread → pending → composer rank.
    plan.sort_by_key(|(at, rank, _)| (*at, *rank));

    let mut out = RowModel::empty();
    let mut next = 0;
    for (at, _, item) in plan {
        push_rows(&mut out, &base[next..at]);
        let height = match item.row {
            RowRef::Thread { .. } => THREAD_H,
            RowRef::Pending { .. } => PENDING_H,
            _ => COMPOSER_H,
        };
        out.push(item.row.clone(), height);
        next = at;
    }
    push_rows(&mut out, &base[next..]);

    out.file_ranges = file_spans(out.rows.iter(), files.len(), out.rows.len());
    out
}

/// The patch rows an anchor covers, in order — the rows an inserted
/// suggestion would replace (spec §7.3).
pub fn rows_for_anchor(files: &[LoadedFile], anchor: &Anchor) -> Vec<DiffRow> {
    let start = anchor.start_line.unwrap_or(anchor.line);
    let end = anchor.line;
    let Some(loaded) = files.iter().find(|l| l.file.path == anchor.path) else {
        return Vec::new();
    };
    let Some(diff) = &loaded.diff else {
        return Vec::new();
    };
    diff.hunks
        .iter()
        .flat_map(|hunk| hunk.rows.iter())
        .filter(|row| {
            matches!(coordinate(row, anchor.side), Some(line) if line >= start && line <= end)
        })
        .cloned()
        .collect()
}

/// The row's line number on `side`, its coordinate for anchoring.
fn coordinate(row: &DiffRow, side: Side) -> Option<u32> {
    match side {
        Side::Left => row.old_no,
        Side::Right => row.new_no,
    }
}

/// Whether a row is one this module inserts.
fn is_comment_row(row: &RowRef) -> bool {
    matches!(
        row,
        RowRef::Thread { .. } | RowRef::Pending { .. } | RowRef::Composer
    )
}

/// The tie-break rank for items sharing an insertion point.
fn kind_rank(row: &RowRef) -> u8 {
    match row {
        RowRef::Thread { .. } => 0,
        RowRef::Pending { .. } => 1,
        _ => 2,
    }
}

/// Append base rows to a model with their measured heights.
fn push_rows(out: &mut RowModel, rows: &[(&RowRef, f32)]) {
    for (row, height) in rows {
        out.push((*row).clone(), *height);
    }
}

/// The row range of each file, located by its header rows. Headers appear
/// in file order, so the *n*-th header owns rows up to the next one.
fn file_spans<'a>(
    rows: impl Iterator<Item = &'a RowRef>,
    files: usize,
    total: usize,
) -> Vec<Range<usize>> {
    let headers: Vec<(usize, usize)> = rows
        .enumerate()
        .filter_map(|(index, row)| match row {
            RowRef::FileHeader { file } => Some((*file as usize, index)),
            _ => None,
        })
        .collect();
    let mut spans = vec![0..0; files];
    for (position, (file, start)) in headers.iter().enumerate() {
        if *file < files {
            let end = headers.get(position + 1).map_or(total, |(_, next)| *next);
            spans[*file] = *start..end;
        }
    }
    spans
}

/// The index, within `range`, of the first row whose coordinate on `side`
/// is `line`.
fn find_line(
    files: &[LoadedFile],
    rows: &[(&RowRef, f32)],
    range: &Range<usize>,
    side: Side,
    line: u32,
) -> Option<usize> {
    rows[range.clone()]
        .iter()
        .position(|(row, _)| row_coordinate(files, row, side) == Some(line))
}

/// Resolve a [`RowRef::Line`]'s (or a [`RowRef::Pair`]'s side) coordinate on
/// `side`. A pair answers from the row its `side` points at: the left row
/// for `Left`, the right row for `Right`.
pub(crate) fn row_coordinate(files: &[LoadedFile], row: &RowRef, side: Side) -> Option<u32> {
    let (file, hunk, index) = match row {
        RowRef::Line { file, hunk, row } => (*file, *hunk, *row),
        RowRef::Pair {
            file,
            hunk,
            left,
            right,
        } => (
            *file,
            *hunk,
            match side {
                Side::Left => left.as_ref().copied()?,
                Side::Right => right.as_ref().copied()?,
            },
        ),
        _ => return None,
    };
    files
        .get(file as usize)?
        .diff
        .as_ref()?
        .hunks
        .get(hunk as usize)?
        .rows
        .get(index as usize)
        .and_then(|diff_row| coordinate(diff_row, side))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::row_model::{build, build_one};
    use crate::model::thread::SubjectType;
    use crate::test_support::loaded;

    #[test]
    fn threads_land_after_the_pair_holding_their_line() {
        use crate::model::diff::DiffMode;
        let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-a\n+b\n c\n")];
        let base = build_one(&files, Some(0), DiffMode::Split);
        let item = InlineItem {
            file: 0,
            placement: Placement::AfterLine {
                side: Side::Right,
                line: 1,
            },
            row: RowRef::Thread { thread: 0 },
        };
        let out = annotate(&base, &files, &[item]);
        // Row 0 is the file header and row 1 the hunk bar; the pair holding
        // new line 1 follows, and the thread lands right after it.
        assert!(matches!(out.rows[2], RowRef::Pair { right: Some(1), .. }));
        assert_eq!(out.rows[3], RowRef::Thread { thread: 0 });
    }

    #[test]
    fn file_level_threads_lead_the_file() {
        let files = vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")];
        let m = build_one(&files, Some(0), crate::model::diff::DiffMode::Unified);
        let out = annotate(
            &m,
            &files,
            &[InlineItem {
                file: 0,
                placement: Placement::AfterHeader,
                row: RowRef::Thread { thread: 0 },
            }],
        );
        let thread = out
            .rows
            .iter()
            .position(|r| matches!(r, RowRef::Thread { .. }))
            .unwrap();
        let line = out
            .rows
            .iter()
            .position(|r| matches!(r, RowRef::Line { .. }))
            .unwrap();
        assert!(thread < line);
        assert!(out.file_ranges[0].contains(&thread));
    }

    #[test]
    fn threads_on_files_that_are_not_shown_are_dropped() {
        let files = vec![
            loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n"),
            loaded("b.rs", "@@ -1 +1 @@\n-a\n+b\n"),
        ];
        let m = build_one(&files, Some(0), crate::model::diff::DiffMode::Unified);
        let out = annotate(
            &m,
            &files,
            &[InlineItem {
                file: 1,
                placement: Placement::AfterHeader,
                row: RowRef::Thread { thread: 0 },
            }],
        );
        assert!(!out.rows.iter().any(|r| matches!(r, RowRef::Thread { .. })));
    }

    fn thread(path: &str, line: Option<u32>, side: Side, subject: SubjectType) -> ReviewThread {
        ReviewThread {
            id: "T".into(),
            is_resolved: false,
            is_outdated: line.is_none(),
            path: path.into(),
            line,
            start_line: None,
            side: Some(side),
            subject_type: subject,
            comments: vec![],
        }
    }

    #[test]
    fn threads_are_placed_by_side_and_line() {
        let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-x\n+y\n z\n")];
        let right = place_thread(
            &thread("a.rs", Some(1), Side::Right, SubjectType::Line),
            0,
            &files,
        )
        .unwrap();
        assert_eq!(
            right.placement,
            Placement::AfterLine {
                side: Side::Right,
                line: 1
            }
        );
        let file_level = place_thread(
            &thread("a.rs", None, Side::Right, SubjectType::File),
            1,
            &files,
        )
        .unwrap();
        assert_eq!(file_level.placement, Placement::AfterHeader);
        let outdated = place_thread(
            &thread("a.rs", None, Side::Right, SubjectType::Line),
            2,
            &files,
        )
        .unwrap();
        assert_eq!(outdated.placement, Placement::EndOfFile);
        assert!(
            place_thread(
                &thread("other.rs", Some(1), Side::Right, SubjectType::Line),
                3,
                &files
            )
            .is_none()
        );

        let model = annotate(&build(&files), &files, &[right]);
        let pos = model
            .rows
            .iter()
            .position(|r| *r == RowRef::Thread { thread: 0 })
            .unwrap();
        assert_eq!(
            model.rows[pos - 1],
            RowRef::Line {
                file: 0,
                hunk: 0,
                row: 1
            }
        ); // after "+y" (new line 1)
    }

    #[test]
    fn left_side_threads_anchor_on_the_old_number() {
        let files = vec![loaded("a.rs", "@@ -1,2 +1,2 @@\n-x\n+y\n z\n")];
        let left = place_thread(
            &thread("a.rs", Some(1), Side::Left, SubjectType::Line),
            0,
            &files,
        )
        .unwrap();
        let model = annotate(&build(&files), &files, &[left]);
        let pos = model
            .rows
            .iter()
            .position(|r| *r == RowRef::Thread { thread: 0 })
            .unwrap();
        assert_eq!(
            model.rows[pos - 1],
            RowRef::Line {
                file: 0,
                hunk: 0,
                row: 0
            }
        ); // after "-x" (old line 1)
    }

    #[test]
    fn items_at_one_point_order_threads_pending_composer() {
        let files = vec![
            loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n"),
            loaded("b.rs", "@@ -1 +1 @@\n-c\n+d\n"),
        ];
        let at_end = |file: u32, row: RowRef| InlineItem {
            file,
            placement: Placement::EndOfFile,
            row,
        };
        // Deliberately out of order: the annotate order must be stable.
        let items = vec![
            at_end(0, RowRef::Composer),
            at_end(0, RowRef::Thread { thread: 7 }),
            InlineItem {
                file: 1,
                placement: Placement::AfterHeader,
                row: RowRef::Thread { thread: 3 },
            },
            at_end(0, RowRef::Pending { local_id: 9 }),
        ];
        let model = annotate(&build(&files), &files, &items);
        let rows = &model.rows;
        // The three file-a items land at the end of a.rs's range, in rank
        // order no matter what order they were passed in.
        let end_a = model.file_ranges[0].end;
        assert_eq!(
            &rows[end_a - 3..end_a],
            [
                RowRef::Thread { thread: 7 },
                RowRef::Pending { local_id: 9 },
                RowRef::Composer,
            ]
        );
        // The file-level thread sits right under b.rs's header.
        let header_b = model.file_ranges[1].start;
        assert_eq!(header_b, end_a);
        assert_eq!(rows[header_b + 1], RowRef::Thread { thread: 3 });
        assert_eq!(model.file_ranges[1], header_b..rows.len());
        assert_eq!(model.heights().len(), rows.len());
    }

    #[test]
    fn annotating_twice_replaces_instead_of_duplicating() {
        let files = vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")];
        let item = InlineItem {
            file: 0,
            placement: Placement::EndOfFile,
            row: RowRef::Thread { thread: 0 },
        };
        let once = annotate(&build(&files), &files, std::slice::from_ref(&item));
        let twice = annotate(&once, &files, &[item]);
        assert_eq!(twice.rows, once.rows);
    }

    #[test]
    fn missing_line_falls_back_to_end_of_file() {
        let files = vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")];
        let item = InlineItem {
            file: 0,
            // New line 9 does not exist in this diff.
            placement: Placement::AfterLine {
                side: Side::Right,
                line: 9,
            },
            row: RowRef::Thread { thread: 0 },
        };
        let model = annotate(&build(&files), &files, &[item]);
        assert_eq!(
            model.rows.last(),
            Some(&RowRef::Thread { thread: 0 }),
            "an anchor outside the diff renders at the file's end"
        );
    }

    #[test]
    fn rows_for_anchor_covers_a_range_on_its_side() {
        let files = vec![loaded("a.rs", "@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n")];
        let anchor = Anchor {
            path: "a.rs".into(),
            side: Side::Right,
            line: 3,
            start_line: Some(2),
            start_side: Some(Side::Right),
        };
        let rows = rows_for_anchor(&files, &anchor);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["c", "d"]
        );
        assert!(rows.iter().all(|r| r.new_no.is_some()));
    }
}
