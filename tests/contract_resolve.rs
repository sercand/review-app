//! Contract test for `GithubClient::set_thread_resolved`: the mutation sends
//! the thread id, the resolved state is read back, and the two documents are
//! the resolve/unresolve pair.

mod common;

use common::client;
use gh_review_desktop::github::threads::{RESOLVE_MUTATION, UNRESOLVE_MUTATION, resolve_vars};
use serde_json::json;
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn resolve_sends_thread_id_and_reads_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            json!({"variables": {"threadId": "PRRT_kwDOA"}}),
        ))
        .and(body_string_contains("resolveReviewThread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            {"data": {"resolveReviewThread": {"thread": {"isResolved": true}}}})))
        .mount(&server)
        .await;
    assert!(
        client(&server.uri())
            .set_thread_resolved("PRRT_kwDOA", true)
            .await
            .unwrap()
    );
}

#[test]
fn mutations_use_the_right_fields() {
    assert!(RESOLVE_MUTATION.contains("resolveReviewThread"));
    assert!(UNRESOLVE_MUTATION.contains("unresolveReviewThread"));
    assert_eq!(resolve_vars("t")["threadId"], "t");
}
