//! `glob` and `grep`.

use std::fmt::Write as _;

use review_sandbox::{MAX_GLOB, MAX_GREP};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    MAX_TOOL_OUTPUT, Tool, ToolCx, ToolOutput, definition, parse, search_root, truncate_middle,
};

/// The `glob` tool.
pub(super) struct Glob;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    directory: String,
}

#[async_trait::async_trait]
impl Tool for Glob {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "glob",
            "List file paths matching a glob pattern. Supports `*` (any run without a slash), \
             `**` (any path) and `?` (one character). Paths come back newest first. Use it to \
             discover what exists before reading. Matching starts from the checkout root unless a \
             directory is given.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern matched against file paths (e.g. \
                            '**/*.html', 'assets/*.png')."
                    },
                    "directory": {
                        "type": "string",
                        "description": "Directory the pattern is matched relative to. Defaults \
                            to the checkout root."
                    }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: GlobInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.pattern.trim().is_empty() {
            return ToolOutput::error("pattern is required");
        }
        let root = search_root(&input.directory);
        let listing = match cx.sandbox.glob(&input.pattern, root).await {
            Ok(l) => l,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mut text = if listing.items.is_empty() {
            format!("no files match {:?} under {root}", input.pattern)
        } else {
            truncate_middle(&listing.items.join("\n"), MAX_TOOL_OUTPUT)
        };
        if listing.truncated {
            let _ = write!(text, "\n… only the first {MAX_GLOB} matches are shown");
        }
        // Already cut: the truncation note stays whole after it.
        ToolOutput::raw_text(text)
    }
}

/// The `grep` tool.
pub(super) struct Grep;

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    directory: String,
    #[serde(default)]
    glob: String,
}

#[async_trait::async_trait]
impl Tool for Grep {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "grep",
            "Search file contents for a regular expression, returning matching lines as \
             path:line:text. Prefer this over running grep through bash: no shell quoting to get \
             right, and the results come back structured. Narrow the search with 'glob' when you \
             know the file type. Relative paths resolve against the checkout root.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression to search file contents for."
                    },
                    "directory": {
                        "type": "string",
                        "description": "Directory to search under. Defaults to the checkout root."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files whose path matches this glob, e.g. \
                            '**/*.go'."
                    }
                },
                "required": ["pattern"]
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: GrepInput = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.pattern.trim().is_empty() {
            return ToolOutput::error("pattern is required");
        }
        let root = search_root(&input.directory);
        let glob = Some(input.glob.as_str()).filter(|g| !g.trim().is_empty());
        let listing = match cx.sandbox.grep(&input.pattern, root, glob).await {
            Ok(l) => l,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mut text = if listing.items.is_empty() {
            format!("no matches for {:?} under {root}", input.pattern)
        } else {
            let mut lines = String::new();
            for (i, m) in listing.items.iter().enumerate() {
                if i > 0 {
                    lines.push('\n');
                }
                let _ = write!(lines, "{}:{}:{}", m.path, m.line, m.text);
            }
            // Cut before the note so a file of very long lines cannot hide it.
            truncate_middle(&lines, MAX_TOOL_OUTPUT)
        };
        if listing.truncated {
            let _ = write!(text, "\n… only the first {MAX_GREP} matches are shown");
        }
        // Already cut: the truncation note stays whole after it.
        ToolOutput::raw_text(text)
    }
}
