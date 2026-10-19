//! The unified patch parser (spec §8.2).
//!
//! GitHub's files API sends a raw unified diff per file. The parser turns it
//! into [`Hunk`]s of [`DiffRow`]s carrying both line numbers, and never panics:
//! a malformed hunk header is a [`ParseError`].

use crate::model::diff::{DiffRow, Hunk, LineKind, RowOrigin, Side};

/// A patch that could not be parsed.
///
/// The only malformed input the parser rejects is a hunk header that is not
/// `@@ -old[,len] +new[,len] @@ [section]`; everything else is tolerated.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("malformed hunk header: {0}")]
pub struct ParseError(/* the offending header */ pub String);

/// Parse a unified patch into hunks.
///
/// Everything before the first `@@` (`diff --git`, `---`, `+++`, `index`) is
/// skipped, so both a bare patch and a full `git diff` are accepted. The
/// no-newline marker flags the row above it and consumes no line number, CRLF
/// input is accepted, and a fully empty line inside a hunk is an empty context
/// line.
pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ParseError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut old_cursor = 0;
    let mut new_cursor = 0;

    // `lines()` splits on `\n` and already eats the `\r` of a CRLF pair; the
    // extra strip covers a lone `\r` on a final line without a newline.
    for raw in patch.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if let Some(rest) = line.strip_prefix("@@") {
            let (old_start, new_start) = parse_hunk_header(rest)?;
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            old_cursor = old_start;
            new_cursor = new_start;
            current = Some(Hunk {
                old_start,
                new_start,
                rows: Vec::new(),
            });
            continue;
        }

        // Anything before the first hunk header is not diff content.
        let Some(hunk) = current.as_mut() else {
            continue;
        };

        // `\ No newline at end of file` flags the row above it.
        if line.starts_with('\\') {
            if let Some(last) = hunk.rows.last_mut() {
                last.no_newline_at_eof = true;
            }
            continue;
        }

        let (kind, text) = if let Some(text) = line.strip_prefix('+') {
            (LineKind::Added, text)
        } else if let Some(text) = line.strip_prefix('-') {
            (LineKind::Removed, text)
        } else if let Some(text) = line.strip_prefix(' ') {
            (LineKind::Context, text)
        } else {
            // A completely empty line is an empty context line; anything else
            // is an unexpected marker, which we keep as context rather than
            // losing the text.
            (LineKind::Context, line)
        };

        let (old_no, new_no) = match kind {
            LineKind::Added => {
                let new_no = Some(new_cursor);
                new_cursor += 1;
                (None, new_no)
            }
            LineKind::Removed => {
                let old_no = Some(old_cursor);
                old_cursor += 1;
                (old_no, None)
            }
            LineKind::Context => {
                let numbers = (Some(old_cursor), Some(new_cursor));
                old_cursor += 1;
                new_cursor += 1;
                numbers
            }
        };

        hunk.rows.push(DiffRow {
            kind,
            old_no,
            new_no,
            text: text.to_owned(),
            no_newline_at_eof: false,
            origin: RowOrigin::Patch,
        });
    }

    if let Some(hunk) = current.take() {
        hunks.push(hunk);
    }

    Ok(hunks)
}

/// The side and line number a row defaults to in a comment (spec §7.3).
///
/// Added and context rows are on the new side; removed rows are on the old
/// side. `None` when the row has no number on that side.
pub fn row_anchor(row: &DiffRow) -> Option<(Side, u32)> {
    match row.kind {
        LineKind::Removed => row.old_no.map(|no| (Side::Left, no)),
        LineKind::Added | LineKind::Context => row.new_no.map(|no| (Side::Right, no)),
    }
}

/// Parse the part of a hunk header after `@@`, returning the two start lines.
fn parse_hunk_header(rest: &str) -> Result<(u32, u32), ParseError> {
    let malformed = || ParseError(format!("@@{rest}"));

    let ranges = rest
        .strip_prefix(' ')
        .and_then(|rest| rest.split_once(" @@"))
        .map(|(ranges, _section)| ranges)
        .ok_or_else(malformed)?;

    let (old, new) = ranges.split_once(' ').ok_or_else(malformed)?;
    let old_start = old
        .strip_prefix('-')
        .and_then(parse_hunk_start)
        .ok_or_else(malformed)?;
    let new_start = new
        .strip_prefix('+')
        .and_then(parse_hunk_start)
        .ok_or_else(malformed)?;

    Ok((old_start, new_start))
}

/// The start line of a `start[,len]` range field; the count is not needed.
fn parse_hunk_start(range: &str) -> Option<u32> {
    range.split(',').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::diff::Side;

    #[test]
    fn numbers_rows_like_github() {
        let patch = "@@ -1,3 +1,3 @@ fn main()\n ctx\n+added\n-removed\n ctx2\n";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!((hunks[0].old_start, hunks[0].new_start), (1, 1));
        let rows = &hunks[0].rows;
        assert_eq!((rows[0].old_no, rows[0].new_no), (Some(1), Some(1)));
        assert_eq!((rows[1].old_no, rows[1].new_no), (None, Some(2)));
        assert_eq!((rows[2].old_no, rows[2].new_no), (Some(2), None));
        assert_eq!((rows[3].old_no, rows[3].new_no), (Some(3), Some(3)));
        assert_eq!(rows[1].text, "added"); // sign stripped
        assert_eq!(row_anchor(&rows[1]), Some((Side::Right, 2)));
        assert_eq!(row_anchor(&rows[2]), Some((Side::Left, 2)));
        assert_eq!(row_anchor(&rows[0]), Some((Side::Right, 1))); // context defaults to Right
    }

    #[test]
    fn multiple_hunks_restart_numbering() {
        let hunks = parse_patch("@@ -1 +1 @@\n-a\n+b\n@@ -10,2 +10,2 @@\n x\n-y\n+z\n").unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[1].rows[2].new_no, Some(11));
    }
}
