//! Checked constructors for [`Tool`].

use serde_json::Value;

use crate::{Error, Result, Tool};

/// Both APIs accept `^[a-zA-Z0-9_-]{1,64}$`.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Tool {
    /// A tool from a JSON Schema, e.g. an ext command's schema from the device `hello`.
    ///
    /// The name must match `^[a-zA-Z0-9_-]{1,64}$` and the schema must be an object schema.
    /// Top-level `$schema` and `title` are dropped; `$defs` and `$ref` are kept.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Result<Tool> {
        let name = name.into();
        if !valid_name(&name) {
            return Err(Error::InvalidRequest(format!(
                "tool name `{name}` must match ^[a-zA-Z0-9_-]{{1,64}}$"
            )));
        }
        let Value::Object(mut schema) = input_schema else {
            return Err(Error::InvalidRequest(format!(
                "tool `{name}`: input schema must be a JSON object"
            )));
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(Error::InvalidRequest(format!(
                "tool `{name}`: input schema must have \"type\": \"object\""
            )));
        }
        schema.remove("$schema");
        schema.remove("title");
        Ok(Tool {
            name,
            description: description.into(),
            input_schema: Value::Object(schema),
        })
    }

    /// A tool whose input is a Rust type, e.g. the agent's `done` or `report` arguments.
    pub fn from_type<T: schemars::JsonSchema>(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Tool> {
        let schema = serde_json::to_value(schemars::schema_for!(T))
            .map_err(|e| Error::InvalidRequest(format!("tool schema: {e}")))?;
        Tool::new(name, description, schema)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    struct Point {
        x: i32,
        y: i32,
    }

    /// Finish the step.
    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    struct Done {
        /// Why the step ended.
        reason: String,
        verdict: Verdict,
        at: Option<Point>,
    }

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum Verdict {
        Passed,
        Failed,
    }

    #[test]
    fn from_type_builds_an_object_schema() {
        let t = Tool::from_type::<Done>("done", "Finish the step.").unwrap();
        let s = &t.input_schema;
        assert_eq!(s["type"], "object");
        assert!(s.get("$schema").is_none());
        assert!(s.get("title").is_none());
        assert_eq!(s["properties"]["reason"]["type"], "string");
        assert_eq!(
            s["properties"]["reason"]["description"],
            "Why the step ended."
        );
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"reason") && required.contains(&"verdict"));
        assert!(!required.contains(&"at"));
        assert!(s.get("$defs").is_some(), "nested types stay in $defs: {s}");
    }

    #[test]
    fn new_checks_name_and_schema() {
        let obj = json!({"type": "object", "properties": {}});
        Tool::new("tap", "Tap.", obj.clone()).unwrap();
        Tool::new("ext_kedis-canvas_1", "x", obj.clone()).unwrap();
        for bad in ["", "has space", "dot.name", &"x".repeat(65)] {
            assert!(Tool::new(bad, "x", obj.clone()).is_err(), "{bad:?}");
        }
        assert!(Tool::new("t", "x", json!({"type": "string"})).is_err());
        assert!(Tool::new("t", "x", json!([1])).is_err());
        let t = Tool::new(
            "t",
            "x",
            json!({"$schema": "s", "title": "T", "type": "object"}),
        )
        .unwrap();
        assert_eq!(t.input_schema, json!({"type": "object"}));
    }
}
