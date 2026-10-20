//! Response classification: turn an HTTP response into a typed [`GhError`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;

/// A classified GitHub API failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GhError {
    /// 401: the credential is missing, expired, or revoked.
    #[error("authentication required (HTTP 401)")]
    Unauthorized,
    /// The account is out of quota (primary) or being throttled (secondary).
    /// The Display names the reset instant when GitHub sent one (spec §13).
    #[error("rate limited by GitHub{}", rate_limit_suffix(*.reset_at))]
    RateLimited { reset_at: Option<SystemTime> },
    /// A 403 that asks the user to authorize the OAuth app for a SAML org.
    /// The Display names the authorization URL when the header carried one
    /// (spec §13).
    #[error("SSO authorization required{}", .url.as_ref().map(|url| format!(" — authorize at {url}")).unwrap_or_default())]
    SsoRequired { url: Option<String> },
    /// Any other 403, carrying GitHub's `message`.
    #[error("{0}")]
    Forbidden(String),
    /// 404.
    #[error("not found (HTTP 404)")]
    NotFound,
    /// 422, carrying GitHub's `message` plus any field errors.
    #[error("{0}")]
    Validation(String),
    /// A 5xx.
    #[error("GitHub server error (HTTP {0})")]
    Server(u16),
    /// A transport failure (DNS, TLS, timeout, connection reset).
    #[error("{0}")]
    Network(String),
    /// A GraphQL response whose `errors[]` was non-empty.
    #[error("GraphQL error: {}", .0.join("; "))]
    GraphQl(Vec<String>),
    /// A body or header that could not be parsed.
    #[error("could not decode response: {0}")]
    Decode(String),
}

impl GhError {
    /// Whether this error means the app must not attempt a write.
    ///
    /// Covers [`GhError::RateLimited`] (we would just burn quota) and
    /// [`GhError::Unauthorized`] (the token is gone; writes would fail anyway).
    pub fn is_write_blocking(&self) -> bool {
        matches!(self, GhError::RateLimited { .. } | GhError::Unauthorized)
    }

    /// Whether a read request may be retried after a backoff.
    pub fn is_retryable_read(&self) -> bool {
        matches!(self, GhError::Server(_) | GhError::Network(_))
    }
}

/// The parts of an HTTP response that drive classification.
#[derive(Default)]
pub struct ResponseMeta {
    /// HTTP status code.
    pub status: u16,
    /// `x-ratelimit-remaining`.
    pub ratelimit_remaining: Option<u32>,
    /// `x-ratelimit-reset`, as a Unix timestamp in seconds.
    pub ratelimit_reset: Option<u64>,
    /// `retry-after`, in seconds. GitHub sends this on secondary rate limits.
    pub retry_after: Option<u64>,
    /// `x-github-sso`, e.g. `required; url=https://github.com/orgs/acme/sso?...`.
    pub sso: Option<String>,
}

impl ResponseMeta {
    /// Extract the rate-limit and SSO headers from a response.
    pub fn from_headers(status: u16, headers: &HeaderMap) -> ResponseMeta {
        fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
            headers.get(name).and_then(|value| value.to_str().ok())
        }

        ResponseMeta {
            status,
            ratelimit_remaining: header(headers, "x-ratelimit-remaining")
                .and_then(|value| value.trim().parse().ok()),
            ratelimit_reset: header(headers, "x-ratelimit-reset")
                .and_then(|value| value.trim().parse().ok()),
            retry_after: header(headers, "retry-after").and_then(|value| value.trim().parse().ok()),
            sso: header(headers, "x-github-sso").map(str::to_owned),
        }
    }
}

/// Classify a response into the [`GhError`] the rest of the app reasons about.
///
/// `now` is injected so that `RateLimited::reset_at` is computed relative to the
/// caller's clock and stays testable.
pub fn classify(meta: &ResponseMeta, body: &str, now: SystemTime) -> GhError {
    match meta.status {
        401 => GhError::Unauthorized,
        403 | 429 => classify_throttled(meta, body, now),
        404 => GhError::NotFound,
        422 => GhError::Validation(validation_message(body)),
        status if (500..600).contains(&status) => GhError::Server(status),
        _ => GhError::Forbidden(message_or_body(body)),
    }
}

/// 403/429 share a shape: an explicit `retry-after` (secondary limit) wins, then
/// an exhausted quota (primary limit), then an SSO challenge, then a plain
/// permission error.
fn classify_throttled(meta: &ResponseMeta, body: &str, now: SystemTime) -> GhError {
    if let Some(retry_after) = meta.retry_after {
        return GhError::RateLimited {
            reset_at: Some(now + Duration::from_secs(retry_after)),
        };
    }

    if meta.ratelimit_remaining == Some(0) {
        return GhError::RateLimited {
            reset_at: meta
                .ratelimit_reset
                .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
        };
    }

    if let Some(sso) = &meta.sso {
        return GhError::SsoRequired {
            url: parse_sso_url(sso),
        };
    }

    GhError::Forbidden(message_or_body(body))
}

