//! Linking a local folder to the GitHub repository it is a clone of.
//!
//! The link is the [`RepoAiSettings::path`] of the repository's entry in
//! `settings.ai.repos`, keyed by `host/owner/name` (stored lowercase, matched
//! case-insensitively). Unlinking clears only the path, so relinking a repo
//! keeps the commands, prepare commands and extra paths the user configured.

use std::path::{Path, PathBuf};

use super::checkout::RemoteRepo;
use crate::persist::settings::{AiSettings, RepoAiSettings};

/// Whether a linked folder can still be used (Local repos, the PR AI tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderStatus {
    /// The folder exists and looks like a git repository.
    Ok,
    /// The folder does not exist: deleted, moved or unmounted since it was
    /// linked.
    Missing,
    /// The folder exists but is not a git repository.
    NotARepo,
}

/// Link `path` as `repo`'s local folder.
///
/// An entry that already exists for the repository is updated in place, so
/// the commands, prepare commands and extra paths it holds survive a relink;
/// a repository with no entry gets a fresh row. The key is stored lowercase
/// and matched case-insensitively, so `Acme/Widgets` links the entry
/// `acme/widgets` rather than creating a second one.
pub fn link_repo(ai: &mut AiSettings, repo: &RemoteRepo, path: PathBuf) {
    let key = repo.key().to_lowercase();
    match ai
        .repos
        .iter_mut()
        .find(|entry| entry.repo.to_lowercase() == key)
    {
        Some(entry) => {
            entry.repo = key;
            entry.path = path;
        }
        None => ai.repos.push(RepoAiSettings {
            repo: key,
            path,
            ..Default::default()
        }),
    }
}

/// Unlink the folder of `key`, keeping everything else the entry holds: the
/// commands, prepare commands and extra paths stay for a relink. An unknown
/// key changes nothing.
pub fn unlink_repo(ai: &mut AiSettings, key: &str) {
    let key = key.to_lowercase();
    if let Some(entry) = ai
        .repos
        .iter_mut()
        .find(|entry| entry.repo.to_lowercase() == key)
    {
        entry.path = PathBuf::new();
    }
}

/// The entries linked to folders on `host`, sorted by repository key.
///
/// An entry without a path is not linked (the PR AI tab can hold commands for
/// a repository before a folder is chosen), and another host's entries are
/// not ours to show.
pub fn linked_repos<'a>(ai: &'a AiSettings, host: &str) -> Vec<&'a RepoAiSettings> {
    let host = host.to_lowercase();
    let mut repos: Vec<&RepoAiSettings> = ai
        .repos
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .filter(|entry| {
            entry
                .repo
                .split('/')
                .next()
                .is_some_and(|entry_host| entry_host.to_lowercase() == host)
        })
        .collect();
    repos.sort_by_cached_key(|entry| entry.repo.to_lowercase());
    repos
}

/// Whether `path` is a usable linked folder.
///
/// Deliberately filesystem-only: the Local repos list and the PR AI tab ask
/// this while painting, and a `git` child process per row per frame would
/// block the GUI thread. The clone check itself is
/// [`read_origin_repo`](super::checkout::read_origin_repo), run on the worker.
pub fn folder_status(path: &Path) -> FolderStatus {
    // A path that names no existing directory (an empty path included) has
    // no folder; `.git` is a directory in a normal clone and a file in a
    // worktree or submodule, so its presence is all that is needed here.
    if !path.is_dir() {
        FolderStatus::Missing
    } else if path.join(".git").exists() {
        FolderStatus::Ok
    } else {
        FolderStatus::NotARepo
    }
}

