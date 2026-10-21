//! The on-disk settings file.
//!
//! **Never put a token here.** Tokens live in the OS keychain
//! ([`crate::auth::store`]); this file holds only non-secret preferences and the
//! cached account identity used to locate the keychain entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Which details tab the pane last showed (redesign Task 13). The AI
    /// tab is unix-only, so on Windows an `Ai` selection loads as
    /// `Overview` (Plan 3).
    #[serde(default, deserialize_with = "load_details_tab")]
    pub details_tab: crate::ui::widgets::metadata_panel::DetailsTab,
}

/// [`Layout`]'s tab loader: the stored tab as-is on unix, with the AI tab
/// flattened to `Overview` on Windows (see
/// [`DetailsTab::for_platform`](crate::ui::widgets::metadata_panel::DetailsTab::for_platform)).
fn load_details_tab<'de, D>(
    deserializer: D,
) -> Result<crate::ui::widgets::metadata_panel::DetailsTab, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(crate::ui::widgets::metadata_panel::DetailsTab::deserialize(deserializer)?.for_platform())
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
    /// The code font size in points: 12, 13 or 14; anything else loads as
    /// 13 (redesign Task 14).
    pub code_font_size: f32,
    /// The five most recent repo/org constraints applied in the PR list,
    /// most recent first (redesign Task 15).
    pub recent_constraints: Vec<String>,
    /// Whether the PR list groups rows under repository headers
    /// (redesign Task 15).
    pub group_by_repo: bool,
    /// The merge method the merge popover starts with, clamped to what the
    /// repository allows.
    pub merge_method: crate::github::merge::MergeMethod,
    /// The AI review settings (spec §10). API keys are not here; they live in
    /// the keychain ([`crate::auth::store::llm_key`]).
    pub ai: AiSettings,
}

/// The wire protocol a provider speaks (spec §10).
///
/// A hand-edited file may name a protocol this build doesn't speak; `#[serde(
/// other)]` loads it as [`LlmApi::Unsupported`] so the settings file still
/// parses and the UI can say so instead of resetting everything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LlmApi {
    /// The Anthropic Messages API.
    #[serde(rename = "anthropic-messages")]
    #[default]
    AnthropicMessages,
    /// The OpenAI Responses API.
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
    /// Anything else: configured, but this build cannot talk to it.
    #[serde(other)]
    Unsupported,
}

/// One configured LLM endpoint. The API key lives in the keychain under the
/// account `llm:<name>` ([`crate::auth::store::llm_key`]), never here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderSettings {
    /// The user-chosen name, e.g. `work`; also the keychain account suffix.
    pub name: String,
    /// Which protocol to speak.
    pub api: LlmApi,
    /// The endpoint base URL.
    pub base_url: String,
    /// Optional extra headers to send with every request.
    pub headers: BTreeMap<String, String>,
}

/// How much reasoning effort to ask the model for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    /// Minimal thinking.
    Low,
    /// The model's default effort.
    Medium,
    /// Extended thinking.
    High,
}

/// A model choice: which provider, which model, and its size limits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    /// The [`ProviderSettings::name`] to route through.
    pub provider: String,
    /// The model id to request.
    pub model: String,
    /// The model's context window in tokens.
    pub context_window: u32,
    /// The per-response output cap in tokens.
    pub max_tokens: u32,
    /// Optional reasoning effort (spec §10).
    pub reasoning: Option<ReasoningLevel>,
}

impl Default for ModelSettings {
    fn default() -> ModelSettings {
        ModelSettings {
            provider: String::new(),
            model: String::new(),
            context_window: 200_000,
            max_tokens: 8192,
            reasoning: None,
        }
    }
}

/// The model picks: the main reviewer and the optional cheaper helper.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiModels {
    /// The main review/chat model.
    pub default: Option<ModelSettings>,
    /// A cheaper model for lookup helpers, when configured.
    pub lookup: Option<ModelSettings>,
}

/// Per-repository AI settings: where the checkout lives and what to run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoAiSettings {
    /// The repository key, `host/owner/name` (matched case-insensitively).
    pub repo: String,
    /// The local checkout folder.
    pub path: PathBuf,
    /// Test/lint commands the agents may run in the sandbox (§8.5).
    pub commands: Vec<String>,
    /// Commands run outside the sandbox as the user, e.g. `cargo fetch`.
    pub prepare: Vec<String>,
    /// Extra paths the sandbox may write.
    pub extra_write_paths: Vec<PathBuf>,
    /// Extra paths the sandbox may not read.
    pub extra_deny_read: Vec<PathBuf>,
}

