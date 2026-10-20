//! PR reviews and the reviewer's server-side pending review (spec §7.2).
//!
//! A *server-side* pending review is one started in GitHub's web UI: it lives
//! on the server, blocks a local submit, and can only be submitted or
//! discarded through GitHub. The REST reviews endpoint lists it (and only the
//! viewer's own pending review) like any other review, with state `PENDING`
//! and no `submitted_at`; its comments come from a separate endpoint.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::RepoRef;
use crate::model::diff::Side;
use crate::model::review::{PendingReview, ReviewEvent};

/// The state a review is in, as GitHub reports it.
#[derive(Clone, Debug, PartialEq)]
pub enum ReviewState {
    /// A submitted approving review.
    Approved,
    /// A submitted blocking review.
    ChangesRequested,
    /// A submitted commenting-only review.
    Commented,
    /// An approval dismissed by a new commit.
    Dismissed,
    /// Not submitted yet: the viewer's draft on the server.
    Pending,
}

impl ReviewState {
    /// Map a REST `state` string; `None` for a state this build does not
    /// know, so the review is skipped rather than shown wrong.
    fn from_wire(state: &str) -> Option<ReviewState> {
        Some(match state {
            "APPROVED" => ReviewState::Approved,
            "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
            "COMMENTED" => ReviewState::Commented,
            "DISMISSED" => ReviewState::Dismissed,
            "PENDING" => ReviewState::Pending,
            _ => return None,
        })
    }
}

/// One review of a PR, submitted or server-pending.
#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    /// The REST review id.
    pub id: u64,
    /// The GraphQL node id, which `addPullRequestReviewThread` takes to add
    /// a comment to a pending review.
    pub node_id: String,
    /// The reviewer's login.
    pub author: String,
    /// The state the review is in.
    pub state: ReviewState,
    /// The review body.
    pub body: String,
    /// When it was submitted; `None` while it is pending.
    pub submitted_at: Option<String>,
}

/// A comment attached to a server-side pending review, fetched so it can
/// render inline in the pending style labelled "pending on GitHub".
#[derive(Clone, Debug, PartialEq)]
pub struct ServerPendingComment {
    /// The API `filename` the comment is anchored to.
    pub path: String,
    /// The line it ends on; `None` for a file-level comment.
    pub line: Option<u32>,
    /// The side it anchors to.
    pub side: Option<Side>,
    /// The line a range starts on.
    pub start_line: Option<u32>,
    /// The comment body.
    pub body: String,
}

/// The viewer's own pending review, if they have one; GitHub only ever lists
/// the viewer's pending review, but the login keeps the match explicit.
pub fn viewer_pending_review<'a>(reviews: &'a [Review], login: &str) -> Option<&'a Review> {
    reviews
        .iter()
        .find(|review| review.state == ReviewState::Pending && review.author == login)
}

/// Parse a REST reviews-list body (as `GET .../pulls/{n}/reviews` returns it)
/// into reviews, skipping entries in a state this build does not know.
pub fn parse_reviews(body: &str) -> Result<Vec<Review>, GhError> {
    let raws: Vec<RawReview> =
        serde_json::from_str(body).map_err(|error| GhError::Decode(error.to_string()))?;
    Ok(raws.into_iter().filter_map(to_review).collect())
}

#[derive(Deserialize)]
struct RawReview {
    id: u64,
    #[serde(default)]
    node_id: String,
    user: Option<RawUser>,
    state: String,
    body: Option<String>,
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct RawUser {
    login: Option<String>,
}

#[derive(Deserialize)]
struct RawPendingComment {
    #[serde(default)]
    path: String,
    line: Option<u32>,
    side: Option<Side>,
    start_line: Option<u32>,
    body: Option<String>,
}

/// Convert a raw review, dropping one whose state this build does not know.
fn to_review(raw: RawReview) -> Option<Review> {
    Some(Review {
        id: raw.id,
        node_id: raw.node_id,
        author: raw
            .user
            .and_then(|user| user.login)
            .unwrap_or_else(|| "ghost".to_owned()),
        state: ReviewState::from_wire(&raw.state)?,
        body: raw.body.unwrap_or_default(),
        submitted_at: raw.submitted_at,
    })
}

/// The REST body for `POST .../pulls/{number}/reviews` (spec §7.2).
///
/// `commit_id` is the draft's pin when it has one — the comments were written
/// against that commit — and `head_sha` only when the draft has no comments
/// and so no pin. `comments` is omitted entirely when empty, and each comment
/// carries no per-comment `commit_id`: the review's own `commit_id` covers
/// them all.
pub fn review_payload(p: &PendingReview, head_sha: &str) -> Value {
    let mut payload = json!({
        "commit_id": p.commit_id.clone().unwrap_or_else(|| head_sha.to_owned()),
        "body": p.body,
        "event": p.event.as_api_str(),
    });
    if !p.comments.is_empty() {
        let comments: Vec<Value> = p
            .comments
            .iter()
            .map(|c| {
                let mut comment = json!({
                    "path": c.anchor.path,
                    "line": c.anchor.line,
                    "side": c.anchor.side,
                    "body": c.body,
                });
                if let Some(start_line) = c.anchor.start_line {
                    comment["start_line"] = json!(start_line);
                }
                if let Some(start_side) = c.anchor.start_side {
                    comment["start_side"] = json!(start_side);
                }
                comment
            })
            .collect();
        payload["comments"] = Value::Array(comments);
    }
    payload
}

impl GithubClient {
    /// Every review of PR `number`, in API order, oldest first.
    ///
    /// GitHub lists the viewer's own pending review among them (state
    /// `PENDING`); nobody else's pending reviews are visible.
    pub async fn list_reviews(&self, repo: &RepoRef, number: u64) -> Result<Vec<Review>, GhError> {
        let path = format!(
            "/repos/{}/{}/pulls/{number}/reviews?per_page=100",
            repo.owner, repo.name
        );
        let raws = self.get_all_pages::<RawReview>(&path).await?;
        Ok(raws.into_iter().filter_map(to_review).collect())
    }

