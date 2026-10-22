//! What the bash approval engine needs beyond the command itself.
//!
//! One struct on [`crate::ai::tools::ToolCx`] so later additions (a model
//! stage, user rules) extend it without touching every `ToolCx { … }` literal.

/// Configuration for `review_approval::classify`, shared by every agent.
#[derive(Clone, Default)]
pub struct ApprovalSetup {
    /// Path-component deny patterns the user added on top of
    /// [`review_approval::default_deny_paths`].
    pub deny_paths: Vec<String>,
}

impl ApprovalSetup {
    /// The built-in deny patterns followed by the user's, without duplicates.
    pub fn effective_deny_paths(&self) -> Vec<String> {
        let mut paths = review_approval::default_deny_paths();
        for path in &self.deny_paths {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalSetup;

    #[test]
    fn effective_deny_paths_extends_defaults_without_duplicates() {
        let defaults = review_approval::default_deny_paths();
        assert_eq!(ApprovalSetup::default().effective_deny_paths(), defaults);

        let setup = ApprovalSetup {
            deny_paths: vec![
                ".ssh".to_string(),
                "secrets".to_string(),
                "secrets".to_string(),
            ],
        };
        let got = setup.effective_deny_paths();
        assert_eq!(got.len(), defaults.len() + 1, "{got:?}");
        assert_eq!(got.iter().filter(|p| *p == ".ssh").count(), 1);
        assert_eq!(got.last().map(String::as_str), Some("secrets"));
    }
}
