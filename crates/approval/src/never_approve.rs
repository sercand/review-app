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
    // Exec wrappers a rule does not know: they run their operand.
    "busybox",
    "toybox",
    "flock",
    "arch",
    "strace",
    "ltrace",
    "valgrind",
    "gdb",
    "lldb",
    "xvfb-run",
    "unbuffer",
    "hyperfine",
    "entr",
    "watchexec",
    "firejail",
    "proxychains",
    "torsocks",
    "unshare",
    "chroot",
    "nsenter",
    "sg",
    "runuser",
    "systemd-run",
    "sandbox-exec",
    // Programs and editors that can run a command from their own script.
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "tar",
    "sqlite3",
    "vi",
    "vim",
    "nvim",
    "ex",
    "emacs",
    // More network clients.
    "http",
    "https",
    "xh",
    "aria2c",
    "wget2",
    "openssl",
    "aws",
    "gcloud",
    "az",
    "gsutil",
    "rclone",
    // Change or remove data (`sort -o` writes).
    "unlink",
    "gzip",
    "gunzip",
    "bzip2",
    "xz",
    "zstd",
    "patch",
    "unzip",
    "trash",
    "sort",
    // Interpreters, runners and wrappers that execute PR files or an operand.
    "tclsh",
    "m4",
    "ts-node",
    "tsx",
    "pwsh",
    "powershell",
    "julia",
    "ed",
    "meson",
    "sbt",
    "mix",
    "dotnet",
    "cmd",
    "wsl",
    "numactl",
    "perf",
    "setarch",
    "faketime",
    "ssh-agent",
    "dbus-run-session",
    // Disk, power, firewall and system-settings tools.
    "diskutil",
    "iptables",
    "ip6tables",
    "nft",
    "ufw",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "security",
    "defaults",
    "sysctl",
    "csrutil",
    "spctl",
    "tccutil",
    "networksetup",
    "scutil",
    "pmset",
    "hdiutil",
    "tmutil",
    "dscl",
    "sysadminctl",
    "visudo",
    "passwd",
    "useradd",
    "userdel",
    "usermod",
    "parted",
    "mke2fs",
    "mkswap",
];

/// Options that name a program to run (matched on the word before any `=`).
const EXEC_OPTIONS: &[&str] = &[
    "--pre",
    "--hostname-bin",
    "--compress-program",
    "--use-compress-program",
    "--to-command",
    "--checkpoint-action",
    "--upload-pack",
    "--receive-pack",
    "--exec-path",
    "--rsh",
    "--pager",
    "--editor",
    "--unzip-command",
];

/// Directories whose binaries are the system's, not the PR's.
const SYSTEM_BIN_DIRS: &[&str] = &[
    "/bin/",
    "/usr/bin/",
    "/usr/local/bin/",
    "/opt/homebrew/bin/",
    "/sbin/",
    "/usr/sbin/",
];

/// Whether a model's "safe" may approve `seg`: it has a literal command word
/// that is a bare ASCII name or an absolute system path (a relative path is
/// PR code) whose basename is not in [`NEVER_APPROVE`], no later word names a
/// listed binary (wrappers, `awk 'BEGIN{system("curl …")}'`), no later word
/// is a working-tree path (`./x`, `dir/x`: PR code handed to a program the
/// table does not know) or an option naming a program to run, it writes
/// nowhere but `/dev/null` or an fd, and it sets no environment variable that
/// changes what runs.
pub fn model_may_approve(seg: &Segment) -> bool {
    let Some(argv0) = seg.argv.first() else {
        return false;
    };
    if seg.is_dynamic(0) {
        return false;
    }
    if argv0.contains('/') && !SYSTEM_BIN_DIRS.iter().any(|d| path_in_dir(argv0, d)) {
        return false;
    }
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    if listed(base) || base.to_ascii_lowercase().starts_with("mkfs.") {
        return false;
    }
    // Command names are ASCII: anything else is a look-alike or a name no
    // trusted binary has.
    if !argv0.chars().all(|c| c.is_ascii_graphic()) {
        return false;
    }
    if seg.argv.iter().skip(1).any(|w| word_names_listed(w)) {
        return false;
    }
    if seg
        .argv
        .iter()
        .skip(1)
        .any(|w| names_exec_option(w) || names_pr_path(w))
    {
        return false;
    }
    if !seg.redirects.iter().all(redirect_ok) {
        return false;
    }
    !seg.assigns.iter().any(|(n, v)| is_risky_env(n, v))
}

/// Whether `word` is an option that names a program to run.
fn names_exec_option(word: &str) -> bool {
    let name = word.split('=').next().unwrap_or(word);
    EXEC_OPTIONS.iter().any(|o| name.eq_ignore_ascii_case(o))
}

