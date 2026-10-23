//! Client behaviour against a mock server: paths, auth, SSE, retries, timeouts, and keys
//! never leaking.

mod common;

use std::{collections::BTreeMap, time::Duration};

use common::*;
use futures::StreamExt;
use review_llm::{
    ApiType, Client, ClientConfig, ContentBlock, Error, Result, RetryPolicy, StopReason,
    StreamEvent,
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

const KEY: &str = "sk-test-secret-key";

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 2,
        base: Duration::from_millis(1),
        max: Duration::from_millis(5),
    }
}

fn client(server: &MockServer, api: ApiType) -> Client {
    let mut cfg =
        ClientConfig::new(api, SecretString::from(KEY)).base_url(server.uri().parse().unwrap());
    cfg.provider = "mock".into();
    cfg.retry = fast_retry();
    cfg.headers = BTreeMap::from([("x-team".to_owned(), "qa".to_owned())]);
    Client::new(cfg).unwrap()
}

fn fixture_text(p: &str) -> String {
    std::fs::read_to_string(fixtures().join(p)).unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn collect(
    s: futures::stream::BoxStream<'static, Result<StreamEvent>>,
) -> Vec<Result<StreamEvent>> {
    s.collect().await
}

/// Event order, and every `BlockStop` block equals the `Done` block at the same position.
fn check_stream(events: &[StreamEvent]) -> review_llm::Reply {
    assert!(matches!(
        events.first(),
        Some(StreamEvent::MessageStart { .. })
    ));
    let Some(StreamEvent::Done(reply)) = events.last() else {
        panic!("no Done: {events:?}")
    };
    let stops: Vec<&ContentBlock> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::BlockStop { block, .. } => Some(block),
            _ => None,
        })
        .collect();
    let starts = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::BlockStart { .. }))
        .count();
    assert_eq!(starts, stops.len());
    let mut stops_by_index: Vec<(usize, &ContentBlock)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::BlockStop { index, block } => Some((*index, block)),
            _ => None,
        })
        .collect();
    stops_by_index.sort_by_key(|(i, _)| *i);
    let ordered: Vec<&ContentBlock> = stops_by_index.into_iter().map(|(_, b)| b).collect();
    assert_eq!(ordered, reply.content.iter().collect::<Vec<_>>());
    reply.clone()
}

#[tokio::test]
async fn anthropic_generate_sends_golden_with_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-team", "qa"))
        .and(body_json(read_json("anthropic/tool_parallel.request.json")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(read_json("anthropic/tool_parallel.response.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let reply = client(&server, ApiType::AnthropicMessages)
        .generate(tool_parallel())
        .await
        .unwrap();
    assert_eq!(reply.tool_uses().count(), 2);
    assert_eq!(reply.meta.provider, "mock");
    assert_eq!(reply.meta.model, MODEL);
    assert_eq!(reply.meta.api, ApiType::AnthropicMessages);
}

#[tokio::test]
async fn anthropic_stream_parallel_tools() {
    let server = MockServer::start().await;
    let mut body = read_json("anthropic/tool_parallel.request.json");
    body["stream"] = json!(true);
    Mock::given(path("/v1/messages"))
        .and(body_json(body))
        .respond_with(sse(fixture_text("anthropic/tool_parallel.sse")))
        .mount(&server)
        .await;
    let events: Vec<StreamEvent> =
        collect(client(&server, ApiType::AnthropicMessages).generate_stream(tool_parallel()))
            .await
            .into_iter()
            .collect::<Result<_>>()
            .unwrap();
    let StreamEvent::MessageStart { usage, .. } = &events[0] else {
        panic!()
    };
    assert_eq!(usage.input, 332, "input usage arrives with MessageStart");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolInputDelta { .. }))
    );
    let reply = check_stream(&events);
    assert_eq!(reply.stop, StopReason::ToolUse);
    assert_eq!(reply.tool_uses().count(), 2);
}

#[tokio::test]
async fn anthropic_stream_text() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(sse(fixture_text("anthropic/text.sse")))
        .mount(&server)
        .await;
    let events: Vec<StreamEvent> =
        collect(client(&server, ApiType::AnthropicMessages).generate_stream(text()))
            .await
            .into_iter()
            .collect::<Result<_>>()
            .unwrap();
    let reply = check_stream(&events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. }))
    );
    assert!(!reply.text().is_empty());
}

