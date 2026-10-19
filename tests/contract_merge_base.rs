//! Contract test for `GithubClient::merge_base` (Task 46): the compare
//! endpoint's `merge_base_commit.sha` is what a local diff reads the base
//! blob at.

mod common;

use gh_review_desktop::model::RepoRef;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::client;

#[tokio::test]
async fn merge_base_reads_the_compare_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/compare/base1...head1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"merge_base_commit": {"sha": "mb"}}"#),
        )
        .mount(&server)
        .await;

    let client = client(&server.uri());
    let repo = RepoRef::new("o", "r");
    assert_eq!(
        client.merge_base(&repo, "base1", "head1").await.unwrap(),
        "mb"
    );
}
