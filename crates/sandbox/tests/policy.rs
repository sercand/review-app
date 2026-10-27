//! The OS-level guarantees the generated policies must provide: the sandboxed
//! shell can write the repo and its tmp dir, cannot write `.git/`, cannot read
//! denied paths, cannot write elsewhere, and never sees a token.
//!
//! These run for real through `sandbox-exec` on macOS and through `bwrap` on
//! Linux; where bubblewrap is missing or broken (CI's Ubuntu may block user
//! namespaces) each test prints `skipping: <reason>` and passes.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use review_sandbox::{
    EnvInput, LocalHost, SandboxOps, SandboxPolicy, SandboxUnavailable, agent_env, sandboxed_launch,
};

const T: Duration = Duration::from_secs(30);

struct Sand {
    _root: tempfile::TempDir, // keeps repo/tmp/secret/sibling alive
    repo: PathBuf,
    sibling: PathBuf,
    secret: PathBuf,
    host: LocalHost,
}

fn run(program: &str, args: &[&str], cwd: &Path) -> anyhow::Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// A temp git repo (one commit), tmp dir, secret dir and sibling dir, plus a
/// `LocalHost` whose shell runs under `sandboxed_launch`. `Ok(None)` (after
/// printing `skipping: …`) when this machine can't sandbox.
fn sandboxed() -> anyhow::Result<Option<Sand>> {
    sandboxed_in("repo", &[])
}

/// Like [`sandboxed`], with the repo directory named `repo_name` and `files`
/// (repo-relative path, contents) written before the shell starts: bubblewrap
/// hides `.env*` files that exist at launch.
fn sandboxed_in(repo_name: &str, files: &[(&str, &str)]) -> anyhow::Result<Option<Sand>> {
    let root = tempfile::tempdir()?;
    let repo = root.path().join(repo_name);
    std::fs::create_dir_all(&repo)?;
    run("git", &["init", "-q", "."], &repo)?;
    run(
        "git",
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
        &repo,
    )?;
    for (f, body) in files {
        if let Some(dir) = repo.join(f).parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(repo.join(f), body)?;
    }
    let tmp = root.path().join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let secret = root.path().join("secret");
    std::fs::create_dir_all(&secret)?;
    std::fs::write(secret.join("key"), b"hunter2")?;
    let sibling = root.path().join("sibling");
    std::fs::create_dir_all(&sibling)?;

    let policy = SandboxPolicy {
        repo: repo.clone(),
        tmp: tmp.clone(),
        write_paths: vec![],
        deny_read: vec![secret.clone()],
        deny_env_reads: true,
    };
    // A realistic agent environment: allowlisted host vars only, and a token
    // that must never reach the shell.
    let mut host_env: Vec<(OsString, OsString)> = ["PATH", "HOME"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(*k), v)))
        .collect();
    host_env.push((OsString::from("GH_TOKEN"), OsString::from("secret-token")));
    let env = agent_env(&EnvInput {
        host_env,
        extra_allow: vec![],
        tmp_dir: tmp.clone(),
        git_user: Some(("t".into(), "t@t".into())),
    })?;
    let launch = match sandboxed_launch(&policy, env) {
        Ok(l) => l,
        // Only the genuinely unavailable environments may skip: a broken
        // fixture must fail the run instead of turning the guarantees green.
        Err(
            e @ (SandboxUnavailable::NoBwrap
            | SandboxUnavailable::BwrapBroken(_)
            | SandboxUnavailable::Unsupported),
        ) => {
            println!("skipping: {e}");
            return Ok(None);
        }
        Err(e @ SandboxUnavailable::Policy(_)) => {
            anyhow::bail!("the sandbox policy could not be built: {e}");
        }
    };
    let host = LocalHost::with_launch(&repo, launch)?;
    Ok(Some(Sand {
        _root: root,
        repo,
        sibling,
        secret,
        host,
    }))
}

#[tokio::test]
async fn can_write_in_the_repo_and_tmp() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let out = s
        .host
        .exec("echo hi > f && echo hi > \"$TMPDIR/f\"", false, T)
        .await?;
    assert_eq!(out.exit_code, 0, "output: {}", out.output);
    assert_eq!(std::fs::read_to_string(s.repo.join("f"))?, "hi\n");
    assert_eq!(std::fs::read_to_string(s.repo.join("../tmp/f"))?, "hi\n");
    Ok(())
}

