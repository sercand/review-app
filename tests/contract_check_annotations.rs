//! Contract test for `GithubClient::check_run_annotations`: the exact
//! annotations path, `per_page`, two pages joined through the `Link` header,
//! and the parsed annotation fields.

mod common;

use common::client;
use gh_review_desktop::model::RepoRef;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Page one: one failure annotation, with a next link.
const PAGE_ONE: &str = r#"[
  {"path":"src/a.rs","start_line":12,"end_line":12,"annotation_level":"failure",
   "title":"clippy","message":"used unwrap()"}
]"#;

/// Page two: a warning with a range and no title (`null` on the wire).
const PAGE_TWO: &str = r#"[
  {"path":"src/b.rs","start_line":3,"end_line":5,"annotation_level":"warning",
   "title":null,"message":"unused variable: x"}
]"#;

#[tokio::test]
async fn pages_check_run_annotations() {
    let server = MockServer::start().await;
    let annotations = "/repos/o/r/check-runs/42/annotations";

    Mock::given(method("GET"))
        .and(path(annotations))
        .and(query_param("per_page", "100"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(PAGE_ONE)
                .insert_header(
                    "link",
                    format!(
                        "<{}{annotations}?per_page=100&page=2>; rel=\"next\"",
                        server.uri()
                    ),
                ),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(annotations))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_TWO))
        .mount(&server)
        .await;

    let got = client(&server.uri())
        .check_run_annotations(&RepoRef::new("o", "r"), 42)
        .await
        .unwrap();

    assert_eq!(got.len(), 2, "both pages are joined");
    assert_eq!(got[0].path, "src/a.rs");
    assert_eq!((got[0].start_line, got[0].end_line), (12, 12));
    assert_eq!(got[0].level, "failure");
    assert_eq!(got[0].title.as_deref(), Some("clippy"));
    assert_eq!(got[0].message, "used unwrap()");

    assert_eq!(got[1].path, "src/b.rs");
    assert_eq!((got[1].start_line, got[1].end_line), (3, 5));
    assert_eq!(got[1].level, "warning");
    assert_eq!(got[1].title, None, "a null title stays None");
    assert_eq!(got[1].message, "unused variable: x");
}
