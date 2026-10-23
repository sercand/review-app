//! Contract tests for the settings-driven LLM client builder and the
//! "Test connection" probe (AI review foundation Task 9).
//!
//! The mock replies are the `review-llm` crate's own recorded fixtures, so
//! these tests pin the app-side wiring to real wire payloads.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use gh_review_desktop::ai::llm::{build_client, test_connection};
use gh_review_desktop::auth::store::{MemoryStore, set_llm_key};
use gh_review_desktop::persist::settings::{LlmApi, ModelSettings, ProviderSettings};
use gh_review_desktop::runtime::{Command, Event, ExecCtx, event_channel, execute};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The recorded Anthropic Messages non-streaming reply.
fn anthropic_reply() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../crates/llm/tests/fixtures/anthropic/text.response.json"
    ))
    .expect("the fixture is JSON")
}

/// The recorded OpenAI Responses non-streaming reply.
fn responses_reply() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../crates/llm/tests/fixtures/responses/text.response.json"
    ))
    .expect("the fixture is JSON")
}

/// The model choice the probe is run with.
fn model() -> ModelSettings {
    ModelSettings {
        provider: "work".into(),
        model: "qwen3.8-flash".into(),
        ..Default::default()
    }
}

/// A provider named `work` speaking `api` at `base_url`, with no extra headers.
fn provider(api: LlmApi, base_url: String) -> ProviderSettings {
    ProviderSettings {
        name: "work".into(),
        api,
        base_url,
        headers: BTreeMap::new(),
    }
}

#[tokio::test]
async fn anthropic_provider_hits_v1_messages_with_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k"))
        .and(body_partial_json(json!({
            "model": "qwen3.8-flash",
            "max_tokens": 16,
            "messages": [{
                "role": "user",
                "content": [{ "type": "text", "text": "Reply with the single word OK." }]
            }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_reply()))
        .expect(1)
        .mount(&server)
        .await;

    let client = build_client(
        &provider(LlmApi::AnthropicMessages, format!("{}/v1", server.uri())),
        SecretString::from("k"),
    )
    .unwrap();
    assert_eq!(
        test_connection(&client, &model()).await.unwrap(),
        "qwen3.8-flash"
    );
}

#[tokio::test]
async fn responses_provider_hits_v1_responses_with_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer k"))
        .and(body_partial_json(json!({
            "model": "qwen3.8-flash",
            "max_output_tokens": 16,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(responses_reply()))
        .expect(1)
        .mount(&server)
        .await;

    let client = build_client(
        &provider(LlmApi::OpenaiResponses, server.uri()),
        SecretString::from("k"),
    )
    .unwrap();
    assert_eq!(
        test_connection(&client, &model()).await.unwrap(),
        "qwen3.8-flash"
    );
}

#[tokio::test]
async fn a_401_is_an_error_that_does_not_echo_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid x-api-key" }
        })))
        .mount(&server)
        .await;

    let client = build_client(
        &provider(LlmApi::AnthropicMessages, format!("{}/v1", server.uri())),
        SecretString::from("sekrit-key"),
    )
    .unwrap();
    let error = test_connection(&client, &model()).await.unwrap_err();
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains("sekrit-key"), "{error}");
}

#[test]
fn unsupported_api_is_rejected() {
    let error = build_client(
        &provider(LlmApi::Unsupported, String::new()),
        SecretString::from("k"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "provider work uses an unsupported API");
}

/// Run the worker's "Test connection" command to completion and return its
/// answer.
async fn test_provider(
    store: Arc<MemoryStore>,
    provider: ProviderSettings,
    key: Option<String>,
) -> Result<String, String> {
    let ctx = Arc::new(ExecCtx::new(store));
    let (tx, rx) = event_channel();
    execute(
        ctx,
        Command::AiTestProvider {
            provider,
            model: model(),
            key,
        },
        tx,
    )
    .await;
    let Some(Event::AiProviderTested {
        provider: answered,
        result,
    }) = rx.recv_timeout(Duration::from_secs(10))
    else {
        panic!("expected the provider test's answer");
    };
    assert_eq!(answered, "work");
    result
}

/// The key typed into the Settings dialog is the one dialed — the keychain
/// is not consulted, and its older key never reaches the wire.
#[tokio::test]
async fn test_connection_prefers_the_typed_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "typed-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_reply()))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    set_llm_key(&*store, "work", "stored-key").unwrap();
    let result = test_provider(
        store,
        provider(LlmApi::AnthropicMessages, format!("{}/v1", server.uri())),
        Some("typed-key".into()),
    )
    .await;

    assert_eq!(result.unwrap(), "qwen3.8-flash");
}

/// With no typed key (the Settings window's current call), the stored key is
/// used, so an already-saved provider still tests.
#[tokio::test]
async fn test_connection_falls_back_to_the_keychain() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "stored-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_reply()))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    set_llm_key(&*store, "work", "stored-key").unwrap();
    let result = test_provider(
        store,
        provider(LlmApi::AnthropicMessages, format!("{}/v1", server.uri())),
        None,
    )
    .await;

    assert_eq!(result.unwrap(), "qwen3.8-flash");
}
