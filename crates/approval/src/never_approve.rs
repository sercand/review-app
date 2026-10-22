//! Commands a local classifier may never approve (spec: model stage).
//!
//! The model only ever sees segments the rules could not vouch for. For
//! binaries that reach the network, run arbitrary code, install software or
//! destroy data, a "safe" answer from a small model is not good enough: it may
//! flag them (the engine turns that into an ask with a warning) or abstain,
//! never approve. Repo `allowed` entries and the rule policies run before the
//! model and are not affected by this table.

use crate::engine::is_risky_env;
use crate::model::{RedirOp, Segment};

/// Binaries matched on the `argv[0]` basename, case-sensitive.
pub const NEVER_APPROVE: &[&str] = &[
    // network
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "telnet",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "ftp",
    "socat",
    "nmap",
    "dig",
    "nslookup",
    "ping",
    "traceroute",
    // interpreters and runtimes
    "python",
    "python3",
    "pypy",
    "node",
    "nodejs",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "Rscript",
    "osascript",
    "swift",
    "java",
    // shells and exec wrappers
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "eval",
    "exec",
    "source",
    ".",
    "sudo",
    "su",
    "doas",
    "nohup",
    "setsid",
    "screen",
    "tmux",
    // package managers and installers
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "pip",
    "pip3",
    "pipx",
    "uv",
    "uvx",
    "poetry",
    "gem",
    "bundle",
    "cargo-install",
    "brew",
    "apt",
    "apt-get",
    "dpkg",
    "yum",
    "dnf",
    "pacman",
    "snap",
    "flatpak",
    "go",
    "cargo",
    "rustup",
    // destructive or privileged
    "rm",
    "rmdir",
    "shred",
    "dd",
    "mkfs",
    "fdisk",
    "mount",
    "umount",
    "chmod",
    "chown",
    "chgrp",
    "chattr",
    "kill",
    "killall",
    "pkill",
    "launchctl",
    "systemctl",
    "crontab",
    "at",
    "ln",
    "mv",
    "cp",
    "touch",
    "tee",
    "truncate",
    "install",
    // build/test runners and remote-acting CLIs run PR code or act remotely
    "git",
    "gh",
    "docker",
    "podman",
    "kubectl",
    "open",
    "xdg-open",
    "make",
    "cmake",
    "ninja",
    "just",
    "bazel",
    "gradle",
    "mvn",
    "rake",
    "pytest",
    "tox",
    "nox",
    "jest",
    "vitest",
    "mocha",
    // Exec wrappers that run another command. Listing them guarantees a model
    // never vouches for a wrapper as written: the engine checks the wrapped
    // command instead (and the model is never asked about the wrapper).
    "nice",
    "stdbuf",
    "ionice",
    "watch",
    "chrt",
    "taskset",
    "caffeinate",
    "script",
    "expect",
    "parallel",
    "xargs",
    "env",
    "timeout",
    "time",
    "command",
    "builtin",
    "find",
];

/// Whether a model's "safe" may approve `seg`: it has a literal command word
/// whose basename is not in [`NEVER_APPROVE`], writes nowhere but `/dev/null`
/// or an fd, and sets no environment variable that changes what runs.
pub fn model_may_approve(seg: &Segment) -> bool {
    let Some(argv0) = seg.argv.first() else {
        return false;
    };
    if seg.is_dynamic(0) {
        return false;
    }
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    if listed(base) {
        return false;
    }
    if !seg.redirects.iter().all(redirect_ok) {
        return false;
    }
    !seg.assigns.iter().any(|(n, v)| is_risky_env(n, v))
}

/// Whether `base` names a table entry, allowing for a case-insensitive
/// filesystem (`CURL`), a version suffix (`python3.12`, `perl5.34`) and the
/// GNU-coreutils `g` prefix (`gtimeout`).
fn listed(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    let unversioned = lower.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    [lower.as_str(), unversioned].into_iter().any(|name| {
        let hit =
            |n: &str| !n.is_empty() && NEVER_APPROVE.iter().any(|e| e.eq_ignore_ascii_case(n));
        hit(name) || name.strip_prefix('g').is_some_and(hit)
    })
}

fn redirect_ok(r: &crate::model::Redirect) -> bool {
    match r.op {
        RedirOp::In | RedirOp::InDup => true,
        RedirOp::OutDup => {
            r.target == "-"
                || r.target == "/dev/null"
                || (!r.target.is_empty() && r.target.chars().all(|c| c.is_ascii_digit()))
        }
        _ => r.target == "/dev/null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn may(cmd: &str) -> bool {
        let segs = parse(cmd).unwrap();
        model_may_approve(&segs[0])
    }

    #[test]
    fn table_rows() {
        for cmd in [
            "curl -s x",
            "/usr/bin/curl x",
            "python3 -c 1",
            "npm install",
            "rm -rf x",
            "touch x",
            "$CMD a",
            "ls > out.txt",
            "PATH=/x ls",
            "pip --version",
            "xargs ls",
            "env FOO=1 ls",
            "find . -name x",
            "timeout 5 ls",
            "CURL evil.sh",
            "Python3 -c 1",
            "Rm -rf x",
            "BASH x.sh",
            "SH -c 'curl x'",
            "perl5.34 -e 1",
            "python3.12 x",
            "pip3.11 install x",
            "gtimeout 5 curl x",
            "gxargs ls",
            "gfind .",
            "git status",
            "gh pr list",
            "docker ps",
            "make test",
            "pytest",
            "open x",
        ] {
            assert!(!may(cmd), "{cmd}");
        }
        for cmd in [
            "jq . f.json",
            "shasum a",
            "mycustomtool --check",
            "ls 2>&1",
            "foo > /dev/null",
        ] {
            assert!(may(cmd), "{cmd}");
        }
    }

    #[test]
    fn basename_is_not_substring() {
        assert!(may("curly x"));
        assert!(may("rmx x"));
    }

    #[test]
    fn empty_argv_never() {
        let segs = parse("FOO=1").unwrap();
        assert!(!model_may_approve(&segs[0]));
    }
}
