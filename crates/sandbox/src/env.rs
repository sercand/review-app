//! Configurable shell launches and the credential-free agent environment.
//!
//! A [`Launch`] is everything needed to start the shell process: program, arguments
//! and a **complete** environment. [`Launch::host`] keeps today's behaviour (the
//! host environment minus the scrubbed variables); [`agent_env`] builds the
//! minimal, credential-free environment the AI agent's shell runs under, so GitHub
//! tokens and the SSH agent socket can never reach it.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Env vars the agent's shell must never inherit, even if named in `extra_allow`:
/// GitHub credentials and the SSH agent socket.
pub const DENIED_ENV: [&str; 5] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "SSH_AUTH_SOCK",
];

/// The base set of host env vars the agent's shell may inherit.
const BASE_ALLOW: [&str; 9] = [
    "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER", "LOGNAME", "SHELL",
];

/// The shell process to start: program, arguments, and a **complete** environment.
///
/// The child starts from an empty environment plus `env` — the caller has decided
/// everything the shell may see. `BashSession` itself appends `--noprofile --norc`
/// to `args` and sets `PS1=`, `PS2=` and `TERM=dumb`, so a `Launch` is only about
/// program, args and env: plain bash is `program = "bash", args = []`, a sandbox
/// wrapper is e.g. `program = "sandbox-exec", args = ["-p", <profile>, "bash"]`.
#[derive(Debug, Clone)]
pub struct Launch {
    /// The program to run (plain `bash`, or a wrapper that execs bash).
    pub program: OsString,
    /// Arguments, before the `--noprofile --norc` `BashSession` appends.
    pub args: Vec<OsString>,
    /// The child's complete environment.
    pub env: BTreeMap<OsString, OsString>,
}

impl Launch {
    /// Today's behaviour: `bash` with the host environment minus [`is_scrubbed`]
    /// variables.
    pub fn host() -> Launch {
        Launch {
            program: OsString::from("bash"),
            args: Vec::new(),
            env: kept_host_env(std::env::vars_os().collect()),
        }
    }
}

/// Whether the env var `key` is kept from the shell: every `OTSIMOQA_*` and `TMOUT`.
fn is_scrubbed(key: &OsStr) -> bool {
    let key = key.as_encoded_bytes();
    key.starts_with(b"OTSIMOQA_") || key == b"TMOUT"
}

/// The host env minus the scrubbed vars — the filtering [`Launch::host`] applies.
/// The agent's shell must not see the host's credentials (`OTSIMOQA_TOKEN`, …),
/// nor a `TMOUT` that would end an idle shell behind the session's back.
fn kept_host_env(vars: Vec<(OsString, OsString)>) -> BTreeMap<OsString, OsString> {
    vars.into_iter()
        .filter(|(key, _)| !is_scrubbed(key))
        .collect()
}

/// Inputs for [`agent_env`].
#[derive(Debug, Clone)]
pub struct EnvInput {
    /// The host environment to filter (normally `std::env::vars_os().collect()`).
    pub host_env: Vec<(OsString, OsString)>,
    /// Extra host env var names the agent may inherit; [`DENIED_ENV`] still wins.
    pub extra_allow: Vec<String>,
    /// Private agent-owned temp dir: `TMPDIR`, `GH_CONFIG_DIR` and the gitconfig
    /// live here.
    pub tmp_dir: PathBuf,
    /// Git identity (`name`, `email`) for the written gitconfig; `None` omits the
    /// `[user]` section.
    pub git_user: Option<(String, String)>,
}

