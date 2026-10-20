//! Live tests against a real endpoint. Ignored by default and never run in CI:
//!
//! ```sh
//! QA_LLM_LIVE_BASE_URL=https://llm.edge.otsimo.com \
//! QA_LLM_LIVE_KEY_FILE=~/.config/kedis-qa/decisions.key \
//! QA_LLM_LIVE_MODEL=qwen3.8-flash \
//! cargo test -p review-llm --test live -- --ignored
//! ```
//!
//! The endpoint must serve Anthropic Messages, OpenAI Responses and `/v1/decisions`.

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
async fn live_decisions() {
    let (client, m) = live(ApiType::Decisions);
    let mut req = decide();
    req.model = Some(m);
    let d = client.decide(req).await.unwrap();
    println!("{:#?}", d.answers);
    assert_eq!(d.answers.len(), 3);
}

/// Loads a real user config end to end: `QA_LLM_LIVE_CONFIG=~/.config/otsimoqa/config.toml`.
#[tokio::test]
#[ignore = "live endpoint"]
async fn live_user_config() {
    use review_llm::{ChatRole, DecisionRole, LLM, LLMConfig};

    #[derive(serde::Deserialize)]
    struct File {
        llm: LLMConfig,
    }
    let path = std::env::var("QA_LLM_LIVE_CONFIG").expect("set QA_LLM_LIVE_CONFIG");
    let path = match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap()),
        None => path,
    };
    let file: File = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let trace = std::env::temp_dir().join(format!(
        "review-llm-live-trace-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&trace);
    let llm = LLM::from_config(&file.llm, |_| None)
        .unwrap()
        .with_trace(&trace);

    let mut req = text();
    req.model = None;
    let reply = llm
        .chat(ChatRole::Agent)
        .unwrap()
        .generate(req)
        .await
        .unwrap();
    println!("agent: {:?} via {:?}", reply.text(), reply.meta);
    check_case("text", &reply);

    let mut d = decide();
    d.model = None;
    let decision = llm
        .decision(DecisionRole::Decide)
        .unwrap()
        .decide(d)
        .await
        .unwrap();
    println!("decide: {:?}", decision.meta);
    assert_eq!(decision.answers.len(), 3);

    let text = std::fs::read_to_string(&trace).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "one line per HTTP exchange");
    for l in &lines {
        assert_eq!(l["response"]["status"], 200);
        let auth = &l["request"]["headers"];
        assert!(auth.get("x-api-key").or(auth.get("authorization")).unwrap() == "[redacted]");
    }
    println!("trace: {}", trace.display());
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

/// Replays the kedis-studio experiment's step 7 (parent gate, type 1-9-9-9): the model must
/// still pick `B`, the keypad's `1`.
#[tokio::test]
#[ignore = "live endpoint"]
async fn live_decisions_kedis_step07() {
    use review_llm::Answer;

    let (client, m) = live(ApiType::Decisions);
    let mut req = kedis_step07();
    req.model = Some(m);
    let d = client.decide(req).await.unwrap();
    let Some(Answer::Choice {
        choice,
        probabilities,
        ..
    }) = d.get("action")
    else {
        panic!("action is a choice")
    };
    let p_b = probabilities
        .iter()
        .find(|p| p.value == "B")
        .map(|p| p.probability);
    println!("kedis step 7: chose {choice}, p(B) = {p_b:?}");
    assert_eq!(choice, "B");
    let Some(Answer::Predicate { probability, .. }) = d.get("goal_reached") else {
        panic!("goal_reached is a predicate")
    };
    assert!(
        *probability < 0.5,
        "the canvas is not open yet: {probability}"
    );
}
