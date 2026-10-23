//! Contract test for `GithubClient::review_threads`: the thread list follows
//! the `after` cursor, and a thread whose comments were truncated gets its
//! remaining comment pages fetched and appended.

mod common;

use common::client;
use gh_review_desktop::model::RepoRef;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One complete review thread as the mock server returns it.
fn thread(id: &str, database_id: u64) -> Value {
    json!({
        "id": id,
        "isResolved": false,
        "isOutdated": false,
        "path": "src/a.rs",
        "line": 12,
        "startLine": null,
        "diffSide": "RIGHT",
        "subjectType": "LINE",
        "comments": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [{
                "databaseId": database_id,
                "author": { "login": "mona", "avatarUrl": null },
                "body": "a comment",
                "createdAt": "2026-10-18T09:00:00Z"
            }]
        }
    })
}

/// Wrap a `reviewThreads` connection in the `THREADS_QUERY` envelope.
fn threads_page(has_next: bool, end_cursor: Value, nodes: Vec<Value>) -> Value {
    json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
        "pageInfo": { "hasNextPage": has_next, "endCursor": end_cursor },
        "nodes": nodes
    }}}}})
}

#[tokio::test]
async fn review_threads_follows_the_after_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": { "after": null } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads_page(
            true,
            json!("c1"),
            vec![thread("PRRT_1", 1)],
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": { "after": "c1" } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads_page(
            false,
            Value::Null,
            vec![thread("PRRT_2", 2)],
        )))
        .mount(&server)
        .await;

    let threads = client(&server.uri())
        .review_threads(&RepoRef::new("o", "r"), 7)
        .await
        .unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].id, "PRRT_1");
    assert_eq!(threads[1].id, "PRRT_2");
}

#[tokio::test]
async fn truncated_thread_comments_are_fetched_and_appended() {
    let server = MockServer::start().await;

    // One thread page; its comments connection says there is more.
    let mut truncated = thread("PRRT_1", 1);
    truncated["comments"]["pageInfo"] = json!({ "hasNextPage": true, "endCursor": "cc1" });
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({ "variables": { "after": null } })))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads_page(
            false,
            Value::Null,
            vec![truncated],
        )))
        .mount(&server)
        .await;

    // The comments page, fetched through `node(id:)`.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            json!({ "variables": { "id": "PRRT_1" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "comments": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{
                    "databaseId": 2,
                    "author": { "login": "octocat", "avatarUrl": null },
                    "body": "the reply",
                    "createdAt": "2026-10-18T09:05:00Z"
                }]
            }}}
        })))
        .mount(&server)
        .await;

    let threads = client(&server.uri())
        .review_threads(&RepoRef::new("o", "r"), 7)
        .await
        .unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(
        threads[0].comments.len(),
        2,
        "the first page plus the fetched one"
    );
    assert_eq!(threads[0].comments[1].database_id, 2);
    assert_eq!(threads[0].comments[1].author, "octocat");
}
