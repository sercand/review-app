//! The GraphQL transport and the PR search query.
//!
//! GraphQL is used where REST cannot express the query (spec §7.1): the PR list
//! is one `search` call per scope. Read-only queries share REST's read-retry
//! policy; the transport itself never retries anything else.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::github::client::{GithubClient, READ_BACKOFF};
use crate::github::error::{GhError, ResponseMeta, classify};
use crate::model::RepoRef;
use crate::model::pr::{CiState, PrSummary, ReviewDecision};

/// The GraphQL document that lists one page of PRs for one search query.
///
/// `first: 50` matches the REST page size the list uses elsewhere; `after`
/// carries the previous page's `endCursor`.
pub const SEARCH_PRS: &str = r#"
query($q: String!, $after: String) {
  search(query: $q, type: ISSUE, first: 50, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number
        title
        isDraft
        updatedAt
        createdAt
        additions
        deletions
        reviewDecision
        totalCommentsCount
        url
        author { login }
        repository { nameWithOwner }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}
"#;

/// One page of PR search results.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchPage {
    /// The PRs on this page, in GitHub's order, with empty nodes skipped.
    pub items: Vec<PrSummary>,
    /// `search.issueCount` — the total number of matching issues.
    pub issue_count: u32,
    /// `pageInfo.endCursor`, when GitHub sent one.
    pub end_cursor: Option<String>,
    /// `pageInfo.hasNextPage`.
    pub has_next: bool,
}

