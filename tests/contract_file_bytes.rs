//! Contract test for `GithubClient::file_bytes` (Task 49): the contents
//! endpoint with the raw accept header, and the size cap that keeps an
//! over-large blob from ever being buffered.

mod common;

use gh_review_desktop::model::RepoRef;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::client;

#[tokio::test]
async fn fetches_raw_bytes_with_encoded_path_and_respects_the_cap() {
    let server = MockServer::start().await;
    let png = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    Mock::given(method("GET"))
        .and(path("/repos/o/r/contents/assets/my%20logo.png"))
        .and(query_param("ref", "sha1"))
        .and(header("accept", "application/vnd.github.raw"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .mount(&server)
        .await;
    let c = client(&server.uri());
    let r = RepoRef::new("o", "r");
    assert_eq!(
        c.file_bytes(&r, "assets/my logo.png", "sha1", 1024)
            .await
            .unwrap(),
        Some(png)
    );
    assert_eq!(
        c.file_bytes(&r, "assets/my logo.png", "sha1", 4)
            .await
            .unwrap(),
        None
    );
}
