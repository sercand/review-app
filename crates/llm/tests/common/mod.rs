//! Shared test cases: the same neutral requests feed the wire goldens, the recorded fixtures,
//! the mock-server tests and the live tests.

#![allow(dead_code)]

use std::path::PathBuf;

use review_llm::{
    ContentBlock, DecideRequest, GenerateRequest, Image, Message, Question, Reply, StopReason,
    Tool, ToolChoice, ToolResultPart,
};
use serde_json::{Value, json};

pub const MODEL: &str = "qwen3.8-flash";

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn red_png() -> Image {
    Image::from_bytes(
        "image/png",
        &std::fs::read(fixtures().join("red.png")).unwrap(),
    )
}

pub fn read_json(path: &str) -> Value {
    let p = fixtures().join(path);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap()
}

/// Compares `actual` with a golden file; `UPDATE_FIXTURES=1` rewrites the file instead.
pub fn golden(path: &str, actual: &Value) {
    let p = fixtures().join(path);
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(&p, serde_json::to_string_pretty(actual).unwrap() + "\n").unwrap();
        return;
    }
    let expected = read_json(path);
    assert_eq!(&expected, actual, "golden mismatch: {path}");
}

fn tool(name: &str, description: &str, schema: Value) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    }
}

fn tap_tools() -> Vec<Tool> {
    vec![
        tool(
            "tap",
            "Tap an on-screen element by its id.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        ),
        tool(
            "done",
            "Finish the step.",
            json!({"type": "object", "properties": {"reason": {"type": "string"}}, "required": ["reason"]}),
        ),
    ]
}

fn weather_tool() -> Tool {
    tool(
        "get_weather",
        "Get the current weather for one city.",
        json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
    )
}

fn screenshot_tool() -> Tool {
    tool(
        "take_screenshot",
        "Take a screenshot of the device screen.",
        json!({"type": "object", "properties": {}}),
    )
}

pub fn model(mut r: GenerateRequest) -> GenerateRequest {
    r.model = Some(MODEL.into());
    r
}

/// Plain text, with a system prompt and cache hints.
pub fn text() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![Message::user_text("Say hi in one word.").cached()]);
    r.system = Some("You are a terse assistant.".into());
    r.max_tokens = Some(256);
    r.cache.system = true;
    model(r)
}

/// One forced tool call: `tool_choice: tap`, no parallel calls.
pub fn tool_forced() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![Message::user_text(
        "The screen shows a Play button with id `btn_play`. Start the game.",
    )]);
    r.tools = tap_tools();
    r.tool_choice = ToolChoice::Tool { name: "tap".into() };
    r.parallel_tool_calls = Some(false);
    r.max_tokens = Some(512);
    model(r)
}

/// Parallel tool calls allowed and required.
pub fn tool_parallel() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![Message::user_text(
        "What is the weather in Paris and in Tokyo? Call get_weather once for each city, in parallel, in this turn.",
    )]);
    r.tools = vec![weather_tool()];
    r.tool_choice = ToolChoice::Any;
    r.parallel_tool_calls = Some(true);
    r.max_tokens = Some(1024);
    model(r)
}

/// A finished turn with two parallel calls and their results; the model answers in text.
pub fn tool_results_parallel() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![
        Message::user_text("What is the weather in Paris and in Tokyo?"),
        Message::assistant(vec![
            ContentBlock::ToolUse {
                id: "call_paris".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"}),
            },
            ContentBlock::ToolUse {
                id: "call_tokyo".into(),
                name: "get_weather".into(),
                input: json!({"city": "Tokyo"}),
            },
        ]),
        Message::user(vec![
            ContentBlock::tool_result(
                "call_tokyo",
                vec![ToolResultPart::Text {
                    text: "Tokyo: 24°C, sunny".into(),
                }],
            ),
            ContentBlock::tool_result(
                "call_paris",
                vec![ToolResultPart::Text {
                    text: "Paris: 12°C, rain".into(),
                }],
            ),
        ]),
    ]);
    r.tools = vec![weather_tool()];
    r.max_tokens = Some(1024);
    model(r)
}

