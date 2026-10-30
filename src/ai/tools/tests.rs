//! The repo tools over a [`LocalHost`] on a temp dir, plus the registry's safety
//! net. No network.
//!
//! Adapted from `qa-agent/tests/tools.rs`: `edit_file` cases are gone (the review
//! agent never writes), and `read_file` numbers its lines.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use review_llm::ToolResultPart;
use review_sandbox::{
    Entry, ExecOutput, GrepMatch, Listing, LocalHost, MAX_GLOB, MAX_GREP, OpError, SandboxOps,
};
use serde_json::{Value, json};

use super::{
    AgentRef, AgentRole, IMAGE_MAX_SIDE, MAX_TOOL_OUTPUT, Registry, Tool, ToolCx, ToolOutput, all,
    is_forbidden_tool_name, truncate_middle, validate_rev,
};
use crate::ai::interaction::{ApprovalDecision, Interaction, NoInteraction};
use crate::ai::testing::{InteractionCall, ScriptedInteraction};

fn tool(name: &str) -> Arc<dyn Tool> {
    all()
        .into_iter()
        .find(|t| t.definition().name == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

fn text(out: &ToolOutput) -> &str {
    assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
    match &out.parts[0] {
        ToolResultPart::Text { text } => text,
        other => panic!("not text: {other:?}"),
    }
}

fn image(out: &ToolOutput) -> &review_llm::Image {
    assert_eq!(out.parts.len(), 1, "{:?}", out.parts);
    match &out.parts[0] {
        ToolResultPart::Image(img) => img,
        other => panic!("not an image: {other:?}"),
    }
}

/// A reviewer's handle, for every test context.
fn agent() -> AgentRef {
    AgentRef {
        id: 1,
        role: AgentRole::Reviewer,
        label: "reviewer".to_string(),
    }
}

/// A context over `sandbox`, rooted at `repo_root`, for one reviewer, with no
/// interaction attached: `bash` asks nobody and is therefore denied anything
/// the classifier would ask about.
fn ctx(sandbox: Arc<dyn SandboxOps>, repo_root: PathBuf) -> ToolCx {
    ctx_with(sandbox, repo_root, Arc::new(NoInteraction))
}

/// A context with `interaction` wired in, and no configured commands.
fn ctx_with(
    sandbox: Arc<dyn SandboxOps>,
    repo_root: PathBuf,
    interaction: Arc<dyn Interaction>,
) -> ToolCx {
    ToolCx {
        sandbox,
        repo_root,
        agent: agent(),
        interaction,
        allowed: Arc::new(std::sync::Mutex::new(Vec::new())),
        for_review: Arc::new(std::sync::Mutex::new(Vec::new())),
        snapshot: None,
        context: None,
        run: crate::test_support::ai_run_state(),
        assigned: Arc::new(Vec::new()),
        spawner: None,
        prepare: None,
        // Most tests here exercise the approval prompt, so they ask; the
        // `auto_mode_*` tests cover the default.
        approval: Arc::new(ApprovalSetup::from_settings(&ask_mode(), None).into()),
    }
}

/// `[ai.auto_approve]` with prompts: what nothing vouched for is asked.
fn ask_mode() -> AutoApproveSettings {
    AutoApproveSettings {
        mode: ApprovalMode::Ask,
        ..AutoApproveSettings::default()
    }
}

/// `ctx` with `allowed` seeded, as a repo's configured commands would be.
fn ctx_allowing(sandbox: Arc<dyn SandboxOps>, repo_root: PathBuf, allowed: &[&str]) -> ToolCx {
    let cx = ctx(sandbox, repo_root);
    cx.allow_segments(allowed.iter().map(|s| (*s).to_string()));
    cx
}

/// The exact lines remembered as approved "for this review".
fn remembered(cx: &ToolCx) -> Vec<String> {
    cx.for_review
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// A context over `host`, with the sandbox's root as the repo root.
fn host_ctx(host: &LocalHost) -> ToolCx {
    ctx(Arc::new(host.clone()), host.root().path().to_path_buf())
}

async fn call(host: &LocalHost, name: &str, input: Value) -> ToolOutput {
    tool(name).call(input, &host_ctx(host)).await
}

async fn bash(cx: &ToolCx, input: Value) -> ToolOutput {
    tool("bash").call(input, cx).await
}

fn host() -> (tempfile::TempDir, LocalHost) {
    let dir = crate::test_support::tempdir();
    let host = LocalHost::new(dir.path()).unwrap();
    (dir, host)
}

#[tokio::test]
async fn bash_tool_reports_exit_code() {
    let (_dir, host) = host();
    // The repo configures these commands, so `bash` runs them without asking.
    let cx = ctx_allowing(
        Arc::new(host.clone()),
        host.root().path().to_path_buf(),
        &["echo", "exit"],
    );

    let out = bash(&cx, json!({"command": "echo hi"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out).trim_end(), "hi");

    // A non-zero exit is information, not a tool error.
    let out = bash(&cx, json!({"command": "echo oops; exit 3"})).await;
    assert!(!out.is_error);
    let t = text(&out);
    assert!(t.contains("oops"), "{t}");
    assert!(t.ends_with("\n(exit code 3)"), "{t}");

    let out = bash(&cx, json!({})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "command is required unless restart is true");

    let out = bash(&cx, json!({"command": "", "restart": true})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(!text(&out).is_empty());

    // Shell state persists across calls until a restart.
    // (A bare assignment and `set | grep`, not `export` and `echo $QA_X`: the
    // engine treats `export` as an unsupported declaration command and asks.)
    bash(&cx, json!({"command": "QA_X=1"})).await;
    let out = bash(&cx, json!({"command": "set | grep QA_X"})).await;
    assert!(text(&out).contains("QA_X=1"), "{}", text(&out));
    let out = bash(&cx, json!({"command": "set | grep QA_X", "restart": true})).await;
    assert!(!text(&out).contains("QA_X=1"), "{}", text(&out));

    // Bad input is a tool error, not a panic.
    let out = bash(&cx, json!({"command": 5})).await;
    assert!(out.is_error);
}

/// A context over `host` with a scripted interaction, for approval tests.
fn interaction_ctx(host: &LocalHost, interaction: Arc<ScriptedInteraction>) -> ToolCx {
    ctx_with(
        Arc::new(host.clone()),
        host.root().path().to_path_buf(),
        interaction,
    )
}

#[tokio::test]
async fn allowed_command_runs_without_asking() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new());
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "ls"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn unknown_command_asks_and_runs_once() {
    let (dir, host) = host();
    let interaction = Arc::new(
        ScriptedInteraction::new()
            .approve(ApprovalDecision::Once)
            .approve(ApprovalDecision::Once),
    );
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch x"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(dir.path().join("x").exists());

    // `Once` is for one call: the next identical command asks again.
    let out = bash(&cx, json!({"command": "touch x"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    let calls = interaction.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in calls {
        match call {
            InteractionCall::Approve { command, .. } => assert_eq!(command, "touch x"),
            other => panic!("unexpected call: {other:?}"),
        }
    }
}

#[tokio::test]
async fn for_review_is_remembered() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch remembered"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(dir.path().join("remembered").exists());

    // The same command again runs without a second approval.
    let out = bash(&cx, json!({"command": "touch remembered"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    let calls = interaction.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
}

#[tokio::test]
async fn for_review_remembers_the_exact_line() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch a && touch b"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(dir.path().join("a").exists());
    assert!(dir.path().join("b").exists());
    assert_eq!(remembered(&cx), ["touch a && touch b"]);
    // The line itself is remembered, not its segments.
    assert!(cx.allowed_snapshot().is_empty());

    // The identical line runs silently.
    let out = bash(&cx, json!({"command": "touch a && touch b"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert_eq!(interaction.calls().len(), 1);

    // A longer line is a different line: no decision is queued, so it asks and
    // is denied.
    let out = bash(&cx, json!({"command": "touch a && touch b && touch c"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert!(!dir.path().join("c").exists());
    assert_eq!(interaction.calls().len(), 2);
}

#[tokio::test]
async fn denied_command_returns_the_reason() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Deny {
        reason: Some("no writes here".to_string()),
    }));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch nope"})).await;
    assert!(out.is_error);
    let t = text(&out);
    assert!(t.contains("the user declined this command"), "{t}");
    assert!(t.contains("no writes here"), "{t}");
    assert!(!dir.path().join("nope").exists());
    assert_eq!(interaction.calls().len(), 1);
}

#[tokio::test]
async fn no_interaction_denies_without_a_reason() {
    let (dir, host) = host();
    let cx = host_ctx(&host);

    let out = bash(&cx, json!({"command": "touch denied"})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "the user declined this command");
    assert!(!dir.path().join("denied").exists());
}

#[tokio::test]
async fn for_review_does_not_grant_appended_arguments() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch a"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(dir.path().join("a").exists());
    assert_eq!(remembered(&cx), ["touch a"]);

    // No decision is queued: the scripted user denies. `touch a b` is a
    // different line from the approved `touch a`, so it asks again.
    let out = bash(&cx, json!({"command": "touch a b"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert!(!dir.path().join("b").exists());
    assert_eq!(interaction.calls().len(), 2);
}

#[tokio::test]
async fn for_review_is_whitespace_insensitive() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch  a"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert_eq!(remembered(&cx), ["touch a"]);

    // Runs of whitespace are collapsed, so the same line with one space runs
    // without asking. Everything else is compared verbatim.
    let out = bash(&cx, json!({"command": "touch a"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(dir.path().join("a").exists());
    assert_eq!(interaction.calls().len(), 1);
}

#[tokio::test]
async fn for_review_never_grants_a_path_qualified_wrapper() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    // `echo x` feeds the pipe; `env` runs `sh`, which tries to run `x`.
    let out = bash(&cx, json!({"command": "echo x | /usr/bin/env sh"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert_eq!(remembered(&cx), ["echo x | /usr/bin/env sh"]);

    // No decision is queued: the scripted user denies. A path-qualified
    // wrapper is not a prefix grant either.
    let out = bash(&cx, json!({"command": "/usr/bin/env sh -c 'touch zz'"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert_eq!(interaction.calls().len(), 2);
}

#[tokio::test]
async fn for_review_never_grants_a_bare_wrapper() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    // `echo true` feeds the pipe, so `sh` reads `true` and exits.
    let out = bash(&cx, json!({"command": "echo true | sh"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    // The whole line is remembered — never `sh` on its own.
    assert_eq!(remembered(&cx), ["echo true | sh"]);
    assert!(cx.allowed_snapshot().is_empty());

    // A bare `sh` entry would let this run silently. No decision is queued, so
    // the scripted user denies it: nothing runs and the call is recorded. (The
    // inner `touch` is not vouched for; `sh -c true` would now run silently.)
    let out = bash(&cx, json!({"command": "sh -c 'touch zz'"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    let calls = interaction.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        matches!(&calls[1], InteractionCall::Approve { command, .. } if command == "sh -c 'touch zz'"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn for_review_preserves_whitespace_inside_quotes() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "sh -c 'touch  hi'"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert_eq!(remembered(&cx), ["sh -c 'touch  hi'"]);

    // No decision is queued: the scripted user denies. Spaces inside the quotes
    // are command text, so this is a different line.
    let out = bash(&cx, json!({"command": "sh -c 'touch hi'"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert_eq!(interaction.calls().len(), 2);
}

#[tokio::test]
async fn for_review_preserves_newlines_inside_quotes() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "sh -c 'touch a'"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert_eq!(remembered(&cx), ["sh -c 'touch a'"]);

    // A newline inside the quotes separates two commands inside `sh -c`; it is
    // not formatting. No decision is queued, so this asks and is denied.
    let out = bash(&cx, json!({"command": "sh -c 'touch\na'"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert_eq!(interaction.calls().len(), 2);
}

#[tokio::test]
async fn for_review_does_not_grant_a_newline_separated_command() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::ForReview));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch a touch b"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(dir.path().join("a").exists());
    assert!(dir.path().join("b").exists());
    assert_eq!(remembered(&cx), ["touch a touch b"]);

    // Clear the marker so the denied call cannot recreate it.
    std::fs::remove_file(dir.path().join("b")).unwrap();

    // A newline outside quotes separates commands; it is not argument
    // whitespace, so this is a different line. No decision is queued: denied.
    let out = bash(&cx, json!({"command": "touch a\ntouch b"})).await;
    assert!(out.is_error, "{:?}", out.parts);
    assert!(!dir.path().join("b").exists());
    assert_eq!(interaction.calls().len(), 2);
}

/// A 1×1 PNG, encoded by the `image` crate.
fn png_1x1() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(1, 1, image::Rgb([10, 20, 30]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[tokio::test]
async fn read_file_text_binary_and_image() {
    let (dir, host) = host();
    std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
    let mut bin = b"abc".to_vec();
    bin.push(0);
    bin.extend_from_slice(b"def");
    std::fs::write(dir.path().join("blob.dat"), &bin).unwrap();
    std::fs::write(dir.path().join("shot.png"), png_1x1()).unwrap();
    // No image extension: found by its magic bytes.
    std::fs::write(dir.path().join("shot.bin"), png_1x1()).unwrap();
    std::fs::write(dir.path().join("big.txt"), "x".repeat(MAX_TOOL_OUTPUT * 2)).unwrap();

    // Text comes back numbered, `cat -n` style.
    let out = call(&host, "read_file", json!({"path": "a.txt"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "     1\thello\n     2\tworld");

    let out = call(&host, "read_file", json!({"path": "blob.dat"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "<binary>7 bytes</binary>");

    // PNG and JPEG are re-encoded (JPEG) through review_llm's encoder.
    for path in ["shot.png", "shot.bin"] {
        let out = call(&host, "read_file", json!({"path": path})).await;
        assert!(!out.is_error, "{path}: {:?}", out.parts);
        let img = image(&out);
        assert_eq!(img.media_type, "image/jpeg", "{path}");
        assert_eq!(img.dimensions().unwrap(), (1, 1));
    }
    let out = call(&host, "read_file", json!({"path": "big.txt"})).await;
    assert!(!out.is_error);
    let t = text(&out);
    assert!(t.contains("... [truncated "), "{}", &t[..100]);
    assert!(t.len() < MAX_TOOL_OUTPUT + 100);

    let out = call(&host, "read_file", json!({"path": "missing.txt"})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "not found: missing.txt");

    let out = call(&host, "read_file", json!({"path": "../outside"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("escapes the project root"),
        "{}",
        text(&out)
    );

    let out = call(&host, "read_file", json!({})).await;
    assert!(out.is_error);
}

#[tokio::test]
async fn read_file_ranges_lines_with_numbers() {
    let (dir, host) = host();
    let content: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    std::fs::write(dir.path().join("ten.txt"), content).unwrap();

    let out = call(
        &host,
        "read_file",
        json!({"path": "ten.txt", "offset": 3, "limit": 2}),
    )
    .await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert_eq!(
        text(&out),
        "     3\tl3\n     4\tl4\n[showing lines 3-4 of 10]"
    );

    // A limit alone starts at line 1 and says what came back.
    let out = call(&host, "read_file", json!({"path": "ten.txt", "limit": 3})).await;
    assert!(!out.is_error);
    assert_eq!(
        text(&out),
        "     1\tl1\n     2\tl2\n     3\tl3\n[showing lines 1-3 of 10]"
    );

    // A range up to the last line still says how long the file is.
    let out = call(
        &host,
        "read_file",
        json!({"path": "ten.txt", "offset": 9, "limit": 2000}),
    )
    .await;
    assert!(!out.is_error);
    assert_eq!(
        text(&out),
        "     9\tl9\n    10\tl10\n[showing lines 9-10 of 10]"
    );

    // No range on a whole file: numbered, no summary.
    let out = call(&host, "read_file", json!({"path": "ten.txt", "offset": 1})).await;
    assert!(!out.is_error);
    let t = text(&out);
    assert!(t.starts_with("     1\tl1\n"), "{t}");
    assert!(!t.contains("[showing"), "{t}");
}

#[tokio::test]
async fn read_file_offset_past_end_is_an_error() {
    let (dir, host) = host();
    let content: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    std::fs::write(dir.path().join("ten.txt"), content).unwrap();

    let out = call(&host, "read_file", json!({"path": "ten.txt", "offset": 11})).await;
    assert!(out.is_error);
    assert!(text(&out).contains("past the end"), "{}", text(&out));

    // An empty file: a plain read says so, while any offset is past the end.
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    let out = call(&host, "read_file", json!({"path": "empty.txt"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "(empty file)");
    let out = call(
        &host,
        "read_file",
        json!({"path": "empty.txt", "offset": 1}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("past the end of the file (0 lines)"),
        "{}",
        text(&out)
    );

    // A zero offset or limit is rejected rather than read as "the start" or "nothing".
    for input in [
        json!({"path": "ten.txt", "offset": 0}),
        json!({"path": "ten.txt", "limit": 0}),
    ] {
        let out = call(&host, "read_file", input.clone()).await;
        assert!(out.is_error, "{input}");
    }
}

#[tokio::test]
async fn read_file_scales_large_images_and_rejects_broken_ones() {
    let (dir, host) = host();

    // A 6000x6000 PNG with some texture: far over the provider limits as is, but
    // under the 15 MiB a sandbox read allows.
    let big = image::RgbImage::from_fn(6000, 6000, |x, y| {
        let v = ((x / 24 + y / 24) as u8) ^ (((x / 3) ^ (y / 3)) & 1) as u8;
        image::Rgb([v, v.wrapping_add(85), v.wrapping_add(170)])
    });
    big.save(dir.path().join("huge.png")).unwrap();
    let size = std::fs::metadata(dir.path().join("huge.png"))
        .unwrap()
        .len();
    assert!(size > 5_000_000 && size < 15 * 1024 * 1024, "{size} bytes");
    let out = call(&host, "read_file", json!({"path": "huge.png"})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    let img = image(&out);
    assert_eq!(img.media_type, "image/jpeg");
    let (w, h) = img.dimensions().unwrap();
    assert_eq!(w, h);
    // The encoder floors the scaled size, so it may land a pixel short.
    assert!((IMAGE_MAX_SIDE - 1..=IMAGE_MAX_SIDE).contains(&w), "{w}");
    assert!(
        img.data.len() < 5_000_000,
        "{} base64 bytes",
        img.data.len()
    );

    // Image extension or magic, but not decodable: a text tool error, no image part.
    std::fs::write(dir.path().join("empty.png"), b"").unwrap();
    std::fs::write(dir.path().join("pic.JPG"), [0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).unwrap();
    std::fs::write(dir.path().join("fake.jpg"), b"not an image").unwrap();
    for path in ["empty.png", "pic.JPG", "fake.jpg"] {
        let out = call(&host, "read_file", json!({"path": path})).await;
        assert!(out.is_error, "{path}");
        assert!(
            text(&out).starts_with(&format!("cannot decode image {path}")),
            "{}",
            text(&out)
        );
    }

    // GIF and WebP pass through (the encoder cannot decode them).
    let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
    std::fs::write(dir.path().join("dot.gif"), gif).unwrap();
    let out = call(&host, "read_file", json!({"path": "dot.gif"})).await;
    assert!(!out.is_error);
    let img = image(&out);
    assert_eq!(img.media_type, "image/gif");
    assert_eq!(
        img.data,
        review_llm::Image::from_bytes("image/gif", gif).data
    );

    // A GIF or WebP name without the magic bytes is not an image: text, or binary.
    std::fs::write(dir.path().join("x.gif"), b"just text").unwrap();
    std::fs::write(dir.path().join("y.webp"), b"bin\0ary").unwrap();
    std::fs::write(dir.path().join("anim.gif"), b"").unwrap();
    for (path, want) in [
        ("x.gif", "     1\tjust text"),
        ("y.webp", "<binary>7 bytes</binary>"),
        ("anim.gif", "(empty file)"),
    ] {
        let out = call(&host, "read_file", json!({"path": path})).await;
        assert!(!out.is_error, "{path}");
        assert_eq!(out.parts.len(), 1, "{path}");
        assert_eq!(text(&out), want, "{path}");
    }
}

/// CRC-32 (IEEE), for a hand-made PNG chunk.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A tiny PNG whose header declares 50 000 × 50 000 pixels is refused by the decode
/// limits at once, before anything that size is allocated.
#[tokio::test]
async fn read_file_refuses_images_declaring_huge_dimensions() {
    let (dir, host) = host();
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&50_000u32.to_be_bytes());
    ihdr.extend_from_slice(&50_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(&ihdr);
    png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    std::fs::write(dir.path().join("bomb.png"), &png).unwrap();

    let started = std::time::Instant::now();
    let out = call(&host, "read_file", json!({"path": "bomb.png"})).await;
    let took = started.elapsed();
    assert!(out.is_error, "{:?}", out.parts);
    assert!(
        text(&out).starts_with("cannot decode image bomb.png"),
        "{}",
        text(&out)
    );
    assert!(text(&out).contains("limit"), "{}", text(&out));
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");
}

/// A sandbox whose `exec` returns a canned result; other ops are unused.
struct CannedExec(ExecOutput);

#[async_trait::async_trait]
impl SandboxOps for CannedExec {
    async fn exec(&self, _: &str, _: bool, timeout: Duration) -> Result<ExecOutput, OpError> {
        assert_eq!(
            timeout,
            Duration::ZERO,
            "the tool asks for the default timeout"
        );
        Ok(self.0.clone())
    }
    async fn read(&self, _: &str) -> Result<Vec<u8>, OpError> {
        unimplemented!()
    }
    async fn write(&self, _: &str, _: &[u8]) -> Result<bool, OpError> {
        unimplemented!()
    }
    async fn list(&self, _: &str) -> Result<Vec<Entry>, OpError> {
        unimplemented!()
    }
    async fn glob(&self, _: &str, _: &str) -> Result<Listing<String>, OpError> {
        unimplemented!()
    }
    async fn grep(&self, _: &str, _: &str, _: Option<&str>) -> Result<Listing<GrepMatch>, OpError> {
        unimplemented!()
    }
    async fn exists(&self, _: &str) -> Result<(bool, bool), OpError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn bash_tool_formats_timeout() {
    let ops = CannedExec(ExecOutput {
        output: "partial".into(),
        exit_code: -1,
        timed_out: true,
        truncated: false,
    });
    let out = tool("bash")
        .call(
            json!({"command": "sleep 999"}),
            &ctx_allowing(Arc::new(ops), PathBuf::from("."), &["sleep"]),
        )
        .await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "partial\n(exit code -1)\n(timed out)");

    let ops = CannedExec(ExecOutput::default());
    let out = tool("bash")
        .call(
            json!({"command": "true"}),
            &ctx(Arc::new(ops), PathBuf::from(".")),
        )
        .await;
    assert_eq!(text(&out), "(no output)");
}

#[tokio::test]
async fn glob_and_grep_tools_format() {
    let (dir, host) = host();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
    std::fs::write(dir.path().join("src/b.txt"), "fn not_rust\n").unwrap();

    let out = call(&host, "glob", json!({"pattern": "**/*.rs"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(text(&out), "src/a.rs");

    let out = call(&host, "glob", json!({"pattern": "*", "directory": "src"})).await;
    assert!(!out.is_error);
    let mut lines: Vec<&str> = text(&out).lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, ["src/a.rs", "src/b.txt"]);

    let out = call(&host, "glob", json!({"pattern": "*.go"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "no files match \"*.go\" under .");

    let out = call(
        &host,
        "glob",
        json!({"pattern": "*.go", "directory": "src"}),
    )
    .await;
    assert_eq!(text(&out), "no files match \"*.go\" under src");

    let out = call(&host, "grep", json!({"pattern": "fn \\w+"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let mut lines: Vec<&str> = text(&out).lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        ["src/a.rs:1:fn main() {}", "src/b.txt:1:fn not_rust"]
    );

    let out = call(&host, "grep", json!({"pattern": "fn", "glob": "**/*.rs"})).await;
    assert_eq!(text(&out), "src/a.rs:1:fn main() {}");

    let out = call(&host, "grep", json!({"pattern": "zzz", "directory": "src"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "no matches for \"zzz\" under src");

    // Truncated listings say so.
    for i in 0..(MAX_GLOB + 5) {
        std::fs::write(dir.path().join(format!("f{i}.log")), "hit\n").unwrap();
    }
    let out = call(&host, "glob", json!({"pattern": "*.log"})).await;
    let t = text(&out);
    assert!(
        t.ends_with(&format!("\n… only the first {MAX_GLOB} matches are shown")),
        "{}",
        &t[t.len() - 80..]
    );
    let out = call(&host, "grep", json!({"pattern": "hit", "glob": "*.log"})).await;
    let t = text(&out);
    assert!(
        t.ends_with(&format!("\n… only the first {MAX_GREP} matches are shown")),
        "{}",
        &t[t.len() - 80..]
    );

    let out = call(&host, "grep", json!({"pattern": "("})).await;
    assert!(out.is_error);
    let out = call(&host, "glob", json!({"pattern": "*", "directory": "nope"})).await;
    assert!(out.is_error);
    let out = call(&host, "glob", json!({"pattern": ""})).await;
    assert!(out.is_error);
}

/// A noise-like 2000×2000 PNG: large enough that its bytes dwarf a 10×10 one.
fn noisy_png(w: u32, h: u32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        let v = ((x / 3) ^ (y / 5)) as u8;
        image::Rgb([v, v.wrapping_add(85), v.wrapping_add(170)])
    })
}

#[tokio::test]
async fn asset_stats_compares_with_siblings() {
    let (dir, host) = host();
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    for name in ["a.png", "b.png", "c.png"] {
        image::RgbImage::from_pixel(10, 10, image::Rgb([10, 20, 30]))
            .save(dir.path().join("assets").join(name))
            .unwrap();
    }
    noisy_png(2000, 2000)
        .save(dir.path().join("assets/big.png"))
        .unwrap();

    let out = call(&host, "asset_stats", json!({"path": "assets/big.png"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let v: Value = serde_json::from_str(text(&out)).unwrap();
    assert_eq!(v["path"], "assets/big.png");
    assert_eq!(v["format"], "png");
    assert_eq!(v["width"], 2000);
    assert_eq!(v["height"], 2000);
    assert_eq!(v["siblings"]["count"], 3);
    assert_eq!(v["siblings"]["median_width"], 10);
    assert_eq!(v["siblings"]["median_height"], 10);
    assert_eq!(
        v["siblings"]["max_bytes"].as_u64().unwrap(),
        std::fs::metadata(dir.path().join("assets/a.png"))
            .unwrap()
            .len()
    );
    assert!(v["ratio_to_median"].as_f64().unwrap() > 10.0, "{v}");
    assert_eq!(
        v["bytes"].as_u64().unwrap(),
        std::fs::metadata(dir.path().join("assets/big.png"))
            .unwrap()
            .len()
    );

    // A non-image has no dimensions; its format comes from the extension. A zero
    // median (empty files) gives a null ratio rather than a division by zero.
    std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("more.txt"), "").unwrap();
    let out = call(&host, "asset_stats", json!({"path": "notes.txt"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let v: Value = serde_json::from_str(text(&out)).unwrap();
    assert_eq!(v["format"], "txt");
    assert_eq!(v["width"], Value::Null);
    assert_eq!(v["height"], Value::Null);
    assert_eq!(v["siblings"]["count"], 1);
    assert_eq!(v["siblings"]["median_bytes"], 0);
    assert_eq!(v["siblings"]["median_width"], Value::Null);
    assert_eq!(v["ratio_to_median"], Value::Null);

    let out = call(&host, "asset_stats", json!({"path": "missing.png"})).await;
    assert!(out.is_error);
    let out = call(&host, "asset_stats", json!({"path": "assets"})).await;
    assert!(out.is_error);
    let out = call(&host, "asset_stats", json!({"path": "../outside.png"})).await;
    assert!(out.is_error);
}

#[tokio::test]
async fn list_dir_reports_sizes_and_kinds() {
    let (dir, host) = host();
    std::fs::create_dir_all(dir.path().join("assets/sub")).unwrap();
    std::fs::write(dir.path().join("assets/a.rs"), "fn main").unwrap();
    std::fs::write(dir.path().join("assets/z.txt"), "abc").unwrap();

    let out = call(&host, "list_dir", json!({"path": "assets"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let lines: Vec<&str> = text(&out).lines().collect();
    assert_eq!(lines.len(), 3, "{}", text(&out));
    assert_eq!(lines[0], "a.rs\t7\tfile");
    assert!(lines[1].starts_with("sub\t"), "{}", lines[1]);
    assert!(lines[1].ends_with("\tdir"), "{}", lines[1]);
    assert_eq!(lines[2], "z.txt\t3\tfile");

    // The checkout root itself is the default for an empty path.
    let out = call(&host, "list_dir", json!({"path": ""})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("assets\t"), "{}", text(&out));

    std::fs::create_dir_all(dir.path().join("empty")).unwrap();
    let out = call(&host, "list_dir", json!({"path": "empty"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "(empty directory)");

    // Past 500 entries the rest are counted, not listed.
    std::fs::create_dir_all(dir.path().join("bulk")).unwrap();
    for i in 0..505 {
        std::fs::write(dir.path().join(format!("bulk/f{i:03}.txt")), "").unwrap();
    }
    let out = call(&host, "list_dir", json!({"path": "bulk"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let t = text(&out);
    assert_eq!(t.lines().count(), 501, "{}", &t[t.len() - 40..]);
    assert!(t.ends_with("\n[5 more]"), "{}", &t[t.len() - 40..]);

    let out = call(&host, "list_dir", json!({"path": "missing"})).await;
    assert!(out.is_error);
    let out = call(&host, "list_dir", json!({"path": "../outside"})).await;
    assert!(out.is_error);
}

#[test]
fn validate_rev_rejects_option_injection() {
    for r in ["--output=/tmp/x", "-p", "a b", "HEAD;rm", ""] {
        assert!(validate_rev(r).is_err(), "{r}");
    }
    for r in ["HEAD~2", "abc123", "origin/main", "HEAD@{1}", "v1.2.3^{}"] {
        assert!(validate_rev(r).is_ok(), "{r}");
    }
}

/// Runs git in `dir` for test setup, failing the test on any error.
fn git_setup(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn git_log_and_show_work_on_a_temp_repo() {
    let (dir, host) = host();
    git_setup(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git_setup(dir.path(), &["add", "."]);
    git_setup(dir.path(), &["commit", "-q", "-m", "first commit"]);
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "bee\n").unwrap();
    git_setup(dir.path(), &["add", "."]);
    git_setup(dir.path(), &["commit", "-q", "-m", "second commit"]);

    let out = call(&host, "git_log", json!({})).await;
    assert!(!out.is_error, "{}", text(&out));
    let lines: Vec<&str> = text(&out).lines().collect();
    assert_eq!(lines.len(), 2, "{}", text(&out));
    assert!(lines[0].contains("second commit"), "{}", text(&out));
    assert!(lines[1].contains("first commit"), "{}", text(&out));

    // n caps the list; a path limits it to commits that touched the file.
    let out = call(&host, "git_log", json!({"n": 1})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out).lines().count(), 1, "{}", text(&out));
    let out = call(&host, "git_log", json!({"path": "b.txt"})).await;
    assert!(!out.is_error, "{}", text(&out));
    let log = text(&out);
    assert!(log.contains("second commit"), "{log}");
    assert!(!log.contains("first commit"), "{log}");

    // git_show prints a commit with its patch.
    let out = call(&host, "git_show", json!({"rev": "HEAD"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("second commit"), "{}", text(&out));
    assert!(text(&out).contains("+bee"), "{}", text(&out));
    // With a path, only that path's part of the commit.
    let out = call(&host, "git_show", json!({"rev": "HEAD", "path": "b.txt"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("+bee"), "{}", text(&out));
    assert!(!text(&out).contains("+two"), "{}", text(&out));

    let out = call(&host, "git_diff", json!({"range": "HEAD~1..HEAD"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("+two"), "{}", text(&out));
    assert!(text(&out).contains("+bee"), "{}", text(&out));
    let out = call(
        &host,
        "git_diff",
        json!({"range": "HEAD~1..HEAD", "path": "a.txt"}),
    )
    .await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("+two"), "{}", text(&out));
    assert!(!text(&out).contains("+bee"), "{}", text(&out));

    // The checkout root as a path means the whole tree: no empty pathspec, no error.
    for (name, input, want) in [
        (
            "git_diff",
            json!({"range": "HEAD~1..HEAD", "path": "."}),
            "+bee",
        ),
        ("git_log", json!({"path": "."}), "second commit"),
        (
            "git_show",
            json!({"rev": "HEAD", "path": "."}),
            "second commit",
        ),
    ] {
        let out = call(&host, name, input).await;
        assert!(!out.is_error, "{name}: {}", text(&out));
        assert!(text(&out).contains(want), "{name}: {}", text(&out));
    }

    // Option injection is refused before git runs.
    let out = call(&host, "git_show", json!({"rev": "--output=/tmp/x"})).await;
    assert!(out.is_error);
    assert!(text(&out).contains("rev"), "{}", text(&out));
    // A rev git does not know is a tool error, not a panic or a success.
    let out = call(&host, "git_show", json!({"rev": "no-such-rev"})).await;
    assert!(out.is_error, "{}", text(&out));
    // A mixed range (`a..b...c`) is refused.
    let out = call(&host, "git_diff", json!({"range": "HEAD~1..HEAD..HEAD"})).await;
    assert!(out.is_error);
}

#[tokio::test]
async fn git_diff_path_outside_repo_is_rejected() {
    let (dir, host) = host();
    git_setup(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git_setup(dir.path(), &["add", "."]);
    git_setup(dir.path(), &["commit", "-q", "-m", "first commit"]);

    let out = call(&host, "git_diff", json!({"range": "HEAD", "path": "../x"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).contains("escapes the project root"),
        "{}",
        text(&out)
    );
    // A path cannot smuggle an option either: not raw, and not through `./` or `..`.
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    for path in ["-p", "./-p", "sub/../-p"] {
        let out = call(&host, "git_diff", json!({"range": "HEAD", "path": path})).await;
        assert!(out.is_error, "{path}: {}", text(&out));
    }
    let out = call(&host, "git_log", json!({"path": "../x"})).await;
    assert!(out.is_error);
}

#[test]
fn truncate_middle_keeps_head_and_tail() {
    assert_eq!(truncate_middle("short", 10), "short");
    assert_eq!(truncate_middle("exactly10!", 10), "exactly10!");

    let s = format!("{}{}", "a".repeat(100), "b".repeat(100));
    let t = truncate_middle(&s, 30);
    assert_eq!(
        t,
        format!(
            "{}\n... [truncated 170 bytes] ...\n{}",
            "a".repeat(20),
            "b".repeat(10)
        )
    );

    // Multi-byte characters at both cuts: never split, and the count matches the cut.
    let s = "é".repeat(100); // 200 bytes
    let t = truncate_middle(&s, 31); // head 20 bytes (10 é), tail 11 -> 10 bytes (5 é)
    let (head, rest) = t.split_once("\n... [truncated ").unwrap();
    let (n, tail) = rest.split_once(" bytes] ...\n").unwrap();
    assert!(head.chars().all(|c| c == 'é'));
    assert!(tail.chars().all(|c| c == 'é'));
    assert!(head.len() <= 20 && tail.len() <= 11);
    assert_eq!(
        head.len() + tail.len() + n.parse::<usize>().unwrap(),
        s.len()
    );

    let s = "日本語".repeat(50);
    let t = truncate_middle(&s, 100);
    assert!(t.contains("[truncated "));
    let (head, _) = t.split_once("\n... ").unwrap();
    assert!(head.len() <= 66 && head.len() >= 64);
}

#[test]
fn tool_definitions_have_valid_schemas() {
    let tools = all();
    let names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
    assert_eq!(
        names,
        [
            "bash",
            "read_file",
            "glob",
            "grep",
            "list_dir",
            "asset_stats",
            "git_show",
            "git_log",
            "git_diff",
            "gh_pr",
            "gh_pr_files",
            "gh_pr_patch",
            "gh_threads",
            "gh_checks",
            "load_skill",
            "ask_lookup",
            "ask_user",
            "request_prepare",
            "record_finding",
            "record_check",
            "done"
        ]
    );
    for t in &tools {
        let def = t.definition();
        assert!(!def.description.is_empty());
        let again = review_llm::Tool::new(&def.name, &def.description, def.input_schema.clone())
            .unwrap_or_else(|e| panic!("{}: {e}", def.name));
        assert_eq!(again, def);
    }
    let req = |name: &str| -> Vec<String> {
        let def = tool(name).definition();
        serde_json::from_value(def.input_schema["required"].clone()).unwrap_or_default()
    };
    assert!(req("bash").is_empty());
    assert_eq!(req("read_file"), ["path"]);
    assert_eq!(req("glob"), ["pattern"]);
    assert_eq!(req("grep"), ["pattern"]);
    assert_eq!(req("list_dir"), ["path"]);
    assert_eq!(req("asset_stats"), ["path"]);
    assert_eq!(req("git_show"), ["rev"]);
    assert!(req("git_log").is_empty());
    assert_eq!(req("git_diff"), ["range"]);
    assert!(req("gh_pr").is_empty());
    assert!(req("gh_pr_files").is_empty());
    assert_eq!(req("gh_pr_patch"), ["path"]);
    assert!(req("gh_threads").is_empty());
    assert!(req("gh_checks").is_empty());
    assert_eq!(req("load_skill"), ["name"]);
    assert_eq!(req("ask_lookup"), ["question"]);
    assert_eq!(req("ask_user"), ["question"]);
    assert_eq!(req("request_prepare"), ["reason"]);
    assert_eq!(
        req("record_finding"),
        ["path", "line", "severity", "title", "body", "evidence"]
    );
    assert_eq!(req("record_check"), ["file", "item", "result", "note"]);
    assert!(req("done").is_empty());
    let props = |name: &str| -> Vec<String> {
        let mut v: Vec<String> = tool(name).definition().input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    };
    assert_eq!(props("bash"), ["command", "restart"]);
    assert_eq!(props("read_file"), ["limit", "offset", "path"]);
    assert_eq!(props("glob"), ["directory", "pattern"]);
    assert_eq!(props("grep"), ["directory", "glob", "pattern"]);
    assert_eq!(props("list_dir"), ["path"]);
    assert_eq!(props("asset_stats"), ["path"]);
    assert_eq!(props("git_show"), ["path", "rev"]);
    assert_eq!(props("git_log"), ["n", "path"]);
    assert_eq!(props("git_diff"), ["path", "range"]);
    assert!(props("gh_pr").is_empty());
    assert!(props("gh_pr_files").is_empty());
    assert_eq!(props("gh_pr_patch"), ["path"]);
    assert_eq!(props("gh_threads"), ["path"]);
    assert!(props("gh_checks").is_empty());
    assert_eq!(props("load_skill"), ["name"]);
    assert_eq!(props("ask_lookup"), ["question"]);
    assert_eq!(props("ask_user"), ["options", "question"]);
    assert_eq!(props("request_prepare"), ["reason"]);
    assert_eq!(
        props("record_finding"),
        [
            "body",
            "checklist_item",
            "evidence",
            "line",
            "path",
            "severity",
            "side",
            "start_line",
            "title"
        ]
    );
    assert_eq!(props("record_check"), ["file", "item", "note", "result"]);
    assert_eq!(props("done"), ["summary"]);
}

/// The registry hands out every definition in order, and looks a tool up by name.
#[test]
fn registry_definitions_and_lookup() {
    let tools = all();
    let count = tools.len();
    let registry = Registry::new(tools);
    assert_eq!(registry.definitions().len(), count);
    assert!(registry.get("bash").is_some());
    assert!(registry.get("read_file").is_some());
    assert!(registry.get("glob").is_some());
    assert!(registry.get("grep").is_some());
    assert!(registry.get("list_dir").is_some());
    assert!(registry.get("asset_stats").is_some());
    assert!(registry.get("git_show").is_some());
    assert!(registry.get("git_log").is_some());
    assert!(registry.get("git_diff").is_some());
    assert!(registry.get("gh_pr").is_some());
    assert!(registry.get("gh_pr_files").is_some());
    assert!(registry.get("gh_pr_patch").is_some());
    assert!(registry.get("gh_threads").is_some());
    assert!(registry.get("gh_checks").is_some());
    assert!(registry.get("load_skill").is_some());
    assert!(registry.get("record_finding").is_some());
    assert!(registry.get("record_check").is_some());
    assert!(registry.get("done").is_some());
    assert!(registry.get("edit_file").is_none());
    assert!(registry.get("").is_none());
}

/// A tool that writes to GitHub or to the repo would undo the review's safety
/// story (spec §6.3): the registry must never grow one.
#[test]
fn registry_has_no_write_or_merge_tools() {
    let names: Vec<String> = all().iter().map(|t| t.definition().name).collect();
    for name in &names {
        assert!(
            !is_forbidden_tool_name(name),
            "{name} is a write or merge tool; the review agent is read-only"
        );
    }
    // The read-only tools every agent needs are there.
    for required in [
        "bash",
        "read_file",
        "glob",
        "grep",
        "list_dir",
        "asset_stats",
        "git_show",
        "git_log",
        "git_diff",
    ] {
        assert!(
            names.iter().any(|n| n == required),
            "missing {required}: {names:?}"
        );
    }

    // The rule itself: any name carrying a write or merge fragment is forbidden,
    // case-insensitively, while the local draft tool Task 14 adds is allowed.
    for bad in [
        "merge",
        "merge_pr",
        "auto_merge",
        "approve",
        "approve_pr",
        "post",
        "post_review",
        "post_comment",
        "Post_Comment",
        "submit_review",
        "Submit_Review",
        "resolve",
        "resolve_thread",
        "edit_file",
        "write_file",
    ] {
        assert!(is_forbidden_tool_name(bad), "{bad} should be forbidden");
    }
    for ok in [
        "submit_review_draft",
        "read_file",
        "git_show",
        "record_finding",
    ] {
        assert!(!is_forbidden_tool_name(ok), "{ok} should be allowed");
    }
}

/// A temp repo (`git init`, one commit) so `git status` has something to say.
fn git_host() -> (tempfile::TempDir, LocalHost) {
    let (dir, host) = host();
    for args in [
        &["init", "-q"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    (dir, host)
}

#[tokio::test]
async fn bash_runs_cd_and_read_only_without_asking() {
    let (_dir, host) = git_host();
    // `NoInteraction` denies everything the engine would ask about.
    let cx = host_ctx(&host);

    let out = bash(&cx, json!({"command": "cd . && ls"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    let out = bash(&cx, json!({"command": "git status"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(
        text(&out).contains("branch") || text(&out).contains("On "),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn bash_denies_secret_paths_without_prompting() {
    let (dir, host) = host();
    std::fs::write(dir.path().join(".env"), "TOKEN=1\n").unwrap();
    let interaction = Arc::new(ScriptedInteraction::new());
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "cat .env"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).starts_with("the command was blocked: "),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains("TOKEN=1"));
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn bash_deny_beats_repo_allowed_entry() {
    let (dir, host) = host();
    std::fs::write(dir.path().join(".env"), "TOKEN=1\n").unwrap();
    let interaction = Arc::new(ScriptedInteraction::new());
    let cx = interaction_ctx(&host, interaction.clone());
    // `touch` is not read-only, so only the allowed entry could vouch for it:
    // the deny must still win over that entry.
    cx.allow_segments(["touch".to_string()]);

    let out = bash(&cx, json!({"command": "touch .env"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).starts_with("the command was blocked: "),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains("TOKEN=1"));
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn bash_still_asks_for_unknown_commands() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Once));
    let cx = interaction_ctx(&host, interaction.clone());

    let out = bash(&cx, json!({"command": "touch asked"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(dir.path().join("asked").exists());
    assert_eq!(interaction.calls().len(), 1, "{:?}", interaction.calls());
}

/// Full trust (`mode = "trust"`) runs what the rules deny and what
/// they would ask, with no prompt and no classifier call.
#[tokio::test]
async fn full_trust_runs_denied_and_unvouched_commands_silently() {
    let (dir, host) = host();
    std::fs::write(dir.path().join(".env"), "TOKEN=1\n").unwrap();
    let interaction = Arc::new(ScriptedInteraction::new());
    let settings = AutoApproveSettings {
        mode: ApprovalMode::Trust,
        ..AutoApproveSettings::default()
    };
    let classifier = FixedClassifier::new(ModelOutcome::Dangerous);
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &settings,
        Some(classifier.clone()),
    );

    // A denied secret path: normally blocked without asking.
    let out = bash(&cx, json!({"command": "cat .env"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(text(&out).contains("TOKEN=1"));

    // An unvouched command: normally an ask.
    let out = bash(&cx, json!({"command": "touch trusted"})).await;
    assert!(!out.is_error, "{t}", t = text(&out));
    assert!(dir.path().join("trusted").exists());

    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
    assert_eq!(
        classifier.calls.load(Ordering::SeqCst),
        0,
        "full trust never asks the model"
    );
}

// ---- the optional classifier stage ----

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use review_approval::model_stage::{ClassifyInput, CommandClassifier, ModelOutcome};

use crate::ai::approval_setup::ApprovalSetup;
use crate::ai::events::{CommandDecision, PipelineEvent};
use crate::persist::settings::{ApprovalMode, AutoApproveSettings};

/// Answers every segment with one outcome and counts the calls.
struct FixedClassifier {
    outcome: ModelOutcome,
    calls: AtomicUsize,
}

impl FixedClassifier {
    fn new(outcome: ModelOutcome) -> Arc<FixedClassifier> {
        Arc::new(FixedClassifier {
            outcome,
            calls: AtomicUsize::new(0),
        })
    }
}

impl CommandClassifier for FixedClassifier {
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

/// Sleeps until `release` is set (or 3 s pass), like a hung model. The test
/// sets `release` at the end so the runtime's shutdown does not wait on it.
struct SlowClassifier {
    release: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl CommandClassifier for SlowClassifier {
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for _ in 0..300 {
            if self.release.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ModelOutcome::Safe
    }
}

struct PanicClassifier;

impl CommandClassifier for PanicClassifier {
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
        panic!("classifier blew up");
    }
}

/// `interaction_ctx` with an approval setup built from `settings`.
fn classifier_ctx(
    host: &LocalHost,
    interaction: Arc<ScriptedInteraction>,
    settings: &AutoApproveSettings,
    classifier: Option<Arc<dyn CommandClassifier>>,
) -> ToolCx {
    let mut cx = interaction_ctx(host, interaction);
    cx.approval = Arc::new(ApprovalSetup::from_settings(settings, classifier).into());
    cx
}

fn model_on() -> AutoApproveSettings {
    AutoApproveSettings {
        local_classifier: true,
        ..ask_mode()
    }
}

/// A command only the model may vouch for: `/usr/bin/pr` (a POSIX text
/// formatter, present at the same path on macOS and Linux CI) is an absolute
/// system binary, no rule policy vouches for it and it is not in the
/// rules do not know it. `-t` drops the dated header so the output is stable.
/// (A relative `./tool.sh` can never be model-approved: it is PR code, I1.)
const MODEL_ONLY_CMD: &str = "/usr/bin/pr -t tool.sh";

/// Writes `tool.sh` (executable, prints `done`): the operand of
/// [`MODEL_ONLY_CMD`] and the file the relative-path asks test runs.
fn write_tool(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("tool.sh");
    std::fs::write(&path, "#!/bin/sh\necho done\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn bash_model_safe_runs_unknown_command() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new());
    let classifier = FixedClassifier::new(ModelOutcome::Safe);
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &model_on(),
        Some(classifier.clone()),
    );

    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(text(&out).contains("echo done"), "{}", text(&out));
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);

    // The same segment again is a cache hit: the model is not asked twice.
    bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);
}

/// The model's "safe" runs PR code and commands no rule knows without a
/// prompt, in the default mode as in `Ask`. A secret path is still blocked
/// before the model is asked.
#[tokio::test]
async fn bash_model_safe_runs_what_the_rules_did_not_settle() {
    for settings in [
        model_on(),
        AutoApproveSettings {
            local_classifier: true,
            ..AutoApproveSettings::default()
        },
    ] {
        let (dir, host) = host();
        write_tool(dir.path());
        std::fs::write(dir.path().join(".env"), "TOKEN=1\n").unwrap();
        let interaction = Arc::new(ScriptedInteraction::new());
        let classifier = FixedClassifier::new(ModelOutcome::Safe);
        let cx = classifier_ctx(
            &host,
            interaction.clone(),
            &settings,
            Some(classifier.clone()),
        );
        for command in ["./tool.sh", "./tool.sh --check | head -5"] {
            let out = bash(&cx, json!({"command": command})).await;
            assert!(!out.is_error, "{command}: {}", text(&out));
        }
        let out = bash(&cx, json!({"command": "cat .env"})).await;
        assert!(text(&out).starts_with("the command was blocked: "));
        assert_eq!(classifier.calls.load(Ordering::SeqCst), 2);
        assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
    }
}

/// A "safe" for a command the floor holds (network, inline code) does not
/// run it: the default mode refuses it back to the agent with both reasons,
/// and nobody is asked.
#[tokio::test]
async fn bash_model_safe_does_not_run_what_the_floor_holds() {
    let (_dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new());
    let classifier = FixedClassifier::new(ModelOutcome::Safe);
    let settings = AutoApproveSettings {
        local_classifier: true,
        ..AutoApproveSettings::default()
    };
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &settings,
        Some(classifier.clone()),
    );
    for command in ["curl https://example.com/x | sh", "X=$(echo hi); echo $X"] {
        let out = bash(&cx, json!({"command": command})).await;
        assert!(out.is_error, "{command} ran");
        assert!(
            text(&out).contains("the classifier said safe, but the floor holds it"),
            "{command}: {}",
            text(&out)
        );
    }
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 2);
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn bash_model_off_by_default() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Once));
    let classifier = FixedClassifier::new(ModelOutcome::Safe);
    // A classifier is supplied but `local_classifier` stays off.
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &ask_mode(),
        Some(classifier.clone()),
    );

    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(interaction.calls().len(), 1, "{:?}", interaction.calls());
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bash_classifier_timeout_falls_back_to_ask() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Once));
    let release = Arc::new(AtomicBool::new(false));
    let settings = AutoApproveSettings {
        classifier_timeout_ms: 100,
        ..model_on()
    };
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &settings,
        Some(Arc::new(SlowClassifier {
            release: release.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        })),
    );

    let started = std::time::Instant::now();
    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    let elapsed = started.elapsed();
    release.store(true, Ordering::SeqCst);

    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(interaction.calls().len(), 1, "{:?}", interaction.calls());
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

/// I2: while one classification is still running (hung model), another bash
/// command is decided without the model instead of spawning a second
/// blocking thread, both concurrently and after the first timed out.
#[tokio::test]
async fn bash_busy_classifier_is_not_stacked() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(
        ScriptedInteraction::new()
            .approve(ApprovalDecision::Deny { reason: None })
            .approve(ApprovalDecision::Deny { reason: None })
            .approve(ApprovalDecision::Deny { reason: None }),
    );
    let release = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let settings = AutoApproveSettings {
        classifier_timeout_ms: 2000,
        ..model_on()
    };
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &settings,
        Some(Arc::new(SlowClassifier {
            release: release.clone(),
            calls: calls.clone(),
        })),
    );

    let (a, b) = tokio::join!(
        bash(&cx, json!({"command": "/usr/bin/pr -t tool.sh"})),
        bash(&cx, json!({"command": "/usr/bin/pr -t -2 tool.sh"})),
    );
    assert!(a.is_error && b.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The hung call is still running: a third command does not wait for the
    // timeout and does not reach the classifier either.
    let started = std::time::Instant::now();
    let c = bash(&cx, json!({"command": "/usr/bin/pr -t -3 tool.sh"})).await;
    let elapsed = started.elapsed();
    release.store(true, Ordering::SeqCst);
    assert!(c.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?}");
    assert_eq!(interaction.calls().len(), 3);
}

/// A cached Safe outcome still approves while another model call is in
/// flight, and the classifier is not called again.
#[tokio::test]
async fn bash_busy_classifier_still_uses_cached_outcome() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new());
    let classifier = FixedClassifier::new(ModelOutcome::Safe);
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &model_on(),
        Some(classifier.clone()),
    );
    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);

    // Another model call is now in flight.
    let _busy = cx.approval.current().try_begin_model().expect("slot free");
    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn bash_classifier_panic_falls_back_to_ask() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Once));
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &model_on(),
        Some(Arc::new(PanicClassifier)),
    );

    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(interaction.calls().len(), 1, "{:?}", interaction.calls());
}

/// The default mode never asks: what nothing vouched for is refused with
/// the reason and a hint, what the rules vouch for runs, a secret path is
/// blocked, and every outcome is announced.
#[tokio::test]
async fn auto_mode_denies_unvouched_commands_without_asking() {
    let (dir, host) = host();
    std::fs::write(dir.path().join(".env"), "TOKEN=1\n").unwrap();
    let interaction = Arc::new(ScriptedInteraction::new());
    let (run, mut events) = crate::test_support::ai_run_state_with_events();
    let mut cx = classifier_ctx(
        &host,
        interaction.clone(),
        &AutoApproveSettings::default(),
        None,
    );
    cx.run = run;

    let out = bash(&cx, json!({"command": "ls"})).await;
    assert!(!out.is_error, "{}", text(&out));

    let out = bash(&cx, json!({"command": "touch made"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).starts_with("the command was not run: touch made: "),
        "{}",
        text(&out)
    );
    assert!(text(&out).contains("read_file"), "{}", text(&out));
    assert!(!dir.path().join("made").exists());

    let out = bash(&cx, json!({"command": "cat .env"})).await;
    assert!(text(&out).starts_with("the command was blocked: "));

    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
    let decisions: Vec<CommandDecision> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            PipelineEvent::CommandDecided { decision, .. } => Some(decision),
            _ => None,
        })
        .collect();
    match decisions.as_slice() {
        [
            CommandDecision::Rules { .. },
            CommandDecision::AutoDenied { .. },
            CommandDecision::Blocked { .. },
        ] => {}
        other => panic!("{other:?}"),
    }
}

/// A settings save reaches a running review: the next command follows it.
#[tokio::test]
async fn saved_settings_apply_to_the_next_command() {
    let (dir, host) = host();
    let interaction = Arc::new(ScriptedInteraction::new());
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &AutoApproveSettings::default(),
        None,
    );
    let out = bash(&cx, json!({"command": "touch made"})).await;
    assert!(out.is_error);

    cx.approval.apply(&AutoApproveSettings {
        mode: ApprovalMode::Trust,
        ..AutoApproveSettings::default()
    });
    let out = bash(&cx, json!({"command": "touch made"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(dir.path().join("made").exists());
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}

#[tokio::test]
async fn bash_dangerous_flag_reaches_dialog_reason() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction =
        Arc::new(ScriptedInteraction::new().approve(ApprovalDecision::Deny { reason: None }));
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &model_on(),
        Some(FixedClassifier::new(ModelOutcome::Dangerous)),
    );

    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(out.is_error);
    match interaction.calls().as_slice() {
        [InteractionCall::Approve { reason, .. }] => {
            assert!(reason.contains("flagged"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

/// A cached Dangerous outcome still asks, with the "flagged" reason, while
/// another model call is in flight (the cache is consulted, no new call).
#[tokio::test]
async fn bash_busy_classifier_cached_dangerous_still_asks_flagged() {
    let (dir, host) = host();
    write_tool(dir.path());
    let interaction = Arc::new(ScriptedInteraction::new());
    let classifier = FixedClassifier::new(ModelOutcome::Dangerous);
    let cx = classifier_ctx(
        &host,
        interaction.clone(),
        &model_on(),
        Some(classifier.clone()),
    );
    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(out.is_error);
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);

    let _busy = cx.approval.current().try_begin_model().expect("slot free");
    let out = bash(&cx, json!({"command": MODEL_ONLY_CMD})).await;
    assert!(out.is_error);
    assert_eq!(classifier.calls.load(Ordering::SeqCst), 1);
    match interaction.calls().as_slice() {
        [
            InteractionCall::Approve { .. },
            InteractionCall::Approve { reason, .. },
        ] => assert!(reason.contains("flagged"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn bash_capability_vouches_before_the_model() {
    let (dir, host) = host();
    // `git commit` is not read-only; only the opt-in capability vouches.
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    let interaction = Arc::new(ScriptedInteraction::new());
    let settings = AutoApproveSettings {
        capabilities: vec!["git:add".into()],
        ..ask_mode()
    };
    let cx = classifier_ctx(&host, interaction.clone(), &settings, None);
    std::fs::write(dir.path().join("f"), "x").unwrap();

    let out = bash(&cx, json!({"command": "git add f"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(interaction.calls().is_empty(), "{:?}", interaction.calls());
}
