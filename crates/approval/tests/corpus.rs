//! Differential tests against da's Parable corpus (vendored in tests/corpus/).
//!
//! We do not compare ASTs. For each case we decide independently from the
//! source whether a correct fail-closed parser must bail (`BAIL_EXPECTED`
//! files: expansions and heredocs outside single quotes must always bail),
//! and for `IN_SCOPE` files `parse` must return `Ok` or one of the documented
//! intentional bails in `allowed_bail`. Every divergence is printed.
//!
//! Adapted from amenocturne/da (MIT), src/shparse/corpus_tests.rs

use review_approval::{Bail, parse};
use std::collections::BTreeMap;

const IN_SCOPE: &[(&str, &str)] = &[
    ("words", include_str!("corpus/01_words.tests")),
    ("commands", include_str!("corpus/02_commands.tests")),
    ("pipelines", include_str!("corpus/03_pipelines.tests")),
    ("lists", include_str!("corpus/04_lists.tests")),
    ("redirects", include_str!("corpus/05_redirects.tests")),
    (
        "negation_time",
        include_str!("corpus/16_negation_time.tests"),
    ),
    ("pipe_stderr", include_str!("corpus/22_pipe_stderr.tests")),
    (
        "ansi_c_quoting",
        include_str!("corpus/24_ansi_c_quoting.tests"),
    ),
    (
        "line_continuation",
        include_str!("corpus/34_line_continuation.tests"),
    ),
];

const BAIL_EXPECTED: &[(&str, &str)] = &[
    (
        "command_substitution",
        include_str!("corpus/12_command_substitution.tests"),
    ),
    ("arithmetic", include_str!("corpus/13_arithmetic.tests")),
    (
        "here_documents",
        include_str!("corpus/14_here_documents.tests"),
    ),
    (
        "process_substitution",
        include_str!("corpus/15_process_substitution.tests"),
    ),
];

/// Intentional divergences: constructs we bail on although da's parser accepts
/// them. Returns the reason, or `None` when the bail is not documented.
fn allowed_bail(src: &str, bail: &Bail) -> Option<&'static str> {
    match bail {
        Bail::Background => Some("`&` bails Background (da treats it as `;`)"),
        // tree-sitter-bash reports ERROR for these valid bash words; we fail closed.
        Bail::Syntax if matches!(src, "foo@bar" | "foo%bar" | "foo#bar" | "foo[0]") => Some(
            "grammar limitation: tree-sitter-bash ERROR on a word with @ % # or [ ] (fail closed)",
        ),
        // tree-sitter yields `!` as a plain word after `!`/`time`; a reserved word
        // as a command name would be a bogus command, so we bail instead.
        Bail::Unsupported("reserved_word") if src.contains("! ") => Some(
            "`!` after `!`/`time` is a reserved word in command position: bail instead of argv[0]=\"!\"",
        ),
        Bail::Unsupported("$") if src.contains("$\"") => {
            Some("`$\"...\"` locale string is an unhandled node kind (Unsupported)")
        }
        _ => None,
    }
}

/// Intentional acceptances: constructs da bails on that we parse on purpose.
/// Returns the reason, or `None` when the acceptance is not documented.
fn allowed_accept(src: &str) -> Option<&'static str> {
    let rest = without_directory_substitutions(src);
    if rest != src && !expect_bail(&rest) {
        return Some("`$(pwd)` / `$(git rev-parse --show-toplevel)` print a directory: accepted");
    }
    let words = src
        .trim_start()
        .strip_prefix("for ")
        .and_then(|s| s.split_once(" in "))
        .and_then(|(_, s)| s.split_once("do"))
        .map(|(words, _)| words);
    if words.is_some_and(|w| !w.contains(['$', '`', '*', '?', '[', '{', '~'])) {
        return Some("`for NAME in literal…; do …; done` is unrolled");
    }
    None
}

