//! Wire shapes from the vendor docs that the recording engine doesn't emit: Anthropic
//! thinking signatures and refusals, OpenAI `encrypted_content`, refusals and incomplete
//! replies, unknown blocks, and decision refusals.

mod common;

use common::*;
use review_llm::{
    Answer, ApiType, ContentBlock, Error, GenerateRequest, Message, StopReason,
    wire::{
        anthropic_reply, anthropic_request, decisions_reply, responses_reply, responses_request,
    },
};
use serde_json::json;

#[test]
fn anthropic_signature_refusal_and_unknown_blocks() {
    let reply = anthropic_reply(&json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude",
        "content": [
            {"type": "thinking", "thinking": "plan", "signature": "sig=="},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "I can't help with that."}
        ],
        "stop_reason": "refusal",
        "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 3}
    }))
    .unwrap();
    assert_eq!(reply.stop, StopReason::Refusal);
    assert!(reply.single_tool_use().is_err());
    assert_eq!(
        reply.content[0],
        ContentBlock::Thinking {
            api: ApiType::AnthropicMessages,
            id: None,
            text: "plan".into(),
            signature: Some("sig==".into()),
        }
    );
    assert!(
        matches!(&reply.content[1], ContentBlock::Raw { api: ApiType::AnthropicMessages, value } if value["data"] == "opaque")
    );
    assert_eq!(
        reply.usage.input, 103,
        "input is the total, including cache"
    );
    assert_eq!((reply.usage.cache_read, reply.usage.cache_write), (90, 3));
}

#[test]
fn stop_sequence_and_unknown_stop() {
    let base = |stop: &str| {
        anthropic_reply(&json!({"content": [], "stop_reason": stop, "usage": {}}))
            .unwrap()
            .stop
    };
    assert_eq!(base("stop_sequence"), StopReason::StopSequence);
    assert_eq!(base("pause_turn"), StopReason::Other("pause_turn".into()));
}

#[test]
fn openai_encrypted_reasoning_refusal_and_incomplete() {
    let reply = responses_reply(&json!({
        "id": "resp_1", "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "a"}, {"type": "summary_text", "text": "b"}], "encrypted_content": "enc"},
            {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
             "content": [{"type": "refusal", "refusal": "No."}]},
            {"type": "web_search_call", "id": "ws_1", "status": "completed"}
        ],
        "usage": {"input_tokens": 50, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 4}}
    }))
    .unwrap();
    assert_eq!(reply.stop, StopReason::Refusal);
    assert_eq!(
        reply.content[0],
        ContentBlock::Thinking {
            api: ApiType::OpenaiResponses,
            id: Some("rs_1".into()),
            text: "a\n\nb".into(),
            signature: Some("enc".into()),
        }
    );
    assert_eq!(reply.text(), "No.");
    assert!(
        matches!(&reply.content[2], ContentBlock::Raw { api: ApiType::OpenaiResponses, value } if value["type"] == "web_search_call")
    );
    assert_eq!(
        (
            reply.usage.input,
            reply.usage.cache_read,
            reply.usage.reasoning
        ),
        (50, 40, 4)
    );

    let incomplete = responses_reply(&json!({
        "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [], "usage": {}
    }))
    .unwrap();
    assert_eq!(incomplete.stop, StopReason::MaxTokens);
}

/// A reply appended to the history replays its thinking only to the API it came from.
#[test]
fn thinking_replays_only_to_its_api() {
    let anthropic = anthropic_reply(&json!({
        "content": [
            {"type": "thinking", "thinking": "plan", "signature": "sig=="},
            {"type": "tool_use", "id": "t1", "name": "tap", "input": {"id": "a"}}
        ],
        "stop_reason": "tool_use", "usage": {}
    }))
    .unwrap();
    let openai = responses_reply(&json!({
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"},
            {"type": "function_call", "call_id": "c1", "name": "tap", "arguments": "{\"id\":\"a\"}"}
        ],
        "usage": {}
    }))
    .unwrap();
    let history = |reply: &review_llm::Reply, id: &str| {
        model(GenerateRequest::new(vec![
            Message::user_text("go"),
            reply.to_message(),
            Message::user(vec![ContentBlock::tool_result(id, vec![])]),
        ]))
    };

    let a = anthropic_request(&history(&anthropic, "t1"), false).unwrap();
    assert_eq!(
        a["messages"][1]["content"][0],
        json!({"type": "thinking", "thinking": "plan", "signature": "sig=="})
    );
    let r = responses_request(&history(&anthropic, "t1"), false).unwrap();
    assert!(
        r["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["type"] != "reasoning")
    );

    let r = responses_request(&history(&openai, "c1"), false).unwrap();
    assert_eq!(r["input"][1]["type"], "reasoning");
    assert_eq!(r["input"][1]["encrypted_content"], "enc");
    assert_eq!(r["input"][2]["type"], "function_call");
    let a = anthropic_request(&history(&openai, "c1"), false).unwrap();
    assert_eq!(
        a["messages"][1]["content"].as_array().unwrap().len(),
        1,
        "only the tool_use"
    );
}

