//! Repository identity.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A GitHub repository.
///
/// Case is preserved exactly as GitHub reported it (so it can be shown and sent
/// back verbatim); use [`RepoRef::id`] for the case-insensitive key used in
/// storage paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// The user or organization that owns the repository.
    pub owner: String,
    /// The repository name.
    pub name: String,
}

impl RepoRef {
    /// Build a reference, preserving the case of both parts.
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// The lowercased `owner/name` key used for storage and comparisons.
    pub fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name).to_lowercase()
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ref_keeps_case_but_keys_lowercase() {
        let r = RepoRef::new("Octo", "Repo");
        assert_eq!(r.to_string(), "Octo/Repo");
        assert_eq!(r.id(), "octo/repo");
    }
}
