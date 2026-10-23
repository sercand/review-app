//! Built-in model presets for the model dialogs (Settings AI tab).
//!
//! The list is a convenience for filling the dialog: choosing a preset copies
//! its id and sizes into the model settings, and "Custom" keeps the id and
//! sizes free text. It is not a whitelist — a provider can serve any model id
//! the user types — so a stale entry only costs a bad default, never a broken
//! setting.
//!
//! Sizes are the vendors' published context window and synchronous output cap
//! (Anthropic Messages, OpenAI Responses). They are stored next to the id
//! because the app uses them for compaction budgets and response caps and has
//! no way to ask the provider.

use crate::persist::settings::LlmApi;

/// One built-in model choice: the id to request and its published limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelPreset {
    /// The API this model is reached through.
    pub api: LlmApi,
    /// The model id to put in [`ModelSettings::model`](crate::persist::settings::ModelSettings::model).
    pub id: &'static str,
    /// The name shown in the preset dropdown.
    pub label: &'static str,
    /// The context window in tokens.
    pub context_window: u32,
    /// The per-response output cap in tokens.
    pub max_tokens: u32,
}

/// The Anthropic presets: the current Claude 5 family, all 1M context with a
/// 128K synchronous output cap (Anthropic's models overview, Oct 2026).
static ANTHROPIC: &[ModelPreset] = &[
    ModelPreset {
        api: LlmApi::AnthropicMessages,
        id: "claude-opus-5-5",
        label: "Claude Opus 5.5",
        context_window: 1_000_000,
        max_tokens: 128_000,
    },
    ModelPreset {
        api: LlmApi::AnthropicMessages,
        id: "claude-sonnet-5-5",
        label: "Claude Sonnet 5.5",
        context_window: 1_000_000,
        max_tokens: 128_000,
    },
    ModelPreset {
        api: LlmApi::AnthropicMessages,
        id: "claude-haiku-5-5",
        label: "Claude Haiku 5.5",
        context_window: 1_000_000,
        max_tokens: 128_000,
    },
    ModelPreset {
        api: LlmApi::AnthropicMessages,
        id: "claude-fable-5-1",
        label: "Claude Fable 5.1",
        context_window: 1_000_000,
        max_tokens: 128_000,
    },
];

/// The OpenAI presets: the GPT-6 family, 1.05M context with a 128K output cap
/// (OpenAI's model catalog, Oct 2026).
static OPENAI: &[ModelPreset] = &[
    ModelPreset {
        api: LlmApi::OpenaiResponses,
        id: "gpt-6-astra",
        label: "GPT-6 Astra",
        context_window: 1_050_000,
        max_tokens: 128_000,
    },
    ModelPreset {
        api: LlmApi::OpenaiResponses,
        id: "gpt-6.1-sol",
        label: "GPT-6.1 Sol",
        context_window: 1_050_000,
        max_tokens: 128_000,
    },
    ModelPreset {
        api: LlmApi::OpenaiResponses,
        id: "gpt-6-luna",
        label: "GPT-6 Luna",
        context_window: 1_050_000,
        max_tokens: 128_000,
    },
];

/// The presets for `api`, most capable first; empty for an API this build
/// cannot speak, which leaves the dialog's id and sizes as free text.
pub fn presets(api: LlmApi) -> &'static [ModelPreset] {
    match api {
        LlmApi::AnthropicMessages => ANTHROPIC,
        LlmApi::OpenaiResponses => OPENAI,
        LlmApi::Unsupported => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every API this build speaks has a short, consistent preset list; an
    /// unsupported API has none, so the dialog falls back to free text.
    #[test]
    fn presets_are_per_api_and_have_nonzero_sizes() {
        for (api, id_prefix) in [
            (LlmApi::AnthropicMessages, "claude-"),
            (LlmApi::OpenaiResponses, "gpt-"),
        ] {
            let list = presets(api);
            assert!(!list.is_empty(), "{api:?} should have presets");
            for preset in list {
                assert_eq!(preset.api, api, "{} belongs to another API", preset.id);
                assert!(
                    preset.id.starts_with(id_prefix),
                    "{} is not a {api:?} id",
                    preset.id
                );
                assert!(!preset.label.is_empty(), "{} needs a label", preset.id);
                assert!(
                    preset.context_window > 0,
                    "{} needs a context window",
                    preset.id
                );
                assert!(preset.max_tokens > 0, "{} needs an output cap", preset.id);
                assert!(
                    preset.max_tokens <= preset.context_window,
                    "{}'s output cap exceeds its context window",
                    preset.id
                );
            }

            let mut ids: Vec<&str> = list.iter().map(|preset| preset.id).collect();
            ids.sort_unstable();
            let count = ids.len();
            ids.dedup();
            assert_eq!(ids.len(), count, "{api:?} lists a model twice");
        }

        assert!(
            presets(LlmApi::Unsupported).is_empty(),
            "an API this build cannot speak has nothing to offer"
        );
    }
}
