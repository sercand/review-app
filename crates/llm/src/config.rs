//! `[llm]` configuration: providers, per-kind defaults and roles.
//!
//! The user config (`~/.config/otsimoqa/config.toml`) holds providers, base defaults and base
//! roles. The project config (`otsimoqa.toml`) may override defaults and roles only. `qa-cli`
//! reads both files and calls [`LLMConfig::overlay`].

use std::{collections::BTreeMap, path::PathBuf};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{ApiType, Error, Kind, ReasoningEffort, Result, Role};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LLMConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<Role, RoleConfig>,
}

/// A connection to one endpoint, which may serve several APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Root URL, with or without a trailing `/v1`. Defaults to the vendor for Anthropic and
    /// OpenAI; required for `decisions`.
    pub base_url: Option<Url>,
    pub apis: Vec<ApiType>,
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub decision_limits: Option<DecisionLimits>,
    /// Name of an environment variable holding the key. Tried first.
    pub api_key_env: Option<String>,
    /// Path of a file holding the key (`~/` is expanded, surrounding whitespace trimmed).
    /// Tried after `api_key_env`. The key itself is never written into the config.
    pub api_key_file: Option<String>,
}

impl ProviderConfig {
    /// The key named by `api_key_env` or `api_key_file`, in that order. `Ok(None)` when
    /// neither is set or the variable is unset; an unreadable or empty file is an error.
    pub fn configured_key(&self, provider: &str) -> Result<Option<SecretString>> {
        if let Some(var) = &self.api_key_env
            && let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Ok(Some(SecretString::from(v.trim().to_owned())));
        }
        let Some(path) = &self.api_key_file else {
            return Ok(None);
        };
        let path = expand_home(path);
        // Never include the file's contents in an error.
        let text = std::fs::read_to_string(&path).map_err(|e| {
            Error::Config(format!(
                "provider `{provider}`: api_key_file {}: {}",
                path.display(),
                e.kind()
            ))
        })?;
        let key = text.trim();
        if key.is_empty() {
            return Err(Error::Config(format!(
                "provider `{provider}`: api_key_file {} is empty",
                path.display()
            )));
        }
        Ok(Some(SecretString::from(key.to_owned())))
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Declared `/v1/decisions` limits (#85). Parsed here; enforced by the evidence builder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionLimits {
    pub max_questions: Option<u32>,
    pub max_choices: Option<u32>,
    pub max_levels: Option<u32>,
    pub max_images: Option<u32>,
    pub video: Option<bool>,
    pub max_body_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub chat: Option<RoleConfig>,
    pub decision: Option<RoleConfig>,
}

/// Which provider, API and model serve a role, plus request defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    pub provider: String,
    /// Required when the provider serves more than one API of the role's kind.
    pub api: Option<ApiType>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning: Option<ReasoningEffort>,
}

impl Defaults {
    pub fn get(&self, kind: Kind) -> Option<&RoleConfig> {
        match kind {
            Kind::Chat => self.chat.as_ref(),
            Kind::Decision => self.decision.as_ref(),
        }
    }
}

impl LLMConfig {
    /// Applies the project config on top of the user config. Per role and per default kind,
    /// the project entry replaces the user entry as a whole.
    pub fn overlay(user: LLMConfig, project: LLMConfig) -> Result<LLMConfig> {
        if !project.providers.is_empty() {
            return Err(Error::Config(
                "providers belong in the user config (~/.config/otsimoqa/config.toml), not in otsimoqa.toml"
                    .into(),
            ));
        }
        let mut merged = user;
        if project.defaults.chat.is_some() {
            merged.defaults.chat = project.defaults.chat;
        }
        if project.defaults.decision.is_some() {
            merged.defaults.decision = project.defaults.decision;
        }
        merged.roles.extend(project.roles);
        Ok(merged)
    }

    /// The entry serving `role`, and whether it came from the kind's default.
    pub fn resolve(&self, role: Role) -> Option<(&RoleConfig, bool)> {
        if let Some(rc) = self.roles.get(&role) {
            return Some((rc, false));
        }
        self.defaults.get(role.kind()).map(|rc| (rc, true))
    }

