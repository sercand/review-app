use std::{collections::HashMap, fmt, path::Path, time::Duration};

use futures::stream::BoxStream;
use secrecy::SecretString;

use crate::{
    ApiType, ChatRole, Client, ClientConfig, DecideRequest, Decision, DecisionRole, Error,
    GenerateRequest, Kind, LLMConfig, Reasoning, ReasoningEffort, Reply, Result, Role, RoleConfig,
    StreamEvent, client::Tag, retry::RetryPolicy,
};

/// The configured providers and roles. Hands out typed per-role model handles.
pub struct LLM {
    config: LLMConfig,
    clients: HashMap<(String, ApiType), Client>,
}

impl fmt::Debug for LLM {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut clients: Vec<_> = self.clients.keys().collect();
        clients.sort();
        f.debug_struct("LLM")
            .field("clients", &clients)
            .field("roles", &self.config.roles.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl LLM {
    /// Validates the merged config and builds one shared client per `(provider, api)` in use.
    ///
    /// A provider's key comes from its `api_key_env`, then its `api_key_file`, then `keys`
    /// (the CLI's keychain / `OTSIMOQA_<PROVIDER>_KEY` lookup).
    pub fn from_config(
        config: &LLMConfig,
        keys: impl Fn(&str) -> Option<SecretString>,
    ) -> Result<Self> {
        config.validate()?;
        let mut entries: Vec<(String, Kind, &RoleConfig)> = Vec::new();
        for kind in [Kind::Chat, Kind::Decision] {
            if let Some(rc) = config.defaults.get(kind) {
                entries.push((format!("defaults.{kind}"), kind, rc));
            }
        }
        for (role, rc) in &config.roles {
            entries.push((format!("roles.{role}"), role.kind(), rc));
        }
        let mut clients = HashMap::new();
        for (what, kind, rc) in entries {
            let api = config.api_for(&what, kind, rc)?;
            let id = (rc.provider.clone(), api);
            if clients.contains_key(&id) {
                continue;
            }
            let p = &config.providers[&rc.provider];
            let key = match p.configured_key(&rc.provider)? {
                Some(k) => k,
                None => keys(&rc.provider).ok_or_else(|| Error::MissingKey {
                    provider: rc.provider.clone(),
                })?,
            };
            let client = Client::new(ClientConfig {
                provider: rc.provider.clone(),
                api,
                base_url: p.base_url.clone(),
                api_key: key,
                timeout: p.timeout_secs.map(Duration::from_secs),
                stream_idle_timeout: None,
                headers: p.headers.clone(),
                retry: RetryPolicy::default(),
                trace: None,
                resolver: None,
            })?;
            clients.insert(id, client);
        }
        Ok(Self {
            config: config.clone(),
            clients,
        })
    }

    /// Appends every HTTP exchange of every role to this JSONL file (keys redacted).
    pub fn with_trace(mut self, path: impl AsRef<Path>) -> Self {
        for client in self.clients.values_mut() {
            *client = client.clone().with_trace(path.as_ref());
        }
        self
    }

    fn resolve(&self, role: Role) -> Result<(Client, &RoleConfig, bool)> {
        let (rc, from_default) =
            self.config
                .resolve(role)
                .ok_or_else(|| Error::RoleNotConfigured {
                    role: role.to_string(),
                })?;
        let api = self
            .config
            .api_for(&format!("roles.{role}"), role.kind(), rc)?;
        let client = self
            .clients
            .get(&(rc.provider.clone(), api))
            .cloned()
            .ok_or_else(|| Error::Config(format!("no client for role `{role}`")))?;
        Ok((client, rc, from_default))
    }

    pub fn chat(&self, role: ChatRole) -> Result<ChatModel> {
        let (client, rc, from_default) = self.resolve(role.into())?;
        Ok(ChatModel {
            client,
            model: rc.model.clone(),
            max_tokens: rc.max_tokens,
            temperature: rc.temperature,
            reasoning: rc.reasoning,
            role,
            from_default,
        })
    }

    pub fn decision(&self, role: DecisionRole) -> Result<DecisionModel> {
        let (client, rc, from_default) = self.resolve(role.into())?;
        Ok(DecisionModel {
            client,
            model: rc.model.clone(),
            role,
            from_default,
        })
    }
}

/// A chat role bound to its client, model and request defaults.
#[derive(Debug, Clone)]
pub struct ChatModel {
    client: Client,
    model: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    reasoning: Option<ReasoningEffort>,
    role: ChatRole,
    from_default: bool,
}

impl ChatModel {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn api(&self) -> ApiType {
        self.client.api()
    }

    fn fill(&self, mut req: GenerateRequest) -> GenerateRequest {
        req.model.get_or_insert_with(|| self.model.clone());
        if req.max_tokens.is_none() {
            req.max_tokens = self.max_tokens;
        }
        if req.temperature.is_none() {
            req.temperature = self.temperature;
        }
        if req.reasoning.is_none() {
            req.reasoning = self.reasoning.map(|effort| Reasoning { effort });
        }
        req
    }

    fn tag(&self) -> Tag {
        Tag {
            role: Some(self.role.into()),
            from_default: self.from_default,
        }
    }

    pub async fn generate(&self, req: GenerateRequest) -> Result<Reply> {
        self.client.generate_as(self.fill(req), self.tag()).await
    }

    pub fn generate_stream(&self, req: GenerateRequest) -> BoxStream<'static, Result<StreamEvent>> {
        self.client.generate_stream_as(self.fill(req), self.tag())
    }
}

/// A decision role bound to its client and model.
#[derive(Debug, Clone)]
pub struct DecisionModel {
    client: Client,
    model: String,
    role: DecisionRole,
    from_default: bool,
}

impl DecisionModel {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn decide(&self, mut req: DecideRequest) -> Result<Decision> {
        req.model.get_or_insert_with(|| self.model.clone());
        let tag = Tag {
            role: Some(self.role.into()),
            from_default: self.from_default,
        };
        self.client.decide_as(req, tag).await
    }
}
