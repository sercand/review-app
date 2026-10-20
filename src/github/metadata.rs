//! PR metadata: `GET /repos/{owner}/{repo}/pulls/{number}`.
//!
//! The fields the detail screen needs to *anchor* a comment — the head SHA
//! (spec §7.3), the base SHA, and the head/base refs — plus the header's
//! summary information.

use serde::Deserialize;

use crate::github::error::GhError;

/// The fields of a PR the detail screen keeps.
///
/// Deserializes from the API's wire shape: the head and base are nested
/// objects, and reviewers are split into `requested_reviewers` (users) and
/// `requested_teams` (teams).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(from = "WireMetadata")]
pub struct PrMetadata {
    /// The API `node_id`.
    pub node_id: String,
    /// The PR number within its repository.
    pub number: u64,
    /// The PR title.
    pub title: String,
    /// The PR body; a `null` body becomes the empty string.
    pub body: String,
    /// The author's login, or `"ghost"` when GitHub reports none.
    pub author: String,
    /// `open` or `closed`.
    pub state: String,
    /// Whether the PR is a draft.
    pub draft: bool,
    /// The base (target) branch name.
    pub base_ref: String,
    /// The base branch tip's SHA.
    pub base_sha: String,
    /// The head (source) branch name.
    pub head_ref: String,
    /// The head SHA a comment must be anchored to (spec §7.3).
    pub head_sha: String,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// How many files the PR changes.
    pub changed_files: u32,
    /// Requested reviewers: user logins first, then `team:<slug>`.
    pub requested_reviewers: Vec<String>,
    /// Label names, in GitHub's order.
    pub labels: Vec<String>,
    /// The milestone title, when the PR has one.
    pub milestone: Option<String>,
    /// The PR's web URL.
    pub html_url: String,
    /// Whether GitHub has decided the PR can be merged; `None` while it is
    /// still computing mergeability.
    pub mergeable: Option<bool>,
    /// The legacy mergeability verdict, e.g. `clean`, `blocked`, `dirty`;
    /// empty when unknown.
    pub mergeable_state: String,
    /// The current mergeability verdict (`merge_state_status`), which
    /// supersedes `mergeable_state` when non-empty; empty when unknown.
    pub merge_state_status: String,
    /// Whether the PR was merged (`merged_at` is set).
    pub merged: bool,
}

/// `GET /repos/{o}/{r}/pulls/{n}` as GitHub sends it.
#[derive(Deserialize)]
struct WireMetadata {
    #[serde(default)]
    node_id: String,
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<WireLogin>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    base: WireBranch,
    #[serde(default)]
    head: WireBranch,
    #[serde(default)]
    additions: u32,
    #[serde(default)]
    deletions: u32,
    #[serde(default)]
    changed_files: u32,
    #[serde(default)]
    requested_reviewers: Vec<WireLogin>,
    #[serde(default)]
    requested_teams: Vec<WireSlug>,
    #[serde(default)]
    labels: Vec<WireLabel>,
    #[serde(default)]
    milestone: Option<WireMilestone>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: String,
    #[serde(default)]
    merge_state_status: String,
    #[serde(default)]
    merged_at: Option<String>,
}

/// One branch (`base` or `head`).
#[derive(Default, Deserialize)]
struct WireBranch {
    #[serde(default, rename = "ref")]
    r#ref: String,
    #[serde(default)]
    sha: String,
}

/// The `login` of a user, wherever GitHub nests one.
#[derive(Deserialize)]
struct WireLogin {
    #[serde(default)]
    login: Option<String>,
}

/// The `slug` of a team.
#[derive(Deserialize)]
struct WireSlug {
    #[serde(default)]
    slug: Option<String>,
}

/// The `name` of a label.
#[derive(Deserialize)]
struct WireLabel {
    #[serde(default)]
    name: Option<String>,
}

/// The `title` of a milestone.
#[derive(Deserialize)]
struct WireMilestone {
    #[serde(default)]
    title: String,
}

impl From<WireMetadata> for PrMetadata {
    fn from(meta: WireMetadata) -> PrMetadata {
        let mut requested_reviewers: Vec<String> = meta
            .requested_reviewers
            .into_iter()
            .filter_map(|user| user.login)
            .collect();
        requested_reviewers.extend(
            meta.requested_teams
                .into_iter()
                .filter_map(|team| team.slug)
                .map(|slug| format!("team:{slug}")),
        );

        PrMetadata {
            node_id: meta.node_id,
            number: meta.number,
            title: meta.title,
            body: meta.body.unwrap_or_default(),
            author: meta
                .user
                .and_then(|user| user.login)
                .unwrap_or_else(|| "ghost".to_owned()),
            state: meta.state,
            draft: meta.draft,
            base_ref: meta.base.r#ref,
            base_sha: meta.base.sha,
            head_ref: meta.head.r#ref,
            head_sha: meta.head.sha,
            additions: meta.additions,
            deletions: meta.deletions,
            changed_files: meta.changed_files,
            requested_reviewers,
            labels: meta
                .labels
                .into_iter()
                .filter_map(|label| label.name)
                .collect(),
            milestone: meta.milestone.map(|milestone| milestone.title),
            html_url: meta.html_url,
            mergeable: meta.mergeable,
            mergeable_state: meta.mergeable_state,
            merge_state_status: meta.merge_state_status,
            merged: meta.merged_at.is_some(),
        }
    }
}

/// Parse a `GET /repos/{o}/{r}/pulls/{n}` body.
pub fn parse_metadata(body: &str) -> Result<PrMetadata, GhError> {
    serde_json::from_str(body).map_err(decode)
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
    fn parses_metadata() {
        let m = parse_metadata(fixture!("pr_metadata.json")).unwrap();
        assert_eq!(
            (m.head_sha.as_str(), m.base_sha.as_str()),
            ("sha_head", "sha_base")
        );
        assert_eq!(
            m.requested_reviewers,
            vec!["alice".to_string(), "team:core".to_string()]
        );
        assert_eq!(m.labels, vec!["bug".to_string()]);
        assert_eq!(m.body, ""); // null body
        assert!(m.draft);
        assert_eq!(m.changed_files, 3);
    }

    #[test]
    fn parses_merge_state() {
        let m = parse_metadata(fixture!("pr_metadata.json")).unwrap();
        assert_eq!(m.mergeable, Some(true));
        assert_eq!(m.mergeable_state, "clean");
        assert_eq!(m.merge_state_status, "clean");
    }

    #[test]
    fn merge_state_defaults_when_absent_or_null() {
        // GitHub computes mergeability asynchronously: `mergeable` is null
        // while it runs, and older hosts may omit the fields entirely.
        let m = parse_metadata(r#"{"number": 7, "mergeable": null, "mergeable_state": "unknown"}"#)
            .unwrap();
        assert_eq!(m.mergeable, None);
        assert_eq!(m.mergeable_state, "unknown");
        assert_eq!(m.merge_state_status, "");
        assert!(!m.merged);
    }

    #[test]
    fn merged_when_merged_at_is_set() {
        let m = parse_metadata(r#"{"number": 7, "merged_at": "2026-01-01T00:00:00Z"}"#).unwrap();
        assert!(m.merged);
    }
}
