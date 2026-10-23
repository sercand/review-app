//! The question the decision model is asked about a command: the built-in
//! default, or a `decision_schema.json` next to the model's weights.

use std::path::Path;

use serde::Deserialize;

/// Name of the schema file in a model directory.
pub const SCHEMA_FILE: &str = "decision_schema.json";

/// The answer values, as a set; the order in a file does not matter.
const VALUES: [&str; 3] = ["approve", "ask", "deny"];

const DEFAULT_THRESHOLD: f32 = 0.60;

/// One multiple-choice question about a command.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionSchema {
    /// The question's id; the answer carries it back.
    pub question: String,
    /// The question's wording.
    pub instructions: String,
    /// `(value, description)` pairs, in the schema's order.
    pub choices: Vec<(String, String)>,
    /// Least probability of `approve` that still counts as safe.
    pub approve_threshold: f32,
}

pub use crate::state::classifier::SchemaSource;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileChoice {
    value: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSchema {
    question: String,
    instructions: String,
    choices: Vec<FileChoice>,
    approve_threshold: Option<f32>,
}

impl DecisionSchema {
    /// The question as shiso takes it.
    pub fn to_question(&self) -> shiso_decision::schema::Question {
        shiso_decision::schema::Question::Choice {
            name: self.question.clone(),
            instructions: self.instructions.clone(),
            choices: self
                .choices
                .iter()
                .map(|(value, description)| shiso_decision::schema::Choice {
                    value: value.clone(),
                    description: Some(description.clone()),
                })
                .collect(),
        }
    }
}

/// The default question, used when a model ships no schema file.
pub fn builtin_schema() -> DecisionSchema {
    let choice = |value: &str, description: &str| (value.to_owned(), description.to_owned());
    DecisionSchema {
        question: "decision".to_owned(),
        instructions: "Should an AI coding agent run this shell command without asking the user?"
            .to_owned(),
        choices: vec![
            choice(
                "approve",
                "The command only reads or inspects files or state inside the working directory \
                 (listing, searching, viewing, diffing, or a build or test that cannot change \
                 anything outside the checkout) and sends nothing over the network.",
            ),
            choice(
                "ask",
                "The command changes files, installs or runs project code, uses the network, or \
                 its effect is unclear; a person should confirm it first.",
            ),
            choice(
                "deny",
                "The command is destructive or dangerous: it deletes or overwrites data beyond \
                 the task, escalates privileges, exposes secrets or credentials, or downloads \
                 and runs remote code.",
            ),
        ],
        approve_threshold: DEFAULT_THRESHOLD,
    }
}

/// The schema of the model in `model_dir`: its `decision_schema.json`, or the
/// built-in one when the file is absent. A file that is present but invalid is
/// an error, never the default.
pub fn load_schema(model_dir: &Path) -> Result<(DecisionSchema, SchemaSource), String> {
    let path = model_dir.join(SCHEMA_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((builtin_schema(), SchemaSource::BuiltIn));
        }
        Err(e) => return Err(format!("invalid decision schema: {SCHEMA_FILE}: {e}")),
    };
    parse(&text)
        .map(|schema| (schema, SchemaSource::Model))
        .map_err(|why| format!("invalid decision schema: {why}"))
}

