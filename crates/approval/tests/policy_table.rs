// Adapted from amenocturne/da (MIT), tests/policy_table.rs
//! Table-driven checks of the rule policies through the library API: what
//! a stack approves, what it leaves alone, and that adding a policy never
//! narrows the result. Nothing here loads the model.

use std::path::Path;

use review_approval::engine::{Decision, classify_stack};
use review_approval::policies::{
    ALL_BUILTIN, CARGO_LOCAL, GIT_ADD, GIT_COMMIT, GIT_FETCH, GIT_PUSH, GIT_READ, HELP_BYPASS,
    MACOS_ONLY, MKDIR_CWD, READ_ONLY,
};
use review_approval::{Policy, PolicyVerdict as Verdict, Segment, SegmentPolicy};

/// da's three-way decision, without the approving policy's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Approve,
    Defer,
    Deny,
}
use Outcome::{Approve, Defer, Deny};

fn classify(cmd: &str, cwd: Option<&Path>, stack: &[&Policy]) -> Outcome {
    let dynamic: Vec<&dyn SegmentPolicy> = stack.iter().map(|p| *p as &dyn SegmentPolicy).collect();
    match classify_stack(cmd, cwd, &dynamic).0 {
        Decision::Approve { .. } => Approve,
        Decision::Defer => Defer,
        Decision::Deny => Deny,
    }
}