/// The `[ai]` table: providers, models, per-repo settings (§10).
///
/// Never put a secret here; API keys live in the keychain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// How many reviewers may run at once.
    pub max_parallel_reviewers: u32,
    /// Token budget per run; 0 means unlimited.
    pub run_token_budget: u64,
    /// Fraction of the context window that triggers compaction.
    pub context_threshold: f32,
    /// Environment variable names the sandbox may pass through.
    pub extra_env_allow: Vec<String>,
    /// The configured endpoints.
    pub providers: Vec<ProviderSettings>,
    /// The model picks.
    pub models: AiModels,
    /// The per-repository settings.
    pub repos: Vec<RepoAiSettings>,
}

impl Default for AiSettings {
    fn default() -> AiSettings {
        AiSettings {
            max_parallel_reviewers: 2,
            run_token_budget: 0,
            context_threshold: 0.7,
            extra_env_allow: Vec::new(),
            providers: Vec::new(),
            models: AiModels::default(),
            repos: Vec::new(),
        }
    }
}

impl AiSettings {
    /// The settings for `host/owner/name`, matched case-insensitively.
    pub fn repo(&self, host: &str, owner: &str, name: &str) -> Option<&RepoAiSettings> {
        let key = format!("{host}/{owner}/{name}").to_lowercase();
        self.repos
            .iter()
            .find(|repo| repo.repo.to_lowercase() == key)
    }

    /// The provider with this exact `name`.
    pub fn provider(&self, name: &str) -> Option<&ProviderSettings> {
        self.providers.iter().find(|p| p.name == name)
    }
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
            code_font_size: 13.0,
            recent_constraints: Vec::new(),
            group_by_repo: false,
            merge_method: crate::github::merge::MergeMethod::default(),
            ai: AiSettings::default(),
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

