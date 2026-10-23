//! The HTTP driver for the OAuth device flow.
//!
//! [`crate::auth::device_flow`] holds the pure state machine; this module is the
//! only part that talks to GitHub. Both device-flow endpoints are unauthenticated
//! (there is no token yet), so no `Authorization` header is ever sent here.

use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

use crate::auth::device_flow::{self, AuthState};
use crate::github::client::user_agent;

pub use crate::auth::device_flow::{AuthFailure, DeviceCode, PollOutcome};

/// The `grant_type` the device-flow token endpoint requires.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The `X-GitHub-Api-Version` every request pins.
const API_VERSION: &str = "2022-11-28";
/// GitHub's default poll interval, per RFC 8628, when the server omits one.
const DEFAULT_INTERVAL_SECS: u64 = 5;

/// Drives the two device-flow HTTP calls against one web/OAuth base.
#[derive(Clone)]
pub struct HttpDeviceFlow {
    web_base: String,
    client: reqwest::Client,
}

impl HttpDeviceFlow {
    /// A flow driver against `web_base`, e.g. `https://github.com`.
    pub fn new(web_base: &str) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, header_value(&user_agent()));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap_or_default();

        HttpDeviceFlow {
            web_base: web_base.trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// `POST {web}/login/device/code` to start a flow.
    pub async fn request_code(
        &self,
        client_id: &str,
        scope: &str,
    ) -> Result<DeviceCode, AuthFailure> {
        let response = self
            .client
            .post(format!("{}/login/device/code", self.web_base))
            .form(&[("client_id", client_id), ("scope", scope)])
            .send()
            .await
            .map_err(http)?;
        let body = body_of(response).await?;
        let parsed: DeviceCodeResponse = serde_json::from_str(&body).map_err(decode)?;

        Ok(DeviceCode {
            device_code: parsed.device_code,
            user_code: parsed.user_code,
            verification_uri: parsed.verification_uri,
            interval: Duration::from_secs(parsed.interval),
            expires_in: Duration::from_secs(parsed.expires_in),
        })
    }

    /// `POST {web}/login/oauth/access_token` for one poll.
    pub async fn poll_token(
        &self,
        client_id: &str,
        device_code: &str,
    ) -> Result<PollOutcome, AuthFailure> {
        let response = self
            .client
            .post(format!("{}/login/oauth/access_token", self.web_base))
            .form(&[
                ("client_id", client_id),
                ("device_code", device_code),
                ("grant_type", DEVICE_GRANT),
            ])
            .send()
            .await
            .map_err(http)?;
        let body = body_of(response).await?;
        let parsed: TokenResponse = serde_json::from_str(&body).map_err(decode)?;

        if let Some(token) = parsed.access_token {
            return Ok(PollOutcome::Token(token));
        }

        if let Some(error) = parsed.error {
            return Ok(match error.as_str() {
                "authorization_pending" => PollOutcome::Pending,
                "slow_down" => PollOutcome::SlowDown {
                    interval: parsed.interval.map(Duration::from_secs),
                },
                "expired_token" => PollOutcome::Expired,
                "access_denied" => PollOutcome::Denied,
                _ => PollOutcome::Fatal(error),
            });
        }

        Err(AuthFailure::Http(
            "token response had neither access_token nor error".to_owned(),
        ))
    }
}

/// Request a code, show it via `on_code`, then poll until the flow ends.
///
/// Returns the access token, or the failure that stopped the flow. `expires_in`
/// bounds the whole loop, so a server that never answers still terminates.
pub async fn run_device_flow(
    flow: &HttpDeviceFlow,
    client_id: &str,
    scope: &str,
    on_code: impl FnOnce(&DeviceCode),
) -> Result<String, AuthFailure> {
    let code = flow.request_code(client_id, scope).await?;
    on_code(&code);

    let deadline = tokio::time::Instant::now() + code.expires_in;
    let mut state = device_flow::start(code);

    loop {
        let (interval, device_code) = match &state {
            AuthState::Polling { code, interval } => (*interval, code.device_code.clone()),
            AuthState::Authenticated(token) => return Ok(token.clone()),
            AuthState::Failed(failure) => return Err(failure.clone()),
            AuthState::Idle => {
                return Err(AuthFailure::Http("device flow never started".to_owned()));
            }
        };

        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(AuthFailure::Expired);
        }

        let outcome = flow.poll_token(client_id, &device_code).await?;
        state = device_flow::advance(state, outcome);
    }
}

/// The `POST /login/device/code` response.
#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

/// The `POST /login/oauth/access_token` response: either a token or an error.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

/// RFC 8628's default interval, used when the server omits `interval`.
fn default_interval() -> u64 {
    DEFAULT_INTERVAL_SECS
}

/// The response body, or a classified HTTP failure.
async fn body_of(response: reqwest::Response) -> Result<String, AuthFailure> {
    let status = response.status();
    let body = response.text().await.map_err(http)?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(AuthFailure::Http(format!("HTTP {status}: {body}")))
    }
}

/// A header value that must be constructible.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("HTTP header value must be valid")
}

/// Map a transport failure to [`AuthFailure::Http`].
fn http(error: reqwest::Error) -> AuthFailure {
    AuthFailure::Http(error.to_string())
}

/// Map a JSON parse failure to [`AuthFailure::Http`].
fn decode(error: serde_json::Error) -> AuthFailure {
    AuthFailure::Http(error.to_string())
}
