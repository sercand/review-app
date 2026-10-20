//! `bash`: one command in the sandbox's persistent shell.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolCx, ToolOutput, definition, parse};

/// The `bash` tool.
pub(super) struct Bash;

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    command: String,
    #[serde(default)]
    restart: bool,
}

#[async_trait::async_trait]
impl Tool for Bash {
    fn definition(&self) -> review_llm::Tool {
        definition(
            "bash",
            "Run a bash command and return its combined output and exit code. It runs from the \
             checkout root; pass paths to the command instead of using 'cd'. Use it for builds, \
             tests, inspecting files and running tools.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to run in the persistent shell (pipes, \
                            redirection and globbing work); it runs from the checkout root: pass \
                            paths to the command instead of using 'cd'. Environment and shell \
                            variables persist across calls. Combined stdout+stderr is returned; \
                            large output is truncated."
                    },
                    "restart": {
                        "type": "boolean",
                        "description": "Reset the persistent shell before running, discarding \
                            accumulated state (cwd, env vars, background jobs). Send with an \
                            empty command to just restart."
                    }
                }
            }),
        )
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolOutput {
        let input: Input = match parse(input) {
            Ok(i) => i,
            Err(out) => return out,
        };
        if input.command.trim().is_empty() && !input.restart {
            return ToolOutput::error("command is required unless restart is true");
        }
        // A zero timeout is the sandbox's default (120 s).
        let res = match cx
            .sandbox
            .exec(&input.command, input.restart, Duration::ZERO)
            .await
        {
            Ok(res) => res,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        // A non-zero exit is information the model needs, not a tool failure.
        let mut text = res.output;
        if res.exit_code != 0 {
            text.push_str(&format!("\n(exit code {})", res.exit_code));
        }
        if res.timed_out {
            text.push_str("\n(timed out)");
        }
        if text.is_empty() {
            // Providers refuse an empty text block.
            text.push_str("(no output)");
        }
        ToolOutput::text(text)
    }
}
