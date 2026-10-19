mod common;

use common::client;
use gh_review_desktop::github::error::GhError;
use serde_json::json;
use wiremock::{matchers::*, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn graphql_rate_limit_error_maps_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            json!({"variables": {"q": "is:pr is:open author:mona sort:updated-desc"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded"}]})))
        .mount(&server)
        .await;
    let err = client(&server.uri())
        .search_prs("is:pr is:open author:mona sort:updated-desc", None)
        .await
        .unwrap_err();
    assert!(matches!(err, GhError::RateLimited { .. }));
}
