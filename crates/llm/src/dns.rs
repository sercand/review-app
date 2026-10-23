//! An optional DNS resolver for a client's connections ([`ClientConfig::resolver`]), so a
//! caller can decide which addresses its requests may reach (the backend's LLM gateway
//! refuses private addresses for org-owned providers). Without one, the system resolver is
//! used, as before.
//!
//! [`ClientConfig::resolver`]: crate::ClientConfig::resolver

use std::{fmt, sync::Arc};

pub use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// A resolver for a client's connections. Only hostnames are resolved: a URL with an IP
/// literal never reaches it. A client with a resolver also follows no redirect (a 3xx is
/// its reply: a redirect to an IP literal would skip the resolver and resend the key) and
/// ignores the system proxy settings (`HTTP(S)_PROXY`, `ALL_PROXY`: through a proxy, the
/// proxy resolves the host), so the resolver sees every host the client connects to.
#[derive(Clone)]
pub struct Resolver(pub Arc<dyn Resolve>);

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Resolver")
    }
}

/// The error a [`Resolve`] returns to refuse a host. A request whose resolution fails with
/// it fails with [`crate::Error::BlockedHost`] instead of a transport error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("blocked host `{0}`")]
pub struct BlockedHost(pub String);

/// `b` with `r` as its resolver, no redirects and no proxy (see [`Resolver`]).
pub(crate) fn guarded(b: reqwest::ClientBuilder, r: Resolver) -> reqwest::ClientBuilder {
    b.dns_resolver(r.0)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
}

/// The refused host, when `e` or one of its sources is a [`BlockedHost`].
pub(crate) fn blocked_host(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur = Some(e);
    while let Some(e) = cur {
        if let Some(b) = e.downcast_ref::<BlockedHost>() {
            return Some(b.0.clone());
        }
        cur = e.source();
    }
    None
}
