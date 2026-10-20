//! `list_dir` and `asset_stats`: the cheap "look at the neighbours" tools.
//!
//! `list_dir` goes through [`review_sandbox::SandboxOps::list`], so it gets the
//! sandbox's path confinement for free. `asset_stats` resolves every path it
//! touches through [`review_sandbox::Root`] (the same confinement, without reading
//! whole files), so a symlink out of the checkout is never probed.

use std::path::Path;

use review_sandbox::Root;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{MAX_TOOL_OUTPUT, Tool, ToolCx, ToolOutput, definition, parse, truncate_middle};

/// The most entries `list_dir` prints before counting the rest.
const MAX_DIR_ENTRIES: usize = 500;

/// The `list_dir` tool.
pub(super) struct ListDir;

#[derive(Deserialize)]
struct ListInput {
    #[serde(default)]
    path: String,
}

#[async_trait::async_trait]
impl Tool for ListDir {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "list_dir",
            "List a directory: one line per entry as name, size in bytes and dir or file, \
             sorted by name. Use it to see what sits next to a file (for example before \
             asset_stats). Relative paths resolve against the checkout root; an empty path \
             lists the root. Very large directories are cut at 500 entries.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list, e.g. 'assets/icons'. Relative paths \
                            resolve against the checkout root; an empty path lists the root."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: ListInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        let path = if input.path.trim().is_empty() {
            "."
        } else {
            input.path.trim()
        };
        let mut entries = match cx.sandbox.list(path).await {
            Ok(e) => e,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        if entries.is_empty() {
            return ToolOutput::text("(empty directory)");
        }
        // `SandboxOps::list` already sorts, but the format is part of the contract.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let total = entries.len();
        let shown = total.min(MAX_DIR_ENTRIES);
        let mut text = entries[..shown]
            .iter()
            .map(|e| {
                let kind = if e.is_dir { "dir" } else { "file" };
                format!("{}\t{}\t{kind}", e.name, e.size)
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Cut before the note so the count always survives a huge listing.
        text = truncate_middle(&text, MAX_TOOL_OUTPUT);
        if total > shown {
            text.push_str(&format!("\n[{} more]", total - shown));
        }
        ToolOutput::raw_text(text)
    }
}

/// The `asset_stats` tool.
pub(super) struct AssetStats;

#[derive(Deserialize)]
struct StatsInput {
    path: String,
}

#[async_trait::async_trait]
impl Tool for AssetStats {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "asset_stats",
            "Report a file's size and, for an image, its format and pixel dimensions, next to \
             the same figures for the other files of its kind in its directory: count, median \
             size, largest size and median dimensions, plus this file's size as a ratio to the \
             median. Use it to judge whether a binary asset is out of line with its neighbours. \
             Relative paths resolve against the checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to inspect (e.g. 'assets/logo@2x.png'). \
                            Relative paths resolve against the checkout root."
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: StatsInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.path.trim().is_empty() {
            return ToolOutput::error("path is required");
        }
        let repo_root = cx.repo_root.clone();
        let path = input.path.trim().to_owned();
        // File metadata and image headers are blocking work.
        match tokio::task::spawn_blocking(move || stats(&repo_root, &path)).await {
            Ok(Ok(text)) => ToolOutput::text(text),
            Ok(Err(message)) => ToolOutput::error(message),
            Err(e) => ToolOutput::error(format!("asset_stats failed: {e}")),
        }
    }
}

/// The stats for `path` under `repo_root`, as compact JSON text.
fn stats(repo_root: &Path, path: &str) -> Result<String, String> {
    let root = Root::new(repo_root).map_err(|e| format!("cannot open the checkout root: {e}"))?;
    let file = root.resolve(path).map_err(|e| e.to_string())?;
    let meta = std::fs::metadata(&file).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("not a regular file: {path}"));
    }
    let bytes = meta.len();
    let (format, width, height) = match probe(&file) {
        Some((format, w, h)) => (Some(format.to_owned()), Some(w), Some(h)),
        None => (extension(path), None, None),
    };

    let Some(parent) = file.parent() else {
        return Err(format!("no parent directory: {path}"));
    };
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let kind = extension(&name);
    let mut sizes: Vec<u64> = Vec::new();
    let mut widths: Vec<u32> = Vec::new();
    let mut heights: Vec<u32> = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(|e| format!("{}: {e}", root.relative(parent)))? {
        let entry = entry.map_err(|e| format!("{}: {e}", root.relative(parent)))?;
        let sibling = entry.file_name().to_string_lossy().into_owned();
        if sibling == name || extension(&sibling) != kind {
            continue;
        }
        // Re-resolve through the root: a sibling symlinked out of the checkout is
        // skipped, so its metadata and header are never read.
        let Ok(path) = root.resolve(&root.relative(&entry.path())) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        sizes.push(meta.len());
        if let Some((_, w, h)) = probe(&path) {
            widths.push(w);
            heights.push(h);
        }
    }

    let max_bytes = sizes.iter().copied().max();
    let median_bytes = median(&mut sizes);
    let median_width = median(&mut widths);
    let median_height = median(&mut heights);
    // A median of zero says nothing about the ratio; null is not "infinite".
    let ratio_to_median = median_bytes
        .filter(|m| *m > 0)
        .map(|m| bytes as f64 / m as f64);
    let value = json!({
        "path": path,
        "bytes": bytes,
        "format": format,
        "width": width,
        "height": height,
        "siblings": {
            "count": sizes.len(),
            "median_bytes": median_bytes,
            "max_bytes": max_bytes,
            "median_width": median_width,
            "median_height": median_height,
        },
        "ratio_to_median": ratio_to_median,
    });
    serde_json::to_string(&value).map_err(|e| e.to_string())
}

/// The middle value: the element for an odd count, the truncated mean of the two
/// middle elements for an even one. `None` for an empty set.
fn median<T: Ord + Copy + Into<u64>>(values: &mut [T]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid].into()
    } else {
        let sum = values[mid - 1].into() + values[mid].into();
        sum / 2
    })
}

/// The image format (`png`, `jpg`, …), width and height of `path`, if its bytes are
/// an image. Only the header is read, never the pixels.
fn probe(path: &Path) -> Option<(&'static str, u32, u32)> {
    let file = std::fs::File::open(path).ok()?;
    let reader = image::ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let (w, h) = reader.into_dimensions().ok()?;
    let name = format.extensions_str().first().copied().unwrap_or("image");
    Some((name, w, h))
}

/// The lowercased extension of `path`'s file name, or `None` when it has none.
fn extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::median;

    #[test]
    fn median_handles_empty_odd_and_even() {
        assert_eq!(median::<u64>(&mut []), None);
        assert_eq!(median(&mut [7u64]), Some(7));
        assert_eq!(median(&mut [3u64, 1, 2]), Some(2));
        assert_eq!(median(&mut [4u64, 1, 3, 2]), Some(2));
    }
}
