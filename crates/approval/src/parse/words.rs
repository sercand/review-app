//! Word nodes to `(value, dynamic)`.
//!
//! `value` is the word after quote removal and backslash resolution; `dynamic`
//! is true when the real value is only known at run time (a `$` expansion or an
//! unquoted brace expansion). Globs are not dynamic: they cannot change the
//! shape of argv0 or a flag. Process substitution, arithmetic expansion and
//! command substitution are bails, never words, except for the two
//! substitutions [`allowed_substitution`] accepts.
//!
//! A `$NAME` whose value [`Vars`] knows is replaced by that value and is not
//! dynamic.

use tree_sitter::Node;

use super::unsupported;
use super::vars::{Vars, is_name};
use crate::model::Bail;

/// Node kinds [`word_value`] understands (besides the container kinds below).
pub(super) const WORD_KINDS: &[&str] = &[
    "word",
    "number",
    "string",
    "raw_string",
    "ansi_c_string",
    "concatenation",
    "simple_expansion",
    "expansion",
    "brace_expression",
];

pub(super) fn is_word_kind(kind: &str) -> bool {
    WORD_KINDS.contains(&kind)
        || matches!(
            kind,
            "command_substitution" | "process_substitution" | "arithmetic_expansion"
        )
}

pub(super) fn text<'a>(node: Node, src: &'a str) -> Result<&'a str, Bail> {
    src.get(node.byte_range()).ok_or(Bail::Syntax)
}

pub(super) fn word_value(node: Node, src: &str, vars: &Vars) -> Result<(String, bool), Bail> {
    match node.kind() {
        // A word/number with children hides a substitution (`10#$(cmd)`).
        "word" | "number" if node.named_child_count() > 0 => {
            check_expansion(node, src)?;
            Err(unsupported(node))
        }
        "word" | "number" => {
            let t = text(node, src)?;
            Ok((unescape_unquoted(t), has_brace_expansion(t)))
        }
        "brace_expression" => Ok((text(node, src)?.to_string(), true)),
        "raw_string" => {
            let t = text(node, src)?;
            let inner = t
                .strip_prefix('\'')
                .and_then(|t| t.strip_suffix('\''))
                .ok_or(Bail::Syntax)?;
            Ok((inner.to_string(), false))
        }
        // `$'...'` is decoded, not kept raw, so `$'\x2d\x72'` is seen as the flag
        // `-r` bash will run. A decoded NUL makes bash truncate the word, so it
        // is treated as dynamic.
        "ansi_c_string" => {
            let t = text(node, src)?;
            let inner = t
                .strip_prefix("$'")
                .and_then(|t| t.strip_suffix('\''))
                .ok_or(Bail::Syntax)?;
            let v = decode_ansi_c(inner);
            let dynamic = v.contains('\0');
            Ok((v, dynamic))
        }
        "simple_expansion" | "expansion" => {
            check_expansion(node, src)?;
            if let Some(v) = plain_name(node, src)?.and_then(|n| vars.unquoted(n)) {
                return Ok((v.to_string(), false));
            }
            Ok((text(node, src)?.to_string(), true))
        }
        "string" => string_value(node, src, vars),
        "concatenation" => concat_value(node, src, vars),
        // Unquoted, the output is split into words: dynamic.
        "command_substitution" => match allowed_substitution(node, src)? {
            Some(canonical) => Ok((canonical.to_string(), true)),
            None => Err(Bail::CommandSubstitution),
        },
        "process_substitution" => Err(Bail::ProcessSubstitution),
        "arithmetic_expansion" => Err(Bail::Arithmetic),
        _ => Err(unsupported(node)),
    }
}

/// Bail when the expansion `node` contains a substitution anywhere inside it.
/// Tree-sitter parses some bodies (a backtick inside `${A:-...}`) as plain
/// words, so the source text is checked as well as the subtree.
pub(super) fn check_expansion(node: Node, src: &str) -> Result<(), Bail> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "arithmetic_expansion" => return Err(Bail::Arithmetic),
            "process_substitution" => return Err(Bail::ProcessSubstitution),
            "command_substitution" => return Err(Bail::CommandSubstitution),
            _ => {}
        }
        let mut cursor = n.walk();
        stack.extend(n.children(&mut cursor));
    }
    let t = text(node, src)?;
    if t.contains("$((") || t.contains("$[") {
        return Err(Bail::Arithmetic);
    }
    if t.contains("<(") || t.contains(">(") {
        return Err(Bail::ProcessSubstitution);
    }
    if t.contains('`') || t.contains("$(") {
        return Err(Bail::CommandSubstitution);
    }
    if has_transform_operator(t) {
        return Err(Bail::Unsupported("parameter_transformation"));
    }
    Ok(())
}

