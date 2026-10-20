//! OpenAI Responses (`/v1/responses`) through `async-openai`.
//!
//! Requests use `async-openai`'s typed `CreateResponse`. Responses come back as JSON through the
//! `byot` calls and are converted item by item, so an output item `async-openai` doesn't know
//! becomes a `Raw` block instead of failing the whole response.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_openai::{
    config::OpenAIConfig,
    error::OpenAIError,
    middleware::HttpRequestFactory,
    types::responses::{
        CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
        FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, ImageDetail, IncludeEnum,
        InputContent, InputFileContent, InputImageContent, InputItem, InputMessage, InputParam,
        InputRole, InputTextContent, Item, MessageItem, MessageType, Reasoning as WireReasoning,
        ReasoningEffort as WireEffort, ReasoningItem, ReasoningSummary, Role as WireRole,
        SummaryPart, SummaryTextContent, Tool as WireTool, ToolChoiceFunction, ToolChoiceOptions,
        ToolChoiceParam,
    },
};
use futures::{StreamExt, stream::BoxStream};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::{
    ApiType, ContentBlock, Error, GenerateRequest, Image, MessageRole, ReasoningEffort, Reply,
    Result, StopReason, StreamEvent, ToolChoice, ToolResultPart, Usage, Video,
    client::CallCtx,
    error::redact_body,
    http::CONNECT_TIMEOUT,
    retry::{Failure, RetryPolicy},
    stream::{Assembler, Partial, drive, parse_tool_input},
    trace::{RequestRec, ResponseRec, StreamRecorder, TraceCall, body_value},
};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const API: ApiType = ApiType::OpenaiResponses;

pub(crate) struct Client {
    /// Has the total request timeout; for `generate`.
    unary: async_openai::Client<OpenAIConfig>,
    /// No total timeout; streams use the idle timeout instead.
    streaming: async_openai::Client<OpenAIConfig>,
    key: SecretString,
    idle: Duration,
    retry: RetryPolicy,
}

impl Client {
    pub fn new(
        base: &str,
        headers: &[(String, String)],
        key: SecretString,
        timeout: Duration,
        idle: Duration,
        retry: RetryPolicy,
        resolver: Option<crate::dns::Resolver>,
    ) -> Result<Self> {
        let mut config = OpenAIConfig::new()
            .with_api_base(format!("{}/v1", base.trim_end_matches('/')))
            .with_api_key(key.expose_secret());
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::try_from(k.as_str())
                .map_err(|e| Error::Config(format!("header name `{k}`: {e}")))?;
            config = config
                .with_header(name, v.as_str())
                .map_err(|e| Error::Config(format!("header `{k}`: {e}")))?;
        }
        let http = |total: Option<Duration>| {
            let mut b = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
            if let Some(r) = &resolver {
                b = crate::dns::guarded(b, r.clone());
            }
            if let Some(t) = total {
                b = b.timeout(t);
            }
            b.build().map_err(Error::from_reqwest)
        };
        // `ReqwestService` without `async-openai`'s own retry layer, so `RetryPolicy` applies.
        let client = |http: reqwest::Client, config: OpenAIConfig| {
            async_openai::Client::build(http.clone(), config)
                .with_http_service(TracedService { client: http })
        };
        Ok(Self {
            unary: client(http(Some(timeout))?, config.clone()),
            streaming: client(http(None)?, config),
            key,
            idle,
            retry,
        })
    }

    fn error(&self, e: OpenAIError) -> Failure {
        Failure::from(match e {
            OpenAIError::ApiError(r) => Error::Http {
                status: r.status_code.as_u16(),
                body: redact_body(&r.api_error.message, self.key.expose_secret()),
            },
            OpenAIError::Reqwest(e) => Error::from_reqwest(e),
            OpenAIError::JSONDeserialize(e, _) => Error::Decode(e.to_string()),
            OpenAIError::StreamError(e) => Error::Decode(e.to_string()),
            OpenAIError::InvalidArgument(m) => Error::InvalidRequest(m),
            other => Error::Transport(other.to_string()),
        })
    }

    pub async fn generate(&self, req: &GenerateRequest, ctx: CallCtx) -> Result<Reply> {
        let body = to_wire_json(req, false)?;
        let call = self.retry.run(|| async {
            self.unary
                .responses()
                .create_byot(body.clone())
                .await
                .map_err(|e| self.error(e))
        });
        let resp: Value = match ctx.trace.clone() {
            Some(trace) => CURRENT.scope(trace, call).await?,
            None => call.await?,
        };
        from_wire(&resp, ctx)
    }

    pub async fn generate_stream(
        &self,
        req: &GenerateRequest,
        ctx: CallCtx,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let body = to_wire_json(req, true)?;
        let open = self.retry.run(|| async {
            self.streaming
                .responses()
                .create_stream_byot::<_, Value>(body.clone())
                .await
                .map_err(|e| self.error(e))
        });
        let wire = match ctx.trace.clone() {
            Some(trace) => CURRENT.scope(trace, open).await?,
            None => open.await?,
        };
        // The service left the opened stream's recorder on the call; events go into it and
        // its line is written when the stream ends or is dropped.
        let recorder = ctx
            .trace
            .as_ref()
            .and_then(|t| t.pending.lock().ok()?.take())
            .map(|r| Arc::new(Mutex::new(r)));
        let key = self.key.clone();
        let wire = wire.map(move |r| {
            let r = r.map_err(|e| match e {
                OpenAIError::ApiError(r) => Error::Http {
                    status: r.status_code.as_u16(),
                    body: redact_body(&r.api_error.message, key.expose_secret()),
                },
                OpenAIError::Reqwest(e) => Error::from_reqwest(e),
                other => Error::Decode(other.to_string()),
            });
            if let Some(mut rec) = recorder.as_ref().and_then(|r| r.lock().ok()) {
                match &r {
                    Ok(v) => {
                        let event = v["type"].as_str().unwrap_or_default().to_owned();
                        rec.events.push(json!({ "event": event, "data": v }));
                    }
                    Err(e) => rec.error = Some(e.to_string()),
                }
            }
            r
        });
        let mut parser = StreamParser::new(ctx);
        Ok(drive(wire, move |v| parser.event(v), self.idle))
    }
}

