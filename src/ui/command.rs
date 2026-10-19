//! The command registry, fuzzy matching, key resolution, and navigation math
//! behind the palette, the `?` help overlay, and the global shortcuts
//! (spec §12).
//!
//! Everything here is pure: the registry is data, [`resolve_key`] turns one
//! [`KeyPress`] into an action id, and [`next_index`] does the clamping both
//! file (`j`/`k`) and hunk (`n`/`p`) navigation share. `app.rs` owns dispatch.

use crate::github::files::PrFile;

/// One dispatchable command, as the palette lists it and the help overlay
/// documents it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    /// The id `resolve_key` returns and `app.rs` dispatches on.
    pub id: &'static str,
    /// The human-readable label the palette fuzzy-matches and draws.
    pub label: &'static str,
    /// How to trigger it without the palette, for the help overlay.
    pub keybinding: Option<&'static str>,
}

/// Every command the palette, the help overlay, and the key handler know.
pub fn registry() -> Vec<ActionEntry> {
    vec![
        ActionEntry {
            id: "palette",
            label: "Open command palette",
            keybinding: Some("Cmd+K"),
        },
        ActionEntry {
            id: "settings",
            label: "Open settings",
            keybinding: None,
        },
        ActionEntry {
            id: "help",
            label: "Show keyboard shortcuts",
            keybinding: Some("?"),
        },
        ActionEntry {
            id: "next_file",
            label: "Next file",
            keybinding: Some("j"),
        },
        ActionEntry {
            id: "prev_file",
            label: "Previous file",
            keybinding: Some("k"),
        },
        ActionEntry {
            id: "next_hunk",
            label: "Next hunk",
            keybinding: Some("n"),
        },
        ActionEntry {
            id: "prev_hunk",
            label: "Previous hunk",
            keybinding: Some("p"),
        },
        ActionEntry {
            id: "toggle_viewed",
            label: "Toggle viewed",
            keybinding: Some("v"),
        },
        ActionEntry {
            id: "mark_viewed_and_next",
            label: "Mark viewed and next",
            keybinding: Some("Shift+V"),
        },
        ActionEntry {
            id: "comment",
            label: "Add comment",
            keybinding: Some("c"),
        },
        ActionEntry {
            id: "reply",
            label: "Reply to thread",
            keybinding: Some("r"),
        },
        ActionEntry {
            id: "toggle_side_pane",
            label: "Toggle side pane",
            keybinding: Some("o"),
        },
        ActionEntry {
            id: "split",
            label: "Split diff",
            keybinding: Some("s"),
        },
        ActionEntry {
            id: "unified",
            label: "Unified diff",
            keybinding: Some("u"),
        },
        ActionEntry {
            id: "submit_review",
            label: "Submit review",
            keybinding: Some("Cmd+Enter"),
        },
        ActionEntry {
            id: "close",
            label: "Close",
            keybinding: Some("Esc"),
        },
    ]
}

/// The entries `query` fuzzy-matches, best first.
///
/// A match is a case-insensitive **subsequence** of the label, so `"togview"`
/// finds "Toggle viewed". Results rank by the position of the first matched
/// character (earlier is better), then by label length (shorter is better);
/// an empty query keeps the registry's own order.
/// One palette row: a command, or a file to jump to (Task 14).
#[derive(Debug, PartialEq)]
pub enum PaletteEntry<'a> {
    /// A registry command.
    Action(&'a ActionEntry),
    /// A file in the open PR; `index` is its index in the session's files.
    File {
        /// The file's index in `PrSession::files`.
        index: u32,
        /// The full path, for the row's label and icon.
        path: String,
    },
}

/// Whether `haystack` contains `needle` as a case-insensitive subsequence.
pub fn fuzzy_hit(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    let mut cursor = 0usize;
    for ch in needle.chars() {
        match hay[cursor..].find(ch) {
            Some(at) => cursor += at + ch.len_utf8(),
            None => return false,
        }
    }
    true
}

