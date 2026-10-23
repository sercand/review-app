//! Anthropic Messages (`/v1/messages`).

use std::time::Duration;

use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::{
    ApiType, ContentBlock, Error, GenerateRequest, MessageRole, ReasoningEffort, Reply, Result,
    StopReason, StreamEvent, ToolChoice, ToolResultPart, Usage,
    client::CallCtx,
    http::Http,
    retry::RetryPolicy,
    stream::{Assembler, Partial, drive},
};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const VERSION: &str = "2023-06-01";
const PATH: &str = "/v1/messages";
/// `max_tokens` is required by the API.
const DEFAULT_MAX_TOKENS: u32 = 4096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const API: ApiType = ApiType::AnthropicMessages;

pub(crate) struct Client {
    http: Http,
    idle: Duration,
}

impl Client {
    pub fn new(
        base: &str,
        mut headers: HeaderMap,
        key: SecretString,
        timeout: Duration,
        idle: Duration,
        retry: RetryPolicy,
        resolver: Option<crate::dns::Resolver>,
    ) -> Result<Self> {
        let mut k = HeaderValue::try_from(key.expose_secret())
            .map_err(|_| Error::Config("api key is not a valid header value".into()))?;
        k.set_sensitive(true);
        headers.insert("x-api-key", k);
        headers.insert("anthropic-version", HeaderValue::from_static(VERSION));
        Ok(Self {
            http: Http::new(base, headers, key, timeout, retry, resolver)?,
            idle,
        })
    }

    pub async fn generate(&self, req: &GenerateRequest, ctx: CallCtx) -> Result<Reply> {
        let body = to_wire(req, false)?;
        let resp = self.http.post_json(PATH, &body, ctx.trace.as_ref()).await?;
        from_wire(&resp, ctx)
    }

    pub async fn generate_stream(
        &self,
        req: &GenerateRequest,
        ctx: CallCtx,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let body = to_wire(req, true)?;
        let wire = self.http.post_sse(PATH, &body, ctx.trace.as_ref()).await?;
        let mut parser = StreamParser::new(ctx);
        Ok(drive(wire, move |v| parser.event(v), self.idle))
    }
}

// ---- request ----

pub(crate) fn to_wire(req: &GenerateRequest, stream: bool) -> Result<Value> {
    let model = req
        .model
        .as_deref()
        .ok_or_else(|| Error::InvalidRequest("model is not set".into()))?;
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = json!({ "model": model, "max_tokens": max_tokens });

    let mut breakpoints = 0;
    if let Some(system) = &req.system {
        body["system"] = if req.cache.system {
            breakpoints += 1;
            json!([{ "type": "text", "text": system, "cache_control": ephemeral() }])
        } else {
            json!(system)
        };
    }

    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect();
        if req.cache.tools
            && let Some(last) = tools.last_mut()
        {
            last["cache_control"] = ephemeral();
            breakpoints += 1;
        }
        body["tools"] = Value::Array(tools);
    }

    if let Some(choice) = tool_choice(req) {
        body["tool_choice"] = choice;
    }

    let mut messages = Vec::new();
    let mut flagged = Vec::new();
    for m in &req.messages {
        let mut blocks: Vec<Value> = m
            .content
            .iter()
            .map(block_to_wire)
            .filter_map(Result::transpose)
            .collect::<Result<_>>()?;
        if blocks.is_empty() {
            continue;
        }
        if m.role == MessageRole::User {
            // The API wants tool results first in a user turn.
            blocks.sort_by_key(|b| b["type"] != "tool_result");
        }
        if m.cache_breakpoint {
            flagged.push(messages.len());
        }
        let role = match m.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        messages.push(json!({ "role": role, "content": blocks }));
    }
    // Keep the latest message breakpoints within the API's limit.
    let room = MAX_CACHE_BREAKPOINTS.saturating_sub(breakpoints);
    let skip = flagged.len().saturating_sub(room);
    for &i in &flagged[skip..] {
        if let Some(block) = messages[i]["content"]
            .as_array_mut()
            .and_then(|bs| bs.iter_mut().rev().find(|b| cacheable(b)))
        {
            block["cache_control"] = ephemeral();
        }
    }
    body["messages"] = Value::Array(messages);

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(r) = req.reasoning {
        let preset: u32 = match r.effort {
            ReasoningEffort::Low => 1024,
            ReasoningEffort::Medium => 4096,
            ReasoningEffort::High => 16384,
        };
        let budget = preset.min(max_tokens.saturating_sub(1));
        if budget < 1024 {
            return Err(Error::InvalidRequest(
                "reasoning needs max_tokens above 1024 on anthropic_messages".into(),
            ));
        }
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    if stream {
        body["stream"] = json!(true);
    }
    Ok(body)
}

