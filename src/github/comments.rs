//! Immediate review comments (spec §7.3).
//!
//! A new comment is anchored exactly as the API takes it: a line comment
//! carries `path`/`line`/`side` (plus `start_line`/`start_side` for a range),
//! a file-level comment carries `subject_type: "file"` and no coordinates at
//! all. Posting is a mutation, so it is sent once and never retried.

use serde_json::{json, Value};

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::comment::CommentTarget;
use crate::model::thread::ThreadComment;
use crate::model::RepoRef;

/// A comment the reviewer is posting now: its body, the commit it is anchored
/// to, and what it is attached to (spec §7.3).
#[derive(Clone, Debug, PartialEq)]
pub struct NewComment {
    /// The head SHA the comment is anchored to.
    pub commit_id: String,
    /// The comment body.
    pub body: String,
    /// The line, range, or file the comment is attached to.
    pub target: CommentTarget,
}

/// The REST body for `POST /repos/{owner}/{repo}/pulls/{number}/comments`.
///
/// A line target sends `path`/`line`/`side` (and `start_line`/`start_side`
/// when the comment spans a range); a file target sends `subject_type:
/// "file"` and never any coordinates.
pub fn comment_payload(c: &NewComment) -> Value {
    let mut payload = json!({ "body": c.body, "commit_id": c.commit_id });
    match &c.target {
        CommentTarget::Line(anchor) => {
            payload["path"] = json!(anchor.path);
            payload["line"] = json!(anchor.line);
            payload["side"] = json!(anchor.side);
            if let Some(start_line) = anchor.start_line {
                payload["start_line"] = json!(start_line);
            }
            if let Some(start_side) = anchor.start_side {
                payload["start_side"] = json!(start_side);
            }
        }
        CommentTarget::File { path } => {
            payload["path"] = json!(path);
            payload["subject_type"] = json!("file");
        }
    }
    payload
}

/// The mutation that adds a comment to the viewer's pending review on
/// GitHub, as a new thread. Unlike the REST review endpoint it takes
/// file-level comments too, which is how the web UI keeps those pending.
pub const ADD_REVIEW_THREAD_MUTATION: &str = r#"
mutation($reviewId: ID!, $path: String!, $body: String!, $line: Int, $side: DiffSide,
         $startLine: Int, $startSide: DiffSide,
         $subjectType: PullRequestReviewThreadSubjectType) {
  addPullRequestReviewThread(input: {
    pullRequestReviewId: $reviewId, path: $path, body: $body, line: $line, side: $side,
    startLine: $startLine, startSide: $startSide, subjectType: $subjectType
  }) {
    thread {
      comments(first: 1) {
        nodes { databaseId author { login avatarUrl } body createdAt state }
      }
    }
  }
}
"#;

/// The variables for [`ADD_REVIEW_THREAD_MUTATION`]: the same anchor the
/// REST payload carries (spec §7.3), in GraphQL's names. A file target sends
/// `subjectType: FILE` and no coordinates.
pub fn review_thread_vars(review_id: &str, c: &NewComment) -> Value {
    let mut vars = json!({ "reviewId": review_id, "body": c.body });
    match &c.target {
        CommentTarget::Line(anchor) => {
            vars["path"] = json!(anchor.path);
            vars["line"] = json!(anchor.line);
            vars["side"] = json!(anchor.side);
            vars["subjectType"] = json!("LINE");
            if let Some(start_line) = anchor.start_line {
                vars["startLine"] = json!(start_line);
            }
            if let Some(start_side) = anchor.start_side {
                vars["startSide"] = json!(start_side);
            }
        }
        CommentTarget::File { path } => {
            vars["path"] = json!(path);
            vars["subjectType"] = json!("FILE");
        }
    }
    vars
}