fn check(stack: &[&Policy], table: &[(&str, Outcome)]) {
    let mut wrong = Vec::new();
    for (cmd, want) in table {
        let got = classify(cmd, Some(Path::new("/repo")), stack);
        if got != *want {
            wrong.push(format!("{cmd:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

#[test]
fn read_only_alone_approves_only_recognised_read_only_forms() {
    check(
        &[&READ_ONLY],
        &[
            // Not on any list: never approved without a flag that covers them.
            ("touch x", Defer),
            ("mv a b", Defer),
            ("ln -s a b", Defer),
            ("curl -s https://example.com", Defer),
            ("zzqq", Defer),
            ("nonexistent-binary --flag", Defer),
            ("pbcopy", Defer),
            ("rm -rf build", Defer),
            ("cp a b", Defer),
            ("mkdir -p out", Defer),
            ("tee out", Defer),
            ("ls | tee out", Defer),
            ("python3 -c 'print(1)'", Defer),
            ("git status", Defer),
            // Plain read-only commands.
            ("ls -la", Approve),
            ("cat README.md | head -20", Approve),
            ("grep -rn foo src", Approve),
            ("jq '.name' package.json", Approve),
            ("wc -l < Cargo.toml", Approve),
            ("set -euo pipefail", Approve),
            ("set -- a b", Defer),
        ],
    );
}

#[test]
fn read_only_binaries_with_writing_arguments_are_not_approved() {
    check(
        &[&READ_ONLY],
        &[
            ("sort in", Approve),
            ("sort -u -k2,2 in", Approve),
            ("sort -o out in", Defer),
            ("sort -uo out in", Defer),
            ("sort --output=out in", Defer),
            ("sort --outp=out in", Defer),
            ("sort --compress-program=sh in", Defer),
            ("uniq in", Approve),
            ("uniq -c", Approve),
            ("uniq -f 1 in", Approve),
            ("uniq in out", Defer),
            ("tree -L 2", Approve),
            ("tree -o out", Defer),
            ("fd -e rs", Approve),
            ("fd -x rm", Defer),
            ("fd --exec rm", Defer),
            ("fd -X rm", Defer),
            ("rg -n foo src", Approve),
            ("rg --pre-glob '*.pdf' foo", Approve),
            ("rg --pre rm foo", Defer),
            ("rg --pre=rm foo", Defer),
            ("bat README.md", Approve),
            ("bat --pager 'rm x' README.md", Defer),
            ("less -N file", Approve),
            ("less -o log file", Defer),
            ("man ls", Approve),
            ("man -P 'rm x' ls", Defer),
            ("base64 -d in", Approve),
            ("base64 -i in -o out", Defer),
            ("xxd in", Approve),
            ("xxd -l 16 in", Approve),
            ("xxd in out", Defer),
            ("file README.md", Approve),
            ("file -C -m magic", Defer),
            ("yq '.a' f.yml", Approve),
            ("yq -i '.a = 1' f.yml", Defer),
            ("xmllint --format f.xml", Approve),
            ("xmllint --output out f.xml", Defer),
            ("date +%s", Approve),
            ("date -u", Approve),
            ("date -j -f %Y 2020 +%s", Approve),
            ("date 010100002030", Defer),
            ("date -s 2030-01-01", Defer),
            ("hostname", Approve),
            ("hostname evil", Defer),
            ("arch", Approve),
            ("arch -x86_64 rm x", Defer),
            ("sysctl -a", Approve),
            ("sysctl -w a.b=1", Defer),
            ("sysctl a.b=1", Defer),
            ("hash -r", Approve),
            ("hash -p /tmp/evil ls", Defer),
            ("ffprobe -v error in.mp4", Approve),
            ("ffprobe -report in.mp4", Defer),
            ("cloc --json src", Approve),
            ("cloc --out=report src", Defer),
            ("command -v ls", Approve),
            ("command rm x", Defer),
            ("env", Approve),
            ("dc -e '!rm x'", Defer),
        ],
    );
}

#[test]
fn sed_is_approved_only_for_scripts_that_cannot_write_or_execute() {
    check(
        &[&READ_ONLY],
        &[
            ("sed -n 1,20p f", Approve),
            ("sed 's/a/b/g' f", Approve),
            ("sed -E 's/(a|b)+/x/' f", Approve),
            ("sed -n '/start/,/end/p' f", Approve),
            ("sed -e 's/a/b/' -e '$d' f", Approve),
            ("sed -n '/x/{p;q}' f", Approve),
            ("sed -n '$=' f", Approve),
            ("sed -i s/a/b/ f", Defer),
            ("sed -i.bak s/a/b/ f", Defer),
            ("sed --in-place s/a/b/ f", Defer),
            ("sed -ni p f", Defer),
            ("sed s/a/b/ -i f", Defer),
            ("sed -n 'w out' f", Defer),
            ("sed -n '1w out' f", Defer),
            ("sed 's/a/b/w out' f", Defer),
            ("sed 's/a/b/e' f", Defer),
            ("sed '1e rm x' f", Defer),
            ("sed -f script.sed f", Defer),
            ("sed", Defer),
        ],
    );
}

#[test]
fn awk_is_approved_only_for_programs_that_cannot_write_or_execute() {
    check(
        &[&READ_ONLY],
        &[
            ("awk '{print $2}' f", Approve),
            (
                "awk -F: '$3 > 100 {n++} END {print n}' /etc/passwd",
                Approve,
            ),
            ("awk -v n=3 'NR <= n' f", Approve),
            ("awk '{print > \"x\"}' f", Defer),
            ("awk '{print >> \"x\"}' f", Defer),
            ("awk '{print > $2}' f", Defer),
            ("awk '{printf \"%s\", $1 > f}' f", Defer),
            ("awk '{print | \"sh\"}' f", Defer),
            ("awk 'BEGIN{system(\"rm x\")}'", Defer),
            ("awk 'BEGIN{\"date\" | getline d}'", Defer),
            ("awk -f prog.awk f", Defer),
            ("gawk -i inplace '{print}' f", Defer),
        ],
    );
}

#[test]
fn find_and_its_exec_clauses() {
    check(
        &[&READ_ONLY],
        &[
            ("find . -name '*.rs' -type f", Approve),
            ("find . -name '*.rs' -exec wc -l {} +", Approve),
            ("find . -type f -exec grep -l foo {} \\;", Approve),
            ("find . -exec rm {} \\;", Defer),
            ("find . -exec sort -o {} {} \\;", Defer),
            ("find . -exec sed -n p {} \\;", Defer),
            ("find . -execdir rm {} +", Defer),
            ("find . -ok rm {} \\;", Defer),
            ("find . -delete", Defer),
            ("find . -fprint out", Defer),
            ("find . -fls out", Defer),
        ],
    );
}

#[test]
fn wrappers_are_approved_only_when_the_wrapped_command_is() {
    check(
        &[&READ_ONLY],
        &[
            ("sh -c 'ls'", Approve),
            ("bash -lc 'ls | head'", Approve),
            ("sh -c 'rm x'", Defer),
            ("sh -c 'ls; rm x'", Defer),
            ("bash -c \"$CMD\"", Defer),
            ("bash --rcfile evil -c ls", Defer),
            ("sh script.sh", Defer),
            ("timeout 5 ls", Approve),
            ("timeout -s KILL 5s ls", Approve),
            ("timeout 5 rm x", Defer),
            ("env FOO=1 ls", Approve),
            ("env -u FOO ls", Approve),
            ("env FOO=1 rm x", Defer),
            ("env -S 'rm x'", Defer),
            ("time ls", Approve),
            ("time rm x", Defer),
            ("time -o report ls", Defer),
            ("command ls", Approve),
            ("command rm x", Defer),
            ("find . -name '*.rs' | xargs wc -l", Approve),
            ("find . -print0 | xargs -0 -n1 cat", Approve),
            ("ls | xargs -I{} cat {}", Approve),
            ("ls | xargs rm", Defer),
            ("ls | xargs", Approve),
            // stdin supplies operands: a second one is uniq's output file,
            // and the first one is sed's script.
            ("ls | xargs uniq", Defer),
            ("ls | xargs sed -n", Defer),
            ("ls | xargs awk", Defer),
            ("ls | xargs sh -c", Defer),
        ],
    );
}

#[test]
fn redirects_and_environment_overrides() {
    check(
        &[&READ_ONLY, &GIT_READ],
        &[
            ("ls 2>/dev/null", Approve),
            ("ls > /dev/null 2>&1", Approve),
            ("ls >&2", Approve),
            ("ls > /tmp/x", Defer),
            ("ls >> log", Defer),
            ("ls &> log", Defer),
            // `1` and `2` are files here, not descriptors.
            ("ls > 1", Defer),
            ("ls > 2", Defer),
            ("ls >& out", Defer),
            ("LC_ALL=C sort in", Approve),
            ("GIT_PAGER=cat git log", Approve),
            ("PAGER=cat man ls", Approve),
            ("PATH=/tmp/evil ls", Defer),
            ("PATH=/tmp/evil; ls", Defer),
            ("GIT_PAGER='rm x' git log", Defer),
            ("GIT_EXTERNAL_DIFF=evil git diff", Defer),
            ("LD_PRELOAD=evil.so ls", Defer),
            ("env PATH=/tmp/evil ls", Defer),
            ("RIPGREP_CONFIG_PATH=cfg rg foo", Defer),
        ],
    );
}

#[test]
fn run_time_words_are_not_trusted_where_a_flag_could_matter() {
    check(
        &[&READ_ONLY, &GIT_READ, &HELP_BYPASS],
        &[
            // No flag of these binaries writes, so an unknown word is fine.
            ("cat \"$FILE\"", Approve),
            ("ls $HOME/src", Approve),
            ("grep -n \"$PATTERN\" src", Approve),
            // Literal dollars are not expansions.
            ("rg 'foo$' src", Approve),
            ("rg \"foo$\" src", Approve),
            ("awk '{print $1}' f", Approve),
            // After `--` a word can only be an operand.
            ("rg foo -- \"$DIR\"", Approve),
            // `X=-delete; find . $X` and friends.
            ("X=-delete; find . $X", Defer),
            ("find \"$DIR\" -name x", Defer),
            ("sort $ARGS in", Defer),
            ("sort \"$X\"", Defer),
            ("rg foo $DIR", Defer),
            ("sed -n \"$S\" f", Defer),
            ("git log $ARGS", Defer),
            ("find . {-delete,-print}", Defer),
            ("$CMD --help", Defer),
            ("$CMD", Defer),
        ],
    );
}

#[test]
fn git_read_is_read_only() {
    check(
        &[&GIT_READ],
        &[
            ("git status", Approve),
            ("git log --oneline -5", Approve),
            ("git -C x log --oneline -5", Approve),
            ("git --no-pager diff --stat", Approve),
            ("git -c color.ui=always log", Approve),
            ("git diff --output-indicator-new=+", Approve),
            ("git log --output=f", Defer),
            ("git diff --output=x", Defer),
            ("git diff --outp=x", Defer),
            ("git show --output x HEAD", Defer),
            ("git diff --ext-diff", Defer),
            ("git -c core.pager='rm x' log", Defer),
            ("git -c alias.x='!rm x' x", Defer),
            ("git --exec-path=/tmp/evil log", Defer),
            ("git branch", Approve),
            ("git branch -a -vv", Approve),
            ("git branch --show-current", Approve),
            ("git branch --list 'feat/*'", Approve),
            ("git branch --contains HEAD", Approve),
            ("git branch new", Defer),
            ("git branch new main", Defer),
            ("git branch -D old", Defer),
            ("git branch -m a b", Defer),
            ("git branch -u origin/main", Defer),
            ("git config --get user.name", Approve),
            ("git config user.name", Approve),
            ("git config --list", Approve),
            ("git config user.name x", Defer),
            ("git config --global user.name x", Defer),
            ("git config --unset user.name", Defer),
            ("git config -e", Defer),
            ("git reflog", Approve),
            ("git reflog expire --all", Defer),
            ("git push", Defer),
            ("git commit -m x", Defer),
        ],
    );
}

#[test]
fn git_and_cargo_write_capabilities() {
    check(
        &[&GIT_ADD, &GIT_COMMIT, &GIT_FETCH, &GIT_PUSH, &CARGO_LOCAL],
        &[
            ("git add .", Approve),
            ("git commit -m wip", Approve),
            ("git fetch origin", Approve),
            ("git push origin main", Approve),
            ("git fetch --upload-pack='rm x' origin", Defer),
            ("git push --receive-pack='rm x' origin", Defer),
            ("git push --exec='rm x' origin", Defer),
            ("git -c core.sshCommand='rm x' push", Defer),
            ("git commit --amend", Defer),
            ("git commit $ARGS", Defer),
            ("cargo build --release", Approve),
            ("cargo $SUB", Defer),
            ("cargo check --fix", Defer),
        ],
    );
}

#[test]
fn macos_help_and_mkdir() {
    check(
        &[&MACOS_ONLY, &HELP_BYPASS, &MKDIR_CWD],
        &[
            ("pbpaste", Approve),
            ("mdls file", Approve),
            ("mdls -plist out file", Defer),
            ("pbcopy", Defer),
            ("terraform --help", Approve),
            ("terraform apply --help", Defer),
            ("mkdir -p src/a", Approve),
            ("mkdir /etc/x", Defer),
            ("mkdir {/etc/x,y}", Defer),
        ],
    );
}

#[test]
fn argv0_path_is_stripped_before_matching() {
    check(
        &[&READ_ONLY, &GIT_READ],
        &[
            ("/bin/ls -la", Approve),
            ("/usr/bin/sort in", Approve),
            ("/usr/bin/sort -o out in", Defer),
            ("/usr/bin/git status", Approve),
            ("/usr/bin/git branch new", Defer),
            ("./rm x", Defer),
            ("/bin/sh -c 'ls'", Approve),
            ("/usr/bin/env ls", Approve),
            ("/usr/bin/env rm x", Defer),
        ],
    );
}

#[test]
fn every_segment_must_be_approved() {
    check(
        &[&READ_ONLY, &GIT_READ],
        &[
            ("ls && git status", Approve),
            ("ls | head; git log -1 || true", Approve),
            ("cd x && rg -n foo | head", Approve),
            (
                "set -e\ncd x\nrg foo | head -20\ngit log --oneline -3",
                Approve,
            ),
            ("ls && rm x", Defer),
            ("rm x && ls", Defer),
            ("ls | sh", Defer),
            ("ls; touch x", Defer),
            ("ls || touch x", Defer),
            ("ls & touch x", Defer),
            ("ls\ntouch x", Defer),
            // Constructs the parser does not follow.
            ("for f in *; do echo $f; done", Defer),
            ("cat <<EOF\nhi\nEOF", Defer),
            ("echo $(ls)", Defer),
            ("echo `ls`", Defer),
            ("(ls)", Defer),
            ("diff <(ls a) <(ls b)", Defer),
        ],
    );
}

static DENY_ENV: Policy = Policy {
    name: "test-deny-env",
    verify: |seg: &Segment, _| {
        let hit = |t: &String| t.contains(".env");
        (seg.argv.iter().any(hit) || seg.redirects.iter().any(|r| hit(&r.target)))
            .then_some(Verdict::Deny)
    },
};

#[test]
fn deny_comes_only_from_a_deny_rule_and_wins_over_everything() {
    check(
        &[&DENY_ENV, &READ_ONLY],
        &[
            ("cat .env", Deny),
            ("ls && cat .env", Deny),
            // A deferred segment earlier in the command does not hide it.
            ("touch x && cat .env", Deny),
            ("timeout 5 cat .env", Deny),
            ("sh -c 'cat .env'", Deny),
            ("ls | xargs cat .env", Deny),
            ("ls > .env", Deny),
            ("python3 .env", Deny),
            // Not recognised is defer, never deny.
            ("touch x", Defer),
            ("rm -rf /", Defer),
            ("ls", Approve),
        ],
    );
}

/// Commands used to probe monotonicity: a mix of approvable, deferrable and
/// policy-specific forms.
const PROBES: &[&str] = &[
    "ls -la",
    "cd x && ls",
    "sort in",
    "sort -o out in",
    "sed -n 1,20p f",
    "sed -i s/a/b/ f",
    "find . -name x -exec wc -l {} +",
    "find . -delete",
    "ls | xargs wc -l",
    "sh -c 'ls && git status'",
    "timeout 5 git status",
    "env FOO=1 cargo build",
    "git status",
    "git branch new",
    "git add . && git commit -m x",
    "git push",
    "git fetch",
    "git pull",
    "git tag v1",
    "git restore --staged f",
    "cargo build",
    "cargo install x",
    "cargo publish",
    "mkdir -p a/b",
    "pbpaste",
    "terraform --help",
    "touch x",
    "FOO=1",
    "ls > out",
    "echo $(ls)",
];

#[test]
fn adding_a_policy_never_narrows_what_is_approved() {
    let path = Some(Path::new("/repo"));
    for cmd in PROBES {
        assert_ne!(
            classify(cmd, path, ALL_BUILTIN),
            Deny,
            "{cmd:?}: rule policies never deny"
        );
        for (k, extra) in ALL_BUILTIN.iter().enumerate() {
            let without: Vec<&Policy> = ALL_BUILTIN
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != k)
                .map(|(_, p)| *p)
                .collect();
            // base ⊂ base + extra, for three bases: nothing, one other
            // policy at a time, and everything else.
            let mut bases: Vec<Vec<&Policy>> = vec![Vec::new(), without.clone()];
            bases.extend(without.iter().map(|p| vec![*p]));
            for base in bases {
                if classify(cmd, path, &base) != Approve {
                    continue;
                }
                for position in [0, base.len()] {
                    let mut wider = base.clone();
                    wider.insert(position, extra);
                    assert_eq!(
                        classify(cmd, path, &wider),
                        Approve,
                        "{cmd:?}: adding {} turned an approval into something else",
                        extra.name,
                    );
                }
            }
        }
    }
}

#[test]
fn an_empty_stack_approves_nothing_that_touches_anything() {
    for cmd in PROBES {
        let got = classify(cmd, None, &[]);
        let noop = matches!(*cmd, "FOO=1");
        assert_eq!(got, if noop { Approve } else { Defer }, "{cmd:?}");
    }
    assert_eq!(classify("cd /tmp", None, &[]), Approve);
}