/// `src` with every `$(pwd)` / `$(git rev-parse --show-toplevel)` replaced
/// by a plain word.
fn without_directory_substitutions(src: &str) -> String {
    let mut out = String::new();
    let mut rest = src;
    while let Some(at) = rest.find("$(") {
        let Some(len) = rest[at..].find(')') else {
            break;
        };
        let inner: Vec<&str> = rest[at + 2..at + len].split_whitespace().collect();
        out.push_str(&rest[..at]);
        if matches!(
            inner.as_slice(),
            ["pwd"] | ["git", "rev-parse", "--show-toplevel"]
        ) {
            out.push_str("DIR");
        } else {
            out.push_str(&rest[at..=at + len]);
        }
        rest = &rest[at + len + 1..];
    }
    out.push_str(rest);
    out
}

/// Inspect the raw input bash and decide whether a correct parser must
/// return `Err(Bail::*)`. Respects single-quote state (nothing inside
/// `'...'` triggers anything). Inside double quotes: `$( ` and backticks
/// still do.
fn expect_bail(src: &str) -> bool {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < n {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = true;
            i += 1;
            continue;
        }
        if b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        // Things that trigger bail regardless of double-quote state:
        if b == b'$' && i + 1 < n && bytes[i + 1] == b'(' {
            return true;
        }
        if b == b'`' {
            return true;
        }
        // Things that trigger only outside quotes:
        if !in_double {
            if b == b'<' && i + 1 < n && (bytes[i + 1] == b'<' || bytes[i + 1] == b'(') {
                return true;
            }
            if b == b'>' && i + 1 < n && bytes[i + 1] == b'(' {
                return true;
            }
        }
        i += 1;
    }

    // Any unquoted `(` that isn't `$(`, `<(`, `>(`, or the start of `((`.
    // Captures subshells, function defs (`f()`), and arithmetic commands.
    if has_unquoted_subshell_paren(src) {
        return true;
    }
    // Any unquoted `{ ` — brace group.
    if contains_unquoted(src, b"{ ") || contains_unquoted(src, b"{\t") {
        return true;
    }
    // Reserved word at any segment boundary.
    if has_reserved_word_at_segment_start(src) {
        return true;
    }
    // Array literal: unquoted `=(`.
    if contains_unquoted(src, b"=(") {
        return true;
    }
    // `[[` conditional expression.
    if contains_unquoted(src, b"[[") {
        return true;
    }
    false
}

