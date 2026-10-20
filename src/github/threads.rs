//! Review threads: the single source of inline comments (spec §7.4).
//!
//! REST lists a PR's comments but not their thread structure or resolved
//! state, so threads come from GraphQL. The thread list is paginated with the
//! `after` cursor; a thread whose comments were truncated gets its remaining
//! comment pages fetched through `node(id:)` and appended, so every thread
//! arrives complete.

use serde_json::{json, Value};

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::thread::{ReviewThread, SubjectType, ThreadComment};
use crate::model::RepoRef;

/// The GraphQL document that lists one page of a PR's review threads.
///
/// `first: 100` is the largest page GraphQL allows; `after` carries the
/// previous page's `endCursor`.
pub const THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          startLine
          diffSide
          subjectType
          comments(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes {
              databaseId
              author { login avatarUrl }
              body
              createdAt
              state
            }
          }
        }
      }
    }
  }
}
"#;

/// The GraphQL document that lists one page of one thread's comments, for a
/// thread whose first page was truncated.
pub const THREAD_COMMENTS_QUERY: &str = r#"
query($id: ID!, $after: String) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          databaseId
          author { login avatarUrl }
          body
          createdAt
          state
        }
      }
    }
  }
}
"#;

/// The mutation that resolves a review thread.
pub const RESOLVE_MUTATION: &str = r#"
mutation($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) {
    thread { isResolved }
  }
}
"#;

/// The mutation that un-resolves a review thread.
pub const UNRESOLVE_MUTATION: &str = r#"
mutation($threadId: ID!) {
  unresolveReviewThread(input: { threadId: $threadId }) {
    thread { isResolved }
  }
}
"#;

/// The variables both resolve mutations take.
pub fn resolve_vars(thread_id: &str) -> Value {
    json!({ "threadId": thread_id })
}

/// A GraphQL connection's pagination fields.
#[derive(Clone, Debug, PartialEq)]
pub struct PageInfo {
    /// `pageInfo.hasNextPage`.
    pub has_next: bool,
    /// `pageInfo.endCursor`, when GitHub sent one.
    pub end_cursor: Option<String>,
}