fn ephemeral() -> Value {
    json!({ "type": "ephemeral" })
}

fn cacheable(block: &Value) -> bool {
    !matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

fn tool_choice(req: &GenerateRequest) -> Option<Value> {
    let mut choice = match &req.tool_choice {
        ToolChoice::Auto if req.parallel_tool_calls.is_none() => return None,
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::Any => json!({ "type": "any" }),
        ToolChoice::None => return Some(json!({ "type": "none" })),
        ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
    };
    if req.parallel_tool_calls == Some(false) {
        choice["disable_parallel_tool_use"] = json!(true);
    }
    Some(choice)
}

fn image_to_wire(img: &crate::Image) -> Value {
    json!({ "type": "image", "source": { "type": "base64", "media_type": img.media_type, "data": img.data } })
}

fn no_video() -> Error {
    Error::InvalidRequest("anthropic_messages has no video input".into())
}

fn block_to_wire(b: &ContentBlock) -> Result<Option<Value>> {
    Ok(Some(match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Image(img) => image_to_wire(img),
        ContentBlock::Video(_) => return Err(no_video()),
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let parts: Vec<Value> = content
                .iter()
                .map(|p| match p {
                    ToolResultPart::Text { text } => Ok(json!({ "type": "text", "text": text })),
                    ToolResultPart::Image(img) => Ok(image_to_wire(img)),
                    ToolResultPart::Video(_) => Err(no_video()),
                })
                .collect::<Result<_>>()?;
            let mut v =
                json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": parts });
            if *is_error {
                v["is_error"] = json!(true);
            }
            v
        }
        ContentBlock::Thinking {
            api: API,
            text,
            signature,
            ..
        } => {
            let mut v = json!({ "type": "thinking", "thinking": text });
            if let Some(s) = signature {
                v["signature"] = json!(s);
            }
            v
        }
        ContentBlock::Raw { api: API, value } => value.clone(),
        // Thinking and raw blocks from another API are not replayed.
        ContentBlock::Thinking { .. } | ContentBlock::Raw { .. } => return Ok(None),
    }))
}

// ---- response ----

fn block_from_wire(v: &Value) -> Result<ContentBlock> {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    Ok(match v["type"].as_str() {
        Some("text") => ContentBlock::Text { text: s("text") },
        Some("thinking") => ContentBlock::Thinking {
            api: API,
            id: None,
            text: s("thinking"),
            signature: v["signature"].as_str().map(str::to_owned),
        },
        Some("tool_use") => ContentBlock::ToolUse {
            id: s("id"),
            name: s("name"),
            input: v.get("input").cloned().unwrap_or_else(|| json!({})),
        },
        _ => ContentBlock::Raw {
            api: API,
            value: v.clone(),
        },
    })
}

fn stop_from_wire(s: Option<&str>) -> StopReason {
    match s {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other(other.to_owned()),
        None => StopReason::Other(String::new()),
    }
}

/// Merges a wire `usage` object into `u`; fields the object lacks are kept.
fn merge_usage(u: &mut Usage, v: &Value) {
    let n = |k: &str| v.get(k).and_then(Value::as_u64);
    let (input, read, write) = (
        n("input_tokens"),
        n("cache_read_input_tokens"),
        n("cache_creation_input_tokens"),
    );
    if let Some(r) = read {
        u.cache_read = r;
    }
    if let Some(w) = write {
        u.cache_write = w;
    }
    if input.is_some() || read.is_some() || write.is_some() {
        // Anthropic's input_tokens excludes cached tokens; ours is the total.
        u.input = input.unwrap_or(0) + u.cache_read + u.cache_write;
    }
    if let Some(o) = n("output_tokens") {
        u.output = o;
    }
}