/// The rate-limit Display's countdown suffix: the seconds until GitHub's
/// reset, when one is known and still ahead of us (spec §13). Empty when
/// the reset is unknown or already past — the toast must not promise a
/// countdown to the past.
fn rate_limit_suffix(reset_at: Option<SystemTime>) -> String {
    let remaining = reset_at.and_then(|reset_at| reset_at.duration_since(SystemTime::now()).ok());
    match remaining {
        Some(remaining) => format!(" — resets in ~{}s", remaining.as_secs()),
        None => String::new(),
    }
}

/// Pull `url=` out of an `x-github-sso` header value.
fn parse_sso_url(value: &str) -> Option<String> {
    let rest = value.split_once("url=")?.1.trim();
    let rest = rest.trim_matches('"');
    let rest = rest.split(';').next().unwrap_or(rest).trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

/// GitHub's human-readable `message` for an error body, falling back to the raw
/// body when it is not JSON.
fn message_or_body(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(message) = value.get("message").and_then(|message| message.as_str()) {
            return message.to_owned();
        }
    body.trim().to_owned()
}

/// Format a 422 body as `"{message}: {error}; {error}"`.
///
/// Each `errors[]` entry is either a bare string or an object with a `message`.
fn validation_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.trim().to_owned();
    };

    let message = value
        .get("message")
        .and_then(|message| message.as_str())
        .unwrap_or("");

    let errors: Vec<String> = value
        .get("errors")
        .and_then(|errors| errors.as_array())
        .map(|errors| {
            errors
                .iter()
                .filter_map(|error| match error {
                    serde_json::Value::String(text) => Some(text.clone()),
                    other => other
                        .get("message")
                        .and_then(|message| message.as_str())
                        .map(str::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();

    match (message.is_empty(), errors.is_empty()) {
        (_, true) => message.to_owned(),
        (true, false) => errors.join("; "),
        (false, false) => format!("{}: {}", message, errors.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn m(status: u16) -> ResponseMeta {
        ResponseMeta {
            status,
            ..Default::default()
        }
    }

    #[test]
    fn classifies_responses() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(classify(&m(401), "", now), GhError::Unauthorized);

        // x-ratelimit-reset is on every response; with quota left a 403 is a permission error
        let perm = ResponseMeta {
            status: 403,
            ratelimit_remaining: Some(10),
            ratelimit_reset: Some(2_000),
            ..Default::default()
        };
        assert_eq!(
            classify(&perm, r#"{"message":"Resource not accessible"}"#, now),
            GhError::Forbidden("Resource not accessible".into())
        );

        let primary = ResponseMeta {
            status: 403,
            ratelimit_remaining: Some(0),
            ratelimit_reset: Some(2_000),
            ..Default::default()
        };
        assert_eq!(
            classify(&primary, "", now),
            GhError::RateLimited {
                reset_at: Some(UNIX_EPOCH + Duration::from_secs(2_000))
            }
        );

        let secondary = ResponseMeta {
            status: 429,
            retry_after: Some(60),
            ..Default::default()
        };
        assert_eq!(
            classify(&secondary, "", now),
            GhError::RateLimited {
                reset_at: Some(now + Duration::from_secs(60))
            }
        );

        let sso = ResponseMeta {
            status: 403,
            sso: Some(
                "required; url=https://github.com/orgs/acme/sso?authorization_request=x".into(),
            ),
            ..Default::default()
        };
        assert!(matches!(classify(&sso, "", now),
        GhError::SsoRequired { url: Some(u) } if u.starts_with("https://github.com/orgs/acme/sso")));

        assert_eq!(classify(&m(404), "", now), GhError::NotFound);
        assert_eq!(
            classify(
                &m(422),
                r#"{"message":"Validation Failed","errors":[{"message":"line must be part of the diff"}]}"#,
                now
            ),
            GhError::Validation("Validation Failed: line must be part of the diff".into())
        );
        assert_eq!(classify(&m(502), "", now), GhError::Server(502));

        assert!(GhError::Server(502).is_retryable_read());
        assert!(!GhError::Validation(String::new()).is_retryable_read());
        assert!(GhError::Unauthorized.is_write_blocking());
    }

    #[test]
    fn rate_limited_display_names_the_reset_countdown() {
        let soon = GhError::RateLimited {
            reset_at: Some(SystemTime::now() + Duration::from_secs(60)),
        };
        let text = soon.to_string();
        assert!(text.starts_with("rate limited by GitHub"), "{text}");
        // The countdown is computed against the wall clock, so accept the
        // one-second boundary rather than race it.
        assert!(
            text.contains("resets in ~59s") || text.contains("resets in ~60s"),
            "{text}"
        );

        // No reset instant, no countdown; a reset already past is no
        // countdown either.
        assert_eq!(
            GhError::RateLimited { reset_at: None }.to_string(),
            "rate limited by GitHub"
        );
        assert_eq!(
            GhError::RateLimited {
                reset_at: Some(SystemTime::now() - Duration::from_secs(5)),
            }
            .to_string(),
            "rate limited by GitHub"
        );
    }

    #[test]
    fn sso_display_names_the_authorization_url() {
        assert_eq!(
            GhError::SsoRequired {
                url: Some("https://github.com/orgs/acme/sso".into()),
            }
            .to_string(),
            "SSO authorization required — authorize at https://github.com/orgs/acme/sso"
        );
        assert_eq!(
            GhError::SsoRequired { url: None }.to_string(),
            "SSO authorization required"
        );
    }
}
