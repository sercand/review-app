//! The pending-review draft and the review verdict (spec §7.2, §7.3).
//!
//! A pending review is the local draft Phase F submits: a verdict, a summary
//! body, and line-anchored comments (file-level comments cannot be pending).
//! The draft is pinned to one commit, and its comment ids are local and
//! unique for the draft's whole life — the counter is persisted so a restart
//! never reissues an id. The mutation rules live in
//! [`crate::state::pending`]; this module is the persisted shape.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::model::comment::Anchor;

/// The verdict a review is submitted with.
///
/// GitHub accepts exactly `COMMENT`, `APPROVE` and `REQUEST_CHANGES`
/// ([`ReviewEvent::as_api_str`]); the PR author may not pick the latter two
/// (enforced where reviews are submitted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEvent {
    /// A review that comments without approving or blocking.
    #[default]
    Comment,
    /// Approve the pull request.
    Approve,
    /// Request changes on the pull request.
    RequestChanges,
}

impl ReviewEvent {
    /// The exact string the REST `event` field takes.
    pub fn as_api_str(&self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// A comment held in the pending review, anchored to a line or a range
/// (spec §7.3): pending comments are line anchors only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingComment {
    /// The draft-allocated local id; never 0, and never reused.
    pub local_id: u64,
    /// The line or range the comment sits on.
    pub anchor: Anchor,
    /// The comment body.
    pub body: String,
}

/// The whole pending-review draft, as persisted (spec §11).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingReview {
    /// The commit the draft is pinned to; `None` while it has no comments.
    pub commit_id: Option<String>,
    /// The review summary body.
    pub body: String,
    /// The verdict the draft will submit with.
    pub event: ReviewEvent,
    /// The pending comments, in the order they were added.
    pub comments: Vec<PendingComment>,
    /// When the draft was started, set by its first added comment.
    pub started_at: Option<SystemTime>,
    /// The last local id this draft issued; ids are allocated from it, so 0 —
    /// the reserved "no optimistic bubble" sentinel — is never issued.
    pub next_local_id: u64,
}

/// Why a draft mutation was refused.
#[derive(Debug, PartialEq)]
pub enum PendingError {
    /// The draft is pinned to another commit; submit or discard it first.
    PinnedToOtherCommit {
        /// The SHA the draft is pinned to.
        pinned: String,
    },
}
