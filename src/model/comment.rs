//! Where a review comment is anchored (spec §7.3).
//!
//! A line comment carries the path as it appears in the diff, a side, and a
//! line number; a multi-line comment adds `start_line`/`start_side`. File-level
//! comments have no coordinates at all.

use serde::{Deserialize, Serialize};

use crate::model::diff::Side;

/// A line comment's coordinate, exactly as GitHub's API takes it.
///
/// `start_line`/`start_side` are `None` for a single-line comment, and a
/// one-row "range" is sent as a single-line comment (spec §7.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// The API `filename` of the file being commented on.
    pub path: String,
    /// The side the comment attaches to.
    pub side: Side,
    /// The line the comment ends on.
    pub line: u32,
    /// The line the comment starts on, for a range.
    pub start_line: Option<u32>,
    /// The side the range starts on; always equal to `side`.
    pub start_side: Option<Side>,
}

/// What a new comment is attached to.
#[derive(Clone, Debug, PartialEq)]
pub enum CommentTarget {
    /// An inline comment on one line or a range of lines.
    Line(Anchor),
    /// A file-level comment, which needs no coordinates and is never pending.
    File {
        /// The API `filename` of the file being commented on.
        path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_serialize_as_github_expects() {
        let range = Anchor {
            path: "src/a.rs".into(),
            side: Side::Right,
            line: 14,
            start_line: Some(10),
            start_side: Some(Side::Right),
        };
        let json = serde_json::to_value(&range).unwrap();
        assert_eq!(json["side"], "RIGHT");
        assert_eq!(json["start_side"], "RIGHT");
        assert_eq!(json["line"], 14);
        assert_eq!(json["start_line"], 10);

        // sides serialize as LEFT/RIGHT, and an anchor round-trips (drafts are
        // persisted as JSON, spec §11)
        let single = Anchor {
            path: "src/old name.rs".into(),
            side: Side::Left,
            line: 3,
            start_line: None,
            start_side: None,
        };
        assert_eq!(serde_json::to_value(&single).unwrap()["side"], "LEFT");
        let back: Anchor = serde_json::from_str(&serde_json::to_string(&single).unwrap()).unwrap();
        assert_eq!(back, single);
    }
}