/// `${x@P}`, `${x@E}`, `${x@A}` …: bash 4.4+ transformation operators. `@P`
/// expands prompt escapes (which can run `$(…)` held in the variable) and
/// `@E` decodes escapes, so the word's value is not what the text says.
/// Looks for `@` followed by a letter inside any `${…}` body; a `@` in other
/// positions (`${@}`, `${x:-a@b}`) may bail needlessly, which fails closed.
fn has_transform_operator(t: &str) -> bool {
    let mut rest = t;
    while let Some(i) = rest.find("${") {
        let body = &rest[i + 2..];
        let end = body.find('}').unwrap_or(body.len());
        let b = &body.as_bytes()[..end];
        if b.windows(2)
            .any(|w| w[0] == b'@' && w[1].is_ascii_alphabetic())
        {
            return true;
        }
        rest = body;
    }
    false
}

/// The variable name of a plain `$NAME` or `${NAME}` (no operator, no
/// special parameter), else `None`.
fn plain_name<'a>(node: Node, src: &'a str) -> Result<Option<&'a str>, Bail> {
    let t = text(node, src)?;
    let name = match node.kind() {
        "simple_expansion" => t.strip_prefix('$'),
        _ => t.strip_prefix("${").and_then(|t| t.strip_suffix('}')),
    };
    Ok(name.filter(|n| is_name(n)))
}

/// The command substitutions a word may hold: `$(pwd)` and
/// `$(git rev-parse --show-toplevel)`, each optionally with `2>/dev/null`.
/// Both print an absolute directory (the toplevel prints nothing outside a
/// repository), so the word is that path followed by the rest of the word.
/// The value keeps the substitution's canonical text: rules see a word that
/// is not a flag, and its later components (`…/.git/config`) are still
/// checked against the deny paths. The substitution must end the word or be
/// followed by `/`: an empty output glued to `-rf` would turn into a flag no
/// rule saw.
///
/// `Ok(None)` for every other substitution (and the backtick forms); the
/// caller bails.
fn allowed_substitution(node: Node, src: &str) -> Result<Option<&'static str>, Bail> {
    let t = text(node, src)?;
    let Some(inner) = t.strip_prefix("$(").and_then(|t| t.strip_suffix(')')) else {
        return Ok(None);
    };
    let mut words: Vec<&str> = inner.split_ascii_whitespace().collect();
    match words.as_slice() {
        [.., "2>/dev/null"] => {
            words.pop();
        }
        [.., "2>", "/dev/null"] => {
            words.truncate(words.len() - 2);
        }
        _ => {}
    }
    let canonical = match words.as_slice() {
        ["pwd"] | ["pwd", "-L" | "-P"] => "$(pwd)",
        ["git", "rev-parse", "--show-toplevel"] => "$(git rev-parse --show-toplevel)",
        _ => return Ok(None),
    };
    let rest = src.get(node.end_byte()..).ok_or(Bail::Syntax)?;
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let ends_word = rest.is_empty()
        || rest.starts_with(|c: char| c == '/' || c.is_ascii_whitespace() || ";|&)<>".contains(c));
    Ok(ends_word.then_some(canonical))
}

/// True when `gap` (text between two sibling nodes) is only backslash-newline
/// continuations, so bash glues the neighbours into one word.
pub(super) fn continuation_only(gap: &str) -> bool {
    !gap.is_empty() && gap.replace("\\\r\n", "").replace("\\\n", "").is_empty()
}

fn concat_value(node: Node, src: &str, vars: &Vars) -> Result<(String, bool), Bail> {
    let mut value = String::new();
    let mut dynamic = false;
    // Unquoted text with a neutral marker for every non-bare piece, so brace
    // expansion can span quoted parts (`{a,"b"}`).
    let mut unquoted = String::new();
    let mut prev_end = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Err(Bail::Unsupported("concatenation"));
        }
        if let Some(end) = prev_end {
            let gap = src.get(end..child.start_byte()).ok_or(Bail::Syntax)?;
            if !gap.is_empty() && !continuation_only(gap) {
                return Err(Bail::Unsupported("concatenation"));
            }
        }
        prev_end = Some(child.end_byte());
        let (v, d) = word_value(child, src, vars)?;
        if matches!(child.kind(), "word" | "number") {
            unquoted.push_str(text(child, src)?);
        } else {
            unquoted.push('\u{1}');
        }
        value.push_str(&v);
        dynamic |= d;
    }
    dynamic |= has_brace_expansion(&unquoted);
    Ok((value, dynamic))
}

