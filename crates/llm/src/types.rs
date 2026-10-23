//! Neutral chat types shared by the Anthropic Messages and OpenAI Responses clients.

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApiType, Error, Result, Video};

/// A chat request. `model` is required; [`crate::Client::generate`] fails when it is unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub model: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: ToolChoice,
    /// `None` leaves the provider default (both APIs allow parallel calls by default).
    pub parallel_tool_calls: Option<bool>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning: Option<Reasoning>,
    pub cache: CacheHints,
}

impl GenerateRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    /// Anthropic: put a cache breakpoint on this message's last block. Ignored by Responses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_breakpoint: bool,
}

impl Message {
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content,
            cache_breakpoint: false,
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content,
            cache_breakpoint: false,
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self::user(vec![ContentBlock::text(text)])
    }

    /// Marks this message as a cache breakpoint.
    pub fn cached(mut self) -> Self {
        self.cache_breakpoint = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image(Image),
    /// A video clip. Sent to Responses as `input_video`; Anthropic has none.
    Video(Video),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultPart>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Model reasoning. Replayed only to the API it came from.
    Thinking {
        api: ApiType,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        text: String,
        /// Anthropic `signature`, or Responses `encrypted_content`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// A wire block `review-llm` does not model. Replayed only to the same API.
    Raw {
        api: ApiType,
        value: Value,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: Vec<ToolResultPart>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content,
            is_error: false,
        }
    }
}

/// An inline image, base64-encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub media_type: String,
    pub data: String,
}

impl Image {
    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// `data:<media_type>;base64,<data>`
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    Text { text: String },
    Image(Image),
    Video(Video),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the tool input.
    pub input_schema: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    /// The model must call at least one tool.
    Any,
    /// The model must not call a tool.
    None,
    /// The model must call this tool.
    Tool { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    pub effort: ReasoningEffort,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHints {
    /// Anthropic: cache breakpoint after the system prompt.
    #[serde(default)]
    pub system: bool,
    /// Anthropic: cache breakpoint after the last tool.
    #[serde(default)]
    pub tools: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub content: Vec<ContentBlock>,
    pub stop: StopReason,
    pub usage: Usage,
    pub meta: CallMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "raw", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    /// Any other wire value, kept as sent.
    Other(String),
}

/// Token counts. `input` is the total input, including cached tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// What actually served a call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallMeta {
    pub api: ApiType,
    pub provider: String,
    pub model: String,
    #[serde(with = "duration_ms")]
    pub latency: Duration,
}

/// A borrowed view of one tool call in a [`Reply`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolUseRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
}

impl Reply {
    /// All text blocks, concatenated.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Every tool call, in order.
    pub fn tool_uses(&self) -> impl Iterator<Item = ToolUseRef<'_>> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(ToolUseRef { id, name, input }),
            _ => None,
        })
    }

    /// The one tool call of a reply that was forced to make exactly one.
    pub fn single_tool_use(&self) -> Result<ToolUseRef<'_>> {
        if self.stop == StopReason::Refusal {
            return Err(Error::ToolCall("the model refused".into()));
        }
        let mut calls = self.tool_uses();
        match (calls.next(), calls.next()) {
            (Some(call), None) => Ok(call),
            (None, _) => Err(Error::ToolCall("the reply has no tool call".into())),
            (Some(_), Some(_)) => Err(Error::ToolCall(format!(
                "expected one tool call, got {}",
                self.tool_uses().count()
            ))),
        }
    }

    /// Converts this reply into the assistant message to append to the history.
    pub fn to_message(&self) -> Message {
        Message::assistant(self.content.clone())
    }
}

/// One event of [`crate::Client::generate_stream`].
///
/// Order: `MessageStart`, then for each block `BlockStart` → deltas → `BlockStop`
/// (blocks can interleave), then `Done`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        id: Option<String>,
        model: String,
        usage: Usage,
    },
    BlockStart {
        index: usize,
        block: BlockStart,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ToolInputDelta {
        index: usize,
        partial_json: String,
    },
    /// The finished block, identical to the one at the same position in `Done`.
    BlockStop {
        index: usize,
        block: ContentBlock,
    },
    Done(Reply),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockStart {
    Text,
    Thinking,
    ToolUse { id: String, name: String },
    Raw,
}

mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reply(content: Vec<ContentBlock>, stop: StopReason) -> Reply {
        Reply {
            content,
            stop,
            usage: Usage::default(),
            meta: CallMeta {
                api: ApiType::AnthropicMessages,
                provider: "p".into(),
                model: "m".into(),
                latency: Duration::ZERO,
            },
        }
    }

    fn call(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "tap".into(),
            input: json!({"id": "a"}),
        }
    }

    #[test]
    fn single_tool_use() {
        let r = reply(
            vec![ContentBlock::text("ok"), call("1")],
            StopReason::ToolUse,
        );
        assert_eq!(r.single_tool_use().unwrap().id, "1");

        let r = reply(vec![call("1"), call("2")], StopReason::ToolUse);
        assert_eq!(r.tool_uses().count(), 2);
        assert!(matches!(r.single_tool_use(), Err(Error::ToolCall(_))));

        let r = reply(vec![ContentBlock::text("no")], StopReason::EndTurn);
        assert!(r.single_tool_use().is_err());

        let r = reply(vec![call("1")], StopReason::Refusal);
        assert!(r.single_tool_use().is_err());
    }

    #[test]
    fn image_data_url() {
        let img = Image::from_bytes("image/png", b"hi");
        assert_eq!(img.data_url(), "data:image/png;base64,aGk=");
    }
}
