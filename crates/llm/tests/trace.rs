//! The JSONL trace file: one line per HTTP attempt, for every API, with keys redacted.

mod common;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use common::*;
use futures::StreamExt;
use review_llm::{ApiType, Client, ClientConfig, RetryPolicy, StreamEvent};
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

const KEY: &str = "sk-trace-secret-key";

fn trace_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("review-llm-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{name}.jsonl"));
    let _ = std::fs::remove_file(&p);
    p
}

fn lines(p: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    assert!(!text.contains(KEY), "the key leaked into the trace");
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn client(base: &str, api: ApiType, trace: &Path) -> Client {
    let mut cfg = ClientConfig::new(api, SecretString::from(KEY)).base_url(base.parse().unwrap());
    cfg.provider = "mock".into();
    cfg.retry = RetryPolicy {
        max_retries: 2,
        base: Duration::from_millis(1),
        max: Duration::from_millis(5),
    };
    cfg.headers.insert("x-team".into(), "qa".into());
    cfg.trace = Some(trace.to_path_buf());
    Client::new(cfg).unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

#[tokio::test]
async fn unary_calls_on_every_api() {
    let server = MockServer::start().await;
    let chat = json!({"content": [{"type": "text", "text": "hi"}], "stop_reason": "end_turn", "usage": {}});
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat.clone())
                .insert_header("x-request-id", "r1"),
        )
        .mount(&server)
        .await;
    let resp = json!({"status": "completed", "output": [], "usage": {}});
    Mock::given(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp.clone()))
        .mount(&server)
        .await;
    let t = trace_file("unary");
    client(&server.uri(), ApiType::AnthropicMessages, &t)
        .generate(text())
        .await
        .unwrap();
    client(&server.uri(), ApiType::OpenaiResponses, &t)
        .generate(text())
        .await
        .unwrap();

    let l = lines(&t);
    assert_eq!(l.len(), 2);
    let a = &l[0];
    assert_eq!(a["api"], "anthropic_messages");
    assert_eq!(a["provider"], "mock");
    assert_eq!(a["model"], MODEL);
    assert_eq!(a["attempt"], 1);
    assert_eq!(a["stream"], false);
    assert!(a["time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(a["request"]["method"], "POST");
    assert!(
        a["request"]["url"]
            .as_str()
            .unwrap()
            .ends_with("/v1/messages")
    );
    assert_eq!(a["request"]["headers"]["x-api-key"], "[redacted]");
    assert_eq!(a["request"]["headers"]["anthropic-version"], "2023-06-01");
    assert_eq!(a["request"]["headers"]["x-team"], "qa");
    assert_eq!(
        a["request"]["body"],
        read_json("anthropic/text.request.json")
    );
    assert_eq!(a["response"]["status"], 200);
    assert_eq!(a["response"]["headers"]["x-request-id"], "r1");
    assert_eq!(a["response"]["body"], chat);
    assert!(a["error"].is_null());

    let r = &l[1];
    assert_eq!(r["api"], "openai_responses");
    assert_eq!(r["request"]["headers"]["authorization"], "[redacted]");
    assert_eq!(r["request"]["headers"]["x-team"], "qa");
    assert_eq!(
        r["request"]["body"],
        read_json("responses/text.request.json")
    );
    assert_eq!(r["response"]["body"], resp);
}

#[tokio::test]
async fn one_line_per_attempt_with_the_same_call_id() {
    for (api, p) in [
        (ApiType::AnthropicMessages, "/v1/messages"),
        (ApiType::OpenaiResponses, "/v1/responses"),
    ] {
        let server = MockServer::start().await;
        Mock::given(path(p))
            .respond_with(ResponseTemplate::new(503).set_body_string(format!("busy, key {KEY}")))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        let ok = match api {
            ApiType::AnthropicMessages => {
                json!({"content": [], "stop_reason": "end_turn", "usage": {}})
            }
            _ => json!({"status": "completed", "output": [], "usage": {}}),
        };
        Mock::given(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok))
            .mount(&server)
            .await;
        let t = trace_file(&format!("retry-{api}"));
        let c = client(&server.uri(), api, &t);
        drop(c.generate(text()).await.unwrap());
        let l = lines(&t);
        assert_eq!(l.len(), 3, "{api}");
        let attempts: Vec<_> = l.iter().map(|x| x["attempt"].as_u64().unwrap()).collect();
        assert_eq!(attempts, [1, 2, 3], "{api}");
        assert!(l.iter().all(|x| x["call_id"] == l[0]["call_id"]), "{api}");
        assert_eq!(l[0]["response"]["status"], 503);
        assert!(
            l[0]["error"].as_str().unwrap().contains("503"),
            "{api}: {}",
            l[0]["error"]
        );
        assert!(l[2]["error"].is_null());
    }
}

