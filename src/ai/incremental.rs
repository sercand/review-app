//! Incremental re-review scope (spec §12): which files changed between the
//! previous run's head and the current one, and the commit log since then.
//!
//! Git-only: spawns the `git` binary against the PR checkout, like
//! [`crate::ai::checkout`]. A force-push (the previous head is no longer an
//! ancestor of the current one) cannot be scoped by a diff — the caller must
//! fall back to a full review.

use std::process::Stdio;

use tokio::process::Command;

/// What an incremental run should review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncrementalScope {
    /// Files touched between the two heads, sorted and deduped.
    Files(Vec<String>),
    /// The diff cannot be trusted (e.g. force-push); review everything.
    FullFallback { reason: String },
}

/// Files changed between `since` and `head` in `repo`.
///
/// `Err` only when git itself cannot run; a semantic failure (force-push)
/// is a [`IncrementalScope::FullFallback`], not an error.
pub async fn incremental_files(
    repo: &std::path::Path,
    since: &str,
    head: &str,
) -> Result<IncrementalScope, String> {
    let ancestor = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", since, head])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn git merge-base: {e}"))?;
    // Non-zero means `since` is not reachable from `head`: the branch was
    // rewritten, so `since..head` would hide files the previous review saw.
    if !ancestor.status.success() {
        return Ok(IncrementalScope::FullFallback {
            reason: format!("force-push: {since} is not an ancestor of {head}"),
        });
    }

    // `core.quotePath=false` keeps non-ASCII paths as raw UTF-8 instead of
    // octal-quoted `"src/\303\251.rs"`, so the names match the PR file list.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg("core.quotePath=false")
        .args(["diff", "--name-only", &format!("{since}..{head}")])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn git diff: {e}"))?;
    if !out.status.success() {
        return Ok(IncrementalScope::FullFallback {
            reason: format!(
                "git diff {since}..{head} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }

    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    files.sort();
    files.dedup();
    Ok(IncrementalScope::Files(files))
}

/// `git log --oneline since..head`, capped at `cap` commits (newest first).
pub async fn commits_since(
    repo: &std::path::Path,
    since: &str,
    head: &str,
    cap: usize,
) -> Result<String, String> {
    let range = format!("{since}..{head}");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--oneline", &format!("-{cap}"), range.as_str()])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn git log: {e}"))?;
    if !out.status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRepo(pub tempfile::TempDir);

    impl TempRepo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            git(dir.path(), &["init", "-q", "."]);
            git(dir.path(), &["config", "user.email", "test@example.com"]);
            git(dir.path(), &["config", "user.name", "Test"]);
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            self.0.path()
        }

        /// Writes the files and commits everything, returning the new head.
        fn commit(&self, files: &[(&str, &str)], message: &str) -> String {
            self.write(files);
            git(self.path(), &["add", "-A"]);
            git(self.path(), &["commit", "-q", "-m", message]);
            rev_parse(self.path(), "HEAD")
        }

        fn write(&self, files: &[(&str, &str)]) {
            for (name, content) in files {
                let path = self.path().join(name);
                std::fs::create_dir_all(path.parent().expect("parent dir")).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
    }

    fn git(repo: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn rev_parse(repo: &std::path::Path, rev: &str) -> String {
        git(repo, &["rev-parse", rev]).trim().to_string()
    }

    #[tokio::test]
    async fn incremental_scope_lists_changed_files_and_detects_force_push() {
        let repo = TempRepo::new();
        repo.write(&[("src/a.rs", "a1\n"), ("src/b.rs", "b1\n")]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        let base = rev_parse(repo.path(), "HEAD");

        let prev_head = repo.commit(
            &[("src/a.rs", "a2\n"), ("src/c.rs", "c1\n")],
            "touch a and c",
        );

        // A normal fast-forward: exactly the files in between, sorted.
        let scope = incremental_files(repo.path(), &base, &prev_head)
            .await
            .unwrap();
        assert_eq!(
            scope,
            IncrementalScope::Files(vec!["src/a.rs".into(), "src/c.rs".into()])
        );

        // Rewriting the reviewed commit (the force-push case): the previous
        // head is no longer an ancestor, so a `since..head` diff would hide
        // files the previous review saw.
        repo.write(&[("src/d.rs", "d1\n")]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "--amend", "--no-edit"]);
        let rewritten = rev_parse(repo.path(), "HEAD");

        let scope = incremental_files(repo.path(), &prev_head, &rewritten)
            .await
            .unwrap();
        match scope {
            IncrementalScope::FullFallback { reason } => {
                assert!(reason.contains("force-push"), "{reason}");
                assert!(
                    reason.contains(&prev_head) && reason.contains(&rewritten),
                    "{reason}"
                );
            }
            other => panic!("expected the force-push fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn incremental_scope_keeps_non_ascii_paths_unquoted() {
        let repo = TempRepo::new();
        repo.write(&[("src/a.rs", "a1\n")]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "base"]);
        let base = rev_parse(repo.path(), "HEAD");

        repo.commit(&[("src/naïve.rs", "n1\n")], "add a unicode path");

        // Without `core.quotePath=false` git would print `"src/na\303\257ve.rs"`.
        let scope = incremental_files(repo.path(), &base, &rev_parse(repo.path(), "HEAD"))
            .await
            .unwrap();
        assert_eq!(scope, IncrementalScope::Files(vec!["src/naïve.rs".into()]));
    }

    #[tokio::test]
    async fn commits_since_lists_the_new_commits() {
        let repo = TempRepo::new();
        repo.write(&[("src/a.rs", "a1\n")]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "base commit"]);
        let base = rev_parse(repo.path(), "HEAD");

        repo.commit(&[("src/a.rs", "a2\n")], "add three");
        repo.commit(&[("src/a.rs", "a3\n")], "fix three");

        let log = commits_since(repo.path(), &base, &rev_parse(repo.path(), "HEAD"), 50)
            .await
            .unwrap();
        assert!(log.contains("add three"), "{log}");
        assert!(log.contains("fix three"), "{log}");
        assert!(!log.contains("base commit"), "{log}");
    }
}