fn parse(text: &str) -> Result<DecisionSchema, String> {
    let file: FileSchema = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if file.question.trim().is_empty() {
        return Err("the question is blank".into());
    }
    if file.instructions.trim().is_empty() {
        return Err("the instructions are blank".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for c in &file.choices {
        if !VALUES.contains(&c.value.as_str()) {
            return Err(format!("unknown value `{}`", c.value));
        }
        if seen.contains(&c.value.as_str()) {
            return Err(format!("duplicate value `{}`", c.value));
        }
        if c.description.trim().is_empty() {
            return Err(format!("the description of `{}` is blank", c.value));
        }
        seen.push(&c.value);
    }
    if let Some(missing) = VALUES.iter().find(|v| !seen.contains(v)) {
        return Err(format!("missing value `{missing}`"));
    }
    let approve_threshold = file.approve_threshold.unwrap_or(DEFAULT_THRESHOLD);
    if !(0.0..=1.0).contains(&approve_threshold) {
        return Err(format!(
            "approve_threshold {approve_threshold} is outside 0..=1"
        ));
    }
    Ok(DecisionSchema {
        question: file.question,
        instructions: file.instructions,
        choices: file
            .choices
            .into_iter()
            .map(|c| (c.value, c.description))
            .collect(),
        approve_threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"question":"q","instructions":"Run it?","choices":[
        {"value":"deny","description":"bad"},
        {"value":"approve","description":"fine"},
        {"value":"ask","description":"unsure"}]}"#;

    fn dir_with(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SCHEMA_FILE), text).unwrap();
        dir
    }

    #[test]
    fn missing_file_uses_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let (schema, source) = load_schema(dir.path()).unwrap();
        assert_eq!(source, SchemaSource::BuiltIn);
        assert_eq!(schema, builtin_schema());
        assert_eq!(schema.approve_threshold, 0.60);
        assert_eq!(schema.question, "decision");
        let values: Vec<_> = schema.choices.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(values, ["approve", "ask", "deny"]);
    }

    #[test]
    fn valid_file_is_used() {
        let dir = dir_with(VALID);
        let (schema, source) = load_schema(dir.path()).unwrap();
        assert_eq!(source, SchemaSource::Model);
        assert_eq!(schema.approve_threshold, 0.60);
        assert_eq!(schema.question, "q");
        assert_eq!(schema.choices[0].0, "deny");
        let with = VALID.replace("\"choices\"", "\"approve_threshold\":0.9,\"choices\"");
        let (schema, _) = load_schema(dir_with(&with).path()).unwrap();
        assert_eq!(schema.approve_threshold, 0.9);
    }

    #[test]
    fn to_question_carries_choices_and_descriptions() {
        use shiso_decision::schema::Question;
        let Question::Choice {
            name,
            instructions,
            choices,
        } = builtin_schema().to_question()
        else {
            panic!("not a choice question");
        };
        assert_eq!(name, "decision");
        assert!(instructions.starts_with("Should an AI coding agent"));
        assert_eq!(choices.len(), 3);
        assert!(choices.iter().all(|c| c.description.is_some()));
    }

    #[test]
    fn invalid_files_are_errors() {
        let bom = format!("\u{feff}{VALID}");
        let cases: Vec<(&str, String)> = vec![
            ("bom", bom),
            (
                "trailing comma",
                VALID.replace("\"unsure\"}", "\"unsure\"},"),
            ),
            (
                "yes/no",
                VALID.replace("approve", "yes").replace("deny", "no"),
            ),
            (
                "missing deny",
                r#"{"question":"q","instructions":"i","choices":[
                {"value":"approve","description":"a"},{"value":"ask","description":"b"}]}"#
                    .to_owned(),
            ),
            (
                "duplicate",
                VALID.replace("\"value\":\"deny\"", "\"value\":\"ask\""),
            ),
            (
                "threshold high",
                VALID.replace("\"choices\"", "\"approve_threshold\":1.5,\"choices\""),
            ),
            (
                "threshold negative",
                VALID.replace("\"choices\"", "\"approve_threshold\":-0.1,\"choices\""),
            ),
            ("empty description", VALID.replace("\"bad\"", "\"\"")),
            ("blank instructions", VALID.replace("Run it?", "  ")),
            ("blank question", VALID.replace("\"q\"", "\"\"")),
            (
                "unknown field",
                VALID.replace("\"question\"", "\"extra\":1,\"question\""),
            ),
        ];
        for (name, text) in cases {
            let dir = dir_with(&text);
            let err = load_schema(dir.path()).expect_err(name);
            assert!(
                err.starts_with("invalid decision schema: "),
                "{name}: {err}"
            );
        }
    }
}
