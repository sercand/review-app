//! The on-disk settings file.
//!
//! **Never put a token here.** Tokens live in the OS keychain
//! ([`crate::auth::store`]); this file holds only non-secret preferences and the
//! cached account identity used to locate the keychain entry.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::github::search::Scope;

/// The cached identity of the signed-in account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// The login name, e.g. `octocat`.
    pub login: String,
    /// The display name, when GitHub reports one.
    pub name: Option<String>,
    /// The avatar image URL, when GitHub reports one.
    pub avatar_url: Option<String>,
    /// The scopes GitHub granted, e.g. `["repo", "read:org"]`.
    pub scopes: Vec<String>,
}

/// User settings, persisted as `settings.toml` in the config directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The GitHub host to talk to. `github.com` unless overridden.
    pub host: String,
    /// An OAuth App client ID override, for forks and Enterprise hosts.
    pub client_id: Option<String>,
    /// The cached account, or `None` when signed out.
    pub account: Option<Account>,
    /// The scopes the PR list starts with.
    ///
    /// A file written by an older version has no key here; the container's
    /// `#[serde(default)]` then supplies [`Settings::default`]'s three scopes.
    /// (A field-level `#[serde(default)]` would override that with `Vec::new()`
    /// and leave the list showing nothing.)
    pub default_scopes: Vec<Scope>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            host: "github.com".to_owned(),
            client_id: None,
            account: None,
            default_scopes: vec![Scope::ReviewRequested, Scope::Assigned, Scope::Authored],
        }
    }
}

impl Settings {
    /// The file name within the config directory.
    const FILE: &'static str = "settings.toml";

    /// Load `settings.toml` from `dir`, falling back to defaults.
    ///
    /// A missing file is normal (first run). An unparsable file is logged and
    /// then ignored, so a corrupt settings file can never brick startup.
    pub fn load_or_default(dir: &Path) -> Settings {
        let path = dir.join(Self::FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no settings file; using defaults");
                return Settings::default();
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read settings; using defaults");
                return Settings::default();
            }
        };

        match toml::from_str(&raw) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not parse settings; using defaults");
                Settings::default()
            }
        }
    }

    /// Write these settings to `dir` atomically.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let path = dir.join(Self::FILE);
        let raw = toml::to_string(self)?;
        super::write_atomic(&path, raw.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    #[test]
    fn settings_default_round_trip_and_partial_files() {
        let dir = tempdir();
        let mut s = Settings::load_or_default(dir.path());
        assert_eq!(s.host, "github.com");
        s.account = Some(Account {
            login: "octocat".into(),
            ..Default::default()
        });
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load_or_default(dir.path()), s);

        std::fs::write(
            dir.path().join("settings.toml"),
            "host = \"ghe.example.com\"\n",
        )
        .unwrap();
        let partial = Settings::load_or_default(dir.path());
        assert_eq!(partial.host, "ghe.example.com");
        assert!(partial.account.is_none());

        std::fs::write(dir.path().join("settings.toml"), "not = [valid").unwrap();
        assert_eq!(Settings::load_or_default(dir.path()).host, "github.com");
    }

    #[test]
    fn default_scopes_are_the_three_queries_and_survive_a_file_without_them() {
        let three = vec![Scope::ReviewRequested, Scope::Assigned, Scope::Authored];
        assert_eq!(Settings::default().default_scopes, three);

        // A file written before `default_scopes` existed keeps the defaults.
        let dir = tempdir();
        std::fs::write(dir.path().join("settings.toml"), "host = \"github.com\"\n").unwrap();
        assert_eq!(Settings::load_or_default(dir.path()).default_scopes, three);

        // An explicit selection round-trips.
        let explicit = Settings {
            default_scopes: vec![Scope::Assigned],
            ..Default::default()
        };
        explicit.save(dir.path()).unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).default_scopes,
            vec![Scope::Assigned]
        );
    }
}
