use std::{future::Future, time::Duration};

use crate::Error;

/// Retries 429, 5xx, timeouts and transport errors with exponential backoff and full jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base: Duration::from_millis(500),
            max: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// No retries; for tests and callers that retry themselves.
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }
}

/// A failed attempt, with the server's `retry-after` when it sent one.
#[derive(Debug)]
pub(crate) struct Failure {
    pub error: Error,
    pub retry_after: Option<Duration>,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

impl RetryPolicy {
    fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(d) = retry_after {
            return d.min(self.max);
        }
        let cap = self
            .base
            .saturating_mul(2u32.saturating_pow(attempt))
            .min(self.max);
        let ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(rand::random_range(0..=ms))
    }

    pub(crate) async fn run<T, F, Fut>(&self, mut attempt: F) -> Result<T, Error>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let mut n = 0;
        loop {
            match attempt().await {
                Ok(v) => return Ok(v),
                Err(f) if n < self.max_retries && f.error.is_retryable() => {
                    tokio::time::sleep(self.delay(n, f.retry_after)).await;
                    n += 1;
                }
                Err(f) => return Err(f.error),
            }
        }
    }
}

/// Parses a `retry-after` header given in seconds.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn fast() -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            base: Duration::from_millis(1),
            max: Duration::from_millis(2),
        }
    }

    #[tokio::test]
    async fn retries_retryable_then_gives_up() {
        let calls = AtomicU32::new(0);
        let r: Result<(), _> = fast()
            .run(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Failure::from(Error::Http {
                    status: 503,
                    body: String::new(),
                }))
            })
            .await;
        assert!(matches!(r, Err(Error::Http { status: 503, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        let calls = AtomicU32::new(0);
        let r: Result<(), _> = fast()
            .run(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Failure::from(Error::Http {
                    status: 400,
                    body: String::new(),
                }))
            })
            .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn succeeds_after_retry() {
        let calls = AtomicU32::new(0);
        let r = fast()
            .run(|| async {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Failure::from(Error::Timeout))
                } else {
                    Ok(7)
                }
            })
            .await;
        assert_eq!(r.unwrap(), 7);
    }

    #[test]
    fn retry_after_is_capped() {
        let p = fast();
        assert_eq!(p.delay(0, Some(Duration::from_secs(30))), p.max);
    }
}
