//! Arbitrary-input safety: no panics, no hangs, no approvals of command substitution.

use review_approval::{ApprovalConfig, Verdict, classify};
use std::time::{Duration, Instant};

fn run(command: &str) -> Verdict {
    classify(
        command,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd: None,
            extra: &[],
        },
    )
}

/// True when `s` has a `$(` outside single quotes and not backslash-escaped
/// (inside double quotes it still counts).
fn contains_unquoted_cmd_sub(s: &str) -> bool {
    let c: Vec<char> = s.chars().collect();
    let (mut i, mut single, mut double) = (0, false, false);
    while i < c.len() {
        match c[i] {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '\\' if !single => i += 1,
            '$' if !single && c.get(i + 1) == Some(&'(') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

#[test]
fn reference_check_tracks_both_quote_kinds() {
    assert!(contains_unquoted_cmd_sub("\"'$(x)\""));
    assert!(contains_unquoted_cmd_sub("echo $(x)"));
    assert!(!contains_unquoted_cmd_sub("echo '$(x)'"));
    assert!(!contains_unquoted_cmd_sub("echo \\$(x)"));
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn random_strings_never_panic_or_run_command_substitution() {
    let alphabet: Vec<char> = "'\"\\$(){}[]<>|&;\n abcdefghij=~-./é日\u{1f600}\t#!*?`"
        .chars()
        .collect();
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    for _ in 0..5000 {
        let target = (rng.next() % 200) as usize;
        let mut s = String::new();
        while s.len() < target {
            s.push(alphabet[(rng.next() % alphabet.len() as u64) as usize]);
        }
        let v = run(&s);
        if contains_unquoted_cmd_sub(&s) {
            assert!(!matches!(v, Verdict::Run { .. }), "{s:?} -> {v:?}");
        }
    }
}

fn assert_ask_within_a_second(s: &str) {
    let start = Instant::now();
    let v = run(s);
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    assert!(matches!(v, Verdict::Ask { .. }), "{v:?}");
}

#[test]
fn over_cap_commands_ask_fast() {
    // 100k characters: rejected by the length cap before parsing.
    assert_ask_within_a_second(&format!("xx{}", "a".repeat(100_000)));
    assert_ask_within_a_second(&format!("{}rm x", "ls ; ".repeat(20_000)));
    assert!(matches!(run(&format!("ls {}", "a".repeat(100_000))),
        Verdict::Ask { reason } if reason == "command too long"));
}

#[test]
fn long_commands_below_cap_are_fast_and_ask() {
    assert_ask_within_a_second(&format!("xx{}", "a".repeat(60_000)));
    assert_ask_within_a_second(&format!("{}rm x", "ls ; ".repeat(12_000)));
}

#[test]
fn deeply_nested_parens_are_fast_and_ask() {
    let s = format!("{}ls{}", "(".repeat(5000), ")".repeat(5000));
    assert_ask_within_a_second(&s);
}

#[test]
fn deeply_nested_braces_are_fast_and_ask() {
    let s = format!("echo {}a{}", "{".repeat(30_000), "}".repeat(30_000));
    assert_ask_within_a_second(&s);
}

#[test]
fn nul_byte_asks() {
    assert!(matches!(run("ls \u{0}"), Verdict::Ask { .. }));
    assert!(matches!(run("\u{0}"), Verdict::Ask { .. }));
}
