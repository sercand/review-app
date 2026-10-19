//! The word-level difference between a removed line and its replacement.
//!
//! GitHub strongly highlights only the part of a changed pair that actually
//! moved; a line that was rewritten wholesale gets no word highlight at all.
//! [`word_diff`] decides both, exactly as the mockup's pair rows draw them.

use std::ops::Range;

/// A change covering more than this fraction of the two lines is a rewrite,
/// not an edit: no word highlight.
const REWRITE_RATIO: u64 = 60;

/// The byte ranges that differ between `old` and `new`, after trimming the
/// longest common prefix and suffix.
///
/// The prefix and suffix are counted in `char`s, so both ranges always fall
/// on `char` boundaries. `None` when the lines are identical, and when the
/// changed parts are more than [`REWRITE_RATIO`] % of
/// `old.len() + new.len()` — a rewritten line gains nothing from a word
/// mark.
pub fn word_diff(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    if old == new {
        return None;
    }

    let mut prefix = 0usize;
    for (old_char, new_char) in old.chars().zip(new.chars()) {
        if old_char != new_char {
            break;
        }
        prefix += old_char.len_utf8();
    }

    // The suffix is trimmed from what the prefix left over, so the two can
    // never overlap.
    let mut suffix = 0usize;
    let new_suffix = new[prefix..].chars().rev();
    for (old_char, new_char) in old[prefix..].chars().rev().zip(new_suffix) {
        if old_char != new_char {
            break;
        }
        suffix += old_char.len_utf8();
    }

    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    let changed = ((old_end - prefix) + (new_end - prefix)) as u64;
    if changed * 100 > (old.len() + new.len()) as u64 * REWRITE_RATIO {
        return None;
    }

    Some((prefix..old_end, prefix..new_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_diff_finds_the_changed_middle() {
        assert_eq!(
            word_diff(
                r#"import { content } from "@/api";"#,
                r#"import { apps, content } from "@/api";"#
            ),
            Some((9..9, 9..15))
        );
        assert_eq!(
            word_diff(
                "      envelope.notification,",
                "      prefilled ?? envelope.notification,"
            ),
            Some((6..6, 6..19))
        );
        assert_eq!(word_diff("naïve x", "naïve y"), Some((7..8, 7..8))); // 'ï' is two bytes
        assert_eq!(word_diff("same", "same"), None);
        assert_eq!(word_diff("let a = 1;", "return foo(bar);"), None); // mostly rewritten
    }
}
