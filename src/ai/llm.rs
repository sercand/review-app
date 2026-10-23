//! Building LLM clients from the app's provider settings and probing them
//! (spec §10).
//!
//! [`build_client`] turns one [`ProviderSettings`] plus a keychain key into a
//! [`review_llm::Client`]; [`test_connection`] is the "Test connection" probe
//! the Settings screen runs.

use secrecy::SecretString;

use crate::ai::model::ClientLlm;
use crate::persist::settings::{LlmApi, ModelSettings, ProviderSettings};

/// The probe's user message: a reply proves the round trip works.
const TEST_PROMPT: &str = "Reply with the single word OK.";

/// The probe's output cap: the answer is one word.
const TEST_MAX_TOKENS: u32 = 16;

/// The [`review_llm::ApiType`] `p`'s protocol maps to.
///
/// A provider whose protocol this build does not speak
/// ([`LlmApi::Unsupported`]) is an error, not a silent fallback.
pub fn api_type(p: &ProviderSettings) -> anyhow::Result<review_llm::ApiType> {
    match p.api {
        LlmApi::AnthropicMessages => Ok(review_llm::ApiType::AnthropicMessages),
        LlmApi::OpenaiResponses => Ok(review_llm::ApiType::OpenaiResponses),
        LlmApi::Unsupported => {
            anyhow::bail!("provider {} uses an unsupported API", p.name);
        }
    }
}

/// Build a [`review_llm::Client`] for one configured provider and API key.
///
/// An empty `base_url` keeps the API's vendor default; a non-empty one must
/// parse as a URL. A provider whose protocol this build does not speak
/// ([`LlmApi::Unsupported`]) is an error, not a silent fallback.
pub fn build_client(p: &ProviderSettings, key: SecretString) -> anyhow::Result<review_llm::Client> {
    let api = api_type(p)?;
    let mut cfg = review_llm::ClientConfig::new(api, key);
    cfg.provider = p.name.clone();
    if !p.base_url.is_empty() {
        cfg = cfg.base_url(url::Url::parse(&p.base_url)?);
    }
    cfg.headers = p.headers.clone();
    Ok(review_llm::Client::new(cfg)?)
}

/// Build the [`ClientLlm`] one model setting runs on: the provider lookup,
/// the client, and the [`LlmApi`] wiring in one step (spec §10.1).
pub fn build_llm(
    provider: &ProviderSettings,
    model: &ModelSettings,
    key: SecretString,
) -> anyhow::Result<ClientLlm> {
    let api = api_type(provider)?;
    let client = build_client(provider, key)?;
    Ok(ClientLlm::new(client, model.clone(), api))
}

/// Probe a provider with one non-streaming generation.
///
/// Returns the model that served the call, or the error's display text —
/// [`review_llm`] redacts the API key from its errors, so the string is safe
/// to show in the UI.
pub async fn test_connection(
    client: &review_llm::Client,
    model: &ModelSettings,
) -> Result<String, String> {
    let request = review_llm::GenerateRequest {
        model: Some(model.model.clone()),
        max_tokens: Some(TEST_MAX_TOKENS),
        ..review_llm::GenerateRequest::new(vec![review_llm::Message::user_text(TEST_PROMPT)])
    };
    client
        .generate(request)
        .await
        .map(|reply| reply.meta.model)
        .map_err(|error| error.to_string())
}
