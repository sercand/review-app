//! Contract tests for the server-side pending review mutations: submitting a
//! pending review posts the event body exactly once, and discarding one
//! deletes it — both are mutations, so neither is retried.

mod common;

use common::client;
use gh_review_desktop::github::reviews::ReviewState;
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::model::review::ReviewEvent;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn submits_and_deletes_server_pending_reviews() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/7/reviews/77/events"))
        .and(body_json(json!({"event": "COMMENT", "body": "done"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 77, "state": "COMMENTED", "body": "done", "user": {"login": "me"},
            "submitted_at": "2026-10-19T00:00:00Z"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/repos/o/r/pulls/7/reviews/78"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 78, "state": "PENDING", "user": {"login": "me"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server.uri());
    let r = c
        .submit_server_pending(&RepoRef::new("o", "r"), 7, 77, ReviewEvent::Comment, "done")
        .await
        .unwrap();
    assert_eq!(r.state, ReviewState::Commented);
    c.delete_server_pending(&RepoRef::new("o", "r"), 7, 78)
        .await
        .unwrap();
}
