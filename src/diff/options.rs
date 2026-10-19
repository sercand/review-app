//! Diff display options (Task 48): hiding whitespace-only hunks and finding
//! text in the diff.
//!
//! GitHub's files API has no `w=1`, so "hide whitespace" is approximated here
//! by dropping hunks whose removed and added rows are equal once all
//! whitespace is removed. Find is a plain case-insensitive substring search
//! over the row model's line rows, capped so a giant PR cannot build a giant
//! match list.

use crate::model::diff::{FileDiff, Hunk, LineKind};

/// Drop every hunk whose changes are whitespace-only.
///
/// A hunk is dropped when its removed rows and added rows, each with **all**
/// whitespace removed, come out equal — the files API has no `w=1`, so this
/// is the approximation of GitHub's "ignore whitespace". A hunk with no
/// changed rows at all qualifies too. Kept hunks are returned untouched, so
/// their row numbers are unchanged.
pub fn hide_whitespace_only(diff: &FileDiff) -> FileDiff {
    FileDiff {
        hunks: diff
            .hunks
            .iter()
            .filter(|hunk| !whitespace_only(hunk))
            .cloned()
            .collect(),
    }
}

/// Whether every change in `hunk` disappears when whitespace is stripped.
fn whitespace_only(hunk: &Hunk) -> bool {
    let stripped = |kind: LineKind| -> Vec<String> {
        hunk.rows
            .iter()
            .filter(|row| row.kind == kind)
            .map(|row| row.text.chars().filter(|c| !c.is_whitespace()).collect())
            .collect()
    };
    stripped(LineKind::Removed) == stripped(LineKind::Added)
}

/// The most matches [`find_matches`] ever returns; past this the search stops.
pub const MAX_FIND_MATCHES: usize = 1_000;

/// Row indices whose text contains `needle`, case-insensitively.
///
/// `rows` yields `(row_index, text)` pairs in display order; the result keeps
/// that order and stops at [`MAX_FIND_MATCHES`]. An empty needle matches
/// nothing — an empty search is "no search", not "every row".
pub fn find_matches<'a>(rows: impl Iterator<Item = (usize, &'a str)>, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let needle = needle.to_lowercase();
    rows.filter(|(_, text)| text.to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .take(MAX_FIND_MATCHES)
        .collect()
}

/// The match after `current`, wrapping to the first; `None` when there is
/// nothing to move to. No current match (or one no longer in the list) means
/// the next match is the first.
pub fn next_match(matches: &[usize], current: Option<usize>) -> Option<usize> {
    let first = *matches.first()?;
    let position = match current.and_then(|id| matches.iter().position(|m| *m == id)) {
        Some(position) => position,
        None => return Some(first),
    };
    Some(matches[(position + 1) % matches.len()])
}

/// The match before `current`, wrapping to the last; `None` when there is
/// nothing to move to. No current match (or one no longer in the list) means
/// the previous match is the last.
pub fn prev_match(matches: &[usize], current: Option<usize>) -> Option<usize> {
    let last = *matches.last()?;
    let position = match current.and_then(|id| matches.iter().position(|m| *m == id)) {
        Some(position) => position,
        None => return Some(last),
    };
    Some(matches[(position + matches.len() - 1) % matches.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse::parse_patch;
    use crate::model::diff::FileDiff;

    #[test]
    fn whitespace_only_hunks_are_hidden() {
        let ws = FileDiff {
            hunks: parse_patch("@@ -1,2 +1,2 @@\n fn a() {\n-\tlet x = 1;\n+\tlet x = 1;  \n")
                .unwrap(),
        };
        assert!(hide_whitespace_only(&ws).hunks.is_empty());
        let real = FileDiff {
            hunks: parse_patch("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n").unwrap(),
        };
        assert_eq!(hide_whitespace_only(&real).hunks.len(), 1);
    }

    #[test]
    fn find_is_case_insensitive_capped_and_wraps() {
        let texts = ["Foo", "bar", "foo"];
        let m = find_matches(texts.iter().copied().enumerate(), "foo");
        assert_eq!(m, vec![0, 2]);
        assert_eq!(next_match(&m, Some(2)), Some(0));
        assert_eq!(prev_match(&m, Some(0)), Some(2));
        let many: Vec<String> = (0..1_500).map(|_| "needle".to_string()).collect();
        assert_eq!(
            find_matches(many.iter().map(String::as_str).enumerate(), "NEEDLE").len(),
            MAX_FIND_MATCHES
        );
        assert!(find_matches(texts.iter().copied().enumerate(), "").is_empty());
    }
}
