//! Live tests against a real endpoint. Ignored by default and never run in CI:
//!
//! ```sh
//! QA_LLM_LIVE_BASE_URL=https://llm.edge.otsimo.com \
//! QA_LLM_LIVE_KEY_FILE=~/.config/reviewer/llm.key \
//! QA_LLM_LIVE_MODEL=qwen3.8-flash \
//! cargo test -p review-llm --test live -- --ignored
//! ```
//!
//! The endpoint must serve Anthropic Messages and OpenAI Responses.

mod common;

use common::*;
use futures::StreamExt;
use review_llm::{ApiType, Client, ClientConfig, GenerateRequest, Result, StreamEvent};
use secrecy::SecretString;

fn live(api: ApiType) -> (Client, String) {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let key_file = var("QA_LLM_LIVE_KEY_FILE");
    let key_file = match key_file.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", var("HOME")),
        None => key_file,
    };
    let key = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_owned();
    let cfg = ClientConfig::new(api, SecretString::from(key))
        .base_url(var("QA_LLM_LIVE_BASE_URL").parse().unwrap());
    (Client::new(cfg).unwrap(), var("QA_LLM_LIVE_MODEL"))
}

fn with_model(mut r: GenerateRequest, m: &str) -> GenerateRequest {
    r.model = Some(m.to_owned());
    r
}

async fn chat_cases_on(api: ApiType) {
    let (client, m) = live(api);
    for (name, req) in [("tool_parallel", tool_parallel()), ("text", text())] {
        let events: Vec<StreamEvent> = client
            .generate_stream(with_model(req, &m))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_>>()
            .unwrap();
        let Some(StreamEvent::Done(reply)) = events.last() else {
            panic!("{api} {name}: no Done")
        };
        check_case(name, reply);
    }
    for (name, req) in chat_cases() {
        let reply = client.generate(with_model(req, &m)).await.unwrap();
        println!("{api} {name}: {:?} {:?}", reply.stop, reply.text());
        check_case(name, &reply);
    }
}

#[tokio::test]
#[ignore = "live endpoint"]
async fn live_anthropic_messages() {
    chat_cases_on(ApiType::AnthropicMessages).await;
}

#[tokio::test]
#[ignore = "live endpoint"]
async fn live_openai_responses() {
    chat_cases_on(ApiType::OpenaiResponses).await;
}

#[tokio::test]
#[ignore = "live endpoint"]
async fn live_responses_video() {
    let (client, m) = live(ApiType::OpenaiResponses);
    let clip = review_llm::Video::from_bytes(
        "video/mp4",
        &std::fs::read(fixtures().join("redblue.mp4")).unwrap(),
    );
    let mut req = GenerateRequest::new(vec![review_llm::Message::user(vec![
        review_llm::ContentBlock::text(
            "What colors appear in this video, in order? Answer briefly.",
        ),
        review_llm::ContentBlock::Video(clip),
    ])]);
    req.max_tokens = Some(512);
    let reply = client.generate(with_model(req, &m)).await.unwrap();
    let text = reply.text().to_lowercase();
    println!("video: {text:?}");
    let (red, blue) = (
        text.find("red").expect("red"),
        text.find("blue").expect("blue"),
    );
    assert!(red < blue, "red comes before blue: {text}");
}
