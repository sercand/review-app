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
    is_forbidden_tool_name, truncate_middle,
};

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

/// A context over `sandbox`, rooted at `repo_root`, for one reviewer.
fn ctx(sandbox: Arc<dyn SandboxOps>, repo_root: PathBuf) -> ToolCx {
    ToolCx {
        sandbox,
        repo_root,
        agent: AgentRef {
            id: 1,
            role: AgentRole::Reviewer,
            label: "reviewer".to_string(),
        },
    }
}

/// A context over `host`, with the sandbox's root as the repo root.
fn host_ctx(host: &LocalHost) -> ToolCx {
    ctx(Arc::new(host.clone()), host.root().path().to_path_buf())
}

async fn call(host: &LocalHost, name: &str, input: Value) -> ToolOutput {
    tool(name).call(input, &host_ctx(host)).await
}

fn host() -> (tempfile::TempDir, LocalHost) {
    let dir = crate::test_support::tempdir();
    let host = LocalHost::new(dir.path()).unwrap();
    (dir, host)
}

#[tokio::test]
async fn bash_tool_reports_exit_code() {
    let (_dir, host) = host();

    let out = call(&host, "bash", json!({"command": "echo hi"})).await;
    assert!(!out.is_error);
    assert_eq!(text(&out).trim_end(), "hi");

    // A non-zero exit is information, not a tool error.
    let out = call(&host, "bash", json!({"command": "echo oops; exit 3"})).await;
    assert!(!out.is_error);
    let t = text(&out);
    assert!(t.contains("oops"), "{t}");
    assert!(t.ends_with("\n(exit code 3)"), "{t}");

    let out = call(&host, "bash", json!({})).await;
    assert!(out.is_error);
    assert_eq!(text(&out), "command is required unless restart is true");

    let out = call(&host, "bash", json!({"command": "", "restart": true})).await;
    assert!(!out.is_error, "{:?}", out.parts);
    assert!(!text(&out).is_empty());

    // Shell state persists across calls until a restart.
    call(&host, "bash", json!({"command": "export QA_X=1"})).await;
    let out = call(&host, "bash", json!({"command": "echo \"x=$QA_X\""})).await;
    assert_eq!(text(&out).trim_end(), "x=1");
    let out = call(
        &host,
        "bash",
        json!({"command": "echo \"x=$QA_X\"", "restart": true}),
    )
    .await;
    assert_eq!(text(&out).trim_end(), "x=");

    // Bad input is a tool error, not a panic.
    let out = call(&host, "bash", json!({"command": 5})).await;
    assert!(out.is_error);
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
            &ctx(Arc::new(ops), PathBuf::from(".")),
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
    assert_eq!(names, ["bash", "read_file", "glob", "grep"]);
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
    // The four read-only tools every agent needs are there.
    for required in ["bash", "read_file", "glob", "grep"] {
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
