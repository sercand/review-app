//! Commits on a PR: `GET /repos/{o}/{r}/pulls/{n}/commits`.
//!
//! The list is what the right-pane commits tab shows (Task 50). A commit's
//! author is the GitHub login when the account exists and the git author name
//! when it does not — GitHub sends `"author": null` for commits pushed with
//! an unlinked email, and the name is the best label there is.

use serde::Deserialize;

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::RepoRef;

/// One commit on a PR, as the commits endpoint lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    /// The commit SHA.
    pub sha: String,
    /// The full commit message; the first line is the subject.
    pub message: String,
    /// The author's login, falling back to the git author name.
    pub author: String,
    /// When it was committed (ISO 8601, as GitHub sent it).
    pub committed_at: String,
}

/// Parse a `GET /repos/{o}/{r}/pulls/{n}/commits` body.
pub fn parse_commits(body: &str) -> Result<Vec<Commit>, GhError> {
    let raws: Vec<RawCommit> = serde_json::from_str(body).map_err(decode)?;
    Ok(raws.into_iter().map(to_commit).collect())
}

impl GithubClient {
    /// Every commit on PR `number`, in API order (oldest first), following
    /// pagination.
    pub async fn list_commits(&self, repo: &RepoRef, number: u64) -> Result<Vec<Commit>, GhError> {
        let path = format!(
            "/repos/{}/{}/pulls/{number}/commits?per_page=100",
            repo.owner, repo.name
        );
        let raws = self.get_all_pages::<RawCommit>(&path).await?;
        Ok(raws.into_iter().map(to_commit).collect())
    }
}

/// One entry of the commits list.
#[derive(Deserialize)]
struct RawCommit {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    commit: RawGitCommit,
    #[serde(default)]
    author: Option<RawLogin>,
}

/// The nested `commit` object: the git metadata.
#[derive(Default, Deserialize)]
struct RawGitCommit {
    #[serde(default)]
    message: String,
    #[serde(default)]
    author: Option<RawGitAuthor>,
    #[serde(default)]
    committer: Option<RawGitAuthor>,
}

/// A git author or committer.
#[derive(Deserialize)]
struct RawGitAuthor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

/// The `login` of a GitHub user.
#[derive(Deserialize)]
struct RawLogin {
    #[serde(default)]
    login: Option<String>,
}

/// Convert one raw entry: the login names the author when GitHub has one,
/// the git author name when the account is `null`.
fn to_commit(raw: RawCommit) -> Commit {
    let login = raw.author.and_then(|author| author.login);
    let git_author = raw.commit.author;
    let committed_at = raw
        .commit
        .committer
        .and_then(|committer| committer.date)
        .or_else(|| git_author.as_ref().and_then(|author| author.date.clone()))
        .unwrap_or_default();
    Commit {
        sha: raw.sha,
        message: raw.commit.message,
        author: login
            .or_else(|| git_author.and_then(|author| author.name))
            .unwrap_or_else(|| "ghost".to_owned()),
        committed_at,
    }
}

/// Map a JSON decode failure to [`GhError::Decode`].
fn decode(error: serde_json::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn parses_commits() {
        let cs = parse_commits(fixture!("commits_page.json")).unwrap();
        assert_eq!(cs.len(), 2);
        assert!(cs[0].message.starts_with("Fix"));
        assert_eq!(cs[1].author, "Mona Lisa"); // null GitHub user
    }
}
