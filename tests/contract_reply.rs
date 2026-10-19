//! Contract test for `GithubClient::reply_to_comment`: the reply body is the
//! only thing sent (no coordinates), and the created comment comes back.

mod common;

use common::client;
use gh_review_desktop::model::RepoRef;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn replies_with_body_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/7/comments/99/replies"))
        .and(body_json(json!({"body": "thanks"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 2, "body": "thanks", "in_reply_to_id": 99, "path": "p",
            "user": {"login": "me"}, "created_at": "2026-10-19T00:00:00Z"})))
        .expect(1)
        .mount(&server)
        .await;
    let got = client(&server.uri())
        .reply_to_comment(&RepoRef::new("o", "r"), 7, 99, "thanks")
        .await
        .unwrap();
    assert_eq!(got.database_id, 2);
}
