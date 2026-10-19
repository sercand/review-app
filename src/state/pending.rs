//! The pending review draft's mutation rules (spec §7.2).
//!
//! [`crate::model::review::PendingReview`] is the persisted shape; this module
//! owns the rules that shape enforces: one draft pinned to one commit, and
//! local comment ids that stay unique for the draft's whole life (the counter
//! is persisted, so a restart never reissues an id).

use std::time::SystemTime;

use crate::model::comment::Anchor;
use crate::model::review::{PendingComment, PendingError, PendingReview, ReviewEvent};

impl PendingReview {
    /// Add a pending comment anchored to `anchor`, returning its local id.
    ///
    /// The first added comment pins the draft to `diff_head_sha`; adding
    /// against a different commit fails with
    /// [`PendingError::PinnedToOtherCommit`] so the reviewer submits or
    /// discards the pinned draft first (spec §7.3).
    pub fn add(
        &mut self,
        anchor: Anchor,
        body: String,
        diff_head_sha: &str,
    ) -> Result<u64, PendingError> {
        match &self.commit_id {
            Some(pinned) if pinned != diff_head_sha => {
                return Err(PendingError::PinnedToOtherCommit {
                    pinned: pinned.clone(),
                });
            }
            Some(_) => {}
            None => self.commit_id = Some(diff_head_sha.to_owned()),
        }
        if self.started_at.is_none() {
            self.started_at = Some(SystemTime::now());
        }
        // Increment first so the first issued id is 1: 0 is the reserved
        // "no optimistic bubble" sentinel.
        self.next_local_id += 1;
        let local_id = self.next_local_id;
        self.comments.push(PendingComment {
            local_id,
            anchor,
            body,
        });
        Ok(local_id)
    }

    /// Replace the body of the comment with `local_id`; whether it was found.
    pub fn edit(&mut self, local_id: u64, body: String) -> bool {
        match self.comments.iter_mut().find(|c| c.local_id == local_id) {
            Some(comment) => {
                comment.body = body;
                true
            }
            None => false,
        }
    }

    /// Drop the comment with `local_id`; whether anything was removed.
    ///
    /// Removing the last comment unpins the draft, so the next `add` pins to
    /// whatever head the reviewer is looking at now.
    pub fn delete(&mut self, local_id: u64) -> bool {
        let before = self.comments.len();
        self.comments.retain(|c| c.local_id != local_id);
        if self.comments.len() == before {
            return false;
        }
        if self.comments.is_empty() {
            self.commit_id = None;
        }
        true
    }

    /// Set the review summary body.
    pub fn set_body(&mut self, body: String) {
        self.body = body;
    }

    /// Set the verdict the draft will submit with.
    pub fn set_event(&mut self, event: ReviewEvent) {
        self.event = event;
    }

    /// Drop the whole draft: comments, body, verdict, and the pin.
    pub fn clear(&mut self) {
        *self = PendingReview::default();
    }

    /// Whether the draft has nothing to submit: no comments and a blank body.
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.body.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::model::review::{PendingError, PendingReview, ReviewEvent};
    use crate::test_support::line_anchor;

    #[test]
    fn add_edit_delete_and_unique_ids() {
        let mut p = PendingReview::default();
        let a = p
            .add(line_anchor("a.rs", 1), "first".into(), "sha1")
            .unwrap();
        let b = p
            .add(line_anchor("a.rs", 2), "second".into(), "sha1")
            .unwrap();
        assert_ne!(a, b);
        assert!(p.edit(a, "edited".into()));
        assert_eq!(p.comments[0].body, "edited");
        assert!(p.delete(a) && p.delete(b));
        assert!(p.is_empty());
        assert!(!p.delete(a));
    }

    #[test]
    fn draft_is_pinned_to_one_commit() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
        assert_eq!(p.commit_id.as_deref(), Some("sha1"));
        assert_eq!(
            p.add(line_anchor("a.rs", 2), "y".into(), "sha2"),
            Err(PendingError::PinnedToOtherCommit {
                pinned: "sha1".into()
            })
        );
        let id = p.comments[0].local_id;
        p.delete(id);
        assert!(p.commit_id.is_none()); // empty draft unpins
        assert!(p.add(line_anchor("a.rs", 2), "y".into(), "sha2").is_ok());
    }

    #[test]
    fn review_event_serializes_exactly() {
        assert_eq!(ReviewEvent::Comment.as_api_str(), "COMMENT");
        assert_eq!(ReviewEvent::Approve.as_api_str(), "APPROVE");
        assert_eq!(ReviewEvent::RequestChanges.as_api_str(), "REQUEST_CHANGES");
    }
}
