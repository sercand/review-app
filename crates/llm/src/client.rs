use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, stream::BoxStream};
use secrecy::SecretString;
use url::Url;

use crate::{
    ApiType, CallMeta, DecideRequest, Decision, Error, GenerateRequest, Kind, Reply, Result, Role,
    StreamEvent, anthropic, decisions,
    http::header_map,
    openai,
    retry::RetryPolicy,
    trace::{Trace, TraceCall},
};

const CHAT_TIMEOUT: Duration = Duration::from_secs(120);
const DECISIONS_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_IDLE: Duration = Duration::from_secs(60);

/// How to reach one API of one provider.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Provider name, recorded in [`CallMeta`].
    pub provider: String,
    pub api: ApiType,
    /// Root URL, with or without a trailing `/v1`. `None` uses the vendor default; required
    /// for `decisions`.
    pub base_url: Option<Url>,
    pub api_key: SecretString,
    /// Total request timeout. Default: 10s for `decisions`, 120s for chat.
    pub timeout: Option<Duration>,
    /// No event for this long fails a stream. Default 60s.
    pub stream_idle_timeout: Option<Duration>,
    pub headers: BTreeMap<String, String>,
    pub retry: RetryPolicy,
    /// Append every HTTP exchange to this JSONL file (keys redacted).
    pub trace: Option<PathBuf>,
    /// Resolves the hosts of its requests; `None` uses the system resolver. A
    /// [`crate::dns::BlockedHost`] from it fails the request with [`Error::BlockedHost`].
    /// With one, the client follows no redirect and ignores the system proxy settings, so
    /// every connection goes through it ([`crate::dns::Resolver`]).
    pub resolver: Option<crate::dns::Resolver>,
}

impl ClientConfig {
    pub fn new(api: ApiType, api_key: SecretString) -> Self {
        Self {
            provider: api.as_str().to_owned(),
            api,
            base_url: None,
            api_key,
            timeout: None,
            stream_idle_timeout: None,
            headers: BTreeMap::new(),
            retry: RetryPolicy::default(),
            trace: None,
            resolver: None,
        }
    }

    pub fn base_url(mut self, url: Url) -> Self {
        self.base_url = Some(url);
        self
    }
}

enum Inner {
    Anthropic(anthropic::Client),
    OpenAi(Box<openai::Client>),
    Decisions(decisions::Client),
}

/// One wire API on one provider: `generate` and `generate_stream` for chat APIs, `decide`
/// for `decisions`. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    provider: Arc<str>,
    api: ApiType,
    inner: Arc<Inner>,
    key: SecretString,
    trace: Option<Trace>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("provider", &self.provider)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

/// The role a call is made for, when it comes through a role handle.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Tag {
    pub role: Option<Role>,
    pub from_default: bool,
}

/// Per-call data for [`CallMeta`] and the trace.
pub(crate) struct CallCtx {
    pub provider: Arc<str>,
    pub api: ApiType,
    pub model: String,
    pub started: Instant,
    pub tag: Tag,
    pub trace: Option<Arc<TraceCall>>,
}

impl CallCtx {
    pub fn meta(&self) -> CallMeta {
        CallMeta {
            api: self.api,
            provider: self.provider.to_string(),
            model: self.model.clone(),
            role: self.tag.role,
            from_default: self.tag.from_default,
            latency: self.started.elapsed(),
        }
    }
}