impl GithubClient {
    /// Every review thread on PR `number`, in API order, each complete.
    ///
    /// Paginates the thread list with the `after` cursor and, for any thread
    /// whose comments connection reports `hasNextPage`, fetches the remaining
    /// comment pages and appends them to that thread's comments.
    pub async fn review_threads(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ReviewThread>, GhError> {
        let mut pages = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let variables = json!({
                "owner": repo.owner,
                "name": repo.name,
                "number": number,
                "after": after,
            });
            let data: Value = self.graphql(THREADS_QUERY, variables).await?;
            let pull_request = data
                .get("repository")
                .and_then(|repository| repository.get("pullRequest"))
                .ok_or_else(|| {
                    GhError::Decode("reviewThreads: missing from the response".to_owned())
                })?;
            let (threads, page) = parse_thread_nodes(pull_request)?;
            pages.extend(threads);
            if !page.has_next {
                break;
            }
            after = page.end_cursor;
            if after.is_none() {
                break; // a next page with no cursor cannot be fetched
            }
        }

        let mut threads = Vec::with_capacity(pages.len());
        for (mut thread, mut comments) in pages {
            while comments.has_next {
                let variables = json!({ "id": thread.id, "after": comments.end_cursor });
                let data: Value = self.graphql(THREAD_COMMENTS_QUERY, variables).await?;
                let connection = data
                    .get("node")
                    .and_then(|node| node.get("comments"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let (more, page) = parse_comments_connection(&connection);
                thread.comments.extend(more);
                comments = page;
                if !comments.has_next || comments.end_cursor.is_none() {
                    break;
                }
            }
            threads.push(thread);
        }
        Ok(threads)
    }

    /// Resolve (`resolved`) or unresolve (`!resolved`) a review thread,
    /// returning the state GitHub reports afterwards.
    ///
    /// A mutation: sent once through the non-retrying GraphQL transport.
    pub async fn set_thread_resolved(
        &self,
        thread_id: &str,
        resolved: bool,
    ) -> Result<bool, GhError> {
        let (mutation, field) = if resolved {
            (RESOLVE_MUTATION, "resolveReviewThread")
        } else {
            (UNRESOLVE_MUTATION, "unresolveReviewThread")
        };
        let data: Value = self
            .graphql_mutation(mutation, resolve_vars(thread_id))
            .await?;
        data.get(field)
            .and_then(|result| result.get("thread"))
            .and_then(|thread| thread.get("isResolved"))
            .and_then(Value::as_bool)
            .ok_or_else(|| GhError::Decode(format!("{field}: missing isResolved")))
    }
}

/// Parse the `reviewThreads` connection under `data`.
pub fn parse_threads_page(data: &Value) -> Result<(Vec<ReviewThread>, PageInfo), GhError> {
    let (threads, page) = parse_thread_nodes(data)?;
    Ok((
        threads.into_iter().map(|(thread, _)| thread).collect(),
        page,
    ))
}

/// Parse the `reviewThreads` connection, keeping each thread's comments
/// connection page info so the fetcher knows which threads are truncated.
fn parse_thread_nodes(data: &Value) -> Result<(Vec<(ReviewThread, PageInfo)>, PageInfo), GhError> {
    let threads = data
        .get("reviewThreads")
        .ok_or_else(|| GhError::Decode("reviewThreads: missing from the response".to_owned()))?;
    let page = parse_page_info(threads.get("pageInfo"));
    let parsed = threads
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().filter_map(parse_thread).collect())
        .unwrap_or_default();
    Ok((parsed, page))
}

/// Parse one `reviewThreads.nodes[]` entry, plus its comments' page info.
///
/// A node without an `id` is not a thread we can act on and is skipped.
fn parse_thread(node: &Value) -> Option<(ReviewThread, PageInfo)> {
    let id = node.get("id")?.as_str()?.to_owned();
    let (comments, comments_page) =
        parse_comments_connection(node.get("comments").unwrap_or(&Value::Null));
    let thread = ReviewThread {
        id,
        is_resolved: node
            .get("isResolved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_outdated: node
            .get("isOutdated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        path: node
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        line: node
            .get("line")
            .and_then(Value::as_u64)
            .map(|line| line as u32),
        start_line: node
            .get("startLine")
            .and_then(Value::as_u64)
            .map(|line| line as u32),
        side: node
            .get("diffSide")
            .and_then(|side| serde_json::from_value(side.clone()).ok()),
        subject_type: match node.get("subjectType").and_then(Value::as_str) {
            Some("FILE") => SubjectType::File,
            _ => SubjectType::Line,
        },
        comments,
    };
    Some((thread, comments_page))
}

/// Parse a `comments` connection: its comments and its page info.
fn parse_comments_connection(connection: &Value) -> (Vec<ThreadComment>, PageInfo) {
    let page = parse_page_info(connection.get("pageInfo"));
    let comments = connection
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().filter_map(parse_comment).collect())
        .unwrap_or_default();
    (comments, page)
}

/// Parse one comment node; one without a `databaseId` cannot be replied to
/// or anchored to and is skipped.
pub(crate) fn parse_comment(node: &Value) -> Option<ThreadComment> {
    let database_id = node.get("databaseId")?.as_u64()?;
    let author = node.get("author");
    Some(ThreadComment {
        pending: node.get("state").and_then(Value::as_str) == Some("PENDING"),
        database_id,
        author: author
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("ghost")
            .to_owned(),
        avatar_url: author
            .and_then(|author| author.get("avatarUrl"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        body: node
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: node
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Parse one `pageInfo` object; absent fields read as "no more pages".
fn parse_page_info(info: Option<&Value>) -> PageInfo {
    PageInfo {
        has_next: info
            .and_then(|info| info.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        end_cursor: info
            .and_then(|info| info.get("endCursor"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::model::diff::Side;

    #[test]
    fn a_comment_in_the_viewers_pending_review_is_pending() {
        let node = |state: &str| {
            serde_json::json!({
                "databaseId": 7, "author": { "login": "me" }, "body": "b",
                "createdAt": "2026-10-16T00:00:00Z", "state": state
            })
        };
        assert!(parse_comment(&node("PENDING")).unwrap().pending);
        assert!(!parse_comment(&node("SUBMITTED")).unwrap().pending);
    }

    #[test]
    fn parses_resolved_outdated_and_file_level_threads() {
        let body: Value = serde_json::from_str(fixture!("review_threads.json")).unwrap();
        let (threads, page) = parse_threads_page(&body["data"]).unwrap();
        assert!(page.has_next && page.end_cursor.is_some());
        assert_eq!(threads[0].id, "PRRT_kwDOA");
        assert!(threads[0].is_resolved);
        assert_eq!(threads[0].side, Some(Side::Right));
        assert_eq!(threads[0].comments.len(), 2); // root + reply
        assert_eq!(threads[0].root_comment_id(), Some(1001));
        assert!(threads[1].is_outdated && threads[1].line.is_none());
        assert_eq!(threads[2].subject_type, SubjectType::File);
    }
}
