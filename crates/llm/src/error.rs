use crate::ApiType;

/// Everything that can go wrong in `review-llm`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid or inconsistent `[llm]` configuration.
    #[error("llm config: {0}")]
    Config(String),
    /// The role has no entry and its kind has no default.
    #[error("llm role `{role}` is not configured and there is no default for its kind")]
    RoleNotConfigured { role: String },
    /// The key resolver returned no key for a provider that is in use.
    #[error("no API key for llm provider `{provider}`")]
    MissingKey { provider: String },
    /// The method is not served by this client's API.
    #[error("`{op}` is not supported by the {api} API")]
    Unsupported { api: ApiType, op: &'static str },
    /// The request failed the checks before sending.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Non-success HTTP response. The body is truncated and has the key redacted.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    /// The request, or a stream waiting for its next event, timed out.
    #[error("request timed out")]
    Timeout,
    /// Connection or TLS failure.
    #[error("transport: {0}")]
    Transport(String),
    /// The client's resolver ([`crate::ClientConfig::resolver`]) refused the host: the
    /// caller's policy, not an outage, so it is neither retried nor a fallback.
    #[error("blocked host `{0}`")]
    BlockedHost(String),
    /// Unparseable wire JSON, SSE, or tool-input JSON.
    #[error("decode: {0}")]
    Decode(String),
    /// A decision reply has no answer for a question.
    #[error("decision reply has no answer for question `{name}`")]
    MissingAnswer { name: String },
    /// `Reply::single_tool_use` found a refusal, no call, or several calls.
    #[error("tool call: {0}")]
    ToolCall(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Whether the retry policy may retry this error.
    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Error::Http { status, .. } => *status == 429 || (500..=599).contains(status),
            Error::Timeout | Error::Transport(_) => true,
            _ => false,
        }
    }

    /// Whether a caller with alternative models (the backend's LLM gateway) should try the
    /// next one after this error: an outage of this provider, that is [`Error::Timeout`],
    /// [`Error::Transport`] or [`Error::Http`] with a 5xx status. A 429 is not eligible: it
    /// is the provider's quota, not an outage, and neither are request or decode errors.
    pub fn is_fallback_eligible(&self) -> bool {
        match self {
            Error::Http { status, .. } => (500..=599).contains(status),
            Error::Timeout | Error::Transport(_) => true,
            _ => false,
        }
    }

    pub(crate) fn from_reqwest(e: reqwest::Error) -> Self {
        if let Some(host) = crate::dns::blocked_host(&e) {
            Error::BlockedHost(host)
        } else if e.is_timeout() {
            Error::Timeout
        } else if e.is_decode() {
            Error::Decode(e.to_string())
        } else {
            // reqwest errors carry the URL, never headers, so no key can leak here.
            Error::Transport(e.to_string())
        }
    }
}

const MAX_BODY: usize = 2048;

/// Truncates an error body and removes the key from it.
pub(crate) fn redact_body(body: &str, key: &str) -> String {
    let mut s = if key.is_empty() {
        body.to_owned()
    } else {
        body.replace(key, "[redacted]")
    };
    if s.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_body_removes_key_and_truncates() {
        let body = format!("bad key sk-secret {}", "x".repeat(5000));
        let out = redact_body(&body, "sk-secret");
        assert!(!out.contains("sk-secret"));
        assert!(out.contains("[redacted]"));
        assert!(out.len() <= MAX_BODY + '…'.len_utf8());
    }

    #[test]
    fn retryable_statuses() {
        let http = |status| Error::Http {
            status,
            body: String::new(),
        };
        assert!(http(429).is_retryable());
        assert!(http(503).is_retryable());
        assert!(!http(400).is_retryable());
        assert!(Error::Timeout.is_retryable());
        assert!(!Error::Decode(String::new()).is_retryable());
    }

    #[test]
    fn fallback_eligibility() {
        let http = |status| Error::Http {
            status,
            body: String::new(),
        };
        // Outages: the next model may answer.
        assert!(Error::Timeout.is_fallback_eligible());
        assert!(Error::Transport("connection refused".into()).is_fallback_eligible());
        for status in [500, 502, 503, 504, 599] {
            assert!(http(status).is_fallback_eligible(), "{status}");
        }
        // The caller's or the provider's quota: another model would not help.
        for status in [400, 401, 403, 404, 429, 499, 600] {
            assert!(!http(status).is_fallback_eligible(), "{status}");
        }
        assert!(!Error::Decode(String::new()).is_fallback_eligible());
        assert!(!Error::InvalidRequest(String::new()).is_fallback_eligible());
        assert!(!Error::Config(String::new()).is_fallback_eligible());
    }
}