/// A screenshot returned inside a tool result.
pub fn tool_result_image() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![
        Message::user_text(
            "Take a screenshot and tell me the main color of the screen in one word.",
        ),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_shot".into(),
            name: "take_screenshot".into(),
            input: json!({}),
        }]),
        Message::user(vec![ContentBlock::tool_result(
            "call_shot",
            vec![
                ToolResultPart::Text {
                    text: "Screenshot:".into(),
                },
                ToolResultPart::Image(red_png()),
            ],
        )]),
    ]);
    r.tools = vec![screenshot_tool()];
    r.max_tokens = Some(1024);
    model(r)
}

pub fn chat_cases() -> Vec<(&'static str, GenerateRequest)> {
    vec![
        ("text", text()),
        ("tool_forced", tool_forced()),
        ("tool_parallel", tool_parallel()),
        ("tool_results_parallel", tool_results_parallel()),
        ("tool_result_image", tool_result_image()),
    ]
}

/// Three question types on one image.
pub fn decide() -> DecideRequest {
    let mut r = DecideRequest::new(
        vec![Message::user(vec![
            ContentBlock::text("Goal: start Level 1. The screenshot shows the current screen."),
            ContentBlock::Image(red_png()),
        ])],
        vec![
            Question::predicate("is_red", "Is the screen mostly red?"),
            Question::choice(
                "next_action",
                "Which action moves toward the goal?",
                [
                    ("tap:btn_level_1", "Tap the 'Level 1' button"),
                    ("wait", "Wait for the screen to change"),
                    ("ask_planner", "None of these fits"),
                ],
            ),
            Question::score(
                "ui_glitch",
                "How broken does the UI look?",
                [
                    ("Clean", "No visual problems"),
                    ("Minor glitch", "Cosmetic issue, still usable"),
                    ("Severe", "Broken layout or unreadable UI"),
                ],
            ),
        ],
    );
    r.model = Some(MODEL.into());
    r
}

// ---- per-case checks on a reply ----

pub fn check_text(reply: &Reply) {
    assert_eq!(reply.stop, StopReason::EndTurn);
    assert!(!reply.text().trim().is_empty());
    assert!(
        reply.usage.input > 0 && reply.usage.output > 0,
        "{:?}",
        reply.usage
    );
}

pub fn check_forced(reply: &Reply) {
    let call = reply.single_tool_use().unwrap();
    assert_eq!(call.name, "tap");
    assert_eq!(call.input["id"], "btn_play");
}

pub fn check_parallel(reply: &Reply) {
    assert_eq!(reply.stop, StopReason::ToolUse);
    let mut cities: Vec<String> = reply
        .tool_uses()
        .inspect(|c| assert_eq!(c.name, "get_weather"))
        .map(|c| c.input["city"].as_str().unwrap().to_owned())
        .collect();
    cities.sort();
    assert_eq!(cities, ["Paris", "Tokyo"]);
    let ids: std::collections::HashSet<_> = reply.tool_uses().map(|c| c.id).collect();
    assert_eq!(ids.len(), 2, "call ids are distinct");
}

pub fn check_results_parallel(reply: &Reply) {
    let text = reply.text();
    assert!(text.contains("12") && text.contains("24"), "{text}");
}

pub fn check_image(reply: &Reply) {
    assert!(
        reply.text().to_lowercase().contains("red"),
        "{}",
        reply.text()
    );
}

pub fn check_case(name: &str, reply: &Reply) {
    match name {
        "text" => check_text(reply),
        "tool_forced" => check_forced(reply),
        "tool_parallel" => check_parallel(reply),
        "tool_results_parallel" => check_results_parallel(reply),
        "tool_result_image" => check_image(reply),
        other => panic!("unknown case {other}"),
    }
}

// ---- the kedis-studio Decisions experiment ----

/// Step 7 of kedis-studio run `DecisionAgentRuns/20261015-160411`: the parent gate asks for
/// a year of birth and the goal says to type 1, 9, 9, 9. The screenshot carries set-of-marks
/// letters; `B` is the keypad's `1`.
pub fn kedis_step07() -> DecideRequest {
    let recorded = read_json("decisions/kedis_step07.request.json");
    let text = recorded["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    let jpg = std::fs::read(fixtures().join("decisions/kedis_step07.jpg")).unwrap();
    let mut req = DecideRequest::new(
        vec![Message::user(vec![
            ContentBlock::text(text),
            ContentBlock::Image(Image::from_bytes("image/jpeg", &jpg)),
        ])],
        serde_json::from_value(recorded["questions"].clone()).unwrap(),
    );
    req.model = Some(MODEL.into());
    req
}