/// The root URL the clients append `/v1/...` to: trailing slashes and a trailing `/v1` are
/// dropped, so `https://x/v1` and `https://x` both reach `https://x/v1/messages`.
fn api_root(url: &str) -> String {
    let url = url.trim_end_matches('/');
    url.strip_suffix("/v1")
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_owned()
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        let base = match (&cfg.base_url, cfg.api) {
            (Some(u), _) => api_root(u.as_str()),
            (None, ApiType::AnthropicMessages) => anthropic::DEFAULT_BASE_URL.to_owned(),
            (None, ApiType::OpenaiResponses) => openai::DEFAULT_BASE_URL.to_owned(),
            (None, ApiType::Decisions) => {
                return Err(Error::Config(format!(
                    "provider `{}`: decisions needs a base_url",
                    cfg.provider
                )));
            }
        };
        let timeout = cfg.timeout.unwrap_or(match cfg.api.kind() {
            Kind::Chat => CHAT_TIMEOUT,
            Kind::Decision => DECISIONS_TIMEOUT,
        });
        let idle = cfg.stream_idle_timeout.unwrap_or(STREAM_IDLE);
        let headers: Vec<(String, String)> = cfg.headers.clone().into_iter().collect();
        let key = cfg.api_key.clone();
        let trace = cfg.trace.as_deref().map(|p| Trace::new(p, key.clone()));
        let inner = match cfg.api {
            ApiType::AnthropicMessages => Inner::Anthropic(anthropic::Client::new(
                &base,
                header_map(&headers)?,
                cfg.api_key,
                timeout,
                idle,
                cfg.retry,
                cfg.resolver.clone(),
            )?),
            ApiType::OpenaiResponses => Inner::OpenAi(Box::new(openai::Client::new(
                &base,
                &headers,
                cfg.api_key,
                timeout,
                idle,
                cfg.retry,
                cfg.resolver.clone(),
            )?)),
            ApiType::Decisions => Inner::Decisions(decisions::Client::new(
                &base,
                header_map(&headers)?,
                cfg.api_key,
                timeout,
                cfg.retry,
                cfg.resolver,
            )?),
        };
        Ok(Self {
            provider: cfg.provider.into(),
            api: cfg.api,
            inner: Arc::new(inner),
            key,
            trace,
        })
    }

    /// Appends every HTTP exchange of this client to a JSONL file (keys redacted).
    pub fn with_trace(mut self, path: impl AsRef<Path>) -> Self {
        self.trace = Some(Trace::new(path.as_ref(), self.key.clone()));
        self
    }

    pub fn api(&self) -> ApiType {
        self.api
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    fn ctx(&self, model: Option<&str>, tag: Tag, stream: bool) -> Result<CallCtx> {
        let model = model
            .ok_or_else(|| Error::InvalidRequest("model is not set".into()))?
            .to_owned();
        let trace = self.trace.as_ref().map(|t| {
            Arc::new(TraceCall::new(
                t,
                self.api,
                &self.provider,
                &model,
                tag.role,
                tag.from_default,
                stream,
            ))
        });
        Ok(CallCtx {
            provider: self.provider.clone(),
            api: self.api,
            model,
            started: Instant::now(),
            tag,
            trace,
        })
    }

    fn unsupported(&self, op: &'static str) -> Error {
        Error::Unsupported { api: self.api, op }
    }

    pub async fn generate(&self, req: GenerateRequest) -> Result<Reply> {
        self.generate_as(req, Tag::default()).await
    }

    pub(crate) async fn generate_as(&self, req: GenerateRequest, tag: Tag) -> Result<Reply> {
        let ctx = self.ctx(req.model.as_deref(), tag, false)?;
        match &*self.inner {
            Inner::Anthropic(c) => c.generate(&req, ctx).await,
            Inner::OpenAi(c) => c.generate(&req, ctx).await,
            Inner::Decisions(_) => Err(self.unsupported("generate")),
        }
    }

    pub fn generate_stream(&self, req: GenerateRequest) -> BoxStream<'static, Result<StreamEvent>> {
        self.generate_stream_as(req, Tag::default())
    }

    pub(crate) fn generate_stream_as(
        &self,
        req: GenerateRequest,
        tag: Tag,
    ) -> BoxStream<'static, Result<StreamEvent>> {
        let this = self.clone();
        futures::stream::once(async move {
            let opened = async {
                let ctx = this.ctx(req.model.as_deref(), tag, true)?;
                match &*this.inner {
                    Inner::Anthropic(c) => c.generate_stream(&req, ctx).await,
                    Inner::OpenAi(c) => c.generate_stream(&req, ctx).await,
                    Inner::Decisions(_) => Err(this.unsupported("generate_stream")),
                }
            }
            .await;
            match opened {
                Ok(s) => s,
                Err(e) => futures::stream::once(async { Err(e) }).boxed(),
            }
        })
        .flatten()
        .boxed()
    }

    pub async fn decide(&self, req: DecideRequest) -> Result<Decision> {
        self.decide_as(req, Tag::default()).await
    }

    pub(crate) async fn decide_as(&self, req: DecideRequest, tag: Tag) -> Result<Decision> {
        let Inner::Decisions(c) = &*self.inner else {
            return Err(self.unsupported("decide"));
        };
        let ctx = self.ctx(req.model.as_deref(), tag, false)?;
        c.decide(&req, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::api_root;

    #[test]
    fn api_root_drops_trailing_v1() {
        for (input, root) in [
            ("https://x.com", "https://x.com"),
            ("https://x.com/", "https://x.com"),
            ("https://x.com/v1", "https://x.com"),
            ("https://x.com/v1/", "https://x.com"),
            ("https://x.com/proxy/v1", "https://x.com/proxy"),
            ("https://x.com/proxy", "https://x.com/proxy"),
            ("https://x.com/v10", "https://x.com/v10"),
        ] {
            assert_eq!(api_root(input), root, "{input}");
        }
    }
}
