//! `ClientConfig::resolver`: the resolver a caller passes decides where requests connect,
//! and a refused host fails the request with `Error::BlockedHost`, not retried and not a
//! fallback, on every API.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use review_llm::{
    ApiType, Client, ClientConfig, Error, GenerateRequest, Message, RetryPolicy,
    dns::{Addrs, BlockedHost, Name, Resolve, Resolver, Resolving},
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// `ok.test` resolves to the mock server; any other host is refused.
struct Static(SocketAddr);

impl Resolve for Static {
    fn resolve(&self, name: Name) -> Resolving {
        let (host, addr) = (name.as_str().to_owned(), self.0);
        Box::pin(async move {
            if host == "ok.test" {
                Ok(Box::new(std::iter::once(addr)) as Addrs)
            } else {
                Err(Box::new(BlockedHost(host)) as Box<dyn std::error::Error + Send + Sync>)
            }
        })
    }
}

fn client(api: ApiType, host: &str, server: &MockServer) -> Client {
    let port = server.address().port();
    let mut cfg = ClientConfig::new(api, SecretString::from("sk-dns"))
        .base_url(format!("http://{host}:{port}").parse().unwrap());
    cfg.retry = RetryPolicy {
        max_retries: 2,
        base: Duration::from_millis(1),
        max: Duration::from_millis(5),
    };
    cfg.resolver = Some(Resolver(Arc::new(Static(*server.address()))));
    Client::new(cfg).unwrap()
}

#[tokio::test]
async fn resolver_decides_where_requests_go() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "pong"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    let req = || {
        let mut r = GenerateRequest::new(vec![Message::user_text("ping")]);
        r.model = Some("m".into());
        r.max_tokens = Some(16);
        r
    };
    let reply = client(ApiType::AnthropicMessages, "ok.test", &server)
        .generate(req())
        .await
        .unwrap();
    assert_eq!(reply.text(), "pong");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    for api in [ApiType::AnthropicMessages, ApiType::OpenaiResponses] {
        let e = client(api, "evil.test", &server)
            .generate(req())
            .await
            .unwrap_err();
        assert!(
            matches!(&e, Error::BlockedHost(h) if h == "evil.test"),
            "{api:?}: {e:?}"
        );
        assert!(!e.is_fallback_eligible(), "{e:?}");
    }
    // Refused before any connection, and not retried.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

fn anthropic_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "pong"}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
}

fn ping() -> GenerateRequest {
    let mut r = GenerateRequest::new(vec![Message::user_text("ping")]);
    r.model = Some("m".into());
    r.max_tokens = Some(16);
    r
}

/// A client with a resolver never follows a redirect: a redirect target that is an IP
/// literal would skip the resolver (and resend the key). The 3xx is the reply.
#[tokio::test]
async fn resolver_clients_do_not_follow_redirects() {
    let target = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(anthropic_ok())
        .mount(&target)
        .await;
    let first = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/messages", target.uri())),
        )
        .mount(&first)
        .await;
    let e = client(ApiType::AnthropicMessages, "ok.test", &first)
        .generate(ping())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Http { status: 307, .. }), "{e:?}");
    let e = client(ApiType::OpenaiResponses, "ok.test", &first)
        .generate(ping())
        .await
        .unwrap_err();
    // `async-openai` reads a non-2xx reply as an error body; an empty 307 fails to decode.
    // Either way the redirect is not followed.
    assert!(
        matches!(e, Error::Http { status: 307, .. } | Error::Decode(_)),
        "{e:?}"
    );
    assert_eq!(target.received_requests().await.unwrap().len(), 0);
    // Without a resolver (v0), redirects are followed as before.
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from("sk-dns"))
        .base_url(first.uri().parse().unwrap());
    cfg.retry = RetryPolicy::none();
    Client::new(cfg).unwrap().generate(ping()).await.unwrap();
    assert_eq!(target.received_requests().await.unwrap().len(), 1);
}

/// Set by [`resolver_clients_ignore_system_proxy`] for its child run.
const PROXY_CHILD: &str = "QA_LLM_PROXY_CHILD";

/// A client with a resolver connects directly: through a system proxy, the proxy would
/// resolve the host and the resolver would never see it. `set_var` needs `unsafe`, which
/// the workspace forbids, so the body runs in a child process with the proxy in its env.
#[test]
fn resolver_clients_ignore_system_proxy() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["proxy_child", "--exact", "--include-ignored", "--nocapture"])
        .env(PROXY_CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
#[ignore = "run by resolver_clients_ignore_system_proxy with a proxy in its env"]
async fn proxy_child() {
    if std::env::var_os(PROXY_CHILD).is_none() {
        return;
    }
    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(anthropic_ok())
        .mount(&proxy)
        .await;
    let direct = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_ok())
        .mount(&direct)
        .await;
    // The proxy settings reqwest reads, pointed at `proxy`: a grandchild with them set.
    if std::env::var_os("HTTP_PROXY").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["proxy_child", "--exact", "--include-ignored", "--nocapture"])
            .env(PROXY_CHILD, "1")
            .env("PROXY_URI", proxy.uri())
            .env("DIRECT_ADDR", direct.address().to_string())
            .env("HTTP_PROXY", proxy.uri())
            .env("http_proxy", proxy.uri())
            .env("ALL_PROXY", proxy.uri())
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(direct.received_requests().await.unwrap().len(), 1, "direct");
        // The unguarded control call went through the proxy.
        assert_eq!(proxy.received_requests().await.unwrap().len(), 1, "proxy");
        return;
    }
    // The grandchild: the servers are its parent's.
    let addr: SocketAddr = std::env::var("DIRECT_ADDR").unwrap().parse().unwrap();
    let url = format!("http://ok.test:{}", addr.port());
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from("sk-dns"))
        .base_url(url.parse().unwrap());
    cfg.retry = RetryPolicy::none();
    cfg.resolver = Some(Resolver(Arc::new(Static(addr))));
    Client::new(cfg).unwrap().generate(ping()).await.unwrap();
    // Control: without a resolver the same URL goes through the proxy (`ok.test` does not
    // resolve, so only the proxy can answer).
    let mut cfg = ClientConfig::new(ApiType::AnthropicMessages, SecretString::from("sk-dns"))
        .base_url(url.parse().unwrap());
    cfg.retry = RetryPolicy::none();
    Client::new(cfg).unwrap().generate(ping()).await.unwrap();
}