#[tokio::test]
async fn cannot_write_dot_git() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let out = s.host.exec("touch .git/x", false, T).await?;
    assert_ne!(out.exit_code, 0, "touch .git/x succeeded: {}", out.output);
    assert!(!s.repo.join(".git/x").exists());

    let head = s.host.exec("git rev-parse HEAD", false, T).await?;
    assert_eq!(head.exit_code, 0, "HEAD unreadable: {}", head.output);
    let out = s
        .host
        .exec("git commit --allow-empty -m x", false, T)
        .await?;
    assert_ne!(
        out.exit_code, 0,
        "git commit succeeded inside the sandbox: {}",
        out.output
    );
    let head2 = s.host.exec("git rev-parse HEAD", false, T).await?;
    assert_eq!(
        head.output.trim(),
        head2.output.trim(),
        "HEAD moved despite the commit failing"
    );
    Ok(())
}

#[tokio::test]
async fn cannot_read_denied_paths() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let key = s.secret.join("key");
    let out = s
        .host
        .exec(&format!("cat '{}'", key.display()), false, T)
        .await?;
    assert_ne!(out.exit_code, 0, "cat {key:?} succeeded: {}", out.output);
    assert!(
        !out.output.contains("hunter2"),
        "secret leaked: {}",
        out.output
    );
    Ok(())
}

/// Final review I3(a): repo-local `.env*` files are unreadable at any depth;
/// similarly named files and ordinary repo files stay readable. The repo name
/// holds regex and string metacharacters.
#[tokio::test]
async fn cannot_read_repo_local_env_files() -> anyhow::Result<()> {
    let Some(s) = sandboxed_in(
        "my.repo+(1) \"q\"",
        &[
            (".env", "hunter2"),
            (".env.local", "hunter2"),
            ("sub/.env", "hunter2"),
            ("sub/deep/.envrc", "hunter2"),
            ("environment.txt", "visible"),
            ("sub/dotenv", "visible"),
            ("README", "visible"),
        ],
    )?
    else {
        return Ok(());
    };
    for f in [".env", ".env.local", "sub/.env", "sub/deep/.envrc"] {
        let out = s.host.exec(&format!("cat '{f}'"), false, T).await?;
        assert_ne!(out.exit_code, 0, "cat {f} succeeded: {}", out.output);
        assert!(
            !out.output.contains("hunter2"),
            "{f} leaked: {}",
            out.output
        );
    }
    for f in ["environment.txt", "sub/dotenv", "README"] {
        let out = s.host.exec(&format!("cat '{f}'"), false, T).await?;
        assert_eq!(out.exit_code, 0, "cat {f}: {}", out.output);
        assert!(out.output.contains("visible"), "{f}: {}", out.output);
    }
    // The recursive search that motivated the rule.
    let out = s.host.exec("grep -r hunter2 . ; true", false, T).await?;
    assert!(!out.output.contains("hunter2:"), "{}", out.output);
    assert!(!out.output.contains(":hunter2"), "{}", out.output);
    // macOS only (Seatbelt matches by name): a file created later is covered too.
    if cfg!(target_os = "macos") {
        let out = s
            .host
            .exec("echo hunter2 > .env.new; cat .env.new", false, T)
            .await?;
        assert_ne!(out.exit_code, 0, "{}", out.output);
    }
    Ok(())
}

#[tokio::test]
async fn cannot_write_outside_allowed_paths() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let x = s.sibling.join("x");
    let out = s
        .host
        .exec(&format!("touch '{}'", x.display()), false, T)
        .await?;
    assert_ne!(out.exit_code, 0, "touch {x:?} succeeded: {}", out.output);
    assert!(!x.exists());
    Ok(())
}

#[tokio::test]
async fn agent_env_has_no_tokens_inside_the_sandbox() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let out = s.host.exec("printenv GH_TOKEN", false, T).await?;
    assert_ne!(
        out.exit_code, 0,
        "GH_TOKEN reached the shell: {}",
        out.output
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs network"]
async fn network_is_allowed() -> anyhow::Result<()> {
    let Some(s) = sandboxed()? else {
        return Ok(());
    };
    let out = s
        .host
        .exec("curl -sI https://github.com -o /dev/null", false, T)
        .await?;
    assert_eq!(out.exit_code, 0, "curl failed: {}", out.output);
    Ok(())
}
