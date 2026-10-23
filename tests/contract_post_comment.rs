//! Contract tests for `GithubClient::post_review_comment`: the exact body is
//! sent once, and a 5xx is returned without a retry (mutations never retry).

mod common;

use common::client;
use gh_review_desktop::github::comments::NewComment;
use gh_review_desktop::github::error::GhError;
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::model::comment::{Anchor, CommentTarget};
use gh_review_desktop::model::diff::Side;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn posts_exact_single_line_body_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/7/comments"))
        .and(body_json(json!({"body": "hi", "commit_id": "sha1", "line": 42, "path": "src/a.rs", "side": "RIGHT"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 1, "body": "hi", "path": "src/a.rs", "line": 42, "side": "RIGHT",
            "user": {"login": "me"}, "created_at": "2026-10-19T00:00:00Z"})))
        .expect(1)
        .mount(&server)
        .await;
    let got = client(&server.uri())
        .post_review_comment(
            &RepoRef::new("o", "r"),
            7,
            &NewComment {
                commit_id: "sha1".into(),
                body: "hi".into(),
                target: CommentTarget::Line(Anchor {
                    path: "src/a.rs".into(),
                    side: Side::Right,
                    line: 42,
                    start_line: None,
                    start_side: None,
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(got.database_id, 1);
}

#[tokio::test]
async fn mutations_are_not_retried_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/7/comments"))
        .respond_with(ResponseTemplate::new(502))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server.uri())
        .post_review_comment(
            &RepoRef::new("o", "r"),
            7,
            &NewComment {
                commit_id: "s".into(),
                body: "b".into(),
                target: CommentTarget::File { path: "p".into() },
            },
        )
        .await;
    assert_eq!(err.unwrap_err(), GhError::Server(502));
}
