//! LLM clients for OtsimoQA (#85, #87): Anthropic Messages, OpenAI Responses and
//! `/v1/decisions`, behind neutral request/reply types.
//!
//! - [`Client`]: one wire API on one provider, with `generate`, `generate_stream` and `decide`.
//! - [`LLM`]: built from the merged `[llm]` config; hands out [`ChatModel`] and
//!   [`DecisionModel`] handles per typed role.

mod anthropic;
mod client;
mod config;
mod decision;
mod decisions;
pub mod dns;
mod error;
mod history;
mod http;
mod llm;
mod media;
mod openai;
mod retry;
mod role;
mod stream;
mod tools;
mod trace;
mod types;

pub use client::{Client, ClientConfig};
pub use config::{DecisionLimits, Defaults, LLMConfig, ProviderConfig, RoleConfig};
pub use decision::{
    Answer, ChoiceOption, ChoiceProbability, DecideRequest, Decision, Level, LevelProbability,
    Question,
};
pub use error::{Error, Result};
pub use history::trim_history;
pub use llm::{ChatModel, DecisionModel, LLM};
pub use media::{ImageFormat, ImageOptions, Video};
pub use retry::RetryPolicy;
pub use role::{ApiType, ChatRole, DecisionRole, Kind, Role};
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

    use crate::{
        ApiType, DecideRequest, Decision, GenerateRequest, Reply, Result, client::CallCtx,
    };

    pub fn anthropic_request(req: &GenerateRequest, stream: bool) -> Result<Value> {
        crate::anthropic::to_wire(req, stream)
    }

    pub fn responses_request(req: &GenerateRequest, stream: bool) -> Result<Value> {
        crate::openai::to_wire_json(req, stream)
    }

    pub fn decisions_request(req: &DecideRequest) -> Result<Value> {
        crate::decisions::to_wire(req)
    }

    fn ctx(api: ApiType, model: &str) -> CallCtx {
        CallCtx {
            provider: "fixture".into(),
            api,
            model: model.to_owned(),
            started: Instant::now(),
            tag: Default::default(),
            trace: None,
        }
    }

    pub fn anthropic_reply(resp: &Value) -> Result<Reply> {
        crate::anthropic::from_wire(resp, ctx(ApiType::AnthropicMessages, "fixture"))
    }

    pub fn responses_reply(resp: &Value) -> Result<Reply> {
        crate::openai::from_wire(resp, ctx(ApiType::OpenaiResponses, "fixture"))
    }

    pub fn decisions_reply(req: &DecideRequest, resp: &Value) -> Result<Decision> {
        crate::decisions::from_wire(req, resp, ctx(ApiType::Decisions, "fixture"))
    }
}
