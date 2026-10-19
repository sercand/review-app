//! The GitHub REST client (and the base URLs the GraphQL client shares).
//!
//! Requests carry the three headers the API requires — `User-Agent`,
//! `X-GitHub-Api-Version`, and `Authorization: Bearer <token>` — and reads may
//! be retried on transport failures or 5xx responses. Writes never are.

use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::github::error::{classify, GhError, ResponseMeta};

/// The `X-GitHub-Api-Version` every request pins.
const API_VERSION: &str = "2022-11-28";
/// The `Accept` value for REST requests.
const REST_ACCEPT: &str = "application/vnd.github+json";
/// How long to wait before each of the (at most three) read retries.
const READ_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
];

/// The API base URLs for one GitHub host.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoints {
    /// The web/OAuth base, e.g. `https://github.com`.
    pub web: String,
    /// The REST base, e.g. `https://api.github.com`.
    pub rest: String,
    /// The GraphQL endpoint, e.g. `https://api.github.com/graphql`.
    pub graphql: String,
}

/// Derive the endpoints for `host` (spec §4.1).
pub fn endpoints_for(host: &str) -> Endpoints {
    if host == "github.com" {
        Endpoints {
            web: "https://github.com".to_owned(),
            rest: "https://api.github.com".to_owned(),
            graphql: "https://api.github.com/graphql".to_owned(),
        }
    } else {
        Endpoints {
            web: format!("https://{host}"),
            rest: format!("https://{host}/api/v3"),
            graphql: format!("https://{host}/api/graphql"),
        }
    }
}

/// The `User-Agent` every request sends.
pub(crate) fn user_agent() -> String {
    format!("gh-review-desktop/{}", crate::version())
}

/// The `rel="next"` URL from an RFC 5988 `Link` header, if there is one.
pub fn next_link(link_header: &str) -> Option<String> {
    link_header.split(',').find_map(|part| {
        let (url, rel) = part.split_once("; rel=")?;
        let rel = rel.trim().trim_matches('"');
        if rel != "next" {
            return None;
        }
        let url = url.trim().trim_start_matches('<').trim_end_matches('>');
        Some(url.to_owned())
    })
}

/// A GitHub client bound to one host (its endpoints) and one token.
#[derive(Clone)]
pub struct GithubClient {
    client: Client,
    endpoints: Endpoints,
}

impl GithubClient {
    /// A client for `host`, with the endpoints of spec §4.1.
    pub fn new(host: &str, token: &str) -> Self {
        Self::with_endpoints(endpoints_for(host), token)
    }

    /// A client with explicit `endpoints` (Enterprise hosts, and tests).
    pub fn with_endpoints(endpoints: Endpoints, token: &str) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, header_value(&user_agent()));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(ACCEPT, HeaderValue::from_static(REST_ACCEPT));
        headers.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .unwrap_or_default();

        GithubClient { client, endpoints }
    }

    /// The endpoints this client talks to.
    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// `GET` a REST path (or an absolute URL) as JSON.
    ///
    /// Returns the parsed body and the `rel="next"` URL from the response's
    /// `Link` header, when there is one. Retryable failures are retried up to
    /// three times.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
    ) -> Result<(T, Option<String>), GhError> {
        let url = self.resolve(path_or_url);
        let (headers, body) = self.get_with_retry(&url).await?;
        let value = serde_json::from_str(&body).map_err(decode)?;
        let next = header(&headers, "link").and_then(next_link);
        Ok((value, next))
    }

    /// `GET` every page of a paginated list endpoint, following `Link` `next`.
    pub async fn get_all_pages<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>, GhError> {
        let mut all = Vec::new();
        let mut next = Some(self.resolve(path));
        while let Some(url) = next {
            let (page, link) = self.get_json::<Vec<T>>(&url).await?;
            all.extend(page);
            next = link;
        }
        Ok(all)
    }

    /// Send a mutating request (POST/PATCH/DELETE) **without retrying**.
    ///
    /// A 2xx response is returned untouched so the caller can read its body; any
    /// other status is classified into a [`GhError`].
    #[allow(dead_code)] // first used by later tasks' write paths
    pub(crate) async fn send_mutation(&self, request: RequestBuilder) -> Result<Response, GhError> {
        let response = request.send().await.map_err(network)?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let meta = ResponseMeta::from_headers(status, response.headers());
        let body = response.text().await.unwrap_or_default();
        Err(classify(&meta, &body, SystemTime::now()))
    }

    /// `GET /user`, plus the granted scopes from `X-OAuth-Scopes`.
    pub async fn me(&self) -> Result<Me, GhError> {
        let url = self.resolve("/user");
        let (headers, body) = self.get_with_retry(&url).await?;
        let user = serde_json::from_str(&body).map_err(decode)?;
        let scopes = header(&headers, "x-oauth-scopes")
            .map(split_scopes)
            .unwrap_or_default();
        Ok(Me { user, scopes })
    }

    /// Turn a REST path into an absolute URL; absolute URLs pass through.
    fn resolve(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_owned()
        } else {
            format!("{}{}", self.endpoints.rest, path_or_url)
        }
    }

    /// One `GET`, returning its headers and body on success.
    async fn get_once(&self, url: &str) -> Result<(HeaderMap, String), GhError> {
        let response = self.client.get(url).send().await.map_err(network)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.map_err(network)?;

        if (200..300).contains(&status) {
            return Ok((headers, body));
        }

        let meta = ResponseMeta::from_headers(status, &headers);
        Err(classify(&meta, &body, SystemTime::now()))
    }

    /// `get_once` with the read-retry policy applied.
    async fn get_with_retry(&self, url: &str) -> Result<(HeaderMap, String), GhError> {
        let mut attempt = 0;
        loop {
            match self.get_once(url).await {
                Ok(result) => return Ok(result),
                Err(error) if error.is_retryable_read() && attempt < READ_BACKOFF.len() => {
                    tokio::time::sleep(READ_BACKOFF[attempt]).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// The subset of `GET /user` that the app caches in settings.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct GitHubUser {
    /// The login name.
    pub login: String,
    /// The display name, when GitHub reports one.
    pub name: Option<String>,
    /// The avatar image URL, when GitHub reports one.
    pub avatar_url: Option<String>,
}

/// `GET /user` plus the token's granted scopes.
#[derive(Clone, Debug, PartialEq)]
pub struct Me {
    /// The authenticated user.
    pub user: GitHubUser,
    /// The granted scopes, from `X-OAuth-Scopes`.
    pub scopes: Vec<String>,
}

/// Split an `X-OAuth-Scopes` value on commas, trimming each scope.
fn split_scopes(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A header value as a string, when it is valid UTF-8.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A header value that must be constructible (the token and user agent).
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).expect("HTTP header value must be valid")
}

/// Map a transport failure to [`GhError::Network`].
fn network(error: reqwest::Error) -> GhError {
    GhError::Network(error.to_string())
}

/// Map a JSON parse failure to [`GhError::Decode`].
fn decode(error: serde_json::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_endpoints_per_host() {
        assert_eq!(
            endpoints_for("github.com"),
            Endpoints {
                web: "https://github.com".into(),
                rest: "https://api.github.com".into(),
                graphql: "https://api.github.com/graphql".into()
            }
        );
        assert_eq!(
            endpoints_for("ghe.acme.io"),
            Endpoints {
                web: "https://ghe.acme.io".into(),
                rest: "https://ghe.acme.io/api/v3".into(),
                graphql: "https://ghe.acme.io/api/graphql".into()
            }
        );
    }

    #[test]
    fn parses_next_link() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(
            next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#),
            None
        );
    }
}
