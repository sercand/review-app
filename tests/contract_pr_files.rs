//! Contract test for `Command::LoadPrFiles`: one request per page, one
//! `Event::PrFilesPage` per page, `done` on the last, every patch parsed.

mod common;

use std::sync::Arc;
use std::time::Duration;

use gh_review_desktop::auth::store::MemoryStore;
use gh_review_desktop::github::files::PatchState;
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::runtime::{Command, Event, ExecCtx, SessionId, event_channel, execute};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::client;

/// Page one: a text file with a patch.
const PAGE_ONE: &str = r#"[
  {"sha":"b1","filename":"a.rs","status":"modified","additions":1,"deletions":1,
   "changes":2,"patch":"@@ -1 +1 @@\n-a\n+b\n"}
]"#;

/// Page two: a binary file (no patch, no changes).
const PAGE_TWO: &str = r#"[
  {"sha":"b2","filename":"logo.png","status":"added","additions":0,"deletions":0,
   "changes":0}
]"#;

#[tokio::test]
async fn pages_pr_files_and_parses_each_patch() {
    let server = MockServer::start().await;
    let files = "/repos/o/r/pulls/7/files";

    Mock::given(method("GET"))
        .and(path(files))
        .and(query_param("per_page", "100"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(PAGE_ONE)
                .insert_header(
                    "link",
                    format!(
                        "<{}{files}?per_page=100&page=2>; rel=\"next\"",
                        server.uri()
                    ),
                ),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(files))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_TWO))
        .mount(&server)
        .await;

    let ctx = Arc::new(ExecCtx::new(Arc::new(MemoryStore::default())));
    ctx.install_client(client(&server.uri()));
    let (tx, rx) = event_channel();

    execute(
        ctx,
        Command::LoadPrFiles {
            session: SessionId(1),
            repo: RepoRef::new("o", "r"),
            number: 7,
        },
        tx,
    )
    .await;

    let Some(Event::PrFilesPage {
        session,
        files,
        done,
    }) = rx.recv_timeout(Duration::from_secs(2))
    else {
        panic!("expected a first files page");
    };
    assert_eq!(session, SessionId(1));
    assert!(!done, "the first page is followed by another");
    assert_eq!(files[0].file.path, "a.rs");
    let diff = files[0].diff.as_ref().expect("the patch was parsed");
    assert_eq!(diff.hunks[0].rows.len(), 2);

    let Some(Event::PrFilesPage { files, done, .. }) = rx.recv_timeout(Duration::from_secs(2))
    else {
        panic!("expected a second files page");
    };
    assert!(done, "the last page says so");
    assert_eq!(files[0].file.patch_state, PatchState::Binary);
    assert!(files[0].diff.is_none(), "a binary file has no diff");
}