impl GithubClient {
    /// Send one GraphQL query and deserialize its `data`.
    ///
    /// A non-empty `errors[]` is a failure even when `data` is present:
    /// a `RATE_LIMITED`-typed error becomes [`GhError::RateLimited`] (the reset
    /// time is not in the body), anything else becomes [`GhError::GraphQl`].
    /// Read-only, so transport failures and 5xx are retried.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T, GhError> {
        let body = self.post_graphql_with_retry(query, &variables).await?;
        decode_data(&body)
    }

    /// Send one GraphQL mutation and deserialize its `data`, **without
    /// retrying**: the single POST's outcome is the answer, whether it
    /// succeeded, failed, or its fate is unknown.
    ///
    /// The `errors[]` handling is the same as [`GithubClient::graphql`].
    pub async fn graphql_mutation<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T, GhError> {
        let body = self.post_graphql_once(query, &variables).await?;
        decode_data(&body)
    }

    /// Search open PRs with `query`, returning the page after `after`.
    pub async fn search_prs(
        &self,
        query: &str,
        after: Option<&str>,
    ) -> Result<SearchPage, GhError> {
        let variables = serde_json::json!({ "q": query, "after": after });
        let data: Value = self.graphql(SEARCH_PRS, variables).await?;
        parse_search(&data)
    }

    /// One GraphQL `POST`, returning the raw body on success.
    async fn post_graphql_once(&self, query: &str, variables: &Value) -> Result<String, GhError> {
        let response = self
            .http()
            .post(&self.endpoints().graphql)
            .json(&serde_json::json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(network)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.map_err(network)?;

        if (200..300).contains(&status) {
            return Ok(body);
        }

        let meta = ResponseMeta::from_headers(status, &headers);
        Err(classify(&meta, &body, std::time::SystemTime::now()))
    }

    /// `post_graphql_once` with the read-retry policy applied.
    async fn post_graphql_with_retry(
        &self,
        query: &str,
        variables: &Value,
    ) -> Result<String, GhError> {
        let mut attempt = 0;
        loop {
            match self.post_graphql_once(query, variables).await {
                Ok(body) => return Ok(body),
                Err(error) if error.is_retryable_read() && attempt < READ_BACKOFF.len() => {
                    tokio::time::sleep(READ_BACKOFF[attempt]).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Unwrap a GraphQL response body: a non-empty `errors[]` wins over `data`;
/// otherwise `data` is deserialized.
fn decode_data<T: DeserializeOwned>(body: &str) -> Result<T, GhError> {
    let envelope: Value = serde_json::from_str(body).map_err(decode)?;

    if let Some(errors) = envelope
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        return Err(graphql_error(errors));
    }

    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    serde_json::from_value(data).map_err(decode)
}

/// Parse the `data` of a `SEARCH_PRS` response.
pub fn parse_search(data: &Value) -> Result<SearchPage, GhError> {
    let search = data
        .get("search")
        .ok_or_else(|| GhError::Decode("search: missing from the response".to_owned()))?;

    let page_info = search.get("pageInfo");
    let end_cursor = page_info
        .and_then(|info| info.get("endCursor"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let has_next = page_info
        .and_then(|info| info.get("hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let items = search
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().filter_map(parse_node).collect())
        .unwrap_or_default();

    Ok(SearchPage {
        items,
        issue_count: search
            .get("issueCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        end_cursor,
        has_next,
    })
}

/// Parse one `search.nodes[]` entry, skipping non-PR nodes.
///
/// GitHub can return `{}` for a node the viewer may not see; such a node has no
/// number and is skipped.
fn parse_node(node: &Value) -> Option<PrSummary> {
    let number = node.get("number").and_then(Value::as_u64)?;
    let name_with_owner = node
        .get("repository")
        .and_then(|repository| repository.get("nameWithOwner"))
        .and_then(Value::as_str)?;
    let (owner, name) = name_with_owner.split_once('/')?;

    Some(PrSummary {
        repo: RepoRef::new(owner, name),
        number,
        title: string(node, "title"),
        author: node
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("ghost")
            .to_owned(),
        draft: node
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        updated_at: string(node, "updatedAt"),
        created_at: string(node, "createdAt"),
        additions: count(node, "additions"),
        deletions: count(node, "deletions"),
        comments: count(node, "totalCommentsCount"),
        scopes: Vec::new(),
        ci: node
            // `commits(last: 1)` answers under `commits.nodes`; the
            // argument is not part of the response shape.
            .get("commits")
            .and_then(|commits| commits.get("nodes"))
            .and_then(Value::as_array)
            .and_then(|nodes| nodes.first())
            .and_then(|node| node.get("commit"))
            .and_then(|commit| commit.get("statusCheckRollup"))
            .and_then(|rollup| rollup.get("state"))
            .and_then(Value::as_str)
            .and_then(CiState::from_rollup),
        review: node
            .get("reviewDecision")
            .and_then(Value::as_str)
            .and_then(ReviewDecision::from_wire),
        url: string(node, "url"),
    })
}

/// A string field, or the empty string when it is absent or null.
fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A numeric field as `u32`, or `0` when it is absent or null.
fn count(value: &Value, key: &str) -> u32 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

/// Map a non-empty GraphQL `errors[]` to a [`GhError`].
fn graphql_error(errors: &[Value]) -> GhError {
    let rate_limited = errors
        .iter()
        .any(|error| error.get("type").and_then(Value::as_str) == Some("RATE_LIMITED"));
    if rate_limited {
        return GhError::RateLimited { reset_at: None };
    }

    GhError::GraphQl(
        errors
            .iter()
            .map(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown GraphQL error")
                    .to_owned()
            })
            .collect(),
    )
}

/// Map a transport failure to [`GhError::Network`].
fn network(error: reqwest::Error) -> GhError {
    GhError::Network(error.to_string())
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
    fn parses_search_page_with_badges_and_skips_empty_nodes() {
        let body: serde_json::Value = serde_json::from_str(fixture!("search_prs.json")).unwrap();
        let page = parse_search(&body["data"]).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.issue_count, 137);
        assert!(page.has_next);
        assert_eq!(page.end_cursor.as_deref(), Some("Y3Vyc29yOjI="));
        let first = &page.items[0];
        assert_eq!(first.repo.to_string(), "octo/repo");
        assert_eq!(
            (first.number, first.additions, first.deletions),
            (12, 40, 3)
        );
        assert_eq!(first.ci, Some(CiState::Failure));
        assert_eq!(first.review, Some(ReviewDecision::ChangesRequested));
        assert!(page.items[1].draft);
        assert_eq!(page.items[1].author, "ghost"); // null author
    }

    #[test]
    fn rate_limited_errors_win_over_their_messages() {
        let rate_limited = graphql_error(&[serde_json::json!({
            "type": "RATE_LIMITED", "message": "API rate limit exceeded"
        })]);
        assert_eq!(rate_limited, GhError::RateLimited { reset_at: None });

        let messages = graphql_error(&[
            serde_json::json!({"message": "Field 'x' doesn't exist"}),
            serde_json::json!({"message": "bad request"}),
        ]);
        assert_eq!(
            messages,
            GhError::GraphQl(vec![
                "Field 'x' doesn't exist".to_owned(),
                "bad request".to_owned()
            ])
        );
    }
}