#[test]
fn anthropic_cache_breakpoints_capped_at_four() {
    let mut req = text();
    req.tools = tool_forced().tools;
    req.cache.tools = true;
    req.messages = (0..4)
        .flat_map(|i| {
            [
                Message::user_text(format!("u{i}")).cached(),
                Message::assistant(vec![ContentBlock::text(format!("a{i}"))]),
            ]
        })
        .collect();
    let w = anthropic_request(&req, false).unwrap();
    let marked: Vec<usize> = w["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| m["content"][0].get("cache_control").is_some())
        .map(|(i, _)| i)
        .collect();
    // system + tools take two of the four; the two latest flagged messages keep theirs.
    assert_eq!(marked, [4, 6]);
    assert!(w["system"][0].get("cache_control").is_some());
    assert!(w["tools"][1].get("cache_control").is_some());
}

#[test]
fn anthropic_tool_results_go_first_in_user_turn() {
    let req = model(GenerateRequest::new(vec![Message::user(vec![
        ContentBlock::text("and also"),
        ContentBlock::tool_result("t1", vec![]),
    ])]));
    let w = anthropic_request(&req, false).unwrap();
    assert_eq!(w["messages"][0]["content"][0]["type"], "tool_result");
}

#[test]
fn reasoning_maps_to_thinking_budget_and_effort() {
    let mut req = text();
    req.max_tokens = Some(8192);
    req.reasoning = Some(review_llm::Reasoning {
        effort: review_llm::ReasoningEffort::High,
    });
    let a = anthropic_request(&req, false).unwrap();
    assert_eq!(
        a["thinking"],
        json!({"type": "enabled", "budget_tokens": 8191})
    );
    let r = responses_request(&req, false).unwrap();
    assert_eq!(r["reasoning"]["effort"], "high");
    assert_eq!(r["include"], json!(["reasoning.encrypted_content"]));

    req.max_tokens = Some(512);
    assert!(matches!(
        anthropic_request(&req, false),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn decision_refusal_and_missing_answer() {
    let req = decide();
    let d = decisions_reply(
        &req,
        &json!({"answers": [
            {"type": "refusal", "name": "is_red"},
            {"type": "choice", "name": "next_action", "choice": "wait", "probabilities": [], "confidence": 0.4},
            {"type": "score", "name": "ui_glitch", "score": 0.2, "probabilities": [], "confidence": 0.9}
        ]}),
    )
    .unwrap();
    assert!(matches!(d.get("is_red"), Some(Answer::Refusal { .. })));

    let err = decisions_reply(
        &req,
        &json!({"answers": [{"type": "refusal", "name": "is_red"}]}),
    )
    .unwrap_err();
    assert!(matches!(err, Error::MissingAnswer { name } if name == "next_action"));
}

#[test]
fn missing_model_is_invalid() {
    let mut req = text();
    req.model = None;
    assert!(matches!(
        anthropic_request(&req, false),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        responses_request(&req, false),
        Err(Error::InvalidRequest(_))
    ));
}

fn clip() -> review_llm::Video {
    review_llm::Video::from_bytes(
        "video/mp4",
        &std::fs::read(fixtures().join("redblue.mp4")).unwrap(),
    )
}

#[test]
fn video_goes_as_input_video_on_responses_and_decisions() {
    let v = clip();
    let req = model(GenerateRequest::new(vec![
        Message::user(vec![
            ContentBlock::text("What happens?"),
            ContentBlock::Video(v.clone()),
        ]),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "record".into(),
            input: json!({}),
        }]),
        Message::user(vec![ContentBlock::tool_result(
            "c1",
            vec![review_llm::ToolResultPart::Video(v.clone())],
        )]),
    ]));
    let r = responses_request(&req, false).unwrap();
    assert_eq!(
        r["input"][0]["content"][1],
        json!({"type": "input_video", "video_url": v.data_url()})
    );
    assert_eq!(
        r["input"][2]["output"][0],
        json!({"type": "input_video", "video_url": v.data_url()})
    );
    assert!(!r.to_string().contains("__review_llm_input_video__"));

    let mut d = decide();
    d.input[0].content.push(ContentBlock::Video(v.clone()));
    let w = review_llm::wire::decisions_request(&d).unwrap();
    assert_eq!(
        w["input"][0]["content"][2],
        json!({"type": "input_video", "video_url": v.data_url()})
    );
}

#[test]
fn anthropic_rejects_video() {
    let req = model(GenerateRequest::new(vec![Message::user(vec![
        ContentBlock::Video(clip()),
    ])]));
    let err = anthropic_request(&req, false).unwrap_err();
    assert!(
        matches!(&err, Error::InvalidRequest(m) if m.contains("no video")),
        "{err}"
    );
    let req = model(GenerateRequest::new(vec![Message::user(vec![
        ContentBlock::tool_result("c", vec![review_llm::ToolResultPart::Video(clip())]),
    ])]));
    assert!(anthropic_request(&req, false).is_err());
}
