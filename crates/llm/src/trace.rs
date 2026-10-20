//! The trace file: one JSONL line per HTTP exchange, with key-bearing headers redacted.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::header::HeaderMap;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};

use crate::{ApiType, Role};

/// An open trace file, shared by every client that traces to the same path.
struct Sink {
    file: Mutex<File>,
}

fn sinks() -> &'static Mutex<HashMap<PathBuf, Weak<Sink>>> {
    static SINKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Sink>>>> = OnceLock::new();
    SINKS.get_or_init(Default::default)
}

fn open_sink(path: &Path) -> Option<Arc<Sink>> {
    let mut map = sinks().lock().ok()?;
    if let Some(sink) = map.get(path).and_then(Weak::upgrade) {
        return Some(sink);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let sink = Arc::new(Sink {
        file: Mutex::new(file),
    });
    map.insert(path.to_owned(), Arc::downgrade(&sink));
    Some(sink)
}

/// A client's trace target. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Trace {
    /// `None` when the file could not be opened; tracing is then a no-op.
    sink: Option<Arc<Sink>>,
    key: SecretString,
}

impl Trace {
    pub fn new(path: &Path, key: SecretString) -> Self {
        Self {
            sink: open_sink(path),
            key,
        }
    }
}

/// One call: shared by its attempts.
pub(crate) struct TraceCall {
    trace: Trace,
    call_id: String,
    api: ApiType,
    provider: String,
    model: String,
    role: Option<Role>,
    from_default: bool,
    stream: bool,
    attempts: AtomicU32,
    /// A stream opened inside an HTTP service, waiting for its events (OpenAI path).
    pub pending: Mutex<Option<StreamRecorder>>,
}

pub(crate) struct RequestRec {
    pub method: String,
    pub url: String,
    pub headers: Value,
    pub body: Value,
}

pub(crate) struct ResponseRec {
    pub status: u16,
    pub headers: Value,
    pub body: Value,
}

fn call_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("c-{nanos:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

impl TraceCall {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        trace: &Trace,
        api: ApiType,
        provider: &str,
        model: &str,
        role: Option<Role>,
        from_default: bool,
        stream: bool,
    ) -> Self {
        Self {
            trace: trace.clone(),
            call_id: call_id(),
            api,
            provider: provider.to_owned(),
            model: model.to_owned(),
            role,
            from_default,
            stream,
            attempts: AtomicU32::new(0),
            pending: Mutex::new(None),
        }
    }

    /// Error text with the key removed.
    pub fn redact(&self, text: &str) -> String {
        crate::error::redact_body(text, self.trace.key.expose_secret())
    }

    /// Starts the next attempt and returns its number (1-based).
    pub fn next_attempt(&self) -> u32 {
        self.attempts.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Headers as a JSON object; auth headers and any value holding the key are redacted.
    pub fn headers(&self, headers: &HeaderMap) -> Value {
        let key = self.trace.key.expose_secret();
        let mut map = Map::new();
        for (name, value) in headers {
            let text = String::from_utf8_lossy(value.as_bytes()).into_owned();
            let secret = matches!(name.as_str(), "authorization" | "x-api-key" | "api-key")
                || (!key.is_empty() && text.contains(key));
            let text = if secret {
                "[redacted]".to_owned()
            } else {
                text
            };
            match map.get_mut(name.as_str()) {
                Some(Value::String(prev)) => {
                    prev.push_str(", ");
                    prev.push_str(&text);
                }
                _ => {
                    map.insert(name.as_str().to_owned(), Value::String(text));
                }
            }
        }
        Value::Object(map)
    }

    /// Appends one line. Never fails the call: write errors are ignored.
    pub fn write(
        &self,
        attempt: u32,
        started: Instant,
        request: &RequestRec,
        response: Option<&ResponseRec>,
        error: Option<&str>,
    ) {
        let Some(sink) = &self.trace.sink else {
            return;
        };
        let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let line = json!({
            "time": rfc3339(SystemTime::now()),
            "call_id": self.call_id,
            "attempt": attempt,
            "api": self.api,
            "provider": self.provider,
            "model": self.model,
            "role": self.role,
            "from_default": self.from_default,
            "stream": self.stream,
            "latency_ms": latency,
            "request": {
                "method": request.method,
                "url": request.url,
                "headers": request.headers,
                "body": request.body,
            },
            "response": response.map(|r| json!({
                "status": r.status,
                "headers": r.headers,
                "body": r.body,
            })),
            "error": error,
        });
        // Response bodies can echo the key (e.g. an auth error); never let it reach the file.
        let key = self.trace.key.expose_secret();
        let mut text = line.to_string();
        if !key.is_empty() {
            text = text.replace(key, "[redacted]");
        }
        text.push('\n');
        if let Ok(mut file) = sink.file.lock() {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// Parses a body as JSON, or keeps it as text.
pub(crate) fn body_value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// One SSE event as recorded in a stream line.
pub(crate) fn sse_event(event: &str, data: &str) -> Value {
    json!({ "event": event, "data": body_value(data.as_bytes()) })
}

/// Collects a stream's events and writes the line once, when the stream ends or is dropped.
pub(crate) struct StreamRecorder {
    pub call: Arc<TraceCall>,
    pub attempt: u32,
    pub started: Instant,
    pub request: RequestRec,
    pub status: u16,
    pub headers: Value,
    pub events: Vec<Value>,
    pub error: Option<String>,
}

impl Drop for StreamRecorder {
    fn drop(&mut self) {
        let response = ResponseRec {
            status: self.status,
            headers: std::mem::take(&mut self.headers),
            body: Value::Array(std::mem::take(&mut self.events)),
        };
        self.call.write(
            self.attempt,
            self.started,
            &self.request,
            Some(&response),
            self.error.as_deref(),
        );
    }
}

/// UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::header::HeaderValue;

    use super::*;

    #[test]
    fn rfc3339_formats_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_792_150_801_123);
        assert_eq!(rfc3339(t), "2026-10-16T11:40:01.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn headers_are_redacted() {
        let trace = Trace {
            sink: None,
            key: SecretString::from("sk-secret"),
        };
        let call = TraceCall::new(&trace, ApiType::Decisions, "p", "m", None, false, false);
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer sk-secret"),
        );
        h.insert("x-api-key", HeaderValue::from_static("other"));
        h.insert("x-custom", HeaderValue::from_static("has sk-secret inside"));
        h.insert("x-team", HeaderValue::from_static("qa"));
        let v = call.headers(&h);
        assert_eq!(v["authorization"], "[redacted]");
        assert_eq!(v["x-api-key"], "[redacted]");
        assert_eq!(v["x-custom"], "[redacted]");
        assert_eq!(v["x-team"], "qa");
        assert!(!v.to_string().contains("sk-secret"));
    }
}
