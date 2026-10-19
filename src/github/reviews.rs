//! PR reviews and the reviewer's server-side pending review (spec §7.2).
//!
//! A *server-side* pending review is one started in GitHub's web UI: it lives
//! on the server, blocks a local submit, and can only be submitted or
//! discarded through GitHub. The REST reviews endpoint lists it (and only the
//! viewer's own pending review) like any other review, with state `PENDING`
//! and no `submitted_at`; its comments come from a separate endpoint.

use serde::Deserialize;
use serde_json::json;

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::diff::Side;
use crate::model::review::ReviewEvent;
use crate::model::RepoRef;

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
        author: raw
            .user
            .and_then(|user| user.login)
            .unwrap_or_else(|| "ghost".to_owned()),
        state: ReviewState::from_wire(&raw.state)?,
        body: raw.body.unwrap_or_default(),
        submitted_at: raw.submitted_at,
    })
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
}

/// Map a body-read failure to [`GhError::Decode`].
fn decode(error: reqwest::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn finds_the_viewers_pending_review() {
        let rs = parse_reviews(fixture!("reviews_page.json")).unwrap();
        assert_eq!(rs.len(), 3);
        assert_eq!(rs[0].state, ReviewState::Approved);
        let p = viewer_pending_review(&rs, "me").unwrap();
        assert_eq!((p.id, p.state.clone()), (77, ReviewState::Pending));
        assert!(viewer_pending_review(&rs, "someone-else").is_none());
    }
}