/// The credential-free environment for the agent's shell.
///
/// From the host env only the base allowlist plus `extra_allow` is kept, and never
/// a [`DENIED_ENV`] var. On top of that: `GIT_TERMINAL_PROMPT=0`,
/// `GIT_CONFIG_NOSYSTEM=1`, `CARGO_NET_OFFLINE=true`, `TMPDIR`, `GH_CONFIG_DIR`
/// (the directory is created) and `GIT_CONFIG_GLOBAL`, pointing at a gitconfig
/// written here that disables credential helpers and, when `git_user` is given,
/// sets the commit identity.
pub fn agent_env(input: &EnvInput) -> std::io::Result<BTreeMap<OsString, OsString>> {
    if let Some((name, email)) = &input.git_user {
        // A newline would inject arbitrary config keys; refuse rather than write it.
        for v in [name.as_str(), email.as_str()] {
            if v.contains(['\n', '\r']) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "git identity must be a single line",
                ));
            }
        }
    }
    let gh_dir = input.tmp_dir.join("gh");
    std::fs::create_dir_all(&gh_dir)?;
    let gitconfig = input.tmp_dir.join("gitconfig");
    // Git's own formatting (tab-indented keys, `key = value`).
    let mut cfg = String::new();
    if let Some((name, email)) = &input.git_user {
        cfg.push_str("[user]\n");
        cfg.push_str(&format!("\tname = {name}\n\temail = {email}\n"));
    }
    // An empty `helper` unsets every inherited credential helper.
    cfg.push_str("[credential]\n\thelper =\n");
    // C2b: the approval engine runs `git -C DIR log` / `cd DIR && git status`
    // silently; refuse to discover bare-repo layouts committed as plain files
    // in a PR checkout (their `config` would be loaded and could run code).
    cfg.push_str("[safe]\n\tbareRepository = explicit\n");
    std::fs::write(&gitconfig, cfg)?;

    let mut env = BTreeMap::new();
    for (key, value) in &input.host_env {
        let allowed = BASE_ALLOW
            .iter()
            .map(OsStr::new)
            .chain(input.extra_allow.iter().map(OsStr::new))
            .any(|k| k == key);
        let denied = DENIED_ENV.iter().any(|k| OsStr::new(k) == key);
        if allowed && !denied {
            env.insert(key.clone(), value.clone());
        }
    }
    env.insert(OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0"));
    env.insert(OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1"));
    env.insert(OsString::from("CARGO_NET_OFFLINE"), OsString::from("true"));
    env.insert(
        OsString::from("TMPDIR"),
        input.tmp_dir.clone().into_os_string(),
    );
    env.insert(OsString::from("GH_CONFIG_DIR"), gh_dir.into_os_string());
    env.insert(
        OsString::from("GIT_CONFIG_GLOBAL"),
        gitconfig.into_os_string(),
    );
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn get<'a>(env: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a str> {
        env.get(OsStr::new(key)).map(|v| v.to_str().unwrap())
    }

    #[test]
    fn agent_env_keeps_only_the_allowlist_and_sets_git_and_gh_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let env = agent_env(&EnvInput {
            host_env: vars(&[
                ("PATH", "/usr/bin"),
                ("HOME", "/h"),
                ("GH_TOKEN", "x"),
                ("SSH_AUTH_SOCK", "/s"),
                ("AWS_SECRET_ACCESS_KEY", "y"),
                ("FOO", "1"),
            ]),
            extra_allow: vec!["FOO".into(), "GH_TOKEN".into()], // the deny list wins
            tmp_dir: tmp.path().into(),
            git_user: Some(("Ada".into(), "ada@example.com".into())),
        })
        .unwrap();
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
        assert_eq!(get(&env, "HOME"), Some("/h"));
        assert_eq!(get(&env, "FOO"), Some("1"));
        for k in ["GH_TOKEN", "SSH_AUTH_SOCK", "AWS_SECRET_ACCESS_KEY"] {
            assert_eq!(get(&env, k), None);
        }
        assert_eq!(get(&env, "GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(get(&env, "GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(get(&env, "CARGO_NET_OFFLINE"), Some("true"));
        assert_eq!(get(&env, "TMPDIR"), tmp.path().to_str());
        assert_eq!(get(&env, "GH_CONFIG_DIR"), tmp.path().join("gh").to_str());
        assert_eq!(
            get(&env, "GIT_CONFIG_GLOBAL"),
            tmp.path().join("gitconfig").to_str()
        );
        assert!(tmp.path().join("gh").is_dir());
        let cfg = std::fs::read_to_string(tmp.path().join("gitconfig")).unwrap();
        assert!(cfg.contains("name = Ada") && cfg.contains("email = ada@example.com"));
        assert!(cfg.contains("[credential]") && cfg.contains("helper ="));
        assert!(cfg.contains("[safe]") && cfg.contains("bareRepository = explicit"));
    }

    #[test]
    fn agent_env_without_git_user_omits_the_user_section() {
        let tmp = tempfile::tempdir().unwrap();
        let env = agent_env(&EnvInput {
            host_env: vars(&[("PATH", "/usr/bin")]),
            extra_allow: vec![],
            tmp_dir: tmp.path().into(),
            git_user: None,
        })
        .unwrap();
        assert_eq!(
            get(&env, "GIT_CONFIG_GLOBAL"),
            tmp.path().join("gitconfig").to_str()
        );
        let cfg = std::fs::read_to_string(tmp.path().join("gitconfig")).unwrap();
        assert!(!cfg.contains("[user]"));
        assert!(cfg.contains("[credential]") && cfg.contains("helper ="));
    }

    #[test]
    fn launch_host_keeps_path_and_scrubs_otsimoqa_and_tmout() {
        // `std::env::set_var` is `unsafe` in edition 2024 and this crate forbids
        // `unsafe`, so the exact filtering `Launch::host` applies is tested through
        // the helper it uses, and its program/args/PATH behaviour through itself.
        let kept = kept_host_env(vars(&[
            ("PATH", "/usr/bin"),
            ("OTSIMOQA_TOKEN", "secret"),
            ("OTSIMOQA_ANY", "1"),
            ("TMOUT", "5"),
        ]));
        assert_eq!(
            kept.get(OsStr::new("PATH")).map(|v| v.to_str().unwrap()),
            Some("/usr/bin")
        );
        for k in ["OTSIMOQA_TOKEN", "OTSIMOQA_ANY", "TMOUT"] {
            assert!(!kept.contains_key(OsStr::new(k)), "{k} must be dropped");
        }
        let launch = Launch::host();
        assert_eq!(launch.program, OsString::from("bash"));
        assert!(launch.args.is_empty());
        assert!(launch.env.contains_key(OsStr::new("PATH")));
        assert!(!launch.env.contains_key(OsStr::new("TMOUT")));
    }
}