pub(crate) fn from_wire(v: &Value, ctx: CallCtx) -> Result<Reply> {
    let content = v["content"]
        .as_array()
        .ok_or_else(|| Error::Decode("messages response has no content".into()))?
        .iter()
        .map(block_from_wire)
        .collect::<Result<Vec<_>>>()?;
    let mut usage = Usage::default();
    merge_usage(&mut usage, &v["usage"]);
    Ok(Reply {
        content,
        stop: stop_from_wire(v["stop_reason"].as_str()),
        usage,
        meta: ctx.meta(),
    })
}

fn stream_error(v: &Value) -> Error {
    let err = &v["error"];
    let status = match err["type"].as_str() {
        Some("overloaded_error") => 529,
        Some("rate_limit_error") => 429,
        Some("api_error") => 500,
        _ => 400,
    };
    Error::Http {
        status,
        body: err["message"].as_str().unwrap_or("stream error").to_owned(),
    }
}

pub(crate) struct StreamParser {
    ctx: CallCtx,
    asm: Assembler,
    usage: Usage,
    stop: Option<String>,
}

impl StreamParser {
    pub fn new(ctx: CallCtx) -> Self {
        Self {
            ctx,
            asm: Assembler::default(),
            usage: Usage::default(),
            stop: None,
        }
    }

    pub fn event(&mut self, v: Value) -> Result<Vec<StreamEvent>> {
        let index = || {
            v["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| Error::Decode("stream event without index".into()))
        };
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("message_start") => {
                let m = &v["message"];
                merge_usage(&mut self.usage, &m["usage"]);
                out.push(StreamEvent::MessageStart {
                    id: m["id"].as_str().map(str::to_owned),
                    model: m["model"].as_str().unwrap_or(&self.ctx.model).to_owned(),
                    usage: self.usage,
                });
            }
            Some("content_block_start") => {
                let cb = &v["content_block"];
                let s = |k: &str| cb[k].as_str().unwrap_or_default().to_owned();
                let partial = match cb["type"].as_str() {
                    Some("text") => Partial::Text(s("text")),
                    Some("thinking") => Partial::Thinking {
                        api: API,
                        id: None,
                        text: s("thinking"),
                        signature: cb["signature"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    },
                    Some("tool_use") => Partial::ToolUse {
                        id: s("id"),
                        name: s("name"),
                        json: String::new(),
                    },
                    _ => Partial::Raw {
                        api: API,
                        value: cb.clone(),
                    },
                };
                out.push(self.asm.start(index()?, partial));
            }
            Some("content_block_delta") => {
                let i = index()?;
                let d = &v["delta"];
                let s = |k: &str| d[k].as_str().unwrap_or_default();
                let ev = match d["type"].as_str() {
                    Some("text_delta") => self.asm.text(i, s("text")),
                    Some("thinking_delta") => self.asm.thinking(i, s("thinking")),
                    Some("input_json_delta") => self.asm.tool_input(i, s("partial_json")),
                    Some("signature_delta") => {
                        self.asm.signature(i, s("signature"));
                        None
                    }
                    _ => None,
                };
                out.extend(ev);
            }
            Some("content_block_stop") => out.push(self.asm.stop(index()?)?),
            Some("message_delta") => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop = Some(s.to_owned());
                }
                merge_usage(&mut self.usage, &v["usage"]);
            }
            Some("message_stop") => {
                let asm = std::mem::take(&mut self.asm);
                out.push(StreamEvent::Done(Reply {
                    content: asm.into_content(),
                    stop: stop_from_wire(self.stop.as_deref()),
                    usage: self.usage,
                    meta: self.ctx.meta(),
                }));
            }
            Some("error") => return Err(stream_error(&v)),
            _ => {}
        }
        Ok(out)
    }
}
