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

#[cfg(test)]
mod tests {
    use super::*;
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
}
