//! The on-disk settings file.
//!
//! **Never put a token here.** Tokens live in the OS keychain
//! ([`crate::auth::store`]); this file holds only non-secret preferences and the
//! cached account identity used to locate the keychain entry.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::github::search::Scope;
use crate::model::diff::DiffMode;
use crate::ui::theme::Theme;

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

/// The window layout the user left the app in (Task 53).
///
/// Tree collapse state is per PR and lives in the session file instead.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// The file-tree pane's width.
    pub left_pane: f32,
    /// The metadata pane's width.
    pub right_pane: f32,
    /// Whether the metadata pane is open.
    pub right_open: bool,
    /// Which details tab the pane last showed (redesign Task 13).
    pub details_tab: crate::ui::widgets::metadata_panel::DetailsTab,
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
    /// The theme preference, defaulted the same container-level way.
    pub theme: Theme,
    /// The pane sizes and open/closed state, defaulted the same way.
    pub layout: Layout,
    /// The diff layout choices, defaulted the same way.
    pub diff: DiffPrefs,
}

/// The diff layout choices, remembered across PRs and restarts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffPrefs {
    /// Unified or split.
    pub mode: DiffMode,
    /// Whether long lines wrap.
    pub wrap: bool,
    /// Whether whitespace-only changes are hidden.
    pub hide_whitespace: bool,
}

impl Default for DiffPrefs {
    fn default() -> DiffPrefs {
        DiffPrefs {
            mode: DiffMode::Split,
            wrap: true,
            hide_whitespace: false,
        }
    }
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            host: "github.com".to_owned(),
            client_id: None,
            account: None,
            default_scopes: vec![Scope::ReviewRequested, Scope::Assigned, Scope::Authored],
            theme: Theme::default(),
            // The detail screen's starting pane sizes; the user's own sizes
            // overwrite these as soon as they drag a divider.
            layout: Layout {
                left_pane: 300.0,
                right_pane: 340.0,
                right_open: false,
                details_tab: crate::ui::widgets::metadata_panel::DetailsTab::default(),
            },
            diff: DiffPrefs::default(),
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
    fn details_tab_round_trips() {
        let dir = tempdir();
        let mut s = Settings::default();
        s.layout.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Checks;
        s.save(dir.path()).unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).layout.details_tab,
            crate::ui::widgets::metadata_panel::DetailsTab::Checks
        );
    }

    #[test]
    fn diff_prefs_default_to_split_and_round_trip() {
        assert_eq!(Settings::default().diff.mode, DiffMode::Split);
        assert!(Settings::default().diff.wrap);
        let dir = tempdir();
        let mut s = Settings::default();
        s.diff.mode = DiffMode::Unified;
        s.diff.wrap = false;
        s.save(dir.path()).unwrap();
        let r = Settings::load_or_default(dir.path());
        assert_eq!((r.diff.mode, r.diff.wrap), (DiffMode::Unified, false));
        std::fs::write(dir.path().join("settings.toml"), "host = \"github.com\"\n").unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).diff.mode,
            DiffMode::Split
        ); // older file
    }

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

    #[test]
    fn layout_survives_save_load() {
        let dir = tempdir();
        let mut s = Settings::load_or_default(dir.path());
        s.layout.left_pane = 321.0;
        s.layout.right_open = false;
        s.save(dir.path()).unwrap();
        let r = Settings::load_or_default(dir.path());
        assert_eq!(r.layout.left_pane, 321.0);
        assert!(!r.layout.right_open);
    }
}
