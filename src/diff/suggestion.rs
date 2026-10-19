//! GitHub review suggestion blocks (spec §7.4).
//!
//! A suggestion is a fenced ```` ```suggestion ```` code block whose contents
//! replace the anchored lines. Only rows that exist on the new side (added
//! and context rows) can be replaced, so a LEFT-side-only selection yields
//! no block at all.

use crate::model::diff::DiffRow;

/// One suggestion found in a comment body.
#[derive(Debug, PartialEq)]
pub struct Suggestion {
    /// The suggested replacement text, each line ending in `\n`.
    pub text: String,
}

/// Wrap the new-side rows of `rows` in a ```` ```suggestion ```` fence.
///
/// Removed rows carry no new-side number and are skipped; if no row has one
/// (a LEFT-side selection) there is nothing to suggest and this returns
/// `None`.
pub fn suggestion_block(rows: &[DiffRow]) -> Option<String> {
    let lines: Vec<&str> = rows
        .iter()
        .filter(|row| row.new_no.is_some())
        .map(|row| row.text.as_str())
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("```suggestion\n{}\n```", lines.join("\n")))
}

/// The suggestions in `md`, in order, found by scanning for fence lines.
///
/// The scan is line-oriented and total: an unterminated fence simply yields
/// no suggestion, whatever the input.
pub fn parse_suggestions(md: &str) -> Vec<Suggestion> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "```suggestion" {
            i += 1;
            continue;
        }
        let mut text = String::new();
        i += 1;
        let mut terminated = false;
        while i < lines.len() {
            if lines[i].trim() == "```" {
                terminated = true;
                i += 1;
                break;
            }
            text.push_str(lines[i]);
            text.push('\n');
            i += 1;
        }
        if terminated {
            out.push(Suggestion { text });
        } else {
            // An unterminated fence yields no suggestion; neither does
            // anything after it, exactly as GitHub treats it.
            return out;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rows_from;

    #[test]
    fn builds_from_new_side_rows_and_parses_back() {
        let rows = rows_from("@@ -1,2 +1,3 @@\n ctx\n-old\n+let x = 1;\n+let y = 2;\n");
        let block = suggestion_block(&rows).unwrap();
        assert_eq!(block, "```suggestion\nctx\nlet x = 1;\nlet y = 2;\n```");

        let md = format!("Please change:\n{block}\nthanks");
        assert_eq!(
            parse_suggestions(&md),
            vec![Suggestion {
                text: "ctx\nlet x = 1;\nlet y = 2;\n".into()
            }]
        );
    }

    #[test]
    fn left_only_selection_and_bad_input() {
        assert!(suggestion_block(&rows_from("@@ -1,1 +0,0 @@\n-gone\n")).is_none());
        assert!(parse_suggestions("just a comment").is_empty());
        assert!(parse_suggestions("```suggestion\nunterminated").is_empty());
    }
}
