//! `read_file`: numbered text, images and a binary placeholder.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_TOOL_OUTPUT, Tool, ToolCx, ToolOutput, definition, parse, truncate_middle};

/// Only this many leading bytes are checked for a NUL when telling binary from text.
const BINARY_SNIFF: usize = 8000;

/// Lines `read_file` returns when the caller gives no `limit`.
const DEFAULT_READ_LIMIT: usize = 2000;

/// The `read_file` tool.
pub(super) struct ReadFile;

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    /// First line to return, 1-based. Defaults to 1.
    #[serde(default)]
    offset: Option<usize>,
    /// How many lines to return. Defaults to [`DEFAULT_READ_LIMIT`].
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "read_file",
            "Read a text file. Lines come back numbered, `cat -n` style. Use offset (1-based \
             line) and limit to read a large file in slices; the result says which lines of how \
             many it shows. Images are returned as images. Binary files (.xlsx, .pdf, .docx, \
             .zip, …) cannot be read as text, so don't try. Relative paths resolve against the \
             checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to read (e.g. 'src/main.rs'). Relative \
                            paths resolve against the checkout root."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "First line to return, 1-based. Defaults to the first \
                            line."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many lines to return. Defaults to 2000."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: ReadInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.path.trim().is_empty() {
            return ToolOutput::error("path is required");
        }
        let data = match cx.sandbox.read(&input.path).await {
            Ok(d) => d,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        // GIF and WebP pass through as is, so only their magic bytes make them images; a
        // PNG or JPEG name is enough, because decoding checks the bytes.
        let mime = sniff_image(&data).or_else(|| {
            image_mime_by_ext(&input.path).filter(|m| matches!(*m, "image/png" | "image/jpeg"))
        });
        if let Some(mime) = mime {
            return image_result(&input.path, mime, data).await;
        }
        if data[..data.len().min(BINARY_SNIFF)].contains(&0) {
            return ToolOutput::text(format!("<binary>{} bytes</binary>", data.len()));
        }
        let content = String::from_utf8_lossy(&data);
        match numbered(&content, input.offset, input.limit) {
            Ok(text) => ToolOutput::raw_text(text),
            Err(message) => ToolOutput::error(format!("{}: {message}", input.path)),
        }
    }
}

/// `content` as `cat -n` style lines from `offset` for at most `limit` lines, with a
/// trailing `[showing lines a-b of N]` whenever that is not the whole file. An empty
/// file is `(empty file)`, never an empty text part (providers reject those).
fn numbered(content: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = match offset {
        Some(0) => return Err("offset must be 1 or more (lines are 1-based)".to_string()),
        Some(offset) => {
            if offset > total {
                let plural = if total == 1 { "line" } else { "lines" };
                return Err(format!(
                    "offset {offset} is past the end of the file ({total} {plural})"
                ));
            }
            offset
        }
        None => 1,
    };
    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
    if limit == 0 {
        return Err("limit must be 1 or more".to_string());
    }
    if total == 0 {
        return Ok("(empty file)".to_string());
    }
    let end = total.min(start.saturating_add(limit - 1));
    let body = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{line}", start + i))
        .collect::<Vec<_>>()
        .join("\n");
    let body = truncate_middle(&body, MAX_TOOL_OUTPUT);
    if start > 1 || end < total {
        // The note stays whole after the cut, so the model always sees the range.
        Ok(format!("{body}\n[showing lines {start}-{end} of {total}]"))
    } else {
        Ok(body)
    }
}

/// Longest edge of an image handed to the model; larger ones are scaled down.
pub const IMAGE_MAX_SIDE: u32 = 1568;
/// The largest GIF or WebP passed through as is (they are not re-encoded).
pub const IMAGE_MAX_PASSTHROUGH: usize = 5 * 1000 * 1000;

/// The options `read_file` re-encodes PNG and JPEG images with: JPEG at quality 80,
/// scaled to [`IMAGE_MAX_SIDE`], which keeps a screenshot well under provider limits.
pub fn image_options() -> review_llm::ImageOptions {
    review_llm::ImageOptions {
        max_side: Some(IMAGE_MAX_SIDE),
        ..review_llm::ImageOptions::default()
    }
}

/// The widest and tallest image `read_file` decodes, in pixels.
pub const IMAGE_MAX_DECODE_SIDE: u32 = 10_000;
/// The most memory one image decode may allocate.
pub const IMAGE_MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// Decodes PNG or JPEG `data` within [`IMAGE_MAX_DECODE_SIDE`] and [`IMAGE_MAX_ALLOC`] (a
/// small file can declare a huge image), then scales and re-encodes it.
fn decode_and_encode(data: &[u8]) -> Result<review_llm::Image, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(IMAGE_MAX_DECODE_SIDE);
    limits.max_image_height = Some(IMAGE_MAX_DECODE_SIDE);
    limits.max_alloc = Some(IMAGE_MAX_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    review_llm::Image::encode_decoded(img, &image_options()).map_err(|e| e.to_string())
}

/// An image part for `data`. PNG and JPEG are decoded (on the blocking pool, within
/// limits), scaled down and re-encoded; GIF and WebP (which the encoder cannot decode)
/// pass through when small enough.
async fn image_result(path: &str, mime: &str, data: Vec<u8>) -> ToolOutput {
    let image = match mime {
        "image/png" | "image/jpeg" => {
            match tokio::task::spawn_blocking(move || decode_and_encode(&data)).await {
                Ok(Ok(img)) => img,
                Ok(Err(e)) => return ToolOutput::error(format!("cannot decode image {path}: {e}")),
                Err(e) => return ToolOutput::error(format!("cannot decode image {path}: {e}")),
            }
        }
        _ if data.is_empty() => return ToolOutput::error(format!("cannot decode image {path}")),
        _ if data.len() > IMAGE_MAX_PASSTHROUGH => {
            return ToolOutput::error(format!(
                "image {path} is {} bytes, over the {IMAGE_MAX_PASSTHROUGH}-byte limit for \
                 {mime}; convert it to PNG or JPEG with bash first",
                data.len()
            ));
        }
        _ => review_llm::Image::from_bytes(mime, &data),
    };
    ToolOutput {
        parts: vec![review_llm::ToolResultPart::Image(image)],
        is_error: false,
    }
}

/// The image media type for `path`'s extension, if it names one.
fn image_mime_by_ext(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// The image media type for `data`'s magic bytes, if they are PNG, JPEG, GIF or WebP.
fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
