//! LLM clients for gh-review-desktop: Anthropic Messages and OpenAI Responses,
//! behind neutral request/reply types.
//!
//! - [`Client`]: one wire API on one provider, with `generate` and `generate_stream`.

mod anthropic;
mod api;
mod client;
pub mod dns;
mod error;
mod history;
mod http;
mod media;
mod openai;
mod retry;
mod stream;
mod tools;
mod trace;
mod types;

pub use api::ApiType;
pub use client::{Client, ClientConfig};
pub use error::{Error, Result};
pub use history::trim_history;
pub use media::{ImageFormat, ImageOptions, Video};
pub use retry::RetryPolicy;
pub use types::{
    BlockStart, CacheHints, CallMeta, ContentBlock, GenerateRequest, Image, Message, MessageRole,
    Reasoning, ReasoningEffort, Reply, StopReason, StreamEvent, Tool, ToolChoice, ToolResultPart,
    ToolUseRef, Usage,
};

/// Wire-format conversions, exposed for golden-fixture tests and request logging.
#[doc(hidden)]
pub mod wire {
    use std::time::Instant;

    use serde_json::Value;

    use crate::{ApiType, GenerateRequest, Reply, Result, client::CallCtx};

    pub fn anthropic_request(req: &GenerateRequest, stream: bool) -> Result<Value> {
        crate::anthropic::to_wire(req, stream)
    }

    pub fn responses_request(req: &GenerateRequest, stream: bool) -> Result<Value> {
        crate::openai::to_wire_json(req, stream)
    }

    fn ctx(api: ApiType, model: &str) -> CallCtx {
        CallCtx {
            provider: "fixture".into(),
            api,
            model: model.to_owned(),
            started: Instant::now(),
            trace: None,
        }
    }

    pub fn anthropic_reply(resp: &Value) -> Result<Reply> {
        crate::anthropic::from_wire(resp, ctx(ApiType::AnthropicMessages, "fixture"))
    }

    pub fn responses_reply(resp: &Value) -> Result<Reply> {
        crate::openai::from_wire(resp, ctx(ApiType::OpenaiResponses, "fixture"))
    }
}