/// What still stands between the user and Start, in the order the PR AI tab
/// shows them: a model to run, a provider that serves it with a stored key,
/// and a local folder to review in.
///
/// `has_key` is injected rather than read here: the keychain is a blocking
/// call, and the panel asks this every frame from a cached probe. The only I/O
/// this function does is [`folder_status`], which is filesystem-only by design
/// (see its docs). The repository key is matched case-insensitively like
/// [`link_repo`].
///
/// A `LlmApi::Unsupported` provider is not called out separately: it is
/// configured, so the model pre-check passes, and the provider's connection
/// test is what reports that the build cannot talk to it.
pub fn start_blockers(
    ai: &AiSettings,
    repo_key: &str,
    has_key: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut blockers = Vec::new();

    match ai.models.default.as_ref() {
        None => blockers.push("Choose a default model in Settings → AI.".to_string()),
        Some(model) => match ai.provider(&model.provider) {
            None => blockers.push(format!(
                "The default model's provider \"{}\" is not configured.",
                model.provider
            )),
            Some(provider) => {
                if !has_key(&provider.name) {
                    blockers.push(format!(
                        "The API key for provider \"{}\" is missing.",
                        provider.name
                    ));
                }
            }
        },
    }

    let linked = ai
        .repos
        .iter()
        .find(|entry| entry.repo.to_lowercase() == repo_key.to_lowercase())
        .filter(|entry| !entry.path.as_os_str().is_empty());
    match linked {
        None => blockers.push(format!("Link a local folder for {repo_key}.")),
        Some(entry) => match folder_status(&entry.path) {
            FolderStatus::Ok => {}
            FolderStatus::Missing => {
                blockers.push(format!("The linked folder for {repo_key} is missing."));
            }
            FolderStatus::NotARepo => blockers.push(format!(
                "The linked folder for {repo_key} is not a git repository."
            )),
        },
    }

    blockers
}

