//! `/v1/decisions` (#85).

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::{
    Answer, ContentBlock, DecideRequest, Decision, Error, Question, Result, Usage, client::CallCtx,
    http::Http, retry::RetryPolicy,
};

const PATH: &str = "/v1/decisions";

pub(crate) struct Client {
    http: Http,
}

impl Client {
    pub fn new(
        base: &str,
        mut headers: HeaderMap,
        key: SecretString,
        timeout: Duration,
        retry: RetryPolicy,
        resolver: Option<crate::dns::Resolver>,
    ) -> Result<Self> {
        let mut auth = HeaderValue::try_from(format!("Bearer {}", key.expose_secret()))
            .map_err(|_| Error::Config("api key is not a valid header value".into()))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        Ok(Self {
            http: Http::new(base, headers, key, timeout, retry, resolver)?,
        })
    }

    pub async fn decide(&self, req: &DecideRequest, ctx: CallCtx) -> Result<Decision> {
        let body = to_wire(req)?;
        let resp = self.http.post_json(PATH, &body, ctx.trace.as_ref()).await?;
        from_wire(req, &resp, ctx)
    }
}

pub(crate) fn to_wire(req: &DecideRequest) -> Result<Value> {
    req.validate()?;
    let model = req
        .model
        .as_deref()
        .ok_or_else(|| Error::InvalidRequest("model is not set".into()))?;
    let input: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            let content: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => {
                        Some(json!({ "type": "input_text", "text": text }))
                    }
                    ContentBlock::Image(img) => {
                        Some(json!({ "type": "input_image", "image_url": img.data_url() }))
                    }
                    ContentBlock::Video(v) => {
                        Some(json!({ "type": "input_video", "video_url": v.data_url() }))
                    }
                    _ => None,
                })
                .collect();
            json!({ "role": "user", "content": content })
        })
        .collect();
    let questions: Vec<Value> = req
        .questions
        .iter()
        .map(|q| match q {
            Question::Predicate { name, instructions } => {
                json!({ "type": "predicate", "name": name, "instructions": instructions })
            }
            Question::Choice {
                name,
                instructions,
                choices,
            } => json!({ "type": "choice", "name": name, "instructions": instructions, "choices": choices }),
            Question::Score {
                name,
                instructions,
                levels,
            } => json!({ "type": "score", "name": name, "instructions": instructions, "levels": levels }),
        })
        .collect();
    Ok(json!({ "model": model, "input": input, "questions": questions }))
}

pub(crate) fn from_wire(req: &DecideRequest, resp: &Value, ctx: CallCtx) -> Result<Decision> {
    let answers: Vec<Answer> = serde_json::from_value(resp["answers"].clone())
        .map_err(|e| Error::Decode(format!("decision answers: {e}")))?;
    for q in &req.questions {
        if !answers.iter().any(|a| a.name() == q.name()) {
            return Err(Error::MissingAnswer {
                name: q.name().to_owned(),
            });
        }
    }
    let u = &resp["usage"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    Ok(Decision {
        answers,
        usage: Usage {
            input: n(&u["input_tokens"]),
            output: n(&u["output_tokens"]),
            cache_read: n(&u["input_tokens_details"]["cached_tokens"]),
            cache_write: n(&u["input_tokens_details"]["cache_write_tokens"]),
            reasoning: n(&u["output_tokens_details"]["reasoning_tokens"]),
        },
        meta: ctx.meta(),
    })
}