    /// The comments attached to a (pending) review, for rendering it inline.
    pub async fn pending_review_comments(
        &self,
        repo: &RepoRef,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<ServerPendingComment>, GhError> {
        let path = format!(
            "/repos/{}/{}/pulls/{number}/reviews/{review_id}/comments?per_page=100",
            repo.owner, repo.name
        );
        let raws = self.get_all_pages::<RawPendingComment>(&path).await?;
        Ok(raws
            .into_iter()
            .map(|comment| ServerPendingComment {
                path: comment.path,
                line: comment.line,
                side: comment.side,
                start_line: comment.start_line,
                body: comment.body.unwrap_or_default(),
            })
            .collect())
    }

    /// Submit a server-side pending review with `event` and `body`.
    ///
    /// A mutation: sent once through the non-retrying path, returning the
    /// review as GitHub now has it (no longer pending).
    pub async fn submit_server_pending(
        &self,
        repo: &RepoRef,
        number: u64,
        review_id: u64,
        event: ReviewEvent,
        body: &str,
    ) -> Result<Review, GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/reviews/{review_id}/events",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        let response = self
            .send_mutation(
                self.http()
                    .post(&url)
                    .json(&json!({ "event": event.as_api_str(), "body": body })),
            )
            .await?;
        let raw: RawReview = response.json().await.map_err(decode)?;
        to_review(raw).ok_or_else(|| GhError::Decode("review: unknown state".to_owned()))
    }

    /// Discard a server-side pending review, deleting it and its comments.
    ///
    /// A mutation: sent once, never retried.
    pub async fn delete_server_pending(
        &self,
        repo: &RepoRef,
        number: u64,
        review_id: u64,
    ) -> Result<(), GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/reviews/{review_id}",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        self.send_mutation(self.http().delete(&url)).await?;
        Ok(())
    }

    /// Submit a review built by [`review_payload`].
    ///
    /// A mutation: sent once through the non-retrying path, returning the
    /// review as GitHub created it.
    pub async fn submit_review(
        &self,
        repo: &RepoRef,
        number: u64,
        payload: &Value,
    ) -> Result<Review, GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/reviews",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        let response = self
            .send_mutation(self.http().post(&url).json(payload))
            .await?;
        let raw: RawReview = response.json().await.map_err(decode)?;
        to_review(raw).ok_or_else(|| GhError::Decode("review: unknown state".to_owned()))
    }
}

/// Map a body-read failure to [`GhError::Decode`].
fn decode(error: reqwest::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::model::review::{PendingReview, ReviewEvent};
    use crate::test_support::{line_anchor, range_anchor};

    #[test]
    fn finds_the_viewers_pending_review() {
        let rs = parse_reviews(fixture!("reviews_page.json")).unwrap();
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0].state, ReviewState::Approved);
        let p = viewer_pending_review(&rs, "me").unwrap();
        assert_eq!((p.id, p.state.clone()), (77, ReviewState::Pending));
        assert!(viewer_pending_review(&rs, "someone-else").is_none());
    }

    #[test]
    fn payload_uses_the_pinned_commit_and_ranges() {
        let mut p = PendingReview::default();
        p.add(line_anchor("src/a.rs", 42), "b1".into(), "sha1")
            .unwrap();
        p.add(range_anchor("src/b.rs", 10, 14), "b2".into(), "sha1")
            .unwrap();
        p.set_body("overall".into());
        p.set_event(ReviewEvent::RequestChanges);
        let v = review_payload(&p, "sha2"); // head moved; the pin wins
        assert_eq!(
            v,
            json!({
            "commit_id": "sha1", "body": "overall", "event": "REQUEST_CHANGES",
            "comments": [
                {"path": "src/a.rs", "line": 42, "side": "RIGHT", "body": "b1"},
                {"path": "src/b.rs", "start_line": 10, "start_side": "RIGHT", "line": 14, "side": "RIGHT", "body": "b2"}
            ]})
        );

        let mut approve = PendingReview::default();
        approve.set_event(ReviewEvent::Approve);
        assert_eq!(
            review_payload(&approve, "sha2"),
            json!({"commit_id": "sha2", "body": "", "event": "APPROVE"})
        );
    }
}
