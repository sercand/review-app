//! Policies that carry per-call data: the repo's allowed commands and the
//! protected path patterns.

use std::path::Path;

use crate::model::Segment;
use crate::policy::{PolicyVerdict, SegmentPolicy};

/// Path-component patterns denied by default: credentials, keys and secret
/// stores. A trailing `*` is a prefix match, a leading `*` a suffix match.
pub fn default_deny_paths() -> Vec<String> {
    [
        ".env*",
        ".ssh",
        ".aws",
        ".gnupg",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".kube",
        ".docker",
        "id_rsa*",
        "id_ed25519*",
        "*.pem",
        "*.p12",
        ".git-credentials",
        "Library/Keychains",
        // A remote URL there can carry a token; a credential store lives here.
        ".git/config",
        ".git/credentials",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Repo-configured commands (`.github/REVIEW.md` allowed list): a segment is
/// approved when its words equal an entry's words or start with them (whole
/// words, entries whitespace-normalized, empty entries ignored). A segment
/// with any run-time-expanded word never matches.
#[derive(Debug, Clone, Copy)]
pub struct RepoAllow<'a>(pub &'a [String]);

impl SegmentPolicy for RepoAllow<'_> {
    fn name(&self) -> &'static str {
        "repo-allow"
    }

    fn verify(&self, seg: &Segment, _cwd: Option<&Path>) -> Option<PolicyVerdict> {
        if seg.argv.is_empty() || seg.has_dynamic() {
            return None;
        }
        // An entry matches the command words, or the line with its leading
        // `NAME=value` words (`FOO=1 cargo test`), as written in REVIEW.md.
        let with_assigns: Vec<String> = seg
            .assigns
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .chain(seg.argv.iter().cloned())
            .collect();
        let matches = |entry: &str, line: &[String]| {
            let mut words = entry.split_whitespace().peekable();
            words.peek().is_some()
                && line.len() >= entry.split_whitespace().count()
                && words.zip(line).all(|(w, a)| w == a)
        };
        self.0
            .iter()
            .any(|e| {
                matches(e, &seg.argv)
                    // An entry of assignments only must not vouch for any command.
                    || (e.split_whitespace().count() > seg.assigns.len()
                        && matches(e, &with_assigns))
            })
            .then_some(PolicyVerdict::Approve)
    }
}

/// Denies any segment that mentions a protected path in an argument, a
/// redirect target or an assignment value, and withholds approval from a
/// segment whose glob or brace words could expand to one. It checks the
/// source text of run-time-expanded words too: they are never proven safe,
/// but only a literal component can make them a deny. Matching ignores ASCII
/// case (macOS volumes are case-insensitive by default).
// Adapted from amenocturne/da (MIT), src/main.rs (`custom_deny_verify`)
///
/// A bare leading-metachar glob (`ls *`, `cat src/*`) is withheld, so it asks:
/// it could match `*.pem` or `id_rsa*`, and the same rule also blocks option
/// injection through `-`-prefixed file names a PR might commit (M4). A glob
/// ending in an ordinary extension no deny pattern can match (`ls *.rs`) is fine.
#[derive(Debug, Clone)]
pub struct DenyPaths {
    /// The patterns split and lower-cased once, not per word (M1).
    lowered: Vec<Vec<String>>,
}

impl DenyPaths {
    /// Builds the policy from path-component deny patterns (a trailing `*`
    /// makes a prefix pattern, e.g. `".env*"`); they are lower-cased and split
    /// once here so each `verify` call only compares.
    pub fn new(patterns: &[String]) -> Self {
        DenyPaths {
            lowered: lowered(patterns),
        }
    }

    fn words(seg: &Segment) -> impl Iterator<Item = &str> {
        seg.argv
            .iter()
            .map(String::as_str)
            .chain(seg.redirects.iter().map(|r| r.target.as_str()))
            .chain(seg.assigns.iter().map(|(_, v)| v.as_str()))
    }
}