tokio::task_local! {
    /// The traced call the current task is making; read by [`TracedService`].
    static CURRENT: Arc<TraceCall>;
}

/// The HTTP service under `async-openai`: plain reqwest (no retry layer, so `RetryPolicy`
/// applies), recording each exchange when the current call is traced.
#[derive(Clone)]
struct TracedService {
    client: reqwest::Client,
}

impl tower::Service<HttpRequestFactory> for TracedService {
    type Response = reqwest::Response;
    type Error = OpenAIError;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<reqwest::Response, OpenAIError>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), OpenAIError>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, factory: HttpRequestFactory) -> Self::Future {
        let client = self.client.clone();
        let call = CURRENT.try_with(Arc::clone).ok();
        Box::pin(async move {
            let request = factory.build().await?;
            let Some(call) = call else {
                return client.execute(request).await.map_err(OpenAIError::Reqwest);
            };
            let attempt = call.next_attempt();
            let started = Instant::now();
            let body = request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .map(body_value)
                .unwrap_or(Value::Null);
            let stream = body["stream"] == Value::Bool(true);
            let rec = RequestRec {
                method: request.method().to_string(),
                url: request.url().to_string(),
                headers: call.headers(request.headers()),
                body,
            };
            let resp = match client.execute(request).await {
                Ok(r) => r,
                Err(e) => {
                    call.write(attempt, started, &rec, None, Some(&e.to_string()));
                    return Err(OpenAIError::Reqwest(e));
                }
            };
            let status = resp.status();
            let headers = resp.headers().clone();
            if status.is_success() && stream {
                if let Ok(mut pending) = call.pending.lock() {
                    *pending = Some(StreamRecorder {
                        call: call.clone(),
                        attempt,
                        started,
                        request: rec,
                        status: status.as_u16(),
                        headers: call.headers(&headers),
                        events: Vec::new(),
                        error: None,
                    });
                }
                return Ok(resp);
            }
            // Buffer the body for the trace, then hand `async-openai` an identical response.
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    call.write(attempt, started, &rec, None, Some(&e.to_string()));
                    return Err(OpenAIError::Reqwest(e));
                }
            };
            let error = (!status.is_success()).then(|| {
                format!(
                    "http {}: {}",
                    status.as_u16(),
                    call.redact(&String::from_utf8_lossy(&bytes))
                )
            });
            let response = ResponseRec {
                status: status.as_u16(),
                headers: call.headers(&headers),
                body: body_value(&bytes),
            };
            call.write(attempt, started, &rec, Some(&response), error.as_deref());
            let mut rebuilt = http::Response::builder().status(status);
            if let Some(h) = rebuilt.headers_mut() {
                *h = headers;
            }
            let rebuilt = rebuilt
                .body(bytes)
                .map_err(|e| OpenAIError::InvalidArgument(e.to_string()))?;
            Ok(reqwest::Response::from(rebuilt))
        })
    }
}

// ---- request ----