#[tokio::test]
async fn responses_generate_and_stream() {
    let server = MockServer::start().await;
    let golden = read_json("responses/tool_parallel.request.json");
    let mut stream_body = golden.clone();
    stream_body["stream"] = json!(true);
    Mock::given(path("/v1/responses"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(header("x-team", "qa"))
        .and(body_json(golden))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(read_json("responses/tool_parallel.response.json")),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/responses"))
        .and(body_json(stream_body))
        .respond_with(sse(fixture_text("responses/tool_parallel.sse")))
        .mount(&server)
        .await;
    let c = client(&server, ApiType::OpenaiResponses);
    let reply = c.generate(tool_parallel()).await.unwrap();
    assert_eq!(reply.tool_uses().count(), 2);

    let events: Vec<StreamEvent> = collect(c.generate_stream(tool_parallel()))
        .await
        .into_iter()
        .collect::<Result<_>>()
        .unwrap();
    let streamed = check_stream(&events);
    assert_eq!(streamed.stop, StopReason::ToolUse);
    assert_eq!(streamed.tool_uses().count(), 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolInputDelta { .. }))
    );
}

#[tokio::test]
async fn responses_stream_text() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/responses"))
        .respond_with(sse(fixture_text("responses/text.sse")))
        .mount(&server)
        .await;
    let events: Vec<StreamEvent> =
        collect(client(&server, ApiType::OpenaiResponses).generate_stream(text()))
            .await
            .into_iter()
            .collect::<Result<_>>()
            .unwrap();
    let reply = check_stream(&events);
    assert_eq!(reply.stop, StopReason::EndTurn);
    assert!(!reply.text().is_empty());
}

#[tokio::test]
async fn retries_429_with_retry_after_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(read_json("anthropic/text.response.json")),
        )
        .mount(&server)
        .await;
    client(&server, ApiType::AnthropicMessages)
        .generate(text())
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn retries_5xx_on_every_api_then_gives_up() {
    for (api, p) in [
        (ApiType::AnthropicMessages, "/v1/messages"),
        (ApiType::OpenaiResponses, "/v1/responses"),
    ] {
        let server = MockServer::start().await;
        Mock::given(path(p))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(json!({"error": {"message": "boom", "type": "api_error"}})),
            )
            .mount(&server)
            .await;
        let err = client(&server, api).generate(text()).await.unwrap_err();
        assert!(
            matches!(err, Error::Http { status: 500, .. }),
            "{api}: {err:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "{api}: 1 + 2 retries"
        );
    }
}

#[tokio::test]
async fn client_errors_are_not_retried_and_redact_the_key() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(format!("invalid x-api-key: {KEY}")),
        )
        .mount(&server)
        .await;
    let err = client(&server, ApiType::AnthropicMessages)
        .generate(text())
        .await
        .unwrap_err();
    let Error::Http { status: 401, body } = &err else {
        panic!("{err:?}")
    };
    assert!(body.contains("[redacted]"));
    assert!(!format!("{err} {err:?}").contains(KEY));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn stream_is_not_retried_after_first_event() {
    let server = MockServer::start().await;
    let full = fixture_text("anthropic/text.sse");
    let cut = full[..full.len() / 2]
        .rsplit_once("\n\n")
        .unwrap()
        .0
        .to_owned()
        + "\n\n";
    Mock::given(path("/v1/messages"))
        .respond_with(sse(cut))
        .mount(&server)
        .await;
    let events = collect(client(&server, ApiType::AnthropicMessages).generate_stream(text())).await;
    assert!(matches!(
        events.first(),
        Some(Ok(StreamEvent::MessageStart { .. }))
    ));
    assert!(matches!(events.last(), Some(Err(Error::Decode(_)))));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn request_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from(KEY))
        .base_url(server.uri().parse().unwrap());
    cfg.timeout = Some(Duration::from_millis(50));
    cfg.retry = RetryPolicy::none();
    let err = Client::new(cfg)
        .unwrap()
        .generate(text())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
}

#[test]
fn keys_never_appear_in_debug() {
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from(KEY));
    cfg.headers.insert("x-team".into(), "qa".into());
    assert!(!format!("{cfg:?}").contains(KEY));
    let c = Client::new(cfg).unwrap();
    assert!(!format!("{c:?}").contains(KEY));
}

#[tokio::test]
async fn base_url_with_v1_is_not_doubled() {
    for suffix in ["/v1", "/v1/"] {
        let server = MockServer::start().await;
        let ok_chat = json!({"content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn", "usage": {}});
        Mock::given(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_chat))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status": "completed", "output": [], "usage": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let base: url::Url = format!("{}{suffix}", server.uri()).parse().unwrap();
        let make = |api| {
            let mut cfg = ClientConfig::new(api, SecretString::from(KEY)).base_url(base.clone());
            cfg.retry = RetryPolicy::none();
            Client::new(cfg).unwrap()
        };
        make(ApiType::AnthropicMessages)
            .generate(text())
            .await
            .unwrap();
        make(ApiType::OpenaiResponses)
            .generate(text())
            .await
            .unwrap();
        // `expect(1)` on each mock is verified when the server drops.
    }
}