impl SegmentPolicy for DenyPaths {
    fn name(&self) -> &'static str {
        "deny-paths"
    }

    fn verify(&self, seg: &Segment, _cwd: Option<&Path>) -> Option<PolicyVerdict> {
        if Self::words(seg).any(|w| token_matches_deny(w, &self.lowered)) {
            Some(PolicyVerdict::Deny)
        } else if Self::words(seg).any(|w| could_match_deny(w, &self.lowered)) {
            Some(PolicyVerdict::Withhold)
        } else {
            None
        }
    }

    fn explain_deny(&self, seg: &Segment) -> Option<String> {
        Self::words(seg)
            .find(|w| token_matches_deny(w, &self.lowered))
            .map(|w| format!("protected path \"{w}\""))
    }
}

fn components(s: &str) -> Vec<&str> {
    s.split(['/', '~', '=', ':'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

fn component_matches(pattern: &str, component: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        component.starts_with(prefix)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        component.ends_with(suffix)
    } else {
        component == pattern
    }
}

pub(crate) fn lowered(patterns: &[String]) -> Vec<Vec<String>> {
    patterns
        .iter()
        .map(|p| {
            components(p)
                .into_iter()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

// Adapted from amenocturne/da (MIT), src/main.rs (`token_matches_deny`)
/// Whether some path component of `token` literally matches a deny pattern.
pub(crate) fn token_matches_deny(token: &str, patterns: &[Vec<String>]) -> bool {
    let token = token.to_ascii_lowercase();
    let tokens = components(&token);
    patterns.iter().any(|pat| {
        tokens
            .windows(pat.len())
            .any(|w| w.iter().zip(pat).all(|(c, p)| component_matches(p, c)))
    })
}

/// Whether `token`, once brace-expanded, has a glob component that could
/// match a deny pattern. Over-approximate on purpose: a quoted `*` looks the
/// same as a bare one here.
pub(crate) fn could_match_deny(token: &str, pats: &[Vec<String>]) -> bool {
    let token = token.to_ascii_lowercase();
    let Some(variants) = expand_braces(&token) else {
        return true;
    };
    variants.iter().any(|v| {
        let tokens = components(v);
        pats.iter().any(|pat| {
            tokens.windows(pat.len()).any(|w| {
                w.iter()
                    .zip(pat)
                    .all(|(c, p)| component_matches(p, c) || glob_could_match(c, p))
            })
        })
    })
}

fn glob_start(c: &str) -> Option<usize> {
    c.char_indices()
        .find(|&(i, ch)| match ch {
            '*' | '?' => true,
            '[' => c[i + 1..].contains(']'),
            _ => false,
        })
        .map(|(i, _)| i)
}

fn glob_end(c: &str) -> usize {
    c.char_indices()
        .rev()
        .find(|&(i, ch)| match ch {
            '*' | '?' | ']' => ch != ']' || c[..i].contains('['),
            _ => false,
        })
        .map_or(0, |(i, _)| i + 1)
}

/// Can the glob `comp` match a name that `pattern` (a deny component:
/// exact, `prefix*` or `*suffix`) matches? A component with no glob
/// characters is compared literally, which `token_matches_deny` already did.
fn glob_could_match(comp: &str, pattern: &str) -> bool {
    let Some(start) = glob_start(comp) else {
        return false;
    };
    let (cpre, csuf) = (&comp[..start], &comp[glob_end(comp)..]);
    let (ppre, psuf) = if let Some(p) = pattern.strip_suffix('*') {
        (p, "")
    } else if let Some(p) = pattern.strip_prefix('*') {
        ("", p)
    } else {
        (pattern, pattern)
    };
    // Without dotglob, a leading metachar never matches a leading dot.
    if cpre.is_empty() && ppre.starts_with('.') {
        return false;
    }
    // `*.rs` against `id_rsa*`: only by an unrealistic `id_rsa.rs`; treating
    // a bare extension glob as a match would ask for every `ls *.rs`.
    if cpre.is_empty() && csuf.starts_with('.') && csuf.len() > 1 && psuf.is_empty() {
        return false;
    }
    (cpre.starts_with(ppre) || ppre.starts_with(cpre))
        && (csuf.ends_with(psuf) || psuf.ends_with(csuf))
}

const MAX_BRACE_VARIANTS: usize = 256;
const MAX_BRACE_WORD: usize = 4096;
const MAX_BRACE_DEPTH: usize = 32;

/// Bash brace expansion. `None` when the result is unknown or too large
/// (sequences like `{1..3}`, more than 256 words); a brace without a
/// top-level comma is literal, as in bash.
pub(crate) fn expand_braces(word: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    expand_into(word, &mut out)?;
    Some(out)
}

fn expand_into(word: &str, out: &mut Vec<String>) -> Option<()> {
    if word.len() > MAX_BRACE_WORD && word.contains('{') {
        return None;
    }
    let bytes = word.as_bytes();
    let mut from = 0;
    while let Some(rel) = word[from..].find('{') {
        let open = from + rel;
        let mut depth = 0usize;
        let mut commas = Vec::new();
        let mut close = None;
        for (i, &b) in bytes.iter().enumerate().skip(open) {
            match b {
                b'{' => {
                    depth += 1;
                    if depth > MAX_BRACE_DEPTH {
                        return None;
                    }
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                b',' if depth == 1 => commas.push(i),
                _ => {}
            }
        }
        let Some(close) = close else { break };
        if commas.is_empty() {
            if word[open + 1..close].contains("..") {
                return None;
            }
            from = open + 1;
            continue;
        }
        let (pre, post) = (&word[..open], &word[close + 1..]);
        let mut start = open + 1;
        for end in commas.into_iter().chain([close]) {
            expand_into(&format!("{pre}{}{post}", &word[start..end]), out)?;
            start = end + 1;
        }
        return Some(());
    }
    if out.len() >= MAX_BRACE_VARIANTS {
        return None;
    }
    out.push(word.to_string());
    Some(())
}

#[cfg(test)]
mod tests {
    use crate::{ApprovalConfig, Verdict, classify, default_deny_paths};

    fn run(cmd: &str, allowed: &[&str]) -> Verdict {
        let allowed: Vec<String> = allowed.iter().map(|s| (*s).to_string()).collect();
        let deny = default_deny_paths();
        classify(
            cmd,
            &ApprovalConfig {
                allowed: &allowed,
                deny_paths: &deny,
                cwd: None,
                extra: &[],
            },
        )
    }

    fn is_run(v: &Verdict, source: &str) -> bool {
        matches!(v, Verdict::Run { source: s } if *s == source)
    }

    fn is_ask(v: &Verdict) -> bool {
        matches!(v, Verdict::Ask { .. })
    }

    fn is_deny(v: &Verdict) -> bool {
        matches!(v, Verdict::Deny { .. })
    }

    #[test]
    fn allow_prefix_boundaries() {
        assert!(is_run(&run("cargo test", &["cargo test"]), "repo-allow"));
        assert!(is_run(
            &run("cargo test --locked", &["cargo test"]),
            "repo-allow"
        ));
        assert!(is_run(
            &run("cargo  test   --locked", &[" cargo   test "]),
            "repo-allow"
        ));
        assert!(is_ask(&run("cargo testify", &["cargo test"])));
        assert!(is_ask(&run("cargo", &["cargo test"])));
        assert!(is_ask(&run("cargo test", &["", "  "])));
        assert!(is_ask(&run("cargo \"test x\"", &["cargo test x"])));
    }

    #[test]
    fn allow_entry_may_include_leading_assignment() {
        let e = &["FOO=1 cargo test"];
        assert!(is_run(&run("FOO=1 cargo test --locked", e), "repo-allow"));
        assert!(is_ask(&run("FOO=2 cargo test", e)));
        assert!(is_ask(&run("FOO=1 rm -rf x", &["FOO=1"])));
        assert!(is_ask(&run("FOO=1 BAR=2 rm x", &["FOO=1 BAR=2"])));
        assert!(is_ask(&run("cargo test", e)));
        assert!(is_ask(&run("FOO=1 cargo test $(evil)", e)));
        // An entry without the assignment still matches the command words.
        assert!(is_run(
            &run("FOO=1 cargo test", &["cargo test"]),
            "repo-allow"
        ));
    }

    #[test]
    fn allow_rejects_dynamic_word() {
        assert!(is_ask(&run("cargo test $X", &["cargo test"])));
        assert!(is_ask(&run("cargo test $(evil)", &["cargo test"])));
        assert!(is_ask(&run("$X test", &["cargo test"])));
    }

    #[test]
    fn deny_beats_allow() {
        assert!(is_deny(&run("cat .env", &["cat"])));
        assert!(is_deny(&run("cat src/.env.local", &["cat"])));
    }

    #[test]
    fn deny_inside_wrapper() {
        for cmd in [
            "timeout 5 cat .env",
            "sh -c 'cat ~/.ssh/id_rsa'",
            "ls && cat .env.local",
            "env FOO=1 cat .env",
            r"find . -name x -exec cat .env \;",
            "touch x && cat .env",
        ] {
            assert!(is_deny(&run(cmd, &[])), "{cmd}");
        }
    }

    #[test]
    fn deny_redirect_target() {
        for cmd in [
            "echo x > .env",
            "cat < .env",
            "xargs cat < .env",
            "sh -c ls < .env",
            "ls | xargs cat < .env",
        ] {
            assert!(is_deny(&run(cmd, &[])), "{cmd}");
        }
    }

    #[test]
    fn deny_matches_components_not_substrings() {
        assert!(is_run(&run("cat environment.md", &[]), "read-only"));
        assert!(is_run(&run("cat src/ssh.rs", &[]), "read-only"));
        assert!(is_deny(&run("cat --file=.ssh/config", &[])));
        assert!(is_deny(&run("cat ~/.aws/credentials", &[])));
        assert!(is_deny(&run("cat server.pem", &[])));
        assert!(is_deny(&run("cat ~/Library/Keychains/login.keychain", &[])));
        for cmd in [
            "cat .git/config",
            "cat ./.git/config",
            "cat .git/credentials",
            "head -n 3 repo/.git/config",
            "cat .GIT/CONFIG",
        ] {
            assert!(is_deny(&run(cmd, &[])), "{cmd}");
        }
        assert!(!is_deny(&run("cat .git/HEAD", &[])));
        assert!(!is_deny(&run("cat .gitignore", &[])));
        assert!(is_run(&run("cat Library/Fonts/a", &[]), "read-only"));
        assert!(is_deny(&run("cat id_rsa.pub", &[])));
        assert!(is_deny(&run("cat \"$HOME/.ssh/id_rsa\"", &[])));
        // A run-time word with no literal protected component is not a deny.
        assert!(is_run(&run("cat $X", &[]), "read-only"));
    }

    #[test]
    fn empty_patterns_deny_nothing() {
        let deny = vec![String::new(), "/".to_string()];
        let v = classify(
            "cat a",
            &ApprovalConfig {
                allowed: &[],
                deny_paths: &deny,
                cwd: None,
                extra: &[],
            },
        );
        assert!(is_run(&v, "read-only"));
    }

    #[test]
    fn reasons_are_plain_words() {
        let cases = [
            ("echo $(ls)", "command substitution"),
            ("cat <<EOF\nhi\nEOF", "heredoc"),
            (
                "if true; then ls; fi",
                "unsupported shell construct: if/for/while",
            ),
            ("ls &&", "syntax error"),
            ("touch x", "touch x: not in the allowed commands"),
        ];
        for (cmd, want) in cases {
            match run(cmd, &[]) {
                Verdict::Ask { reason } => {
                    assert!(reason.contains(want), "{cmd}: {reason}");
                    assert!(!reason.contains("::") && !reason.contains('('), "{reason}");
                }
                other => panic!("{cmd}: {other:?}"),
            }
        }
        assert_eq!(run("", &[]), Verdict::Run { source: "empty" });
        assert_eq!(run("  \n ", &[]), Verdict::Run { source: "empty" });
    }

    #[test]
    fn globs_and_braces_that_could_hit_a_deny_pattern_ask() {
        for cmd in [
            "cat .e*",
            "cat .en[v]",
            "cat .en?",
            "cat .{env,x}",
            "cat .*",
            "cat id*",
            "cat *",
            "cat ~/.s*/config",
            "cat {1..3}",
            "cat *rsa",
            "cat x/.ssh?",
        ] {
            assert!(is_ask(&run(cmd, &[])), "{cmd}");
        }
        // Even a repo allow entry cannot approve them.
        assert!(is_ask(&run("cat .e*", &["cat"])));
        assert!(is_ask(&run("cat *", &["cat"])));
        // A literal deny elsewhere in the command still wins.
        assert!(is_deny(&run("cat .e* && cat .env", &[])));
        assert!(is_deny(&run("cat .e* > .env", &[])));
        for cmd in [
            "ls *.rs",
            "rg foo src/*.rs",
            "cat src/{a,b}.rs",
            "ls src/?.x",
            "cat [abc].txt",
            "[ -f x ]",
            "awk '{print $1}' f",
            "sed -n '/x/{p;q}' f",
        ] {
            assert!(!is_ask(&run(cmd, &[])) || cmd.starts_with('['), "{cmd}");
        }
        assert!(is_run(&run("ls *.rs", &[]), "read-only"));
        assert!(is_run(&run("rg foo src/*.rs", &[]), "read-only"));
        assert!(is_run(&run("cat src/{a,b}.rs", &[]), "read-only"));
    }

    #[test]
    fn deny_ignores_ascii_case() {
        assert!(is_deny(&run("cat .ENV", &[])));
        assert!(is_deny(&run("cat x.PEM", &[])));
        assert!(is_deny(&run("cat ~/.SSH/ID_RSA", &[])));
        assert!(is_deny(&run("cat ~/library/keychains/x", &[])));
    }

    #[test]
    fn unicode_whitespace_is_not_empty() {
        for cmd in ["\u{a0}", "\u{3000}", "\u{85}", "\u{2028}"] {
            assert!(is_ask(&run(cmd, &[])), "{cmd:?}");
        }
        assert_eq!(run("\t \r\n", &[]), Verdict::Run { source: "empty" });
    }

    #[test]
    fn colon_separates_components() {
        assert!(is_deny(&run("git show HEAD:.env", &[])));
        assert!(is_deny(&run("git show HEAD:config/.ssh/x", &[])));
    }

    #[test]
    fn deny_reason_names_the_path() {
        match run("cat < .env", &[]) {
            Verdict::Deny { reason } => assert_eq!(reason, "cat: protected path \".env\""),
            other => panic!("{other:?}"),
        }
        match run("ls && cat ~/.ssh/id_rsa", &[]) {
            Verdict::Deny { reason } => {
                assert!(
                    reason.contains("protected path \"~/.ssh/id_rsa\""),
                    "{reason}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn brace_expansion() {
        let ex = |w: &str| super::expand_braces(w);
        assert_eq!(ex("a"), Some(vec!["a".into()]));
        assert_eq!(ex("{a,b}c"), Some(vec!["ac".into(), "bc".into()]));
        assert_eq!(
            ex("{a,b{c,d}}"),
            Some(vec!["a".into(), "bc".into(), "bd".into()])
        );
        assert_eq!(ex("{a}"), Some(vec!["{a}".into()]));
        assert_eq!(ex("{a,b"), Some(vec!["{a,b".into()]));
        assert_eq!(ex("{1..3}"), None);
        assert_eq!(ex("{a,b}{c,d}{e,f}{g,h}{i,j}{k,l}{m,n}{o,p}{q,r}"), None);
    }

    #[test]
    fn glob_compatibility() {
        let pats = default_deny_paths();
        let pats = super::lowered(&pats);
        let could = |t: &str| super::could_match_deny(t, &pats);
        assert!(could(".e*") && could("id_*") && could("*") && could("*.p?m"));
        assert!(!could("*.rs") && !could("?.x") && !could("src/*.rs") && !could("plain"));
        assert!(could("foo*") && !could("[abc].txt"));
    }

    #[test]
    fn multi_component_patterns_are_glob_checked() {
        assert!(is_ask(&run("cat ~/Library/Keych*ns/x", &[])));
        assert!(is_ask(&run("cat ~/Lib*/Keychains/x", &[])));
        assert!(is_ask(&run(
            "cat ~/Library/Keych*ns/login.keychain-db",
            &["cat"]
        )));
        assert!(is_run(&run("cat ~/Library/Fonts/*.ttf", &[]), "read-only"));
    }

    #[test]
    fn shell_options_that_enable_dotglob_do_not_run_silently() {
        for cmd in [
            "GLOBIGNORE=x; cat *.env",
            "GLOBIGNORE=x; cat ~/*.aws/credentials",
            "GLOBIGNORE=x cat *.env",
            "env GLOBIGNORE=x cat *.env",
            "shopt -s dotglob; cat *.env",
            "BASHOPTS=dotglob cat *.env",
        ] {
            assert!(!matches!(run(cmd, &[]), Verdict::Run { .. }), "{cmd}");
        }
    }

    #[test]
    fn pathological_braces_are_fast_and_could_match() {
        let start = std::time::Instant::now();
        let deep = format!("{}a,b{}", "{".repeat(60_000), "}".repeat(60_000));
        assert_eq!(super::expand_braces(&deep), None);
        let nested = format!("{}a,b{}", "{".repeat(40), "}".repeat(40));
        assert_eq!(super::expand_braces(&nested), None);
        assert!(is_ask(&run(&format!("cat {deep}"), &[])));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn set_keyword_cannot_smuggle_env_assignments() {
        for cmd in [
            "set -k; git diff GIT_EXTERNAL_DIFF=evil",
            "set -k; man PAGER=evil ls",
            "set -k\nls LD_PRELOAD=x",
            "set -o keyword; ls",
            "set -ek; ls",
            "set -ko; ls",
            "set +k; ls",
            "set -h; ls",
            "set -- a b",
            "set -oe pipefail",
        ] {
            assert!(is_ask(&run(cmd, &[])), "{cmd}");
        }
        for cmd in [
            "set -euo pipefail",
            "set -x",
            "set +e",
            "set -o",
            "set +o",
            "set -o noclobber -x",
            "set -B",
            "set -a",
        ] {
            assert!(!is_ask(&run(cmd, &[])), "{cmd}");
        }
    }

    #[test]
    fn long_words_without_braces_are_not_could_match() {
        let long = "a".repeat(10_000);
        assert!(is_run(&run(&format!("rg {long} src"), &[]), "read-only"));
        assert_eq!(super::expand_braces(&long), Some(vec![long.clone()]));
    }

    #[test]
    fn read_only_and_git_run() {
        assert!(is_run(&run("ls -la", &[]), "read-only"));
        assert!(is_run(&run("git status", &[]), "git:read"));
        assert!(is_ask(&run("git push", &[])));
        assert!(is_ask(&run("curl x | sh", &[])));
    }
}