pub(crate) fn to_wire(req: &GenerateRequest, stream: bool) -> Result<CreateResponse> {
    let model = req
        .model
        .clone()
        .ok_or_else(|| Error::InvalidRequest("model is not set".into()))?;
    let mut items = Vec::new();
    for m in &req.messages {
        match m.role {
            MessageRole::User => user_items(&m.content, &mut items)?,
            MessageRole::Assistant => assistant_items(&m.content, &mut items)?,
        }
    }
    let tools: Vec<WireTool> = req
        .tools
        .iter()
        .map(|t| {
            WireTool::Function(FunctionTool {
                name: t.name.clone(),
                description: Some(t.description.clone()),
                parameters: Some(t.input_schema.clone()),
                // Our schemas need not satisfy strict mode.
                strict: Some(false),
                ..Default::default()
            })
        })
        .collect();
    let tool_choice = match &req.tool_choice {
        ToolChoice::Auto => None,
        ToolChoice::Any => Some(ToolChoiceParam::Option(ToolChoiceOptions::Required)),
        ToolChoice::None => Some(ToolChoiceParam::Option(ToolChoiceOptions::None)),
        ToolChoice::Tool { name } => Some(ToolChoiceParam::Function(ToolChoiceFunction {
            name: name.clone(),
        })),
    };
    let reasoning = req.reasoning.map(|r| WireReasoning {
        effort: Some(match r.effort {
            ReasoningEffort::Low => WireEffort::Low,
            ReasoningEffort::Medium => WireEffort::Medium,
            ReasoningEffort::High => WireEffort::High,
        }),
        summary: Some(ReasoningSummary::Auto),
        ..Default::default()
    });
    Ok(CreateResponse {
        model: Some(model),
        instructions: req.system.clone(),
        input: InputParam::Items(items),
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        parallel_tool_calls: req.parallel_tool_calls,
        max_output_tokens: req.max_tokens,
        temperature: req.temperature,
        include: reasoning
            .is_some()
            .then(|| vec![IncludeEnum::ReasoningEncryptedContent]),
        reasoning,
        store: Some(false),
        stream: stream.then_some(true),
        ..Default::default()
    })
}

fn input_text(text: &str) -> InputContent {
    InputContent::InputText(InputTextContent {
        text: text.to_owned(),
        prompt_cache_breakpoint: None,
    })
}

fn input_image(img: &Image) -> InputContent {
    InputContent::InputImage(InputImageContent {
        detail: ImageDetail::Auto,
        image_url: Some(img.data_url()),
        ..Default::default()
    })
}

/// `async-openai` has no `input_video` part. Video goes into the typed request as an
/// `input_file` tagged with this filename, and [`to_wire_json`] rewrites it.
const VIDEO_MARKER: &str = "__review_llm_input_video__";

fn input_video(v: &Video) -> InputContent {
    InputContent::InputFile(InputFileContent {
        file_data: Some(v.data_url()),
        filename: Some(VIDEO_MARKER.to_owned()),
        ..Default::default()
    })
}

/// Rewrites every video placeholder into `{"type": "input_video", "video_url": …}`.
fn rewrite_video(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("input_file")
                && map.get("filename").and_then(Value::as_str) == Some(VIDEO_MARKER)
            {
                let url = map.remove("file_data").unwrap_or(Value::Null);
                map.clear();
                map.insert("type".into(), Value::from("input_video"));
                map.insert("video_url".into(), url);
                return;
            }
            map.values_mut().for_each(rewrite_video);
        }
        Value::Array(items) => items.iter_mut().for_each(rewrite_video),
        _ => {}
    }
}

fn raw_item(value: &Value) -> Result<InputItem> {
    serde_json::from_value(value.clone())
        .map_err(|e| Error::InvalidRequest(format!("raw openai_responses item: {e}")))
}

