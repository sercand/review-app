//! Commands AI review agents really ran, through `classify` with the app's
//! default configuration (working directory set, no repo allowed list, the
//! default deny paths, no model). With unapproved commands denied back to the
//! agent, each needless ask costs it a retry; each counter-example here must
//! still not run.

use std::path::Path;

use review_approval::{ApprovalConfig, Verdict, classify, default_deny_paths};

fn run(command: &str) -> Verdict {
    let deny = default_deny_paths();
    classify(
        command,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &deny,
            cwd: Some(Path::new("/abs/repo")),
            extra: &[],
        },
    )
}

#[test]
fn real_agent_commands_run() {
    for cmd in [
        r#"cd crates/shiso-models/src/nemotron_transcribe && wc -l align.rs align_tests.rs && ls && grep -n -i "nemo\|sentencepiece\|nvidia" align.rs align_tests.rs | head -40"#,
        r#"cd crates/shiso-models/src/nemotron_transcribe && sed -n 400,800p align.rs"#,
        r#"cd "$(git rev-parse --show-toplevel)"; sed -n 90,150p crates/shiso-audio/src/vocab.rs; grep -n "Vocab" -B2 -A3 crates/shiso-audio/src/error.rs | head -30; grep -rn "pub use\|pub mod" crates/shiso-audio/src/lib.rs | head -30"#,
        r#"cd /abs/repo && ls target 2>/dev/null | head; N=crates/shiso-models/src/nemotron_transcribe; grep -n "fn aligned_refs" -A25 $N/spm_tests.rs $N/spm.rs | head -60"#,
        r#"for p in shiso-models shiso-engine; do echo "== $p"; awk -v p="$p" '$0=="name = \""p"\""{f=1} f&&/^\]/{print; f=0} f' Cargo.lock | grep -E 'name|version'; done"#,
        r#"grep -rn "fn resample\|linspace\|fn ramp" crates/ --include=*.rs | grep -v "nemotron_transcribe/align.rs""#,
        r#"awk 'NR>=2725 && NR<=2740' Cargo.lock | grep -n name"#,
    ] {
        let v = run(cmd);
        assert!(matches!(v, Verdict::Run { .. }), "{cmd}\n  {v:?}");
    }
}

/// `git diff … -- '**/Cargo.toml'` still asks: the deny paths cannot tell a
/// git pathspec from a shell glob, and a bare `**` component could match a
/// protected name (`id_rsa*`). Recorded so a change here is deliberate.
#[test]
fn glob_pathspec_still_asks() {
    let v = run("git diff master...HEAD -- '**/Cargo.toml' Cargo.toml | head -150");
    assert!(matches!(v, Verdict::Ask { .. }), "{v:?}");
}

#[test]
fn counter_examples_do_not_run() {
    for cmd in [
        // sed that writes, executes or reads another file.
        "sed -n '1w /tmp/x' f",
        "sed -e 's/x/y/e' f",
        "sed -n 1p f > out",
        // awk that runs a command or writes.
        r#"awk 'BEGIN{system("curl x")}'"#,
        r#"awk '{print > "f"}' x"#,
        // Loops over run-time words, other compound commands.
        "for f in $(ls); do cat $f; done",
        "for f in *; do cat $f; done",
        "for f in a b; do curl $f; done",
        "while true; do ls; done",
        "if true; then ls; fi",
        // Substitutions other than the two directory ones.
        "X=$(curl x); echo $X",
        r#"cd "$(curl x)""#,
        r#"cd "$(git rev-parse --git-dir)" && cat config"#,
        // Assignments that change what runs.
        "PATH=./bin; ls",
        "for PATH in ./bin; do ls; done",
        "LD_PRELOAD=./x.so; ls",
        "GIT_DIR=x; git log",
        // A known value is still checked where it lands.
        "N=curl; $N x",
        "N=-delete; find . $N",
        "N=out.txt; echo hi > $N",
        // A recursive search that can reach `.git/config` or a secret.
        "grep -r url .",
        "grep -rn token src",
        "grep -R --include=*.rs x .",
        // Into `.git` through a substitution.
        r#"cd "$(git rev-parse --show-toplevel)/.git" && cat config"#,
    ] {
        let v = run(cmd);
        assert!(!matches!(v, Verdict::Run { .. }), "{cmd}\n  {v:?}");
    }
}

#[test]
fn protected_paths_are_denied_through_variables_loops_and_substitutions() {
    for cmd in [
        "N=.env; cat $N",
        "for f in a .env; do cat $f; done",
        r#"cat "$(git rev-parse --show-toplevel)/.git/config""#,
        r#"cat "$(pwd)/.env""#,
        "D=.git; cat $D/config",
    ] {
        let v = run(cmd);
        assert!(matches!(v, Verdict::Deny { .. }), "{cmd}\n  {v:?}");
    }
}