/// The palette's rows: fuzzy-matched actions first, then fuzzy-matched
/// files by path (Task 14). Files only arrive while a PR is open.
pub fn palette_entries<'a>(
    actions: &'a [ActionEntry],
    files: &[PrFile],
    query: &str,
) -> Vec<PaletteEntry<'a>> {
    let mut out: Vec<PaletteEntry> = fuzzy_filter(actions, query)
        .into_iter()
        .map(PaletteEntry::Action)
        .collect();
    out.extend(
        files
            .iter()
            .enumerate()
            .filter(|(_, file)| fuzzy_hit(&file.path, query))
            .map(|(index, file)| PaletteEntry::File {
                index: index as u32,
                path: file.path.clone(),
            }),
    );
    out
}

pub fn fuzzy_filter<'a>(entries: &'a [ActionEntry], query: &str) -> Vec<&'a ActionEntry> {
    if query.is_empty() {
        return entries.iter().collect();
    }
    let needle = query.to_lowercase();
    let mut scored: Vec<(usize, usize, &ActionEntry)> = Vec::new();
    for entry in entries {
        let label = entry.label.to_lowercase();
        let mut cursor = 0usize;
        let mut first: Option<usize> = None;
        let mut matched = true;
        for ch in needle.chars() {
            match label[cursor..].find(ch) {
                Some(at) => {
                    let pos = cursor + at;
                    if first.is_none() {
                        first = Some(pos);
                    }
                    cursor = pos + ch.len_utf8();
                }
                None => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            scored.push((first.unwrap_or(0), entry.label.chars().count(), entry));
        }
    }
    scored.sort_by_key(|(first, length, entry)| (*first, *length, entry.id));
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

/// One keyboard event, reduced to what the resolver needs.
///
/// `key` is the egui key name (`"v"`, `"?"`, `"Enter"`, `"Escape"`, …);
/// `command` is ⌘ on macOS and Ctrl elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPress {
    /// The key itself; letters arrive lowercase unless shifted (`"V"`).
    pub key: String,
    /// Whether Shift was held.
    pub shift: bool,
    /// ⌘ on macOS, Ctrl elsewhere.
    pub command: bool,
}

/// The action this key press asks for, if any.
///
/// Unmodified single keys resolve **only** while no text edit is focused, so
/// typing `s` or `v` in a comment box never navigates the diff. `Cmd+K`,
/// `Cmd+Enter`, and `Escape` always resolve — the palette must open from a
/// focused text edit, a review must submit from the review body, and `Esc`
/// must close whatever is on top.
pub fn resolve_key(k: &KeyPress, text_focused: bool) -> Option<&'static str> {
    if k.command {
        return match k.key.as_str() {
            "k" | "K" => Some("palette"),
            "Enter" => Some("submit_review"),
            _ => None,
        };
    }
    if k.key == "Escape" {
        return Some("close");
    }
    if text_focused {
        return None;
    }
    if k.shift {
        return match k.key.as_str() {
            "V" | "v" => Some("mark_viewed_and_next"),
            _ => None,
        };
    }
    match k.key.as_str() {
        "j" => Some("next_file"),
        "k" => Some("prev_file"),
        "n" => Some("next_hunk"),
        "p" => Some("prev_hunk"),
        "v" => Some("toggle_viewed"),
        "c" => Some("comment"),
        "r" => Some("reply"),
        "o" => Some("toggle_side_pane"),
        "s" => Some("split"),
        "u" => Some("unified"),
        "?" => Some("help"),
        _ => None,
    }
}

