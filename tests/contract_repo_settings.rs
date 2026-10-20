//! Contract test for reading a repository's merge-method allow flags.

mod common;

use common::client;
use gh_review_desktop::model::RepoRef;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn reads_merge_allow_flags() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "r",
            "allow_merge_commit": true,
            "allow_squash_merge": false,
            "allow_rebase_merge": true,
        })))
        .mount(&server)
        .await;
    let s = client(&server.uri())
        .repo_settings(&RepoRef::new("o", "r"))
        .await
        .unwrap();
    assert!(s.allow_merge_commit);
    assert!(!s.allow_squash_merge);
    assert!(s.allow_rebase_merge);
}

#[tokio::test]
async fn missing_flags_default_to_allowed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "r" })))
        .mount(&server)
        .await;
    let s = client(&server.uri())
        .repo_settings(&RepoRef::new("o", "r"))
        .await
        .unwrap();
    assert!(s.allow_merge_commit && s.allow_squash_merge && s.allow_rebase_merge);
}
