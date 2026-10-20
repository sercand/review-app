//! Neutral `/v1/decisions` types (#85).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{CallMeta, ContentBlock, Error, Message, MessageRole, Result, Usage};

/// One request: shared evidence and the questions to answer about it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecideRequest {
    /// Filled in by a [`crate::DecisionModel`] handle when left unset.
    pub model: Option<String>,
    /// User messages holding only text and images.
    pub input: Vec<Message>,
    pub questions: Vec<Question>,
}

impl DecideRequest {
    pub fn new(input: Vec<Message>, questions: Vec<Question>) -> Self {
        Self {
            model: None,
            input,
            questions,
        }
    }

    /// Checks done before sending.
    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(Error::InvalidRequest(msg));
        if self.questions.is_empty() {
            return invalid("a decision request needs at least one question".into());
        }
        for m in &self.input {
            if m.role != MessageRole::User {
                return invalid("decision input holds only user messages".into());
            }
            for b in &m.content {
                if !matches!(
                    b,
                    ContentBlock::Text { .. } | ContentBlock::Image(_) | ContentBlock::Video(_)
                ) {
                    return invalid("decision input holds only text, images and video".into());
                }
            }
        }
        let mut names = HashSet::new();
        for q in &self.questions {
            if !names.insert(q.name()) {
                return invalid(format!("duplicate question name `{}`", q.name()));
            }
            match q {
                Question::Predicate { .. } => {}
                Question::Choice { name, choices, .. } => {
                    if choices.is_empty() {
                        return invalid(format!("choice `{name}` has no choices"));
                    }
                    let mut values = HashSet::new();
                    for c in choices {
                        if c.value.is_empty() {
                            return invalid(format!("choice `{name}` has an empty value"));
                        }
                        if !values.insert(c.value.as_str()) {
                            return invalid(format!(
                                "choice `{name}` repeats the value `{}`",
                                c.value
                            ));
                        }
                    }
                }
                Question::Score { name, levels, .. } => {
                    if levels.is_empty() {
                        return invalid(format!("score `{name}` has no levels"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// Probability that a condition is true.
    Predicate { name: String, instructions: String },
    /// One option from an unordered set.
    Choice {
        name: String,
        instructions: String,
        choices: Vec<ChoiceOption>,
    },
    /// A score against ordered levels, lowest first.
    Score {
        name: String,
        instructions: String,
        levels: Vec<Level>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub label: String,
    pub description: String,
}

impl Question {
    pub fn predicate(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        Question::Predicate {
            name: name.into(),
            instructions: instructions.into(),
        }
    }

    pub fn choice<V: Into<String>, D: Into<String>>(
        name: impl Into<String>,
        instructions: impl Into<String>,
        choices: impl IntoIterator<Item = (V, D)>,
    ) -> Self {
        Question::Choice {
            name: name.into(),
            instructions: instructions.into(),
            choices: choices
                .into_iter()
                .map(|(v, d)| ChoiceOption {
                    value: v.into(),
                    description: d.into(),
                })
                .collect(),
        }
    }

    pub fn score<L: Into<String>, D: Into<String>>(
        name: impl Into<String>,
        instructions: impl Into<String>,
        levels: impl IntoIterator<Item = (L, D)>,
    ) -> Self {
        Question::Score {
            name: name.into(),
            instructions: instructions.into(),
            levels: levels
                .into_iter()
                .map(|(l, d)| Level {
                    label: l.into(),
                    description: d.into(),
                })
                .collect(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Question::Predicate { name, .. }
            | Question::Choice { name, .. }
            | Question::Score { name, .. } => name,
        }
    }
}

/// The answers to a [`DecideRequest`], one per question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub answers: Vec<Answer>,
    pub usage: Usage,
    pub meta: CallMeta,
}

impl Decision {
    pub fn get(&self, name: &str) -> Option<&Answer> {
        self.answers.iter().find(|a| a.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Predicate {
        name: String,
        probability: f64,
        /// Endpoint-specific extra fields (e.g. `label_mass`).
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Choice {
        name: String,
        choice: String,
        #[serde(default)]
        probabilities: Vec<ChoiceProbability>,
        #[serde(default)]
        confidence: f64,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Score {
        name: String,
        score: f64,
        #[serde(default)]
        probabilities: Vec<LevelProbability>,
        #[serde(default)]
        confidence: f64,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    /// The model declined to answer. Callers treat it as inconclusive, never a pass.
    Refusal { name: String },
}

impl Answer {
    pub fn name(&self) -> &str {
        match self {
            Answer::Predicate { name, .. }
            | Answer::Choice { name, .. }
            | Answer::Score { name, .. }
            | Answer::Refusal { name } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceProbability {
    pub value: String,
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelProbability {
    /// 0-based level index.
    pub value: u32,
    #[serde(default)]
    pub label: String,
    pub probability: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Image;

    fn req(questions: Vec<Question>) -> DecideRequest {
        DecideRequest::new(vec![Message::user_text("evidence")], questions)
    }

    #[test]
    fn valid_request_passes() {
        let r = req(vec![
            Question::predicate("goal", "Is the board visible?"),
            Question::choice("next", "Which?", [("a", "tap A"), ("wait", "wait")]),
            Question::score(
                "glitch",
                "How broken?",
                [("Clean", "fine"), ("Severe", "bad")],
            ),
        ]);
        r.validate().unwrap();
    }

    #[test]
    fn rejects_bad_requests() {
        let bad = [
            req(vec![]),
            req(vec![
                Question::predicate("a", "x"),
                Question::predicate("a", "y"),
            ]),
            req(vec![Question::choice("c", "x", Vec::<(&str, &str)>::new())]),
            req(vec![Question::choice("c", "x", [("a", "1"), ("a", "2")])]),
            req(vec![Question::choice("c", "x", [("", "1")])]),
            req(vec![Question::score("s", "x", Vec::<(&str, &str)>::new())]),
            DecideRequest::new(
                vec![Message::assistant(vec![ContentBlock::text("x")])],
                vec![Question::predicate("a", "x")],
            ),
            DecideRequest::new(
                vec![Message::user(vec![ContentBlock::tool_result("t", vec![])])],
                vec![Question::predicate("a", "x")],
            ),
        ];
        for r in bad {
            assert!(
                matches!(r.validate(), Err(Error::InvalidRequest(_))),
                "{r:?}"
            );
        }
        let with_image = DecideRequest::new(
            vec![Message::user(vec![ContentBlock::Image(Image::from_bytes(
                "image/png",
                b"x",
            ))])],
            vec![Question::predicate("a", "x")],
        );
        with_image.validate().unwrap();
    }
}
