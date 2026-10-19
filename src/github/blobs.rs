//! Raw file contents at a commit: `GET /repos/{o}/{r}/contents/{path}?ref={sha}`
//! with `Accept: application/vnd.github.raw` (Task 45).
//!
//! The contents path is a *path*, not a query value: each `/`-separated
//! segment is percent-encoded (spaces, `#`, `?`, `%`, non-ASCII) while the
//! `/` separators themselves stay literal, so `src/new name#1.rs` reaches the
//! API as `src/new%20name%231.rs`.

use std::time::SystemTime;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{HeaderValue, ACCEPT};

use crate::github::client::{GithubClient, READ_BACKOFF};
use crate::github::error::{classify, GhError, ResponseMeta};
use crate::model::RepoRef;

/// The `Accept` value that asks for the raw blob instead of the JSON metadata.
const RAW_ACCEPT: &str = "application/vnd.github.raw";

/// Everything outside RFC 3986's unreserved set, minus the `-._~` that
/// `NON_ALPHANUMERIC` also encodes; `/` separators are kept by splitting
/// around them rather than by this set.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode each `/`-separated segment of `path`, keeping the `/`s.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

impl GithubClient {
    /// `GET /repos/{o}/{r}/contents/{encode_path(path)}?ref={sha}` as raw text.
    ///
    /// A read: retried under the same policy as [`Self::get_json`].
    pub async fn file_content(
        &self,
        repo: &RepoRef,
        path: &str,
        sha: &str,
    ) -> Result<String, GhError> {
        let url = format!(
            "{}/repos/{}/{}/contents/{}?ref={}",
            self.endpoints().rest,
            repo.owner,
            repo.name,
            encode_path(path),
            sha
        );

        let mut attempt = 0;
        loop {
            match self.get_raw_once(&url).await {
                Ok(body) => return Ok(body),
                Err(error) if error.is_retryable_read() && attempt < READ_BACKOFF.len() => {
                    tokio::time::sleep(READ_BACKOFF[attempt]).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// One raw `GET`, classified like [`GithubClient`] classifies its JSON reads.
    async fn get_raw_once(&self, url: &str) -> Result<String, GhError> {
        let response = self
            .http()
            .get(url)
            .header(ACCEPT, HeaderValue::from_static(RAW_ACCEPT))
            .send()
            .await
            .map_err(network)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.map_err(network)?;

        if (200..300).contains(&status) {
            return Ok(body);
        }

        let meta = ResponseMeta::from_headers(status, &headers);
        Err(classify(&meta, &body, SystemTime::now()))
    }
}

/// Map a transport failure to [`GhError::Network`].
fn network(error: reqwest::Error) -> GhError {
    GhError::Network(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_each_segment() {
        assert_eq!(encode_path("src/new name#1.rs"), "src/new%20name%231.rs");
        assert_eq!(encode_path("docs/ünïcode.md"), "docs/%C3%BCn%C3%AFcode.md");
    }
}