    /// The API an entry of `kind` uses on its provider.
    pub(crate) fn api_for(&self, what: &str, kind: Kind, rc: &RoleConfig) -> Result<ApiType> {
        let provider = self
            .providers
            .get(&rc.provider)
            .ok_or_else(|| Error::Config(format!("{what}: unknown provider `{}`", rc.provider)))?;
        if let Some(api) = rc.api {
            if api.kind() != kind {
                return Err(Error::Config(format!(
                    "{what}: api `{api}` is a {} API, but this is a {kind} entry",
                    api.kind()
                )));
            }
            if !provider.apis.contains(&api) {
                return Err(Error::Config(format!(
                    "{what}: provider `{}` does not serve `{api}`",
                    rc.provider
                )));
            }
            return Ok(api);
        }
        let candidates: Vec<ApiType> = provider
            .apis
            .iter()
            .copied()
            .filter(|a| a.kind() == kind)
            .collect();
        match candidates.as_slice() {
            [api] => Ok(*api),
            [] => Err(Error::Config(format!(
                "{what}: provider `{}` serves no {kind} API",
                rc.provider
            ))),
            many => Err(Error::Config(format!(
                "{what}: provider `{}` serves several {kind} APIs; set `api` to one of: {}",
                rc.provider,
                many.iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Checks providers and every configured entry. Keys are checked by [`crate::LLM`].
    pub fn validate(&self) -> Result<()> {
        for (name, p) in &self.providers {
            if p.api_key_env
                .as_deref()
                .is_some_and(|v| v.trim().is_empty())
            {
                return Err(Error::Config(format!(
                    "provider `{name}`: api_key_env is empty"
                )));
            }
            if p.apis.is_empty() {
                return Err(Error::Config(format!("provider `{name}` lists no apis")));
            }
            if p.apis.contains(&ApiType::Decisions) && p.base_url.is_none() {
                return Err(Error::Config(format!(
                    "provider `{name}` serves `decisions` and needs a base_url"
                )));
            }
        }
        for kind in [Kind::Chat, Kind::Decision] {
            if let Some(rc) = self.defaults.get(kind) {
                self.api_for(&format!("defaults.{kind}"), kind, rc)?;
            }
        }
        for (role, rc) in &self.roles {
            self.api_for(&format!("roles.{role}"), role.kind(), rc)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatRole, DecisionRole};

    const USER: &str = r#"
[providers.edge]
base_url = "https://llm.edge.otsimo.com"
apis = ["openai_responses", "anthropic_messages", "decisions"]
[providers.edge.decision_limits]
max_choices = 100

[providers.anthropic]
apis = ["anthropic_messages"]

[defaults]
chat = { provider = "edge", api = "anthropic_messages", model = "qwen3.8-flash", max_tokens = 8192 }
decision = { provider = "edge", model = "qwen3.8-flash" }

[roles]
decide-fast = { provider = "edge", model = "fast" }
agent = { provider = "anthropic", model = "claude" }
"#;

    fn parse(s: &str) -> Result<LLMConfig, toml::de::Error> {
        toml::from_str(s)
    }

    #[test]
    fn parses_and_validates_user_config() {
        let c = parse(USER).unwrap();
        c.validate().unwrap();
        assert_eq!(
            c.providers["edge"]
                .decision_limits
                .as_ref()
                .unwrap()
                .max_choices,
            Some(100)
        );
    }

    #[test]
    fn unknown_role_key_fails_with_valid_keys() {
        let err = parse("[roles]\nagnet = { provider = \"p\", model = \"m\" }").unwrap_err();
        assert!(err.to_string().contains("expected one of: agent"), "{err}");
    }

    #[test]
    fn unknown_field_fails() {
        assert!(parse("[roles]\nagent = { provider = \"p\", modle = \"m\" }").is_err());
    }

    #[test]
    fn resolve_role_then_default() {
        let c = parse(USER).unwrap();
        let (rc, from_default) = c.resolve(ChatRole::Agent.into()).unwrap();
        assert_eq!((rc.model.as_str(), from_default), ("claude", false));
        let (rc, from_default) = c.resolve(ChatRole::Vision.into()).unwrap();
        assert_eq!((rc.model.as_str(), from_default), ("qwen3.8-flash", true));
        let (rc, from_default) = c.resolve(DecisionRole::Decide.into()).unwrap();
        assert_eq!((rc.model.as_str(), from_default), ("qwen3.8-flash", true));
    }

    #[test]
    fn overlay_replaces_whole_entries() {
        let user = parse(USER).unwrap();
        let project = parse(
            r#"
[defaults]
decision = { provider = "edge", model = "project-default" }
[roles]
agent = { provider = "edge", api = "openai_responses", model = "qwen3.8-flash" }
"#,
        )
        .unwrap();
        let c = LLMConfig::overlay(user, project).unwrap();
        c.validate().unwrap();
        let agent = &c.roles[&Role::Chat(ChatRole::Agent)];
        assert_eq!(agent.api, Some(ApiType::OpenaiResponses));
        assert_eq!(agent.max_tokens, None, "no field-level merge");
        assert_eq!(
            c.defaults.decision.as_ref().unwrap().model,
            "project-default"
        );
        assert_eq!(c.defaults.chat.as_ref().unwrap().model, "qwen3.8-flash");
        assert_eq!(
            c.roles[&Role::Decision(DecisionRole::DecideFast)].model,
            "fast"
        );
    }

    #[test]
    fn project_providers_are_rejected() {
        let project = parse("[providers.x]\napis = [\"decisions\"]").unwrap();
        assert!(matches!(
            LLMConfig::overlay(LLMConfig::default(), project),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn api_inference_and_errors() {
        let base = parse(USER).unwrap();
        let check = |roles: &str| {
            let mut c = base.clone();
            c.roles = parse(roles).unwrap().roles;
            c.validate()
        };
        // edge serves two chat APIs: `api` is required.
        let err = check("[roles]\nvision = { provider = \"edge\", model = \"m\" }").unwrap_err();
        assert!(err.to_string().contains("set `api`"), "{err}");
        // a chat role on a decisions api
        let err =
            check("[roles]\nvision = { provider = \"edge\", api = \"decisions\", model = \"m\" }")
                .unwrap_err();
        assert!(err.to_string().contains("decision API"), "{err}");
        // a decision role on a chat-only provider
        let err =
            check("[roles]\ndecide = { provider = \"anthropic\", model = \"m\" }").unwrap_err();
        assert!(err.to_string().contains("no decision API"), "{err}");
        // unknown provider
        let err = check("[roles]\ndecide = { provider = \"nope\", model = \"m\" }").unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "{err}");
        // api the provider doesn't serve
        let err = check(
            "[roles]\nagent = { provider = \"anthropic\", api = \"openai_responses\", model = \"m\" }",
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not serve"), "{err}");
    }

    #[test]
    fn configured_key_env_then_file() {
        let dir = std::env::temp_dir().join(format!("review-llm-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key");
        std::fs::write(&file, "  file-key\n").unwrap();
        let provider = |env: Option<&str>, file: Option<&std::path::Path>| ProviderConfig {
            base_url: None,
            apis: vec![ApiType::AnthropicMessages],
            timeout_secs: None,
            headers: BTreeMap::new(),
            decision_limits: None,
            api_key_env: env.map(str::to_owned),
            api_key_file: file.map(|f| f.display().to_string()),
        };
        use secrecy::ExposeSecret;
        let key = |p: ProviderConfig| {
            p.configured_key("p")
                .unwrap()
                .map(|k| k.expose_secret().to_owned())
        };
        // A set variable wins over the file (`HOME` is always set in tests).
        let home = std::env::var("HOME").unwrap();
        assert_eq!(key(provider(Some("HOME"), Some(&file))), Some(home));
        // An unset variable falls back to the file, trimmed.
        assert_eq!(
            key(provider(Some("QA_LLM_TEST_SURELY_UNSET"), Some(&file))),
            Some("file-key".into())
        );
        assert_eq!(key(provider(None, None)), None);
        // A missing file is an error that never echoes contents.
        let err = provider(None, Some(&dir.join("missing")))
            .configured_key("p")
            .unwrap_err();
        assert!(err.to_string().contains("api_key_file"), "{err}");
        std::fs::write(&file, "\n").unwrap();
        assert!(provider(None, Some(&file)).configured_key("p").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_key_fields() {
        let c = parse(
            "[providers.edge]\nbase_url = \"https://x\"\napis = [\"decisions\"]\napi_key_env = \"EDGE_KEY\"\napi_key_file = \"~/.config/kedis-qa/decisions.key\"",
        )
        .unwrap();
        c.validate().unwrap();
        let p = &c.providers["edge"];
        assert_eq!(p.api_key_env.as_deref(), Some("EDGE_KEY"));
        assert_eq!(
            p.api_key_file.as_deref(),
            Some("~/.config/kedis-qa/decisions.key")
        );
        let bad = parse("[providers.edge]\napis = [\"anthropic_messages\"]\napi_key_env = \" \"")
            .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn decisions_provider_needs_base_url() {
        let c = parse("[providers.d]\napis = [\"decisions\"]").unwrap();
        assert!(c.validate().is_err());
    }
}