/// The provider whose stored key gates Start: the one serving the default
/// model, when both a default model and that provider are configured.
///
/// [`start_blockers`] reports a *missing* key; the app uses this to tell a
/// missing key from one whose presence the probe has not answered yet — the
/// keychain read blocks, so presence arrives from a cached probe, and a name
/// the probe has not answered is unknown, not missing.
pub fn key_gated_provider(ai: &AiSettings) -> Option<String> {
    let model = ai.models.default.as_ref()?;
    ai.provider(&model.provider)
        .map(|provider| provider.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::settings::{AiModels, LlmApi, ModelSettings, ProviderSettings};
    use crate::test_support::tempdir;

    fn remote(host: &str, owner: &str, name: &str) -> RemoteRepo {
        RemoteRepo {
            host: host.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
        }
    }

    fn entry(repo: &str, path: PathBuf) -> RepoAiSettings {
        RepoAiSettings {
            repo: repo.to_string(),
            path,
            ..Default::default()
        }
    }

    /// Linking the same repository twice under different spellings updates
    /// the one entry instead of adding a second, and keeps its commands.
    #[test]
    fn link_is_case_insensitive_and_deduplicates() {
        let mut ai = AiSettings {
            repos: vec![RepoAiSettings {
                commands: vec!["cargo test".to_string()],
                prepare: vec!["cargo fetch".to_string()],
                ..entry("github.com/acme/widgets", PathBuf::from("/old"))
            }],
            ..Default::default()
        };

        link_repo(
            &mut ai,
            &remote("GitHub.com", "Acme", "Widgets"),
            PathBuf::from("/new"),
        );
        assert_eq!(ai.repos.len(), 1, "the existing row is updated in place");
        assert_eq!(ai.repos[0].repo, "github.com/acme/widgets");
        assert_eq!(ai.repos[0].path, PathBuf::from("/new"));
        assert_eq!(ai.repos[0].commands, vec!["cargo test".to_string()]);
        assert_eq!(ai.repos[0].prepare, vec!["cargo fetch".to_string()]);

        link_repo(
            &mut ai,
            &remote("github.com", "acme", "other"),
            PathBuf::from("/other"),
        );
        assert_eq!(ai.repos.len(), 2, "a new repo gets its own row");
        assert_eq!(ai.repos[1].repo, "github.com/acme/other");
        assert_eq!(ai.repos[1].path, PathBuf::from("/other"));
        assert!(ai.repos[1].commands.is_empty(), "nothing to keep yet");
    }

    /// Unlinking clears the folder only: the commands, prepare commands and
    /// extra paths stay, so relinking restores them.
    #[test]
    fn unlink_keeps_commands_and_prepare() {
        let mut ai = AiSettings {
            repos: vec![RepoAiSettings {
                repo: "github.com/acme/widgets".to_string(),
                path: PathBuf::from("/src/widgets"),
                commands: vec!["cargo test".to_string()],
                prepare: vec!["cargo fetch".to_string()],
                extra_write_paths: vec![PathBuf::from("/src/widgets/target")],
                extra_deny_read: vec![PathBuf::from("/secrets")],
            }],
            ..Default::default()
        };

        unlink_repo(&mut ai, "GitHub.com/Acme/Widgets");
        assert_eq!(ai.repos.len(), 1, "the row stays, with no folder");
        assert!(ai.repos[0].path.as_os_str().is_empty());
        assert_eq!(ai.repos[0].commands, vec!["cargo test".to_string()]);
        assert_eq!(ai.repos[0].prepare, vec!["cargo fetch".to_string()]);
        assert_eq!(
            ai.repos[0].extra_write_paths,
            vec![PathBuf::from("/src/widgets/target")]
        );
        assert_eq!(ai.repos[0].extra_deny_read, vec![PathBuf::from("/secrets")]);

        // An unknown key changes nothing.
        unlink_repo(&mut ai, "github.com/acme/other");
        assert_eq!(ai.repos.len(), 1);
        assert!(ai.repos[0].path.as_os_str().is_empty());
    }

    /// The Local repos list shows linked entries of the signed-in host only,
    /// sorted by repository key.
    #[test]
    fn linked_repos_skips_empty_path_and_other_hosts() {
        let ai = AiSettings {
            repos: vec![
                entry("github.com/acme/zeta", PathBuf::from("/z")),
                entry("gitlab.com/acme/alpha", PathBuf::from("/g")),
                entry("github.com/acme/beta", PathBuf::new()),
                entry("github.com/acme/alpha", PathBuf::from("/a")),
            ],
            ..Default::default()
        };

        let linked: Vec<&str> = linked_repos(&ai, "GitHub.com")
            .iter()
            .map(|repo| repo.repo.as_str())
            .collect();
        assert_eq!(
            linked,
            vec!["github.com/acme/alpha", "github.com/acme/zeta"]
        );
        assert_eq!(linked_repos(&ai, "ghe.example.com").len(), 0);
    }

    /// A linked folder can be gone, not a repository, or ready to use.
    #[test]
    fn folder_status_missing_and_not_a_repo() {
        let dir = tempdir();
        assert_eq!(
            folder_status(&dir.path().join("gone")),
            FolderStatus::Missing
        );

        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(folder_status(&plain), FolderStatus::NotARepo);

        let clone = dir.path().join("clone");
        std::fs::create_dir_all(&clone).unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&clone)
            .args(["init", "-b", "main"])
            .output()
            .expect("git on PATH");
        assert!(status.status.success(), "git init failed");
        assert_eq!(folder_status(&clone), FolderStatus::Ok);
    }

    fn provider(name: &str) -> ProviderSettings {
        ProviderSettings {
            name: name.to_string(),
            api: LlmApi::AnthropicMessages,
            ..Default::default()
        }
    }

    fn default_model(provider: &str) -> ModelSettings {
        ModelSettings {
            provider: provider.to_string(),
            model: "claude-test".to_string(),
            ..Default::default()
        }
    }

    /// Every missing piece is named, in the order the Start pre-check shows
    /// them: model, provider, key, link, folder.
    #[test]
    fn start_blockers_lists_each_missing_piece() {
        let dir = tempdir();
        let key = "GitHub.com/Acme/Widgets";
        let no_key = |_: &str| false;
        let has_key = |_: &str| true;

        // Nothing configured: no default model to check a provider or key for,
        // and nothing linked.
        let mut ai = AiSettings::default();
        let blockers = start_blockers(&ai, key, &no_key);
        assert_eq!(blockers.len(), 2, "{blockers:?}");
        assert!(blockers[0].contains("default model"), "{blockers:?}");
        assert!(blockers[1].contains("local folder"), "{blockers:?}");

        // A default model whose provider is not in `ai.providers`.
        ai.models = AiModels {
            default: Some(default_model("work")),
            ..Default::default()
        };
        let blockers = start_blockers(&ai, key, &no_key);
        assert_eq!(blockers.len(), 2, "{blockers:?}");
        assert!(blockers[0].contains("\"work\""), "{blockers:?}");
        assert!(blockers[0].contains("not configured"), "{blockers:?}");

        // The provider exists but has no stored key.
        ai.providers.push(provider("work"));
        let blockers = start_blockers(&ai, key, &no_key);
        assert_eq!(blockers.len(), 2, "{blockers:?}");
        assert!(blockers[0].contains("API key"), "{blockers:?}");
        assert!(blockers[0].contains("\"work\""), "{blockers:?}");

        // Key stored, but no entry for the repository at all.
        let blockers = start_blockers(&ai, key, &has_key);
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("Link a local folder"), "{blockers:?}");
        assert!(blockers[0].contains(key), "{blockers:?}");

        // The entry's key is matched case-insensitively, and its folder can be
        // gone or not a repository.
        ai.repos
            .push(entry("github.com/Acme/Widgets", dir.path().join("gone")));
        let blockers = start_blockers(&ai, key, &has_key);
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("is missing"), "{blockers:?}");

        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        ai.repos[0].path = plain;
        let blockers = start_blockers(&ai, key, &has_key);
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("not a git repository"), "{blockers:?}");

        // An entry whose path was cleared is not linked, even with commands.
        ai.repos[0].path = PathBuf::new();
        let blockers = start_blockers(&ai, key, &has_key);
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("Link a local folder"), "{blockers:?}");
    }

    /// A configured model, key and clone leave nothing to fix.
    #[test]
    fn start_blockers_empty_when_ready() {
        let dir = tempdir();
        let clone = dir.path().join("widgets");
        std::fs::create_dir_all(&clone).unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&clone)
            .args(["init", "-b", "main"])
            .output()
            .expect("git on PATH");
        assert!(status.status.success(), "git init failed");

        let ai = AiSettings {
            providers: vec![provider("work")],
            models: AiModels {
                default: Some(default_model("work")),
                ..Default::default()
            },
            repos: vec![entry("github.com/acme/widgets", clone.clone())],
            ..Default::default()
        };

        let seen = std::cell::RefCell::new(Vec::new());
        let has_key = |name: &str| {
            seen.borrow_mut().push(name.to_string());
            name == "work"
        };
        assert!(
            start_blockers(&ai, "github.com/acme/widgets", &has_key).is_empty(),
            "a configured model, key and clone start cleanly"
        );
        assert_eq!(
            seen.into_inner(),
            vec!["work".to_string()],
            "the key is asked for the configured provider's name"
        );
    }

    /// The key gate names the provider serving the default model, and nothing
    /// when either is missing: the app uses it to tell "no key" from "the
    /// probe has not answered yet".
    #[test]
    fn key_gated_provider_names_the_default_models_provider() {
        let mut ai = AiSettings::default();
        assert_eq!(key_gated_provider(&ai), None, "no default model");

        ai.models = AiModels {
            default: Some(default_model("work")),
            ..Default::default()
        };
        assert_eq!(
            key_gated_provider(&ai),
            None,
            "the default model's provider is not configured"
        );

        ai.providers.push(provider("work"));
        assert_eq!(key_gated_provider(&ai).as_deref(), Some("work"));
    }
}
