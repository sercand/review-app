//! The OS-level sandbox policy for the agent's shell (spec §8.2).
//!
//! [`SandboxPolicy`] states what the shell may write (the repo minus `.git/`,
//! its private tmp dir, toolchain caches) and must not read (credential files).
//! [`seatbelt_profile`] renders that policy as an SBPL profile for
//! `sandbox-exec`; [`bwrap_args`] renders it as bubblewrap arguments. Every path
//! is canonicalized first: Seatbelt matches the resolved path, so an
//! uncanonicalized one (e.g. macOS `/var/folders/…` vs `/private/var/…`) would
//! silently fail to match. [`sandboxed_launch`] turns a policy into the
//! [`Launch`] that [`BashSession`] runs.
//!
//! [`BashSession`]: crate::BashSession

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::env::Launch;

/// What the agent's shell may write and must not read.
///
/// `repo` and `tmp` are mandatory; `write_paths` are extra directories the
/// agent may write (toolchain caches, see [`default_write_paths`]);
/// `deny_read` are paths whose contents must stay invisible (credentials, see
/// [`default_deny_read`]). Missing `write_paths`/`deny_read` entries are
/// skipped when the policy is rendered; a missing `repo`/`tmp` is an error.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// The project root: writable, except its `.git/` directory.
    pub repo: PathBuf,
    /// The agent-owned temp dir: fully writable.
    pub tmp: PathBuf,
    /// Extra writable directories (spec §8.2 caches).
    pub write_paths: Vec<PathBuf>,
    /// Paths the agent must not read (spec §8.2 credentials).
    pub deny_read: Vec<PathBuf>,
}

