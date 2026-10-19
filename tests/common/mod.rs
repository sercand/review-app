//! Shared helpers for the integration (contract) tests.
//!
//! Integration tests cannot use `src/test_support.rs` (it is `#[cfg(test)]` and
//! therefore absent from the published library), so their helpers live here.

use gh_review_desktop::github::client::{Endpoints, GithubClient};

/// A client pointed at a mock server, with a fixed token.
pub fn client(uri: &str) -> GithubClient {
    GithubClient::with_endpoints(
        Endpoints {
            web: uri.to_string(),
            rest: uri.to_string(),
            graphql: format!("{uri}/graphql"),
        },
        "tok",
    )
}
