//! Client behaviour against a mock server: paths, auth, SSE, retries, timeouts, role handles,
//! and keys never leaking.

mod common;

use std::{collections::BTreeMap, time::Duration};

use common::*;
use futures::StreamExt;
use review_llm::{
    ApiType, ChatRole, Client, ClientConfig, ContentBlock, DecisionRole, Error, LLM, LLMConfig,
    Result, RetryPolicy, StopReason, StreamEvent,
};
use secrecy::SecretString;
use serde_json::{Value, json};
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
async fn decisions_decide() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/decisions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_json(read_json("decisions/decide.request.json")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(read_json("decisions/decide.response.json")),
        )
        .mount(&server)
        .await;
    let d = client(&server, ApiType::Decisions)
        .decide(decide())
        .await
        .unwrap();
    assert_eq!(d.answers.len(), 3);
    assert_eq!(d.meta.api, ApiType::Decisions);
}

#[tokio::test]
async fn unsupported_operations() {
    let server = MockServer::start().await;
    let chat = client(&server, ApiType::AnthropicMessages);
    assert!(matches!(
        chat.decide(decide()).await,
        Err(Error::Unsupported { op: "decide", .. })
    ));
    let dec = client(&server, ApiType::Decisions);
    assert!(matches!(
        dec.generate(text()).await,
        Err(Error::Unsupported { op: "generate", .. })
    ));
    let mut s = dec.generate_stream(text());
    assert!(matches!(
        s.next().await,
        Some(Err(Error::Unsupported { .. }))
    ));
    assert!(s.next().await.is_none());
}

#[tokio::test]
async fn retries_429_with_retry_after_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/decisions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(read_json("decisions/decide.response.json")),
        )
        .mount(&server)
        .await;
    client(&server, ApiType::Decisions)
        .decide(decide())
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
    Mock::given(path("/v1/decisions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let mut cfg = ClientConfig::new(ApiType::Decisions, SecretString::from(KEY))
        .base_url(server.uri().parse().unwrap());
    cfg.timeout = Some(Duration::from_millis(50));
    cfg.retry = RetryPolicy::none();
    let err = Client::new(cfg)
        .unwrap()
        .decide(decide())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
}

fn llm_config(server: &MockServer) -> LLMConfig {
    toml::from_str(&format!(
        r#"
[providers.edge]
base_url = "{uri}"
apis = ["openai_responses", "anthropic_messages", "decisions"]

[defaults]
chat = {{ provider = "edge", api = "anthropic_messages", model = "default-chat", max_tokens = 777 }}
decision = {{ provider = "edge", model = "default-decision" }}

[roles]
agent = {{ provider = "edge", api = "openai_responses", model = "agent-model" }}
"#,
        uri = server.uri()
    ))
    .unwrap()
}

fn key_for(name: &str) -> Option<SecretString> {
    (name == "edge").then(|| SecretString::from(KEY))
}

#[tokio::test]
async fn llm_role_handles_fill_model_and_defaults() {
    let server = MockServer::start().await;
    let reply = |model: &str| json!({"content": [{"type": "text", "text": "hi"}], "model": model, "stop_reason": "end_turn", "usage": {}});
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply("default-chat")))
        .mount(&server)
        .await;
    Mock::given(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"status": "completed", "output": [], "usage": {}})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/decisions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(read_json("decisions/decide.response.json")),
        )
        .mount(&server)
        .await;
    let llm = LLM::from_config(&llm_config(&server), key_for).unwrap();
    assert!(!format!("{llm:?}").contains(KEY));

    // vision: no role entry, uses the chat default
    let mut req = text();
    req.model = None;
    req.max_tokens = None;
    let vision = llm.chat(ChatRole::Vision).unwrap();
    let r = vision.generate(req.clone()).await.unwrap();
    assert_eq!(r.meta.role, Some(ChatRole::Vision.into()));
    assert!(r.meta.from_default);
    assert_eq!(r.meta.model, "default-chat");

    // agent: its own entry on the Responses API
    let agent = llm.chat(ChatRole::Agent).unwrap();
    assert_eq!(agent.api(), ApiType::OpenaiResponses);
    let r = agent.generate(req).await.unwrap();
    assert!(!r.meta.from_default);
    assert_eq!(r.meta.model, "agent-model");

    let d = llm.decision(DecisionRole::DecideFast).unwrap();
    let mut dreq = decide();
    dreq.model = None;
    let dec = d.decide(dreq).await.unwrap();
    assert_eq!(dec.meta.model, "default-decision");
    assert_eq!(dec.meta.role, Some(DecisionRole::DecideFast.into()));

    let reqs = server.received_requests().await.unwrap();
    let body = |p: &str| -> Value {
        serde_json::from_slice(&reqs.iter().find(|r| r.url.path() == p).unwrap().body).unwrap()
    };
    assert_eq!(
        body("/v1/messages")["max_tokens"],
        777,
        "role default fills max_tokens"
    );
    assert_eq!(body("/v1/messages")["model"], "default-chat");
    assert_eq!(body("/v1/responses")["model"], "agent-model");
    assert_eq!(body("/v1/decisions")["model"], "default-decision");
}

#[test]
fn llm_missing_key_and_unconfigured_role() {
    let server_uri = "http://127.0.0.1:9";
    let cfg: LLMConfig = toml::from_str(&format!(
        r#"
[providers.edge]
base_url = "{server_uri}"
apis = ["decisions"]
[roles]
decide = {{ provider = "edge", model = "m" }}
"#
    ))
    .unwrap();
    let err = LLM::from_config(&cfg, |_| None).unwrap_err();
    assert!(matches!(err, Error::MissingKey { provider } if provider == "edge"));

    let llm = LLM::from_config(&cfg, key_for).unwrap();
    llm.decision(DecisionRole::Decide).unwrap();
    assert!(matches!(
        llm.decision(DecisionRole::DecideFast),
        Err(Error::RoleNotConfigured { role }) if role == "decide-fast"
    ));
    assert!(matches!(
        llm.chat(ChatRole::Agent),
        Err(Error::RoleNotConfigured { .. })
    ));
}

#[test]
fn keys_never_appear_in_debug() {
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from(KEY));
    cfg.headers.insert("x-team".into(), "qa".into());
    assert!(!format!("{cfg:?}").contains(KEY));
    let c = Client::new(cfg).unwrap();
    assert!(!format!("{c:?}").contains(KEY));
}

#[test]
fn llm_reads_key_from_api_key_file() {
    let dir = std::env::temp_dir().join(format!("review-llm-http-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("key");
    std::fs::write(&file, format!("{KEY}\n")).unwrap();
    let cfg: LLMConfig = toml::from_str(&format!(
        r#"
[providers.edge]
base_url = "http://127.0.0.1:9"
apis = ["decisions"]
api_key_env = "QA_LLM_TEST_SURELY_UNSET"
api_key_file = "{}"
[defaults]
decision = {{ provider = "edge", model = "m" }}
"#,
        file.display()
    ))
    .unwrap();
    let llm = LLM::from_config(&cfg, |_| None).unwrap();
    llm.decision(DecisionRole::Decide).unwrap();
    assert!(!format!("{llm:?} {cfg:?}").contains(KEY));
    std::fs::remove_dir_all(&dir).unwrap();
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
        Mock::given(path("/v1/decisions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(read_json("decisions/decide.response.json")),
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
        make(ApiType::Decisions).decide(decide()).await.unwrap();
        // `expect(1)` on each mock is verified when the server drops.
    }
}