/// The element of `order` to move to from `current`, clamped at both ends.
///
/// `order` holds the positions to walk (file indices for `j`/`k`, row indices
/// for `n`/`p`), in travel order. With no current position the first step
/// lands on the first element going forward and the last going back; a current
/// position outside `order` (a file the filters just hid) moves from where it
/// would sit. At either end the position stays put.
pub fn next_index(order: &[usize], current: Option<usize>, forward: bool) -> Option<usize> {
    let (&first, &last) = (order.first()?, order.last()?);
    let Some(current) = current else {
        return Some(if forward { first } else { last });
    };
    if let Some(pos) = order.iter().position(|&i| i == current) {
        return Some(if forward {
            order.get(pos + 1).copied().unwrap_or(last)
        } else {
            order[pos.saturating_sub(1)]
        });
    }
    // `current` is outside `order`: walk from where it would sit.
    let at = order
        .iter()
        .position(|&i| i > current)
        .unwrap_or(order.len());
    Some(if forward {
        order.get(at).copied().unwrap_or(last)
    } else {
        order[at.saturating_sub(1)]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: &str) -> KeyPress {
        KeyPress {
            key: k.into(),
            shift: false,
            command: false,
        }
    }

    #[test]
    fn palette_lists_actions_then_files() {
        use crate::test_support::files_at;
        let actions = registry();
        let files = files_at(&[
            "src/lib/data-tools/push.ts",
            "src/lib/notifications/schema.ts",
        ]);
        let e = palette_entries(&actions, &files, "push");
        assert!(matches!(
            e.last(),
            Some(PaletteEntry::File { index: 0, .. })
        ));
        assert!(palette_entries(&actions, &[], "split")
            .iter()
            .all(|x| matches!(x, PaletteEntry::Action(_))));
    }

    #[test]
    fn fuzzy_matches_subsequence_and_ranks() {
        let es = registry();
        assert!(fuzzy_filter(&es, "togview")
            .iter()
            .any(|e| e.id == "toggle_viewed"));
        assert!(fuzzy_filter(&es, "zzz").is_empty());
    }

    #[test]
    fn keys_resolve_only_without_text_focus() {
        assert_eq!(resolve_key(&key("v"), false), Some("toggle_viewed"));
        assert_eq!(resolve_key(&key("s"), true), None); // typing "s" in a comment
        assert_eq!(
            resolve_key(
                &KeyPress {
                    shift: true,
                    ..key("V")
                },
                false
            ),
            Some("mark_viewed_and_next")
        );
        assert_eq!(
            resolve_key(
                &KeyPress {
                    command: true,
                    ..key("K")
                },
                true
            ),
            Some("palette")
        );
        assert_eq!(
            resolve_key(
                &KeyPress {
                    command: true,
                    ..key("Enter")
                },
                true
            ),
            Some("submit_review")
        );
        assert_eq!(resolve_key(&key("Escape"), true), Some("close"));
        assert_eq!(resolve_key(&key("n"), false), Some("next_hunk"));
    }

    #[test]
    fn navigation_clamps() {
        let order = [3, 5, 9];
        assert_eq!(next_index(&order, None, true), Some(3));
        assert_eq!(next_index(&order, Some(5), true), Some(9));
        assert_eq!(next_index(&order, Some(9), true), Some(9));
        assert_eq!(next_index(&order, Some(3), false), Some(3));
    }

    #[test]
    fn navigation_walks_and_survives_a_hidden_current_position() {
        let order = [3, 5, 9];
        assert_eq!(next_index(&order, None, false), Some(9));
        assert_eq!(next_index(&order, Some(5), false), Some(3));
        // The current file was just filtered out: step from where it sits.
        assert_eq!(next_index(&order, Some(4), true), Some(5));
        assert_eq!(next_index(&order, Some(4), false), Some(3));
        assert_eq!(next_index(&order, Some(100), true), Some(9));
        assert_eq!(next_index(&[], None, true), None);
    }

    #[test]
    fn registry_covers_every_documented_action() {
        let ids: Vec<&str> = registry().iter().map(|e| e.id).collect();
        for id in [
            "palette",
            "help",
            "next_file",
            "prev_file",
            "next_hunk",
            "prev_hunk",
            "toggle_viewed",
            "mark_viewed_and_next",
            "comment",
            "reply",
            "toggle_side_pane",
            "split",
            "unified",
            "submit_review",
            "close",
        ] {
            assert!(ids.contains(&id), "registry is missing {id}");
        }
    }

    #[test]
    fn ranking_prefers_earlier_first_match_then_shorter_labels() {
        let entries = [
            ActionEntry {
                id: "far",
                label: "Go to file",
                keybinding: None,
            },
            ActionEntry {
                id: "near",
                label: "File properties",
                keybinding: None,
            },
        ];
        let hits = fuzzy_filter(&entries, "file");
        assert_eq!(hits[0].id, "near", "the label starting with f wins");
    }
}