/// Parse a REST comment body (as returned by the create and reply endpoints)
/// into a [`ThreadComment`]; the REST `id` becomes `database_id`.
pub fn comment_from_rest(v: &Value) -> Result<ThreadComment, GhError> {
    let database_id = v
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| GhError::Decode("comment: missing id".to_owned()))?;
    let user = v.get("user");
    Ok(ThreadComment {
        pending: false,
        database_id,
        author: user
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("ghost")
            .to_owned(),
        avatar_url: user
            .and_then(|user| user.get("avatar_url"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        body: v
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created_at: v
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

impl GithubClient {
    /// Post an immediate review comment, returning it as GitHub created it.
    ///
    /// A mutation: sent once, never retried.
    pub async fn post_review_comment(
        &self,
        repo: &RepoRef,
        number: u64,
        c: &NewComment,
    ) -> Result<ThreadComment, GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/comments",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        let response = self
            .send_mutation(self.http().post(&url).json(&comment_payload(c)))
            .await?;
        let body = response.json::<Value>().await.map_err(decode)?;
        comment_from_rest(&body)
    }

    /// Add a comment to the viewer's pending review on GitHub (node id
    /// `review_id`), returning it as GitHub created it — still pending.
    ///
    /// A mutation: sent once through the non-retrying GraphQL transport.
    pub async fn add_review_thread(
        &self,
        review_id: &str,
        c: &NewComment,
    ) -> Result<ThreadComment, GhError> {
        let data: Value = self
            .graphql_mutation(ADD_REVIEW_THREAD_MUTATION, review_thread_vars(review_id, c))
            .await?;
        data.pointer("/addPullRequestReviewThread/thread/comments/nodes/0")
            .and_then(crate::github::threads::parse_comment)
            .ok_or_else(|| {
                GhError::Decode("addPullRequestReviewThread: missing the new comment".to_owned())
            })
    }

    /// Reply to a review thread's root comment, returning the reply as
    /// GitHub created it.
    ///
    /// A reply inherits its anchor from the thread, so the body is the only
    /// thing sent. A mutation: sent once, never retried.
    pub async fn reply_to_comment(
        &self,
        repo: &RepoRef,
        number: u64,
        root_comment_id: u64,
        body: &str,
    ) -> Result<ThreadComment, GhError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/comments/{root_comment_id}/replies",
            self.endpoints().rest,
            repo.owner,
            repo.name
        );
        let response = self
            .send_mutation(self.http().post(&url).json(&json!({ "body": body })))
            .await?;
        let body = response.json::<Value>().await.map_err(decode)?;
        comment_from_rest(&body)
    }
}

/// Map a body-read failure to [`GhError::Decode`].
fn decode(error: reqwest::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::comment::Anchor;
    use crate::model::diff::Side;

    /// A range anchor on the right side, from `start` to `end`.
    fn range_anchor(path: &str, start: u32, end: u32) -> Anchor {
        Anchor {
            path: path.to_owned(),
            side: Side::Right,
            line: end,
            start_line: Some(start),
            start_side: Some(Side::Right),
        }
    }

    #[test]
    fn review_thread_vars_anchor_the_way_the_web_ui_does() {
        let line = NewComment {
            commit_id: "sha".into(),
            body: "b".into(),
            target: CommentTarget::Line(Anchor {
                path: "a.rs".into(),
                side: Side::Right,
                line: 3,
                start_line: None,
                start_side: None,
            }),
        };
        assert_eq!(
            review_thread_vars("PRR_1", &line),
            json!({ "reviewId": "PRR_1", "path": "a.rs", "body": "b",
                    "line": 3, "side": "RIGHT", "subjectType": "LINE" })
        );
        let range = NewComment {
            target: CommentTarget::Line(range_anchor("a.rs", 2, 5)),
            ..line.clone()
        };
        let vars = review_thread_vars("PRR_1", &range);
        assert_eq!(
            (vars["startLine"].clone(), vars["startSide"].clone()),
            (json!(2), json!("RIGHT"))
        );
        assert_eq!(vars["line"], json!(5));
        let file = NewComment {
            target: CommentTarget::File {
                path: "README.md".into(),
            },
            ..line
        };
        assert_eq!(
            review_thread_vars("PRR_1", &file),
            json!({ "reviewId": "PRR_1", "path": "README.md", "body": "b", "subjectType": "FILE" })
        );
    }

    #[test]
    fn payload_shapes() {
        let range = comment_payload(&NewComment {
            commit_id: "s".into(),
            body: "b".into(),
            target: CommentTarget::Line(range_anchor("p", 10, 14)),
        });
        assert_eq!(
            range,
            json!({"body": "b", "commit_id": "s", "path": "p",
                "start_line": 10, "start_side": "RIGHT", "line": 14, "side": "RIGHT"})
        );

        let file = comment_payload(&NewComment {
            commit_id: "s".into(),
            body: "b".into(),
            target: CommentTarget::File {
                path: "src/new name.rs".into(),
            },
        });
        assert_eq!(
            file,
            json!({"body": "b", "commit_id": "s", "path": "src/new name.rs", "subject_type": "file"})
        );
    }
}
