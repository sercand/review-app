use std::fmt;

use serde::{Deserialize, Serialize};

/// The three wire APIs `review-llm` speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiType {
    AnthropicMessages,
    OpenaiResponses,
    Decisions,
}

/// What a role or API is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Chat,
    Decision,
}

impl ApiType {
    pub fn kind(self) -> Kind {
        match self {
            ApiType::AnthropicMessages | ApiType::OpenaiResponses => Kind::Chat,
            ApiType::Decisions => Kind::Decision,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApiType::AnthropicMessages => "anthropic_messages",
            ApiType::OpenaiResponses => "openai_responses",
            ApiType::Decisions => "decisions",
        }
    }
}

impl fmt::Display for ApiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Chat => "chat",
            Kind::Decision => "decision",
        })
    }
}

/// Roles served by chat models (`generate`, `generate_stream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatRole {
    /// `agent` steps and escalation (#88).
    Agent,
    /// `via: llm` questions and checks (#108).
    Vision,
    /// Authoring and explore sessions (#97, #98).
    Authoring,
}

/// Roles served by decision models (`decide`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionRole {
    /// Per-turn action selection.
    DecideFast,
    /// Verdicts and harder checks.
    Decide,
}

/// Any role; the key type of `[llm.roles]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Chat(ChatRole),
    Decision(DecisionRole),
}

impl ChatRole {
    pub const ALL: [ChatRole; 3] = [ChatRole::Agent, ChatRole::Vision, ChatRole::Authoring];

    pub fn as_str(self) -> &'static str {
        match self {
            ChatRole::Agent => "agent",
            ChatRole::Vision => "vision",
            ChatRole::Authoring => "authoring",
        }
    }
}

impl DecisionRole {
    pub const ALL: [DecisionRole; 2] = [DecisionRole::DecideFast, DecisionRole::Decide];

    pub fn as_str(self) -> &'static str {
        match self {
            DecisionRole::DecideFast => "decide-fast",
            DecisionRole::Decide => "decide",
        }
    }
}

impl Role {
    pub fn kind(self) -> Kind {
        match self {
            Role::Chat(_) => Kind::Chat,
            Role::Decision(_) => Kind::Decision,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Chat(r) => r.as_str(),
            Role::Decision(r) => r.as_str(),
        }
    }
}

impl From<ChatRole> for Role {
    fn from(r: ChatRole) -> Self {
        Role::Chat(r)
    }
}

impl From<DecisionRole> for Role {
    fn from(r: DecisionRole) -> Self {
        Role::Decision(r)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ChatRole::ALL
            .iter()
            .map(|r| Role::Chat(*r))
            .chain(DecisionRole::ALL.iter().map(|r| Role::Decision(*r)))
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                let valid: Vec<&str> = ChatRole::ALL
                    .iter()
                    .map(|r| r.as_str())
                    .chain(DecisionRole::ALL.iter().map(|r| r.as_str()))
                    .collect();
                format!(
                    "unknown llm role `{s}`, expected one of: {}",
                    valid.join(", ")
                )
            })
    }
}

impl Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parse_round_trip() {
        for r in ChatRole::ALL
            .map(Role::from)
            .into_iter()
            .chain(DecisionRole::ALL.map(Role::from))
        {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
    }

    #[test]
    fn unknown_role_lists_valid_keys() {
        let err = "agnet".parse::<Role>().unwrap_err();
        assert!(err.contains("agnet"));
        assert!(err.contains("agent, vision, authoring, decide-fast, decide"));
    }
}
