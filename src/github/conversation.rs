//! The PR conversation: issue comments and referenced issues (Task 51).
//!
//! The conversation is GitHub's issue-comment stream on a PR — distinct
//! from the inline review threads — and the PR body's `#N` and
//! `owner/repo#N` tokens name the issues the PR references.

use serde::Deserialize;

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::RepoRef;

/// A `#N` or `owner/repo#N` reference found in a PR body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRef {
    /// The repository the issue lives in; `None` is the PR's own repository.
    pub repo: Option<String>,
    /// The issue number within that repository.
    pub number: u64,
}

/// One conversation (issue) comment on a PR.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationComment {
    /// The commenter's login, or `"ghost"` when GitHub reports none.
    pub author: String,
    /// The comment body.
    pub body: String,
    /// When it was created (ISO 8601, as GitHub sent it).
    pub created_at: String,
}

/// Parse a `GET /repos/{o}/{r}/issues/{n}/comments` body.
pub fn parse_issue_comments(body: &str) -> Result<Vec<ConversationComment>, GhError> {
    let raws: Vec<RawIssueComment> = serde_json::from_str(body).map_err(decode)?;
    Ok(raws.into_iter().map(to_comment).collect())
}

/// Find the `#N` and `owner/repo#N` references in `body`, de-duplicated in
/// order of first appearance.
///
/// A `#` only starts a reference at the start of a line or after whitespace
/// or `(` — so `abc#5` and URL fragments are not references — unless the
/// token before it is exactly `owner/repo`, GitHub's cross-repository form.
pub fn referenced_issues(body: &str) -> Vec<IssueRef> {
    let mut found: Vec<IssueRef> = Vec::new();
    for (hash, _) in body.match_indices('#') {
        if let Some(issue) = reference_at(body, hash) {
            if !found.contains(&issue) {
                found.push(issue);
            }
        }
    }
    found
}

/// The reference one `#` at `hash` starts, when it starts one.
fn reference_at(body: &str, hash: usize) -> Option<IssueRef> {
    let digits_end = body[hash + 1..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(body.len(), |offset| hash + 1 + offset);
    let number = body[hash + 1..digits_end].parse::<u64>().ok()?;

    let start = token_start(body, hash);
    let token = &body[start..hash];
    let at_boundary =
        start == 0 || is_boundary(body[..start].chars().next_back().expect("start > 0"));
    if token.is_empty() {
        // A bare `#N`: a reference only at a word boundary.
        return at_boundary.then_some(IssueRef { repo: None, number });
    }
    // The cross-repository form: exactly `owner/repo`, both parts named,
    // starting at a word boundary. Anything longer (`a/b/c#1`, a URL) is
    // not a reference.
    let (owner, repo) = token.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') || !at_boundary {
        return None;
    }
    Some(IssueRef {
        repo: Some(token.to_owned()),
        number,
    })
}

/// The byte index where the run of path characters ending at `end` starts.
fn token_start(body: &str, end: usize) -> usize {
    let mut start = end;
    while let Some(c) = body[..start].chars().next_back() {
        if !is_path_char(c) {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

/// Whether `c` can appear inside an `owner/repo` token.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')
}

/// Whether a reference may start right after `c`.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || c == '('
}

impl GithubClient {
    /// Every conversation (issue) comment on PR `number`, oldest first,
    /// following pagination.
    pub async fn list_issue_comments(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ConversationComment>, GhError> {
        let path = format!(
            "/repos/{}/{}/issues/{number}/comments?per_page=100",
            repo.owner, repo.name
        );
        let raws = self.get_all_pages::<RawIssueComment>(&path).await?;
        Ok(raws.into_iter().map(to_comment).collect())
    }
}

/// One issue comment as GitHub sends it.
#[derive(Deserialize)]
struct RawIssueComment {
    #[serde(default)]
    user: Option<RawLogin>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    created_at: String,
}

/// The `login` of a GitHub user.
#[derive(Deserialize)]
struct RawLogin {
    #[serde(default)]
    login: Option<String>,
}

/// Convert one raw comment; a deleted account reads as `"ghost"`.
fn to_comment(raw: RawIssueComment) -> ConversationComment {
    ConversationComment {
        author: raw
            .user
            .and_then(|user| user.login)
            .unwrap_or_else(|| "ghost".to_owned()),
        body: raw.body.unwrap_or_default(),
        created_at: raw.created_at,
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
    fn extracts_referenced_issues() {
        assert_eq!(
            referenced_issues("Fixes #12 and closes acme/api#34; see #12 again, not abc#5"),
            vec![
                IssueRef {
                    repo: None,
                    number: 12
                },
                IssueRef {
                    repo: Some("acme/api".into()),
                    number: 34
                },
            ]
        );
    }

    #[test]
    fn ignores_url_fragments_and_keeps_parenthesised_refs() {
        assert!(referenced_issues("see https://github.com/o/r/pull/12#discussion_r1").is_empty());
        assert_eq!(
            referenced_issues("blockers: (#7)"),
            vec![IssueRef {
                repo: None,
                number: 7
            }]
        );
    }

    #[test]
    fn parses_issue_comments() {
        let cs = parse_issue_comments(fixture!("issue_comments.json")).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].author, "alice");
        assert_eq!(cs[0].created_at, "2026-10-01T09:00:00Z");
        assert_eq!(cs[1].created_at, "2026-10-03T09:00:00Z");
    }
}
