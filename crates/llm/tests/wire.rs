//! Request goldens (neutral → wire JSON) and recorded responses (wire JSON → neutral).

mod common;

use common::*;
use review_llm::{
    Answer, ContentBlock,
    wire::{
        anthropic_reply, anthropic_request, decisions_reply, decisions_request, responses_reply,
        responses_request,
    },
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
    golden(
        "decisions/decide.request.json",
        &decisions_request(&decide()).unwrap(),
    );
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

#[test]
fn recorded_decision() {
    let req = decide();
    let d = decisions_reply(&req, &read_json("decisions/decide.response.json")).unwrap();
    match d.get("is_red").unwrap() {
        Answer::Predicate {
            probability, extra, ..
        } => {
            assert!(*probability > 0.5);
            assert!(extra.contains_key("label_mass"), "extra fields are kept");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(d.get("next_action"), Some(Answer::Choice { probabilities, .. }) if probabilities.len() == 3)
    );
    assert!(
        matches!(d.get("ui_glitch"), Some(Answer::Score { probabilities, .. }) if probabilities.len() == 3)
    );
    assert!(d.usage.input > 0);
}

/// The kedis-studio experiment's step 7 request is reproduced exactly from neutral types, and
/// its recorded response parses into the answers the experiment acted on (#85).
#[test]
fn kedis_experiment_step07_round_trips() {
    let req = kedis_step07();
    let ContentBlock::Image(img) = &req.input[0].content[1] else {
        panic!("the second part is the screenshot")
    };
    let mut expected = read_json("decisions/kedis_step07.request.json");
    expected["input"][0]["content"][1]["image_url"] = img.data_url().into();
    assert_eq!(decisions_request(&req).unwrap(), expected);

    let d = decisions_reply(&req, &read_json("decisions/kedis_step07.response.json")).unwrap();
    let Some(Answer::Choice {
        choice,
        probabilities,
        confidence,
        extra,
        ..
    }) = d.get("action")
    else {
        panic!("action is a choice")
    };
    assert_eq!(choice, "B", "the keypad's 1");
    assert_eq!(probabilities.len(), 13, "wait + 12 marked elements");
    let p_b = probabilities
        .iter()
        .find(|p| p.value == "B")
        .unwrap()
        .probability;
    assert!(p_b > 0.8, "{p_b}");
    assert!(*confidence > 0.9);
    assert!(extra.contains_key("label_mass"));
    for name in ["goal_reached", "looks_broken"] {
        let Some(Answer::Predicate { probability, .. }) = d.get(name) else {
            panic!("{name} is a predicate")
        };
        assert!(*probability < 0.1, "{name}: {probability}");
    }
    assert_eq!((d.usage.input, d.usage.cache_read), (1916, 1050));
}
