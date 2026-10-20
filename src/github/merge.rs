//! Merging a PR: `PUT /repos/{owner}/{repo}/pulls/{number}/merge` plus the
//! repository's merge-method allow flags.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::RepoRef;

/// How GitHub combines the PR's commits into the base branch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    /// A merge commit joining head and base.
    #[default]
    Merge,
    /// One commit with all the PR's changes.
    Squash,
    /// The PR's commits replayed onto the base.
    Rebase,
}

impl MergeMethod {
    /// The REST `merge_method` value.
    pub fn as_api_str(self) -> &'static str {
        match self {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        }
    }
}

/// Everything the merge endpoint takes, fixed at click time.
#[derive(Clone, Debug)]
pub struct MergeRequest {
    /// The head SHA the reviewer saw; GitHub refuses if the head moved.
    pub sha: String,
    /// How to combine the commits.
    pub method: MergeMethod,
    /// The commit title (first line).
    pub commit_title: String,
    /// The commit body; omitted from the payload when empty.
    pub commit_message: String,
}

/// The REST body for `PUT .../pulls/{number}/merge`.
///
/// `sha` makes the merge conditional on the head the user reviewed, so a head
/// that moved underneath them fails with 409 instead of merging surprises.
pub fn merge_payload(r: &MergeRequest) -> Value {
    let mut payload = json!({
        "sha": r.sha,
        "merge_method": r.method.as_api_str(),
        "commit_title": r.commit_title,
    });
    if !r.commit_message.is_empty() {
        payload["commit_message"] = json!(r.commit_message);
    }
    payload
}

/// What the merge endpoint reports back.
#[derive(Clone, Debug, PartialEq)]
pub struct MergeResult {
    /// The SHA of the merge commit (or squashed/rebased commit) on the base.
    pub sha: String,
    /// Whether the merge happened.
    pub merged: bool,
    /// GitHub's message, e.g. "Pull Successfully merged".
    pub message: String,
}

/// `PUT .../merge` as GitHub sends it.
#[derive(Deserialize)]
struct WireMergeResult {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    message: String,
}

impl GithubClient {
    /// Merge PR `number` with a body built by [`merge_payload`].
    ///
    /// A mutation: sent once through the non-retrying path. GitHub answers
    /// 405 (blocked by branch protection) or 409 (head moved / unmergeable)
    /// with a human-readable message, preserved in the returned [`GhError`].
    pub async fn merge_pr(
        &self,
        repo: &RepoRef,
        number: u64,
        payload: &Value,
    ) -> Result<MergeResult, GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/merge",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        let response = self
            .send_mutation(self.http().put(&url).json(payload))
            .await?;
        let raw: WireMergeResult = response.json().await.map_err(decode)?;
        Ok(MergeResult {
            sha: raw.sha,
            merged: raw.merged,
            message: raw.message,
        })
    }
}

/// The repository's merge-method allow flags (`GET /repos/{o}/{r}`).
///
/// Missing flags load as allowed: a host that does not report them should not
/// lock the user out of a method that would in fact work.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoSettings {
    /// Whether "Create a merge commit" is offered.
    pub allow_merge_commit: bool,
    /// Whether "Squash and merge" is offered.
    pub allow_squash_merge: bool,
    /// Whether "Rebase and merge" is offered.
    pub allow_rebase_merge: bool,
}

impl RepoSettings {
    /// Whether `method` may be used in this repository.
    pub fn allows(&self, method: MergeMethod) -> bool {
        match method {
            MergeMethod::Merge => self.allow_merge_commit,
            MergeMethod::Squash => self.allow_squash_merge,
            MergeMethod::Rebase => self.allow_rebase_merge,
        }
    }
}

/// `GET /repos/{o}/{r}` down to the merge allow flags.
#[derive(Deserialize)]
struct WireRepoSettings {
    #[serde(default = "always")]
    allow_merge_commit: bool,
    #[serde(default = "always")]
    allow_squash_merge: bool,
    #[serde(default = "always")]
    allow_rebase_merge: bool,
}

fn always() -> bool {
    true
}

impl GithubClient {
    /// The repository's merge-method allow flags. A read; retried like any other.
    pub async fn repo_settings(&self, repo: &RepoRef) -> Result<RepoSettings, GhError> {
        let path = format!("/repos/{}/{}", repo.owner, repo.name);
        let (raw, _link) = self.get_json::<WireRepoSettings>(&path).await?;
        Ok(RepoSettings {
            allow_merge_commit: raw.allow_merge_commit,
            allow_squash_merge: raw.allow_squash_merge,
            allow_rebase_merge: raw.allow_rebase_merge,
        })
    }
}

/// Map a body-read failure to [`GhError::Decode`].
fn decode(error: reqwest::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_payload_sets_sha_method_and_message() {
        let payload = merge_payload(&MergeRequest {
            sha: "abc123".into(),
            method: MergeMethod::Squash,
            commit_title: "Add feature (#7)".into(),
            commit_message: "details".into(),
        });
        assert_eq!(
            payload,
            json!({
                "sha": "abc123",
                "merge_method": "squash",
                "commit_title": "Add feature (#7)",
                "commit_message": "details",
            })
        );
    }

    #[test]
    fn merge_payload_omits_an_empty_commit_message() {
        let payload = merge_payload(&MergeRequest {
            sha: "abc123".into(),
            method: MergeMethod::Merge,
            commit_title: "Merge pull request #7 into main".into(),
            commit_message: String::new(),
        });
        assert_eq!(payload.get("commit_message"), None);
        assert_eq!(payload["merge_method"], "merge");
    }
}