fn user_items(blocks: &[ContentBlock], items: &mut Vec<InputItem>) -> Result<()> {
    let mut parts = Vec::new();
    let flush = |parts: &mut Vec<InputContent>, items: &mut Vec<InputItem>| {
        if !parts.is_empty() {
            items.push(InputItem::Item(Item::Message(MessageItem::Input(
                InputMessage {
                    content: std::mem::take(parts),
                    role: InputRole::User,
                    status: None,
                },
            ))));
        }
    };
    for b in blocks {
        match b {
            ContentBlock::Text { text } => parts.push(input_text(text)),
            ContentBlock::Image(img) => parts.push(input_image(img)),
            ContentBlock::Video(v) => parts.push(input_video(v)),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                flush(&mut parts, items);
                let output = match content.as_slice() {
                    [ToolResultPart::Text { text }] => FunctionCallOutput::Text(text.clone()),
                    parts => FunctionCallOutput::Content(
                        parts
                            .iter()
                            .map(|p| match p {
                                ToolResultPart::Text { text } => input_text(text),
                                ToolResultPart::Image(img) => input_image(img),
                                ToolResultPart::Video(v) => input_video(v),
                            })
                            .collect(),
                    ),
                };
                items.push(InputItem::Item(Item::FunctionCallOutput(
                    FunctionCallOutputItemParam {
                        call_id: Some(tool_use_id.clone()),
                        output,
                        id: None,
                        status: None,
                        name: None,
                        namespace: None,
                        caller: None,
                    },
                )));
            }
            ContentBlock::Raw { api: API, value } => {
                flush(&mut parts, items);
                items.push(raw_item(value)?);
            }
            ContentBlock::Thinking { .. } | ContentBlock::Raw { .. } => {}
            ContentBlock::ToolUse { .. } => {
                return Err(Error::InvalidRequest(
                    "tool_use belongs in an assistant message".into(),
                ));
            }
        }
    }
    flush(&mut parts, items);
    Ok(())
}

fn assistant_items(blocks: &[ContentBlock], items: &mut Vec<InputItem>) -> Result<()> {
    let mut text = String::new();
    let flush = |text: &mut String, items: &mut Vec<InputItem>| {
        if !text.is_empty() {
            items.push(InputItem::EasyMessage(EasyInputMessage {
                r#type: MessageType::Message,
                role: WireRole::Assistant,
                content: EasyInputContent::Text(std::mem::take(text)),
                phase: None,
            }));
        }
    };
    for b in blocks {
        match b {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolUse { id, name, input } => {
                flush(&mut text, items);
                items.push(InputItem::Item(Item::FunctionCall(FunctionToolCall {
                    arguments: input.to_string(),
                    call_id: id.clone(),
                    namespace: None,
                    name: name.clone(),
                    id: None,
                    status: None,
                    caller: None,
                    r#async: None,
                })));
            }
            ContentBlock::Thinking {
                api: API,
                id,
                text: t,
                signature,
            } => {
                flush(&mut text, items);
                let summary = if t.is_empty() {
                    vec![]
                } else {
                    vec![SummaryPart::SummaryText(SummaryTextContent {
                        text: t.clone(),
                    })]
                };
                items.push(InputItem::Item(Item::Reasoning(ReasoningItem {
                    id: id.clone(),
                    summary,
                    content: None,
                    encrypted_content: signature.clone(),
                    status: None,
                })));
            }
            ContentBlock::Raw { api: API, value } => {
                flush(&mut text, items);
                items.push(raw_item(value)?);
            }
            ContentBlock::Thinking { .. } | ContentBlock::Raw { .. } => {}
            ContentBlock::Image(_) | ContentBlock::Video(_) | ContentBlock::ToolResult { .. } => {
                return Err(Error::InvalidRequest(
                    "images, video and tool results belong in a user message".into(),
                ));
            }
        }
    }
    flush(&mut text, items);
    Ok(())
}

// ---- response ----

/// One output item as a block, and whether it holds a refusal.
fn block_from_item(item: &Value) -> Result<(ContentBlock, bool)> {
    let s = |k: &str| item[k].as_str().unwrap_or_default().to_owned();
    Ok(match item["type"].as_str() {
        Some("message") => {
            let mut text = String::new();
            let mut refusal = false;
            for part in item["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("output_text") => text.push_str(part["text"].as_str().unwrap_or_default()),
                    Some("refusal") => {
                        refusal = true;
                        text.push_str(part["refusal"].as_str().unwrap_or_default());
                    }
                    _ => {}
                }
            }
            (ContentBlock::Text { text }, refusal)
        }
        Some("reasoning") => {
            let texts = |k: &str| -> Vec<&str> {
                item[k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect()
            };
            let mut text = texts("summary");
            if text.is_empty() {
                text = texts("content");
            }
            (
                ContentBlock::Thinking {
                    api: API,
                    id: item["id"].as_str().map(str::to_owned),
                    text: text.join("\n\n"),
                    signature: item["encrypted_content"].as_str().map(str::to_owned),
                },
                false,
            )
        }
        Some("function_call") => (
            ContentBlock::ToolUse {
                id: s("call_id"),
                name: s("name"),
                input: parse_tool_input(item["arguments"].as_str().unwrap_or_default())?,
            },
            false,
        ),
        _ => (
            ContentBlock::Raw {
                api: API,
                value: item.clone(),
            },
            false,
        ),
    })
}

