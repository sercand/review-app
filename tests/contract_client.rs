mod common;

use common::client;
use gh_review_desktop::github::error::GhError;
use serde_json::{json, Value};
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn me_sends_required_headers_and_reads_scopes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer tok"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(header_exists("user-agent"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-oauth-scopes", "repo, read:org")
                .set_body_json(json!({"login": "octocat", "name": "Mona", "avatar_url": null})),
        )
        .mount(&server)
        .await;
    let me = client(&server.uri()).me().await.unwrap();
    assert_eq!(me.user.login, "octocat");
    assert_eq!(me.scopes, vec!["repo".to_string(), "read:org".to_string()]);
}

#[tokio::test]
async fn unauthorized_maps_to_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&server)
        .await;
    assert_eq!(
        client(&server.uri()).me().await.unwrap_err(),
        GhError::Unauthorized
    );
}

#[tokio::test]
async fn follows_link_pagination() {
    let server = MockServer::start().await;
    let next = format!("<{}/items?page=2>; rel=\"next\"", server.uri());
    Mock::given(method("GET"))
        .and(path("/items"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([3])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/items"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", next.as_str())
                .set_body_json(json!([1, 2])),
        )
        .mount(&server)
        .await;
    let all: Vec<Value> = client(&server.uri()).get_all_pages("/items").await.unwrap();
    assert_eq!(all, vec![json!(1), json!(2), json!(3)]);
}

#[tokio::test]
async fn retries_reads_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "octocat"})))
        .mount(&server)
        .await;
    assert_eq!(
        client(&server.uri()).me().await.unwrap().user.login,
        "octocat"
    );
}
