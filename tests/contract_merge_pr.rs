//! Contract test for merging a PR: the payload is PUT exactly once to the
//! merge endpoint (a mutation, never retried) and the result comes back.

mod common;

use common::client;
use gh_review_desktop::github::merge::{merge_payload, MergeMethod, MergeRequest};
use gh_review_desktop::model::RepoRef;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn puts_merge_payload_and_returns_result() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/repos/o/r/pulls/7/merge"))
        .and(body_json(json!({
            "sha": "sha1",
            "merge_method": "squash",
            "commit_title": "Add feature (#7)",
            "commit_message": "details",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sha": "merge_sha", "merged": true, "message": "Pull Successfully merged"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let payload = merge_payload(&MergeRequest {
        sha: "sha1".into(),
        method: MergeMethod::Squash,
        commit_title: "Add feature (#7)".into(),
        commit_message: "details".into(),
    });
    let result = client(&server.uri())
        .merge_pr(&RepoRef::new("o", "r"), 7, &payload)
        .await
        .unwrap();
    assert!(result.merged);
    assert_eq!(result.sha, "merge_sha");
}

#[tokio::test]
async fn reports_not_merged_without_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/repos/o/r/pulls/7/merge"))
        .respond_with(ResponseTemplate::new(405).set_body_json(json!({
            "message": "This branch has not been approved",
            "documentation_url": "https://docs.github.com"
        })))
        .mount(&server)
        .await;
    let payload = merge_payload(&MergeRequest {
        sha: "sha1".into(),
        method: MergeMethod::Merge,
        commit_title: "t".into(),
        commit_message: String::new(),
    });
    let error = client(&server.uri())
        .merge_pr(&RepoRef::new("o", "r"), 7, &payload)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        gh_review_desktop::github::error::GhError::Forbidden(m) if m.contains("not been approved")
    ));
}
