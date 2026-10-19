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

    /// `GET /repos/{o}/{r}/contents/{encode_path(path)}?ref={sha}` as raw
    /// bytes, capped at `max` (Task 49).
    ///
    /// `Ok(None)` means the blob is over the cap: `Content-Length` is checked
    /// before any buffering, and the streamed body is abandoned the moment it
    /// exceeds `max`, so an over-large blob is never held in memory. A read:
    /// retried under the same policy as [`Self::file_content`].
    pub async fn file_bytes(
        &self,
        repo: &RepoRef,
        path: &str,
        sha: &str,
        max: u64,
    ) -> Result<Option<Vec<u8>>, GhError> {
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
            match self.get_bytes_once(&url, max).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) if error.is_retryable_read() && attempt < READ_BACKOFF.len() => {
                    tokio::time::sleep(READ_BACKOFF[attempt]).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// One raw byte `GET` with the cap applied, classified like
    /// [`GithubClient`] classifies its JSON reads.
    async fn get_bytes_once(&self, url: &str, max: u64) -> Result<Option<Vec<u8>>, GhError> {
        let response = self
            .http()
            .get(url)
            .header(ACCEPT, HeaderValue::from_static(RAW_ACCEPT))
            .send()
            .await
            .map_err(network)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();

        if !(200..300).contains(&status) {
            let body = response.text().await.map_err(network)?;
            let meta = ResponseMeta::from_headers(status, &headers);
            return Err(classify(&meta, &body, SystemTime::now()));
        }

        // The advertised size settles it without reading a byte of an
        // over-cap body.
        if response.content_length().is_some_and(|len| len > max) {
            return Ok(None);
        }
        // Chunked bodies advertise nothing, so read in chunks and bail as
        // soon as the accumulated size passes the cap.
        let mut body: Vec<u8> = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            if body.len() as u64 + chunk.len() as u64 > max {
                return Ok(None);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    }

    /// `GET /repos/{o}/{r}/compare/{base}...{head}` → `merge_base_commit.sha`.
    ///
    /// The merge base is where a locally computed diff reads the base blob:
    /// the common ancestor of the PR's base and head, so the diff matches
    /// what GitHub itself would show (Task 46).
    pub async fn merge_base(
        &self,
        repo: &RepoRef,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<String, GhError> {
        #[derive(serde::Deserialize)]
        struct Compare {
            merge_base_commit: MergeBaseCommit,
        }
        #[derive(serde::Deserialize)]
        struct MergeBaseCommit {
            sha: String,
        }

        let path = format!(
            "/repos/{}/{}/compare/{}...{}",
            repo.owner, repo.name, base_sha, head_sha
        );
        let (compare, _link) = self.get_json::<Compare>(&path).await?;
        Ok(compare.merge_base_commit.sha)
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