fn string_value(node: Node, src: &str, vars: &Vars) -> Result<(String, bool), Bail> {
    let end = node.end_byte().checked_sub(1).ok_or(Bail::Syntax)?;
    let mut pos = node.start_byte() + 1;
    let mut value = String::new();
    let mut dynamic = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Literal text, the quotes and stray `$` tokens are covered by the gaps.
        if !child.is_named() || child.kind() == "string_content" {
            continue;
        }
        if child.start_byte() < pos || child.end_byte() > end {
            return Err(Bail::Syntax);
        }
        value.push_str(&unescape_double(
            src.get(pos..child.start_byte()).ok_or(Bail::Syntax)?,
        ));
        pos = child.end_byte();
        match child.kind() {
            "simple_expansion" | "expansion" => {
                check_expansion(child, src)?;
                match plain_name(child, src)?.and_then(|n| vars.quoted(n)) {
                    Some(v) => value.push_str(v),
                    None => {
                        value.push_str(text(child, src)?);
                        dynamic = true;
                    }
                }
            }
            // Quoted, the output stays one word: an absolute path (or empty).
            "command_substitution" => match allowed_substitution(child, src)? {
                Some(canonical) => value.push_str(canonical),
                None => return Err(Bail::CommandSubstitution),
            },
            "arithmetic_expansion" => return Err(Bail::Arithmetic),
            _ => return Err(unsupported(child)),
        }
    }
    value.push_str(&unescape_double(src.get(pos..end).ok_or(Bail::Syntax)?));
    Ok((value, dynamic))
}

/// Backslash resolution for an unquoted word: `\c` is `c`, `\<newline>` vanishes.
fn unescape_unquoted(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\r') if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            Some('\n') => {}
            Some(n) => out.push(n),
            None => out.push('\\'),
        }
    }
    out
}

/// Backslash resolution inside double quotes: only `\"`, `\\`, `\$`, `` \` ``
/// and `\<newline>` are special; any other backslash stays.
fn unescape_double(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some(n @ ('"' | '\\' | '$' | '`')) => {
                out.push(n);
                chars.next();
            }
            Some('\n') => {
                chars.next();
            }
            Some('\r') => {
                chars.next();
                if chars.peek() == Some(&'\n') {
                    chars.next();
                } else {
                    out.push_str("\\\r");
                }
            }
            _ => out.push('\\'),
        }
    }
    out
}

fn decode_ansi_c(t: &str) -> String {
    let c: Vec<char> = t.chars().collect();
    let mut out = String::with_capacity(t.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] != '\\' || i + 1 >= c.len() {
            out.push(c[i]);
            i += 1;
            continue;
        }
        i += 1;
        let e = c[i];
        i += 1;
        match e {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(e),
            'x' | 'u' | 'U' => {
                let max = match e {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let (n, used) = take_radix(&c[i..], 16, max);
                i += used;
                match (used, e) {
                    (0, _) => {
                        out.push('\\');
                        out.push(e);
                    }
                    (_, 'x') => out.push(char::from(u8::try_from(n).unwrap_or(0))),
                    _ => out.push(char::from_u32(n).unwrap_or('\u{fffd}')),
                }
            }
            '0'..='7' => {
                let (n, used) = take_radix(&c[i - 1..], 8, 3);
                i += used - 1;
                out.push(char::from(u8::try_from(n & 0xff).unwrap_or(0)));
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn take_radix(c: &[char], radix: u32, max: usize) -> (u32, usize) {
    let mut n: u32 = 0;
    let mut used = 0;
    for ch in c.iter().take(max) {
        match ch.to_digit(radix) {
            Some(d) => {
                n = n.saturating_mul(radix).saturating_add(d);
                used += 1;
            }
            None => break,
        }
    }
    (n, used)
}

/// Whether `t` (unquoted text) contains `{a,b}` or `{1..3}` style expansion.
/// Escaped braces do not count. One pass with a stack of open braces, each
/// remembering whether it has seen a top-level `,` or `..`.
fn has_brace_expansion(t: &str) -> bool {
    let b: Vec<char> = t.chars().collect();
    let mut open: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '\\' => i += 1,
            '{' => open.push(false),
            '}' => {
                if open.pop() == Some(true) {
                    return true;
                }
            }
            ',' => {
                if let Some(top) = open.last_mut() {
                    *top = true;
                }
            }
            '.' if b.get(i + 1) == Some(&'.') => {
                if let Some(top) = open.last_mut() {
                    *top = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}