#[tokio::test]
async fn streams_record_their_events() {
    for (api, p, fixture) in [
        (
            ApiType::AnthropicMessages,
            "/v1/messages",
            "anthropic/text.sse",
        ),
        (
            ApiType::OpenaiResponses,
            "/v1/responses",
            "responses/text.sse",
        ),
    ] {
        let server = MockServer::start().await;
        let body = std::fs::read_to_string(fixtures().join(fixture)).unwrap();
        Mock::given(path(p))
            .respond_with(sse(body))
            .mount(&server)
            .await;
        let t = trace_file(&format!("stream-{api}"));
        let events: Vec<_> = client(&server.uri(), api, &t)
            .generate_stream(text())
            .collect()
            .await;
        assert!(matches!(events.last(), Some(Ok(StreamEvent::Done(_)))));
        let l = lines(&t);
        assert_eq!(l.len(), 1, "{api}");
        assert_eq!(l[0]["stream"], true);
        assert_eq!(l[0]["request"]["body"]["stream"], true);
        assert_eq!(l[0]["response"]["status"], 200);
        let ev = l[0]["response"]["body"].as_array().unwrap();
        assert!(ev.len() > 3, "{api}");
        let first = ev[0]["event"].as_str().unwrap();
        assert!(
            first == "message_start" || first == "response.created",
            "{api}: {first}"
        );
        assert!(ev[0]["data"].is_object());
    }
}

#[tokio::test]
async fn transport_errors_have_no_response() {
    let t = trace_file("transport");
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from(KEY))
        .base_url("http://127.0.0.1:9".parse().unwrap());
    cfg.retry = RetryPolicy::none();
    cfg.trace = Some(t.clone());
    assert!(Client::new(cfg).unwrap().generate(text()).await.is_err());
    let l = lines(&t);
    assert_eq!(l.len(), 1);
    assert!(l[0]["response"].is_null());
    assert!(l[0]["error"].is_string());
}

#[tokio::test]
async fn key_in_error_bodies_and_headers_is_redacted() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("x-echo", format!("key={KEY}").as_str())
                .set_body_json(
                    json!({"error": {"message": format!("bad key {KEY}"), "type": "auth"}}),
                ),
        )
        .mount(&server)
        .await;
    let t = trace_file("redact");
    assert!(
        client(&server.uri(), ApiType::OpenaiResponses, &t)
            .generate(text())
            .await
            .is_err()
    );
    let l = lines(&t); // asserts the key is absent from the whole file
    assert_eq!(l[0]["response"]["headers"]["x-echo"], "[redacted]");
    assert!(l[0]["error"].as_str().unwrap().contains("[redacted]"));
}

#[tokio::test]
async fn unwritable_trace_path_never_fails_the_call() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"content": [], "stop_reason": "end_turn", "usage": {}})),
        )
        .mount(&server)
        .await;
    let bad = PathBuf::from("/dev/null/not-a-dir/trace.jsonl");
    client(&server.uri(), ApiType::AnthropicMessages, &bad)
        .generate(text())
        .await
        .unwrap();
}