/// Whether `word` points into the working tree, where a PR's own code lives:
/// `./x`, `../x` or `dir/x`, either as the word itself or as the value of
/// `--opt=value`. Absolute paths and `~` paths are not covered; an option's own
/// text (`--name=a/b`) is not a path, but a separate word after `-o` is
/// (the filter cannot tell it from an operand).
fn names_pr_path(word: &str) -> bool {
    let value = match word.strip_prefix('-') {
        Some(_) => match word.split_once('=') {
            Some((_, v)) => v,
            None => return false,
        },
        None => word,
    };
    if value.starts_with(['/', '~']) {
        return false;
    }
    value.starts_with("./")
        || value.starts_with("../")
        || (!word.starts_with('-') && value.contains('/'))
}

/// `path` is `dir` plus one file name: no `..`, no nesting.
fn path_in_dir(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/') && rest != "..")
}

/// Whether any piece of `word`, split on characters outside
/// `[A-Za-z0-9._+-]`, names a listed binary. Pieces made of dots only (`.`,
/// `..`) are paths, not the `.` builtin.
fn word_names_listed(word: &str) -> bool {
    word.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')))
        .any(|piece| !piece.chars().all(|c| c == '.') && listed(piece))
}

/// Whether `base` names a table entry, allowing for a case-insensitive
/// filesystem (`CURL`), a version suffix (`python3.12`, `perl5.34`,
/// `node-18`, `ruby_3`) and the GNU-coreutils `g` prefix (`gtimeout`).
fn listed(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower).to_string();
    let unversioned = lower
        .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
        .trim_end_matches(['-', '_']);
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
            // I1: wrappers, relative programs, listed words as operands.
            "flock /tmp/l curl x",
            "./tool.sh",
            "scripts/build.sh",
            "/tmp/x/tool",
            "/usr/bin/../../tmp/x",
            "/usr/bin/sub/tool",
            "ls curl",
            "mytool --to-command=curl",
            "node-18 x.js",
            "ruby_3 x.rb",
            "sort -o f f",
            // Interpreters, runners and wrappers an earlier table missed.
            "numactl ./x",
            "perf stat ./evil",
            "setarch x86_64 ./evil",
            "faketime 2020-01-01 scripts/evil",
            "ssh-agent ./evil",
            "dbus-run-session ./evil",
            "tclsh evil.tcl",
            "m4 evil.m4",
            "ts-node x.ts",
            "tsx x.ts",
            "pwsh x.ps1",
            "powershell x.ps1",
            "julia x.jl",
            "ed -s f",
            "meson setup b",
            "sbt run",
            "mix run",
            "dotnet run",
            "cmd /c evil",
            "wsl ls",
            // `.exe` is not a different program.
            "CURL.EXE x",
            "curl.exe x",
            "Python.EXE x.py",
            "pwsh.exe x.ps1",
            "env CURL.EXE x",
            "ls cmd.exe",
            // Disk, power, firewall and system-settings tools.
            "mkfs.ext4 /dev/sda1",
            "mkfs.xfs /dev/sdb",
            "diskutil eraseDisk JHFS+ X /dev/disk2",
            "iptables -F",
            "ufw disable",
            "shutdown -h now",
            "reboot",
            "halt",
            "security dump-keychain",
            "defaults write x y -bool true",
            // PR code passed as an operand of an unlisted program.
            "mytool ./evil.sh",
            "mytool ../evil",
            "mytool scripts/evil.sh",
            "mytool a.txt src/x.rs",
            "mytool -o a/b",
            "mytool --config=./evil.toml",
            "mytool --config=../evil.toml",
            // Options that name a program to run.
            "rg --pre ./evil pattern",
            "rg --pre=evil pattern",
            "rg --hostname-bin=./evil x",
            "mytool --compress-program=x f",
            "mytool --to-command=x f",
            "mytool --checkpoint-action=exec=x f",
            "mytool --use-compress-program x f",
            "mytool --upload-pack=x f",
            // A command name that is not plain ASCII is not a program we know.
            "ls\u{3000}-la",
            "c\u{430}t README.md",
            "\u{ff52}\u{ff4d} -rf /",
            "rm\u{200c} -rf /",
        ] {
            assert!(!may(cmd), "{cmd}");
        }
        for cmd in [
            "jq . f.json",
            "shasum a",
            "mycustomtool --check",
            "ls 2>&1",
            "foo > /dev/null",
            "/usr/bin/mytool x",
            "mytool a.txt . ..",
            "mytool --name=a/b",
            "mytool /abs/path/file",
            "mytool a.txt",
            "mytool ~/x",
            "mytool --preview x",
            "mytool --pre-commit-hint",
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