        match toml::from_str::<Settings>(&raw) {
            Ok(mut settings) => {
                // Only the offered sizes are valid; anything else (an old
                // file, a hand edit) loads as the default (Task 14).
                if ![12.0, 13.0, 14.0].contains(&settings.code_font_size) {
                    settings.code_font_size = 13.0;
                }
                settings
            }
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

    /// Remember `entry` as the newest recent repo/org constraint: trimmed,
    /// de-duplicated, most recent first, at most five (Task 15).
    pub fn push_recent(&mut self, entry: String) {
        crate::state::pr_list::push_recent(&mut self.recent_constraints, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    #[test]
    fn code_font_size_is_clamped_to_the_offered_sizes() {
        let dir = tempdir();
        std::fs::write(dir.path().join("settings.toml"), "code_font_size = 30.0\n").unwrap();
        assert_eq!(Settings::load_or_default(dir.path()).code_font_size, 13.0);
        std::fs::write(dir.path().join("settings.toml"), "code_font_size = 14.0\n").unwrap();
        assert_eq!(Settings::load_or_default(dir.path()).code_font_size, 14.0);
    }

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

    /// The AI tab is unix-only: on Windows a saved `Ai` selection loads as
    /// `Overview`, so the pane never opens an empty tab (Plan 3).
    #[test]
    #[cfg(not(unix))]
    fn ai_tab_hidden_on_windows_loads_as_overview() {
        let dir = tempdir();
        std::fs::write(
            dir.path().join("settings.toml"),
            "[layout]\ndetails_tab = \"Ai\"\n",
        )
        .unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).layout.details_tab,
            crate::ui::widgets::metadata_panel::DetailsTab::Overview
        );
    }

    #[test]
    fn merge_method_defaults_to_merge_and_round_trips() {
        use crate::github::merge::MergeMethod;
        assert_eq!(Settings::default().merge_method, MergeMethod::Merge);
        let dir = tempdir();
        let s = Settings {
            merge_method: MergeMethod::Squash,
            ..Settings::default()
        };
        s.save(dir.path()).unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).merge_method,
            MergeMethod::Squash
        );
        // A file written before the feature loads with the default.
        std::fs::write(dir.path().join("settings.toml"), "host = \"github.com\"\n").unwrap();
        assert_eq!(
            Settings::load_or_default(dir.path()).merge_method,
            MergeMethod::Merge
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
    fn recent_constraints_are_most_recent_first_and_capped() {
        let mut s = Settings::default();
        s.push_recent("octo/repo".into());
        s.push_recent("acme".into());
        s.push_recent("octo/repo".into()); // moves to front, no duplicate
        assert_eq!(s.recent_constraints[0], "octo/repo");
        for i in 0..10 {
            s.push_recent(format!("org{i}"));
        }
        assert_eq!(s.recent_constraints.len(), 5);
    }

    /// The `settings.toml` example from spec §10, copied verbatim.
    const SPEC_EXAMPLE: &str = r#"
[ai]
max_parallel_reviewers = 2
run_token_budget = 0            # 0 = unlimited
context_threshold = 0.7

[[ai.providers]]
name = "work"
api = "anthropic-messages"      # or "openai-responses"
base_url = "https://llm.example.com"
headers = {}                    # optional extra headers

[ai.models.default]
provider = "work"
model = "model-id"
context_window = 200000
max_tokens = 8192
reasoning = "medium"            # optional: low | medium | high

# optional: a cheaper model for lookup helpers
# [ai.models.lookup]

[[ai.repos]]
repo = "github.com/owner/name"
path = "/Users/me/src/owner/name"
commands = ["cargo test", "cargo clippy", "cargo fmt --check"]
prepare = ["cargo fetch"]       # run outside the sandbox as the user (§8.5)
extra_write_paths = []
extra_deny_read = []
"#;

    #[test]
    fn ai_settings_parse_the_spec_example() {
        let s: Settings = toml::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(s.ai.max_parallel_reviewers, 2);
        assert_eq!(s.ai.run_token_budget, 0);
        assert_eq!(s.ai.context_threshold, 0.7);
        assert_eq!(s.ai.providers[0].api, LlmApi::AnthropicMessages);
        assert_eq!(s.ai.providers[0].name, "work");
        assert_eq!(s.ai.providers[0].base_url, "https://llm.example.com");
        assert_eq!(
            s.ai.provider("work").unwrap().api,
            LlmApi::AnthropicMessages
        );
        assert!(s.ai.provider("nope").is_none());
        let default = s.ai.models.default.as_ref().unwrap();
        assert_eq!(default.provider, "work");
        assert_eq!(default.model, "model-id");
        assert_eq!(default.context_window, 200_000);
        assert_eq!(default.max_tokens, 8192);
        assert_eq!(default.reasoning, Some(ReasoningLevel::Medium));
        assert!(s.ai.models.lookup.is_none());
        // Repo matching is case-insensitive.
        let repo = s.ai.repo("GitHub.com", "Owner", "Name").unwrap();
        assert_eq!(repo.prepare, vec!["cargo fetch"]);
        assert_eq!(repo.commands.len(), 3);
        assert!(s.ai.repo("gitlab.com", "Owner", "Name").is_none());
    }

    #[test]
    fn an_old_settings_file_without_ai_still_loads() {
        let dir = tempdir();
        std::fs::write(
            dir.path().join("settings.toml"),
            "host = \"ghe.example.com\"\ncode_font_size = 14.0\n",
        )
        .unwrap();
        let s = Settings::load_or_default(dir.path());
        assert_eq!(s.ai, AiSettings::default());
        assert_eq!(s.host, "ghe.example.com");
        assert_eq!(s.code_font_size, 14.0);
    }

    #[test]
    fn an_unknown_api_does_not_reset_the_settings_file() {
        // A hand-edited `api` value loads as `Unsupported` rather than failing
        // the whole file, so the rest of the settings survive.
        let dir = tempdir();
        std::fs::write(
            dir.path().join("settings.toml"),
            "theme = \"Dark\"\n\n[[ai.providers]]\nname = \"x\"\napi = \"chat-completions\"\n",
        )
        .unwrap();
        let s = Settings::load_or_default(dir.path());
        assert_eq!(s.theme, Theme::Dark);
        assert_eq!(s.ai.providers.len(), 1);
        assert_eq!(s.ai.providers[0].api, LlmApi::Unsupported);
    }

    #[test]
    fn ai_settings_round_trip() {
        let s: Settings = toml::from_str(SPEC_EXAMPLE).unwrap();
        let raw = toml::to_string(&s).unwrap();
        assert_eq!(toml::from_str::<Settings>(&raw).unwrap(), s);
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