/// The toolchain caches the agent may write (spec §8.2), joined onto `home`.
pub fn default_write_paths(home: &Path) -> Vec<PathBuf> {
    [
        ".cargo/registry",
        ".cargo/git",
        ".npm",
        ".cache",
        "Library/Caches",
        "go/pkg/mod",
        ".gradle",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect()
}

/// The credential paths the agent must not read (spec §8.2), joined onto
/// `home`, with the full path `app_config` (this app's own config dir) last.
pub fn default_deny_read(home: &Path, app_config: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        ".ssh",
        ".config/gh",
        ".git-credentials",
        ".netrc",
        ".aws",
        // The approval engine's text-level DenyPaths list (`.npmrc`,
        // `.pypirc`, `.kube`, `.gnupg`, `.docker`) is only a convenience; this
        // OS-level list is the guarantee, so it must cover the same homes.
        ".npmrc",
        ".pypirc",
        ".kube",
        ".gnupg",
        ".docker",
        "Library/Keychains",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect();
    paths.push(app_config.to_path_buf());
    paths
}

/// Escape `s` for a double-quoted SBPL string and wrap it in quotes.
fn sbpl_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `path` as a quoted SBPL string.
fn sbpl_path(path: &Path) -> String {
    sbpl_str(&path.display().to_string())
}

/// The policy paths that exist, canonicalized (missing entries skipped).
fn existing_canonical(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

/// Render `policy` as a Seatbelt (SBPL) profile for `sandbox-exec`.
///
/// `repo` and `tmp` are canonicalized (a failure is returned as an
/// `io::Error`); `write_paths` and `deny_read` skip missing entries. The
/// profile allows everything by default, denies all file writes, then allows
/// exactly: the repo except `.git/`, the tmp dir, each writable path, and the
/// `/dev` entries a shell needs; finally it denies reads of each `deny_read`
/// path (`subpath` for directories, `literal` for files). Network is not
/// restricted.
pub fn seatbelt_profile(policy: &SandboxPolicy) -> std::io::Result<String> {
    let repo = std::fs::canonicalize(&policy.repo)?;
    let tmp = std::fs::canonicalize(&policy.tmp)?;
    let mut p =
        String::from("(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n");
    p.push_str(&format!(
        "    (require-all (subpath {}) (require-not (subpath {})))\n",
        sbpl_path(&repo),
        sbpl_str(&format!("{}/.git", repo.display())),
    ));
    p.push_str(&format!("    (subpath {})\n", sbpl_path(&tmp)));
    for w in existing_canonical(&policy.write_paths) {
        p.push_str(&format!("    (subpath {})\n", sbpl_path(&w)));
    }
    p.push_str(
        "    (literal \"/dev/null\")
    (literal \"/dev/zero\")
    (literal \"/dev/tty\")
    (literal \"/dev/ptmx\")
    (regex #\"^/dev/ttys[0-9]+$\")
    (literal \"/dev/dtracehelper\")
)
",
    );
    for d in existing_canonical(&policy.deny_read) {
        let op = if d.is_dir() { "subpath" } else { "literal" };
        p.push_str(&format!("(deny file-read* ({op} {}))\n", sbpl_path(&d)));
    }
    Ok(p)
}

/// Render `policy` as bubblewrap (`bwrap`) arguments, up to but excluding the
/// `-- <program>` tail.
///
/// Like [`seatbelt_profile`], `repo`/`tmp` must canonicalize and missing
/// optional paths are skipped. The root is bound read-only; the repo, tmp dir
/// and writable paths bind read-write; `.git/` is re-bound read-only over the
/// repo (when it exists); denied directories get a `--tmpfs` and denied files
/// are replaced by `/dev/null`. Network stays shared (no `--unshare-net`).
pub fn bwrap_args(policy: &SandboxPolicy) -> std::io::Result<Vec<OsString>> {
    let repo = std::fs::canonicalize(&policy.repo)?;
    let tmp = std::fs::canonicalize(&policy.tmp)?;
    let mut args: Vec<OsString> = Vec::new();
    let push = |args: &mut Vec<OsString>, flag: &str, a: &Path| {
        args.push(flag.into());
        args.push(a.as_os_str().to_os_string());
    };
    push(&mut args, "--ro-bind", Path::new("/"));
    args.push(Path::new("/").as_os_str().to_os_string());
    push(&mut args, "--dev", Path::new("/dev"));
    push(&mut args, "--proc", Path::new("/proc"));
    push(&mut args, "--bind", &repo);
    args.push(repo.as_os_str().to_os_string());
    let git = repo.join(".git");
    if git.exists() {
        push(&mut args, "--ro-bind", &git);
        args.push(git.as_os_str().to_os_string());
    }
    push(&mut args, "--bind", &tmp);
    args.push(tmp.as_os_str().to_os_string());
    for w in existing_canonical(&policy.write_paths) {
        push(&mut args, "--bind", &w);
        args.push(w.as_os_str().to_os_string());
    }
    for d in existing_canonical(&policy.deny_read) {
        if d.is_dir() {
            push(&mut args, "--tmpfs", &d);
        } else {
            push(&mut args, "--ro-bind", Path::new("/dev/null"));
            args.push(d.as_os_str().to_os_string());
        }
    }
    args.push("--die-with-parent".into());
    push(&mut args, "--chdir", &repo);
    Ok(args)
}

/// The OS refuses to sandbox the agent shell.
#[derive(Debug, thiserror::Error)]
pub enum SandboxUnavailable {
    /// Linux: the `bwrap` binary is missing.
    #[error("bubblewrap (bwrap) is not installed")]
    NoBwrap,
    /// Linux: `bwrap` exists but can't create a sandbox here (e.g. unprivileged
    /// user namespaces disabled, as on CI Ubuntu runners).
    #[error("bubblewrap can't create a sandbox here: {0}")]
    BwrapBroken(String),
    /// Neither Seatbelt nor bubblewrap exists on this OS.
    #[error("sandboxing isn't supported on this OS")]
    Unsupported,
    /// A mandatory policy path (`repo`/`tmp`) could not be canonicalized.
    #[error("can't build the sandbox policy: {0}")]
    Policy(#[from] std::io::Error),
}

/// The [`Launch`] that runs `bash` on this OS under `policy`, with `env` as
/// its complete environment.
///
/// macOS: `sandbox-exec -p <seatbelt_profile(policy)> bash`. Linux: `bwrap
/// <bwrap_args(policy)> -- bash`, but only after a probe (`bwrap --ro-bind / /
/// true`) succeeds — unprivileged user namespaces are commonly disabled, and
/// a broken bwrap must surface as [`SandboxUnavailable::BwrapBroken`] rather
/// than a dead shell. The probe is a **blocking** `std::process::Command`
/// call: run this off the GUI thread. Anything else:
/// [`SandboxUnavailable::Unsupported`].
pub fn sandboxed_launch(
    policy: &SandboxPolicy,
    env: BTreeMap<OsString, OsString>,
) -> Result<Launch, SandboxUnavailable> {
    #[cfg(target_os = "macos")]
    {
        Ok(Launch {
            program: OsString::from("sandbox-exec"),
            args: vec![
                OsString::from("-p"),
                seatbelt_profile(policy)?.into(),
                OsString::from("bash"),
            ],
            env,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let probe = std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .output();
        match probe {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SandboxUnavailable::NoBwrap);
            }
            Err(e) => return Err(SandboxUnavailable::BwrapBroken(e.to_string())),
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let why = if stderr.is_empty() {
                    format!("the probe exited with {}", out.status)
                } else {
                    stderr
                };
                return Err(SandboxUnavailable::BwrapBroken(why));
            }
            Ok(_) => {}
        }
        let mut args = bwrap_args(policy)?;
        args.push(OsString::from("--"));
        args.push(OsString::from("bash"));
        Ok(Launch {
            program: OsString::from("bwrap"),
            args,
            env,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (policy, env);
        Err(SandboxUnavailable::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn canon(p: &Path) -> String {
        std::fs::canonicalize(p).unwrap().display().to_string()
    }

    #[test]
    fn seatbelt_protects_dot_git_and_canonicalizes_paths() {
        let t = tempfile::tempdir().unwrap(); // on macOS this lives under /var → /private/var
        let repo = t.path().join("my \"repo\""); // quote + space
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let tmp = t.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let p = seatbelt_profile(&SandboxPolicy {
            repo: repo.clone(),
            tmp,
            write_paths: vec![],
            deny_read: vec![],
        })
        .unwrap();
        let canon = canon(&repo).replace('"', "\\\"");
        assert!(p.contains(&format!("(require-not (subpath \"{canon}/.git\"))")));
        if cfg!(target_os = "macos") {
            assert!(!p.contains("\"/var/"));
        }
    }

    /// Start index of the contiguous subsequence `pat` in `args`.
    fn seq(args: &[String], pat: &[&str]) -> usize {
        args.windows(pat.len())
            .position(|w| w == pat)
            .unwrap_or_else(|| panic!("{pat:?} not found in {args:?}"))
    }

    #[test]
    fn bwrap_binds_repo_rw_and_dot_git_ro_and_hides_denied_dirs() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let tmp = t.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let secret = t.path().join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        let netrc = t.path().join("netrc");
        std::fs::write(&netrc, b"x").unwrap();
        let args: Vec<String> = bwrap_args(&SandboxPolicy {
            repo: repo.clone(),
            tmp: tmp.clone(),
            write_paths: vec![],
            deny_read: vec![secret.clone(), netrc.clone()],
        })
        .unwrap()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

        let repo = canon(&repo);
        let tmp = canon(&tmp);
        let secret = canon(&secret);
        let netrc = canon(&netrc);
        let git = format!("{repo}/.git");

        // Exact pairs, in the mandated order: repo rw before .git ro.
        let i = seq(&args, &["--ro-bind", "/", "/"]);
        let j = seq(&args, &["--dev", "/dev"]);
        let k = seq(&args, &["--proc", "/proc"]);
        let l = seq(&args, &["--bind", &repo, &repo]);
        let m = seq(&args, &["--ro-bind", &git, &git]);
        let n = seq(&args, &["--bind", &tmp, &tmp]);
        let o = seq(&args, &["--tmpfs", &secret]);
        let p = seq(&args, &["--ro-bind", "/dev/null", &netrc]);
        let q = seq(&args, &["--die-with-parent"]);
        let r = seq(&args, &["--chdir", &repo]);
        assert!(
            i < j && j < k && k < l && l < m && m < n && n < o && o < p && p < q && q < r,
            "bad order: {i} {j} {k} {l} {m} {n} {o} {p} {q} {r} in {args:?}"
        );
        assert!(!args.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn missing_write_and_deny_paths_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let tmp = t.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let policy = SandboxPolicy {
            repo,
            tmp,
            write_paths: vec![t.path().join("gone-write")],
            deny_read: vec![t.path().join("gone-deny")],
        };
        let p = seatbelt_profile(&policy).unwrap();
        assert!(!p.contains("gone-write"));
        assert!(!p.contains("gone-deny"));
        let args = bwrap_args(&policy).unwrap();
        assert!(args.iter().all(|a| {
            let a = a.to_string_lossy();
            !a.contains("gone-write") && !a.contains("gone-deny")
        }));
    }

    #[test]
    fn seatbelt_denies_read_of_dirs_by_subpath_and_files_by_literal() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let tmp = t.path().join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let secret = t.path().join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        let netrc = t.path().join("netrc");
        std::fs::write(&netrc, b"x").unwrap();
        let p = seatbelt_profile(&SandboxPolicy {
            repo,
            tmp,
            write_paths: vec![],
            deny_read: vec![secret.clone(), netrc.clone()],
        })
        .unwrap();
        assert!(p.contains(&format!(
            "(deny file-read* (subpath \"{}\"))",
            canon(&secret)
        )));
        assert!(p.contains(&format!(
            "(deny file-read* (literal \"{}\"))",
            canon(&netrc)
        )));
    }

    #[test]
    fn defaults_match_the_spec_lists() {
        let home = Path::new("/h");
        let w = default_write_paths(home);
        assert_eq!(
            w,
            vec![
                Path::new("/h/.cargo/registry"),
                Path::new("/h/.cargo/git"),
                Path::new("/h/.npm"),
                Path::new("/h/.cache"),
                Path::new("/h/Library/Caches"),
                Path::new("/h/go/pkg/mod"),
                Path::new("/h/.gradle"),
            ]
        );
        let d = default_deny_read(home, Path::new("/app/config"));
        assert_eq!(
            d,
            vec![
                Path::new("/h/.ssh"),
                Path::new("/h/.config/gh"),
                Path::new("/h/.git-credentials"),
                Path::new("/h/.netrc"),
                Path::new("/h/.aws"),
                Path::new("/h/.npmrc"),
                Path::new("/h/.pypirc"),
                Path::new("/h/.kube"),
                Path::new("/h/.gnupg"),
                Path::new("/h/.docker"),
                Path::new("/h/Library/Keychains"),
                Path::new("/app/config"),
            ]
        );
    }
}
