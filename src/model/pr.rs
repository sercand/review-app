//! Pull-request summaries as GitHub's GraphQL `search` returns them.

use crate::model::RepoRef;

/// The combined state of a PR head's checks, from
/// `statusCheckRollup.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiState {
    /// Checks are still running.
    Pending,
    /// Every check passed.
    Success,
    /// At least one check failed or errored.
    Failure,
}

impl CiState {
    /// Map a GraphQL `StatusState` to a [`CiState`].
    ///
    /// `SUCCESS` is a success; `FAILURE` and `ERROR` are failures; `PENDING`
    /// and `EXPECTED` are pending. Anything else (including `null`) is unknown.
    pub fn from_rollup(state: &str) -> Option<CiState> {
        match state {
            "SUCCESS" => Some(CiState::Success),
            "FAILURE" | "ERROR" => Some(CiState::Failure),
            "PENDING" | "EXPECTED" => Some(CiState::Pending),
            _ => None,
        }
    }
}

/// GitHub's review decision for a PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// At least one approving review, none requesting changes.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Reviews are required but not yet complete.
    ReviewRequired,
}

impl ReviewDecision {
    /// Map a GraphQL `PullRequestReviewDecision` to a [`ReviewDecision`].
    pub fn from_wire(value: &str) -> Option<ReviewDecision> {
        match value {
            "APPROVED" => Some(ReviewDecision::Approved),
            "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
            "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
            _ => None,
        }
    }
}

/// One row of the PR list.
#[derive(Clone, Debug, PartialEq)]
pub struct PrSummary {
    /// The repository the PR lives in.
    pub repo: RepoRef,
    /// The PR number within the repository.
    pub number: u64,
    /// The PR title.
    pub title: String,
    /// The author's login, or `"ghost"` when GitHub reports none.
    pub author: String,
    /// Whether the PR is a draft.
    pub draft: bool,
    /// When the PR was last updated (ISO 8601, as GitHub sent it).
    pub updated_at: String,
    /// When the PR was created (ISO 8601, as GitHub sent it).
    pub created_at: String,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// The number of comments on the PR.
    pub comments: u32,
    /// The combined check state, when GitHub reports one.
    pub ci: Option<CiState>,
    /// The review decision, when GitHub reports one.
    pub review: Option<ReviewDecision>,
    /// The PR's web URL.
    pub url: String,
}
