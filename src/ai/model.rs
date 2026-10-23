//! The [`Llm`] seam between the agent loop and the wire client (spec §10).
//!
//! The agent loop never touches [`review_llm::Client`] directly: it streams
//! through [`Llm`], which tests replace with `crate::ai::testing::ScriptedLlm`.
//! [`ClientLlm`] is the production adapter; [`fill_request`] applies the
//! per-model defaults the loop leaves unset.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::persist::settings::{ModelSettings, ReasoningLevel};

/// One model endpoint the agent loop streams from.
#[async_trait]
pub trait Llm: Send + Sync {
    /// Opens one streamed generation; the returned stream carries the
    /// [`review_llm::StreamEvent`]s, ending in `Done` or an error.
    async fn stream(
        &self,
        req: review_llm::GenerateRequest,
    ) -> review_llm::Result<BoxStream<'static, review_llm::Result<review_llm::StreamEvent>>>;

    /// The model choice this endpoint serves.
    fn model(&self) -> &ModelSettings;
}

/// Fills the per-model defaults `req` leaves unset.
///
/// `model`, `max_tokens` and `reasoning` are only filled when the caller left
/// them `None` — the agent loop's explicit choices always win. The Anthropic
/// prompt-cache hints for the system prompt and tool list are set for the
/// Messages API only; the Responses API has no equivalent.
fn fill_request(
    req: &mut review_llm::GenerateRequest,
    model: &ModelSettings,
    api: review_llm::ApiType,
) {
    if req.model.is_none() {
        req.model = Some(model.model.clone());
    }
    if req.max_tokens.is_none() {
        req.max_tokens = Some(model.max_tokens);
    }
    if req.reasoning.is_none() {
        req.reasoning = model.reasoning.map(|level| review_llm::Reasoning {
            effort: match level {
                ReasoningLevel::Low => review_llm::ReasoningEffort::Low,
                ReasoningLevel::Medium => review_llm::ReasoningEffort::Medium,
                ReasoningLevel::High => review_llm::ReasoningEffort::High,
            },
        });
    }
    if api == review_llm::ApiType::AnthropicMessages {
        req.cache.system = true;
        req.cache.tools = true;
    }
}

/// An [`Llm`] backed by a [`review_llm::Client`] on one configured provider.
pub struct ClientLlm {
    client: review_llm::Client,
    model: ModelSettings,
    api: review_llm::ApiType,
}

impl ClientLlm {
    /// Wraps `client`. `api` is the protocol `client` speaks (the same
    /// [`review_llm::ApiType`] its [`review_llm::ClientConfig`] was built
    /// with); it decides whether the Anthropic cache hints are set.
    pub fn new(
        client: review_llm::Client,
        model: ModelSettings,
        api: review_llm::ApiType,
    ) -> ClientLlm {
        ClientLlm { client, model, api }
    }
}

#[async_trait]
impl Llm for ClientLlm {
    async fn stream(
        &self,
        mut req: review_llm::GenerateRequest,
    ) -> review_llm::Result<BoxStream<'static, review_llm::Result<review_llm::StreamEvent>>> {
        fill_request(&mut req, &self.model, self.api);
        Ok(self.client.generate_stream(req))
    }

    fn model(&self) -> &ModelSettings {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::fill_request;
    use crate::persist::settings::{ModelSettings, ReasoningLevel};
    use review_llm::{ApiType, CacheHints, GenerateRequest, Reasoning, ReasoningEffort};

    fn model() -> ModelSettings {
        ModelSettings {
            provider: "work".into(),
            model: "claude-test".into(),
            context_window: 100_000,
            max_tokens: 4096,
            reasoning: Some(ReasoningLevel::High),
        }
    }

    #[test]
    fn fill_request_applies_model_defaults_for_anthropic() {
        let mut req = GenerateRequest::default();
        fill_request(&mut req, &model(), ApiType::AnthropicMessages);
        assert_eq!(req.model.as_deref(), Some("claude-test"));
        assert_eq!(req.max_tokens, Some(4096));
        assert_eq!(
            req.reasoning,
            Some(Reasoning {
                effort: ReasoningEffort::High
            })
        );
        assert_eq!(
            req.cache,
            CacheHints {
                system: true,
                tools: true
            }
        );
    }

    #[test]
    fn fill_request_leaves_openai_cache_hints_unset() {
        let mut req = GenerateRequest::default();
        fill_request(&mut req, &model(), ApiType::OpenaiResponses);
        assert_eq!(req.model.as_deref(), Some("claude-test"));
        assert_eq!(req.max_tokens, Some(4096));
        assert_eq!(req.cache, CacheHints::default());
    }

    #[test]
    fn fill_request_never_overwrites_values_the_loop_set() {
        let mut req = GenerateRequest {
            model: Some("other-model".into()),
            max_tokens: Some(64),
            reasoning: Some(Reasoning {
                effort: ReasoningEffort::Low,
            }),
            ..GenerateRequest::default()
        };
        fill_request(&mut req, &model(), ApiType::AnthropicMessages);
        assert_eq!(req.model.as_deref(), Some("other-model"));
        assert_eq!(req.max_tokens, Some(64));
        assert_eq!(
            req.reasoning,
            Some(Reasoning {
                effort: ReasoningEffort::Low
            })
        );
    }

    #[test]
    fn fill_request_leaves_reasoning_unset_when_model_has_none() {
        let mut model = model();
        model.reasoning = None;
        let mut req = GenerateRequest::default();
        fill_request(&mut req, &model, ApiType::AnthropicMessages);
        assert_eq!(req.reasoning, None);
    }
}
