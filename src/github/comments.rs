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

/// Parse a REST comment body (as returned by the create and reply endpoints)
/// into a [`ThreadComment`]; the REST `id` becomes `database_id`.
pub fn comment_from_rest(v: &Value) -> Result<ThreadComment, GhError> {
    let database_id = v
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| GhError::Decode("comment: missing id".to_owned()))?;
    let user = v.get("user");
    Ok(ThreadComment {
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
