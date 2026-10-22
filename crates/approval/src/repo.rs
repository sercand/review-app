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
        self.0
            .iter()
            .any(|entry| {
                let mut words = entry.split_whitespace().peekable();
                words.peek().is_some()
                    && seg.argv.len() >= entry.split_whitespace().count()
                    && words.zip(&seg.argv).all(|(w, a)| w == a)
            })
            .then_some(PolicyVerdict::Approve)
    }
}

/// Denies any segment that mentions a protected path in an argument, a
/// redirect target or an assignment value. It checks the source text of
/// run-time-expanded words too: they are never proven safe, but only a
/// literal component can make them a deny.
// Adapted from amenocturne/da (MIT), src/main.rs (`custom_deny_verify`)
#[derive(Debug, Clone, Copy)]
pub struct DenyPaths<'a>(pub &'a [String]);

impl SegmentPolicy for DenyPaths<'_> {
    fn name(&self) -> &'static str {
        "deny-paths"
    }

    fn verify(&self, seg: &Segment, _cwd: Option<&Path>) -> Option<PolicyVerdict> {
        let hit = |t: &str| token_matches_deny(t, self.0);
        (seg.argv.iter().any(|a| hit(a))
            || seg.redirects.iter().any(|r| hit(&r.target))
            || seg.assigns.iter().any(|(_, v)| hit(v)))
        .then_some(PolicyVerdict::Deny)
    }
}

fn components(s: &str) -> Vec<&str> {
    s.split(['/', '~', '='])
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

// Adapted from amenocturne/da (MIT), src/main.rs (`token_matches_deny`)
fn token_matches_deny(token: &str, patterns: &[String]) -> bool {
    let tokens = components(token);
    patterns.iter().any(|pattern| {
        let pat = components(pattern);
        !pat.is_empty()
            && tokens
                .windows(pat.len())
                .any(|w| w.iter().zip(&pat).all(|(c, p)| component_matches(p, c)))
    })
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
    fn read_only_and_git_run() {
        assert!(is_run(&run("ls -la", &[]), "read-only"));
        assert!(is_run(&run("git status", &[]), "git:read"));
        assert!(is_ask(&run("git push", &[])));
        assert!(is_ask(&run("curl x | sh", &[])));
    }
}