fn usage_from_wire(v: &Value) -> Usage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Usage {
        input: n(&v["input_tokens"]),
        output: n(&v["output_tokens"]),
        cache_read: n(&v["input_tokens_details"]["cached_tokens"]),
        cache_write: 0,
        reasoning: n(&v["output_tokens_details"]["reasoning_tokens"]),
    }
}

fn stop_from_wire(resp: &Value, has_tool_use: bool, refusal: bool) -> StopReason {
    match resp["status"].as_str() {
        Some("completed") if refusal => StopReason::Refusal,
        Some("completed") if has_tool_use => StopReason::ToolUse,
        Some("completed") => StopReason::EndTurn,
        Some("incomplete") => match resp["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            other => StopReason::Other(format!("incomplete: {}", other.unwrap_or_default())),
        },
        Some("failed") => StopReason::Other(format!(
            "failed: {}",
            resp["error"]["message"].as_str().unwrap_or_default()
        )),
        other => StopReason::Other(other.unwrap_or_default().to_owned()),
    }
}

pub(crate) fn from_wire(resp: &Value, ctx: CallCtx) -> Result<Reply> {
    let mut content = Vec::new();
    let mut refusal = false;
    for item in resp["output"].as_array().into_iter().flatten() {
        let (block, r) = block_from_item(item)?;
        refusal |= r;
        content.push(block);
    }
    let has_tool_use = content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
    Ok(Reply {
        stop: stop_from_wire(resp, has_tool_use, refusal),
        usage: usage_from_wire(&resp["usage"]),
        content,
        meta: ctx.meta(),
    })
}

pub(crate) struct StreamParser {
    ctx: CallCtx,
    asm: Assembler,
    refusal: bool,
}

impl StreamParser {
    pub fn new(ctx: CallCtx) -> Self {
        Self {
            ctx,
            asm: Assembler::default(),
            refusal: false,
        }
    }

    pub fn event(&mut self, v: Value) -> Result<Vec<StreamEvent>> {
        let index = || {
            v["output_index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| Error::Decode("stream event without output_index".into()))
        };
        let delta = || v["delta"].as_str().unwrap_or_default();
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("response.created") => {
                let r = &v["response"];
                out.push(StreamEvent::MessageStart {
                    id: r["id"].as_str().map(str::to_owned),
                    model: r["model"].as_str().unwrap_or(&self.ctx.model).to_owned(),
                    usage: Usage::default(),
                });
            }
            Some("response.output_item.added") => {
                let item = &v["item"];
                let s = |k: &str| item[k].as_str().unwrap_or_default().to_owned();
                let partial = match item["type"].as_str() {
                    Some("message") => Partial::Text(String::new()),
                    Some("reasoning") => Partial::Thinking {
                        api: API,
                        id: item["id"].as_str().map(str::to_owned),
                        text: String::new(),
                        signature: None,
                    },
                    Some("function_call") => Partial::ToolUse {
                        id: s("call_id"),
                        name: s("name"),
                        json: String::new(),
                    },
                    _ => Partial::Raw {
                        api: API,
                        value: item.clone(),
                    },
                };
                out.push(self.asm.start(index()?, partial));
            }
            Some("response.output_text.delta" | "response.refusal.delta") => {
                out.extend(self.asm.text(index()?, delta()));
            }
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                out.extend(self.asm.thinking(index()?, delta()));
            }
            Some("response.function_call_arguments.delta") => {
                out.extend(self.asm.tool_input(index()?, delta()));
            }
            Some("response.output_item.done") => {
                let (block, refusal) = block_from_item(&v["item"])?;
                self.refusal |= refusal;
                out.push(self.asm.stop_with(index()?, block));
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let r = &v["response"];
                let asm = std::mem::take(&mut self.asm);
                let content = asm.into_content();
                let has_tool_use = content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
                out.push(StreamEvent::Done(Reply {
                    stop: stop_from_wire(r, has_tool_use, self.refusal),
                    usage: usage_from_wire(&r["usage"]),
                    content,
                    meta: self.ctx.meta(),
                }));
            }
            Some("error") => {
                return Err(Error::Http {
                    status: 500,
                    body: v["message"].as_str().unwrap_or("stream error").to_owned(),
                });
            }
            _ => {}
        }
        Ok(out)
    }
}

/// Serializes a request the way it goes on the wire; for fixtures and logging.
pub(crate) fn to_wire_json(req: &GenerateRequest, stream: bool) -> Result<Value> {
    let mut v =
        serde_json::to_value(to_wire(req, stream)?).map_err(|e| Error::Decode(e.to_string()))?;
    rewrite_video(&mut v);
    Ok(v)
}
