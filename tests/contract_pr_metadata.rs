//! Contract test for `Command::LoadPrMetadata`: one request to the pull
//! endpoint, one `Event::PrMetadataLoaded`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use gh_review_desktop::auth::store::MemoryStore;
use gh_review_desktop::model::RepoRef;
use gh_review_desktop::runtime::{event_channel, execute, Command, Event, ExecCtx, SessionId};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::client;

/// A PR with a null body, one requested user, and one requested team.
const BODY: &str = r#"{
  "node_id": "PR_kw1",
  "number": 7,
  "title": "Add the thing",
  "body": null,
  "user": {"login": "octocat"},
  "state": "open",
  "draft": true,
  "base": {"ref": "main", "sha": "sha_base"},
  "head": {"ref": "feature", "sha": "sha_head"},
  "additions": 12,
  "deletions": 4,
  "changed_files": 3,
  "requested_reviewers": [{"login": "alice"}],
  "requested_teams": [{"slug": "core"}],
  "labels": [{"name": "bug"}],
  "milestone": null,
  "html_url": "https://github.com/o/r/pull/7"
}"#;

#[tokio::test]
async fn loads_pr_metadata_from_the_pull_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = Arc::new(ExecCtx::new(Arc::new(MemoryStore::default())));
    ctx.install_client(client(&server.uri()));
    let (tx, rx) = event_channel();

    execute(
        ctx,
        Command::LoadPrMetadata {
            session: SessionId(1),
            repo: RepoRef::new("o", "r"),
            number: 7,
        },
        tx,
    )
    .await;

    let Some(Event::PrMetadataLoaded { session, meta }) = rx.recv_timeout(Duration::from_secs(2))
    else {
        panic!("expected loaded metadata");
    };
    assert_eq!(session, SessionId(1));
    assert_eq!(
        (meta.head_sha.as_str(), meta.base_sha.as_str()),
        ("sha_head", "sha_base")
    );
    assert_eq!(
        meta.requested_reviewers,
        vec!["alice".to_string(), "team:core".to_string()]
    );
    assert_eq!(meta.body, "");
    assert!(meta.draft);
}
