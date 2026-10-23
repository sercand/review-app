//! The wire APIs a [`crate::Client`] speaks.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The two wire APIs `review-llm` speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiType {
    /// Anthropic Messages (`/v1/messages`).
    AnthropicMessages,
    /// OpenAI Responses (`/v1/responses`).
    OpenaiResponses,
}

impl ApiType {
    /// The name settings and saved profiles use for this API.
    pub fn as_str(self) -> &'static str {
        match self {
            ApiType::AnthropicMessages => "anthropic-messages",
            ApiType::OpenaiResponses => "openai-responses",
        }
    }
}

impl fmt::Display for ApiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_names_are_the_settings_spelling() {
        assert_eq!(ApiType::AnthropicMessages.as_str(), "anthropic-messages");
        assert_eq!(ApiType::OpenaiResponses.as_str(), "openai-responses");
    }
}
