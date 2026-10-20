//! Contract test for submitting a review: the payload is posted exactly once
//! to the reviews endpoint (a mutation, never retried) and the created review
//! comes back.

mod common;

use common::client;
use gh_review_desktop::github::reviews::{ReviewState, review_payload};
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::model::review::{PendingReview, ReviewEvent};
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn posts_review_and_returns_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/7/reviews"))
        .and(body_json(json!(
            {"commit_id": "sha1", "body": "lgtm", "event": "APPROVE"}
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5, "state": "APPROVED", "body": "lgtm", "user": {"login": "me"},
            "submitted_at": "2026-01-01T00:00:00Z"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut p = PendingReview::default();
    p.set_body("lgtm".into());
    p.set_event(ReviewEvent::Approve);
    let r = client(&server.uri())
        .submit_review(&RepoRef::new("o", "r"), 7, &review_payload(&p, "sha1"))
        .await
        .unwrap();
    assert_eq!(r.state, ReviewState::Approved);
}