/// `(` that starts a subshell, function def, or arithmetic command.
/// Ignores `$(`, `<(`, `>(`, and the second `(` of `((`.
fn has_unquoted_subshell_paren(src: &str) -> bool {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < n {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && b == b'(' {
            let prev = if i > 0 { bytes[i - 1] } else { 0 };
            if prev != b'$' && prev != b'<' && prev != b'>' && prev != b'(' {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn has_reserved_word_at_segment_start(src: &str) -> bool {
    // Walk the source, tracking quote state, and at every "command start"
    // position (start of input, or after `;` / `|` / `&&` / `||` / `&`)
    // check if the next word is a reserved keyword.
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut at_start = true;
    let mut in_single = false;
    let mut in_double = false;
    while i < n {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = true;
            i += 1;
            continue;
        }
        if b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_double {
            i += 1;
            continue;
        }

        if b == b' ' || b == b'\t' || b == b'\n' {
            i += 1;
            continue;
        }
        if b == b';' || b == b'|' || b == b'&' {
            at_start = true;
            // Skip doubled operators
            if i + 1 < n && (bytes[i + 1] == b'&' || bytes[i + 1] == b'|' || bytes[i + 1] == b';') {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if at_start {
            // Read the next word (unquoted alphanumeric-ish)
            let start = i;
            while i < n {
                let x = bytes[i];
                if x == b' '
                    || x == b'\t'
                    || x == b'\n'
                    || x == b';'
                    || x == b'|'
                    || x == b'&'
                    || x == b'<'
                    || x == b'>'
                    || x == b'('
                    || x == b')'
                    || x == b'\''
                    || x == b'"'
                    || x == b'='
                {
                    break;
                }
                i += 1;
            }
            let word = &src[start..i];
            if matches!(
                word,
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "while"
                    | "until"
                    | "do"
                    | "done"
                    | "for"
                    | "in"
                    | "case"
                    | "esac"
                    | "select"
                    | "function"
                    | "coproc"
            ) {
                return true;
            }
            at_start = false;
            continue;
        }
        i += 1;
    }
    false
}

fn contains_unquoted(src: &str, needle: &[u8]) -> bool {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let k = needle.len();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i + k <= n {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            continue;
        }
        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double && &bytes[i..i + k] == needle {
            return true;
        }
        i += 1;
    }
    false
}

#[derive(Debug)]
struct Case {
    name: String,
    input: String,
}

fn parse_cases(src: &str) -> Vec<Case> {
    let mut out = Vec::new();
    let mut lines = src.lines();
    while let Some(line) = lines.next() {
        if let Some(name) = line.strip_prefix("=== ") {
            let mut input_buf = String::new();
            for l in lines.by_ref() {
                if l == "---" {
                    break;
                }
                if !input_buf.is_empty() {
                    input_buf.push('\n');
                }
                input_buf.push_str(l);
            }
            // skip the expected s-exp block
            for l in lines.by_ref() {
                if l == "---" {
                    break;
                }
            }
            out.push(Case {
                name: name.trim().to_string(),
                input: input_buf,
            });
        }
    }
    out
}

#[test]
fn corpus_differential() {
    let mut total = 0;
    let mut ok = 0;
    let mut failures: Vec<String> = Vec::new();
    let mut allowed: BTreeMap<&'static str, usize> = BTreeMap::new();

    for (label, contents) in IN_SCOPE {
        for c in parse_cases(contents) {
            total += 1;
            let must_bail = expect_bail(&c.input);
            match (parse(&c.input), must_bail) {
                (Ok(_), false) | (Err(_), true) => ok += 1,
                (Ok(segs), true) => match allowed_accept(&c.input) {
                    Some(reason) => {
                        *allowed.entry(reason).or_default() += 1;
                        ok += 1;
                    }
                    None => failures.push(format!(
                        "[{label}/{}] da expects a bail, got Ok({} segs) for {:?}",
                        c.name,
                        segs.len(),
                        c.input
                    )),
                },
                (Err(e), false) => match allowed_bail(&c.input, &e) {
                    Some(reason) => {
                        *allowed.entry(reason).or_default() += 1;
                        ok += 1;
                    }
                    None => failures.push(format!(
                        "[{label}/{}] undocumented bail {e:?} for {:?}",
                        c.name, c.input
                    )),
                },
            }
        }
    }

    for (label, contents) in BAIL_EXPECTED {
        for c in parse_cases(contents) {
            total += 1;
            let must_bail = expect_bail(&c.input);
            match (parse(&c.input), must_bail) {
                (Err(_), true) | (Ok(_), false) => ok += 1,
                (Ok(segs), true) => match allowed_accept(&c.input) {
                    Some(reason) => {
                        *allowed.entry(reason).or_default() += 1;
                        ok += 1;
                    }
                    None => failures.push(format!(
                        "[{label}/{}] must bail, got Ok({} segs) for {:?}",
                        c.name,
                        segs.len(),
                        c.input
                    )),
                },
                (Err(e), false) => match allowed_bail(&c.input, &e) {
                    Some(reason) => {
                        *allowed.entry(reason).or_default() += 1;
                        ok += 1;
                    }
                    None => failures.push(format!(
                        "[{label}/{}] undocumented bail {e:?} for {:?}",
                        c.name, c.input
                    )),
                },
            }
        }
    }

    eprintln!("corpus: {ok}/{total} passed");
    for (reason, n) in &allowed {
        eprintln!("allowed divergence x{n}: {reason}");
    }
    for f in &failures {
        eprintln!("FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "corpus differential: {} failures out of {total}",
        failures.len()
    );
}
