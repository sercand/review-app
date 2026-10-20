//! Request goldens (neutral → wire JSON) and recorded responses (wire JSON → neutral).

mod common;

use common::*;
use review_llm::{
    ContentBlock,
    wire::{anthropic_reply, anthropic_request, responses_reply, responses_request},
};

#[test]
fn request_goldens() {
    for (name, req) in chat_cases() {
        golden(
            &format!("anthropic/{name}.request.json"),
            &anthropic_request(&req, false).unwrap(),
        );
        golden(
            &format!("responses/{name}.request.json"),
            &responses_request(&req, false).unwrap(),
        );
    }
}

#[test]
fn stream_flag_is_set() {
    let a = anthropic_request(&text(), true).unwrap();
    assert_eq!(a["stream"], true);
    let r = responses_request(&text(), true).unwrap();
    assert_eq!(r["stream"], true);
}

#[test]
fn recorded_anthropic_responses() {
    for (name, _) in chat_cases() {
        let reply =
            anthropic_reply(&read_json(&format!("anthropic/{name}.response.json"))).unwrap();
        check_case(name, &reply);
    }
}

#[test]
fn recorded_responses_responses() {
    for (name, _) in chat_cases() {
        let reply =
            responses_reply(&read_json(&format!("responses/{name}.response.json"))).unwrap();
        check_case(name, &reply);
    }
}

#[test]
fn recorded_thinking_blocks() {
    let a = anthropic_reply(&read_json("anthropic/text.response.json")).unwrap();
    assert!(
        a.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { text, .. } if !text.is_empty()))
    );
    let r = responses_reply(&read_json("responses/text.response.json")).unwrap();
    assert!(
        r.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { id: Some(_), .. }))
    );
}
