//! Review threads: the single source of inline comments (spec §7.4).
//!
//! A thread groups the root comment and its replies, and carries the anchor
//! (path, line, side) the comments hang off — or just the path for a
//! file-level thread.

use crate::model::diff::Side;

/// One comment inside a review thread.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadComment {
    /// The comment's REST database id (GraphQL `databaseId`).
    pub database_id: u64,
    /// The author's login, `ghost` when GitHub reports no author.
    pub author: String,
    /// The author's avatar URL, when GitHub reports one.
    pub avatar_url: Option<String>,
    /// The comment body.
    pub body: String,
    /// `createdAt`, verbatim as GitHub sent it.
    pub created_at: String,
}

/// What a review thread is anchored to.
#[derive(Clone, Debug, PartialEq)]
pub enum SubjectType {
    /// A line or line range in the diff.
    Line,
    /// The file as a whole; the thread has no coordinates.
    File,
}

/// One GitHub review thread: its state, its anchor, and its comments.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewThread {
    /// The GraphQL node id, used by the resolve mutations.
    pub id: String,
    /// Whether the reviewer resolved the thread.
    pub is_resolved: bool,
    /// Whether the anchored line moved out from under the thread.
    pub is_outdated: bool,
    /// The API `filename` the thread is anchored to.
    pub path: String,
    /// The line the thread ends on, absent for file-level threads and for
    /// outdated threads whose line no longer exists.
    pub line: Option<u32>,
    /// The line the thread starts on, for a range.
    pub start_line: Option<u32>,
    /// The side the thread is anchored to.
    pub side: Option<Side>,
    /// Whether this is a line or a file-level thread.
    pub subject_type: SubjectType,
    /// The thread's comments, root first, in API order.
    pub comments: Vec<ThreadComment>,
}

impl ReviewThread {
    /// The database id of the thread's root comment, for `POST …/replies`.
    pub fn root_comment_id(&self) -> Option<u64> {
        self.comments.first().map(|comment| comment.database_id)
    }
}
