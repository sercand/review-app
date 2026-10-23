use gh_review_desktop::auth::device_http::*;
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

#[tokio::test]
async fn device_flow_requests_code_then_polls_until_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("client_id=cid"))
        .and(body_string_contains("scope=repo+read%3Aorg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc", "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900, "interval": 0 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"error": "authorization_pending"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_x", "token_type": "bearer", "scope": "repo,read:org" })))
        .mount(&server)
        .await;

    let flow = HttpDeviceFlow::new(&server.uri());
    let mut shown = None;
    let token = run_device_flow(&flow, "cid", "repo read:org", |c| {
        shown = Some(c.user_code.clone())
    })
    .await
    .unwrap();
    assert_eq!(token, "gho_x");
    assert_eq!(shown.as_deref(), Some("ABCD-1234"));
}

#[tokio::test]
async fn disabled_device_flow_is_a_config_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc", "user_code": "U", "verification_uri": "v", "expires_in": 900, "interval": 0 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"error": "device_flow_disabled"})),
        )
        .mount(&server)
        .await;
    let err = run_device_flow(
        &HttpDeviceFlow::new(&server.uri()),
        "cid",
        "repo read:org",
        |_| {},
    )
    .await;
    assert!(matches!(err, Err(AuthFailure::Config(_))));
}
