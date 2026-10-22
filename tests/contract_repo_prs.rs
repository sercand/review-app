//! Contract test for the Local tab's per-repo PR search (AI settings and
//! local repos Task 8): the worker sends the search with the `repo:`
//! qualifier the reducer composed and answers with the parsed page.

#![cfg(unix)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use gh_review_desktop::auth::store::MemoryStore;
use gh_review_desktop::runtime::{Command, Event, ExecCtx, event_channel, execute};
use gh_review_desktop::state::local_repos::{LocalReposMsg, LocalReposState, reduce};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The reducer's command reaches GitHub as one GraphQL `search` whose query
/// carries `repo:owner/name` derived from the key, and the reply echoes the
/// generation and lowercased key.
#[tokio::test]
async fn repo_pr_search_sends_the_repo_qualifier() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({
            "variables": { "q": "is:pr is:open repo:acme/widgets" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "search": {
                "issueCount": 1,
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{
                    "number": 7,
                    "title": "Fix the widget",
                    "isDraft": false,
                    "updatedAt": "2026-10-22T00:00:00Z",
                    "createdAt": "2026-10-21T00:00:00Z",
                    "additions": 3,
                    "deletions": 1,
                    "reviewDecision": "REVIEW_REQUIRED",
                    "totalCommentsCount": 0,
                    "url": "https://github.com/acme/widgets/pull/7",
                    "author": { "login": "mona" },
                    "repository": { "nameWithOwner": "acme/widgets" },
                    "commits": { "nodes": [] }
                }]
            }}
        })))
        .expect(1)
        .mount(&server)
        .await;

    // The command is the reducer's own, so this pins the real
    // reducer -> worker contract, mixed-case key and all.
    let mut state = LocalReposState::default();
    let mut cmds = reduce(
        &mut state,
        LocalReposMsg::Refresh(vec!["GitHub.com/Acme/Widgets".into()]),
    );
    assert_eq!(cmds.len(), 1);
    let cmd = cmds.pop().expect("the refresh issued one search");
    assert!(
        matches!(&cmd, Command::SearchRepoPrs { query, .. }
            if query == "is:pr is:open repo:acme/widgets"),
        "the reducer composes the repo qualifier: {cmd:?}"
    );

    let ctx = Arc::new(ExecCtx::new(Arc::new(MemoryStore::default())));
    ctx.install_client(common::client(&server.uri()));
    let (tx, rx) = event_channel();
    execute(ctx, cmd, tx).await;

    let Some(Event::RepoPrsLoaded {
        generation,
        repo,
        result,
    }) = rx.recv_timeout(Duration::from_secs(10))
    else {
        panic!("expected the repo search's answer");
    };
    assert_eq!(generation, state.generation, "the generation is echoed");
    assert_eq!(repo, "github.com/acme/widgets", "the key is echoed");
    let page = result.expect("the search succeeded");
    assert_eq!(page.issue_count, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].number, 7);
    assert_eq!(page.items[0].repo.to_string(), "acme/widgets");
}
