//! JSON and SSE POSTs with retries, shared by the in-crate API clients.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eventsource_stream::Eventsource as _;
use futures::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::{
    Error, Result,
    error::redact_body,
    retry::{Failure, RetryPolicy, retry_after},
    trace::{RequestRec, ResponseRec, StreamRecorder, TraceCall, body_value, sse_event},
};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Http {
    client: reqwest::Client,
    base: String,
    headers: HeaderMap,
    key: SecretString,
    timeout: Duration,
    retry: RetryPolicy,
}

pub(crate) fn header_map(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::try_from(k.as_str())
            .map_err(|e| Error::Config(format!("header name `{k}`: {e}")))?;
        let mut value = HeaderValue::try_from(v.as_str())
            .map_err(|e| Error::Config(format!("header `{k}`: {e}")))?;
        value.set_sensitive(true);
        map.insert(name, value);
    }
    Ok(map)
}

impl Http {
    /// `headers` must already hold the auth header.
    pub fn new(
        base: &str,
        headers: HeaderMap,
        key: SecretString,
        timeout: Duration,
        retry: RetryPolicy,
        resolver: Option<crate::dns::Resolver>,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if let Some(r) = resolver {
            builder = crate::dns::guarded(builder, r);
        }
        let client = builder.build().map_err(Error::from_reqwest)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            headers,
            key,
            timeout,
            retry,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// One attempt. Writes a trace line for every outcome except a successful stream, whose
    /// line is written by the [`StreamRecorder`] it returns.
    async fn send(
        &self,
        path: &str,
        body: &Value,
        total: Option<Duration>,
        trace: Option<&Arc<TraceCall>>,
    ) -> Result<Sent, Failure> {
        let mut builder = self
            .client
            .post(self.url(path))
            .headers(self.headers.clone())
            .json(body);
        if let Some(t) = total {
            builder = builder.timeout(t);
        }
        let request = builder.build().map_err(Error::from_reqwest)?;
        let started = Instant::now();
        let attempt = trace.map(|t| t.next_attempt()).unwrap_or(0);
        let record = trace.map(|t| RequestRec {
            method: request.method().to_string(),
            url: request.url().to_string(),
            headers: t.headers(request.headers()),
            body: body.clone(),
        });
        let write =
            |record: &Option<RequestRec>, response: Option<&ResponseRec>, error: Option<&Error>| {
                if let (Some(t), Some(rec)) = (trace, record) {
                    t.write(
                        attempt,
                        started,
                        rec,
                        response,
                        error.map(|e| e.to_string()).as_deref(),
                    );
                }
            };

        let resp = match self.client.execute(request).await {
            Ok(r) => r,
            Err(e) => {
                let e = Error::from_reqwest(e);
                write(&record, None, Some(&e));
                return Err(e.into());
            }
        };
        let status = resp.status();
        let headers = trace.map(|t| t.headers(resp.headers())).unwrap_or_default();
        let after = retry_after(resp.headers());

        if status.is_success() && total.is_none() {
            // A stream: its line is written when the stream ends.
            let recorder = match (trace, record) {
                (Some(t), Some(request)) => Some(StreamRecorder {
                    call: t.clone(),
                    attempt,
                    started,
                    request,
                    status: status.as_u16(),
                    headers,
                    events: Vec::new(),
                    error: None,
                }),
                _ => None,
            };
            return Ok(Sent::Stream(Box::new((resp, recorder))));
        }

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let e = Error::from_reqwest(e);
                write(&record, None, Some(&e));
                return Err(e.into());
            }
        };
        let response = ResponseRec {
            status: status.as_u16(),
            headers,
            body: body_value(&bytes),
        };
        if status.is_success() {
            write(&record, Some(&response), None);
            return Ok(Sent::Body(bytes.to_vec()));
        }
        let error = Error::Http {
            status: status.as_u16(),
            body: redact_body(&String::from_utf8_lossy(&bytes), self.key.expose_secret()),
        };
        write(&record, Some(&response), Some(&error));
        Err(Failure {
            error,
            retry_after: after,
        })
    }

    pub async fn post_json(
        &self,
        path: &str,
        body: &Value,
        trace: Option<&Arc<TraceCall>>,
    ) -> Result<Value> {
        self.retry
            .run(|| async {
                let Sent::Body(bytes) = self.send(path, body, Some(self.timeout), trace).await?
                else {
                    unreachable!("a request with a total timeout is never a stream")
                };
                serde_json::from_slice(&bytes)
                    .map_err(|e| Failure::from(Error::Decode(format!("response body: {e}"))))
            })
            .await
    }

    /// Opens an SSE stream; retried only until the response headers arrive. Each item is the
    /// JSON `data` of one event.
    pub async fn post_sse(
        &self,
        path: &str,
        body: &Value,
        trace: Option<&Arc<TraceCall>>,
    ) -> Result<impl Stream<Item = Result<Value>> + Send + Unpin + 'static> {
        let sent = self
            .retry
            .run(|| self.send(path, body, None, trace))
            .await?;
        let Sent::Stream(opened) = sent else {
            unreachable!("a request without a total timeout is a stream")
        };
        let (resp, recorder) = *opened;
        let recorder = Arc::new(Mutex::new(recorder));
        Ok(resp
            .bytes_stream()
            .eventsource()
            .filter_map(move |ev| {
                let recorder = recorder.clone();
                async move {
                    let mut rec = recorder.lock().ok();
                    let rec = rec.as_mut().and_then(|r| r.as_mut());
                    match ev {
                        Err(e) => {
                            let e = Error::Decode(format!("sse: {e}"));
                            if let Some(r) = rec {
                                r.error = Some(e.to_string());
                            }
                            Some(Err(e))
                        }
                        Ok(ev) => {
                            if let Some(r) = rec {
                                r.events.push(sse_event(&ev.event, &ev.data));
                            }
                            if ev.data.trim().is_empty() || ev.data == "[DONE]" {
                                return None;
                            }
                            Some(
                                serde_json::from_str(&ev.data)
                                    .map_err(|e| Error::Decode(format!("sse data: {e}"))),
                            )
                        }
                    }
                }
            })
            .boxed())
    }
}

/// What one attempt produced.
enum Sent {
    Body(Vec<u8>),
    Stream(Box<(reqwest::Response, Option<StreamRecorder>)>),
}
