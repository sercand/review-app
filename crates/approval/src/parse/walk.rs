//! Tree walk: statements to [`Segment`]s.
//!
//! Every node kind the walker understands is listed in [`HANDLED`]; any other
//! kind is a [`Bail`]: the specific one from [`bail_for`] for compound
//! constructs, else [`Bail::Unsupported`]. [`emit`] flattens lists, pipelines,
//! negations and redirected statements into one segment per simple command.

use tree_sitter::Node;

use super::unsupported;
use super::words::{continuation_only, is_word_kind, text, word_value};
use crate::model::{Bail, RedirOp, Redirect, Segment, Separator};

/// Maximum statement nesting before the walker gives up.
const MAX_DEPTH: usize = 64;

/// Statement-level node kinds the walker understands. Word kinds are listed in
/// `words::WORD_KINDS`.
pub(super) const HANDLED: &[&str] = &[
    "program",
    "list",
    "pipeline",
    "negated_command",
    "redirected_statement",
    "command",
    "command_name",
    "variable_assignment",
    "variable_assignments",
    "file_redirect",
    "comment",
];

/// Words bash reserves at command position. tree-sitter only treats most of
/// them as keywords inside their own compound statement; after `!` or `time`
/// they come out as plain words, which must never become a simple command.
const RESERVED: &[&str] = &[
    "{", "}", "if", "then", "else", "elif", "fi", "while", "until", "do", "done", "for", "in",
    "case", "esac", "select", "function", "coproc", "[[", "]]", "!",
];

/// Whether `node` (a word about to join `seg`) would sit in command position,
/// possibly after `time [-p] [--]`, and is an unquoted reserved word.
fn reserved_at_command_position(seg: &Segment, node: Node, src: &str) -> Result<bool, Bail> {
    let leading = seg
        .argv
        .iter()
        .all(|a| matches!(a.as_str(), "time" | "-p" | "--"));
    Ok(leading && node.kind() == "word" && RESERVED.contains(&text(node, src)?))
}

/// The bail for a node kind with no explicit rule.
fn bail_for(node: Node) -> Bail {
    match node.kind() {
        "subshell" => Bail::Subshell,
        "compound_statement" => Bail::BraceGroup,
        "if_statement"
        | "while_statement"
        | "for_statement"
        | "c_style_for_statement"
        | "case_statement"
        | "select_statement" => Bail::CompoundCommand,
        "function_definition" => Bail::FunctionDef,
        "heredoc_redirect" => Bail::Heredoc,
        "herestring_redirect" => Bail::HereString,
        _ => unsupported(node),
    }
}

fn ensure_handled(node: Node) -> Result<(), Bail> {
    let kind = node.kind();
    if HANDLED.contains(&kind) || is_word_kind(kind) {
        Ok(())
    } else {
        Err(bail_for(node))
    }
}

pub(super) fn segments(root: Node, src: &str) -> Result<Vec<Segment>, Bail> {
    ensure_handled(root)?;
    let mut out: Vec<Segment> = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if !child.is_named() {
            // Separator tokens between top-level statements.
            match child.kind() {
                ";" | "\n" => continue,
                ";;" => return Err(Bail::Syntax),
                "&" => return Err(Bail::Background),
                _ => return Err(unsupported(child)),
            }
        }
        if child.kind() == "comment" {
            continue;
        }
        // Top-level statements are separated by `;` or a newline.
        if let Some(prev) = out.last_mut() {
            prev.follows = Separator::Semi;
        }
        emit(child, src, 0, &mut out)?;
    }
    Ok(out)
}

/// Append the segments of statement `node`. The last one keeps
/// `Separator::End`; the caller sets the separator that follows it.
fn emit(node: Node, src: &str, depth: usize, out: &mut Vec<Segment>) -> Result<(), Bail> {
    if depth > MAX_DEPTH {
        return Err(Bail::Unsupported("depth"));
    }
    ensure_handled(node)?;
    match node.kind() {
        "command" => out.push(command(node, src)?),
        "list" | "pipeline" => sequence(node, src, depth, out)?,
        "negated_command" => {
            // `!` only flips the exit status; the command is classified as is.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match (child.is_named(), child.kind()) {
                    (false, "!") => {}
                    (true, _) => emit(child, src, depth + 1, out)?,
                    _ => return Err(unsupported(child)),
                }
            }
        }
        "redirected_statement" => redirected(node, src, depth, out)?,
        "variable_assignment" => {
            let mut seg = empty_segment();
            seg.assigns.push(assignment(node, src)?);
            out.push(seg);
        }
        // Several bare assignments: `A=1 B=2`.
        "variable_assignments" => {
            let mut seg = empty_segment();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if !child.is_named() || child.kind() != "variable_assignment" {
                    return Err(unsupported(child));
                }
                seg.assigns.push(assignment(child, src)?);
            }
            out.push(seg);
        }
        _ => return Err(unsupported(node)),
    }
    Ok(())
}

/// `list` and `pipeline`: statements joined by `&&`, `||`, `|`, `|&`.
fn sequence(node: Node, src: &str, depth: usize, out: &mut Vec<Segment>) -> Result<(), Bail> {
    let mut pending: Option<Separator> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            pending = Some(match child.kind() {
                "&&" => Separator::And,
                "||" => Separator::Or,
                "|" => Separator::Pipe,
                "|&" => Separator::PipeBoth,
                ";" | "\n" => Separator::Semi,
                "&" => return Err(Bail::Background),
                _ => return Err(unsupported(child)),
            });
            continue;
        }
        if child.kind() == "comment" {
            continue;
        }
        if let (Some(sep), Some(prev)) = (pending.take(), out.last_mut()) {
            prev.follows = sep;
        }
        emit(child, src, depth + 1, out)?;
    }
    Ok(())
}

/// `redirected_statement`: a body (usually a command) plus redirects. With no
/// body (`> f`) the segment is just the redirects.
fn redirected(node: Node, src: &str, depth: usize, out: &mut Vec<Segment>) -> Result<(), Bail> {
    let body = node.child_by_field_name("body");
    let start = out.len();
    let mut redirects = Vec::new();
    let mut trailing = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Err(unsupported(child));
        }
        if Some(child.id()) == body.map(|b| b.id()) {
            emit(child, src, depth + 1, out)?;
        } else if child.kind() == "comment" {
            continue;
        } else {
            let (r, words) = redirect(child, src)?;
            redirects.push(r);
            trailing.extend(words);
        }
    }
    if body.is_none() {
        out.push(empty_segment());
    }
    // When the last command of a list or pipeline has a redirect, tree-sitter
    // wraps the whole list as the body. Bash binds the redirect to the last
    // simple command, so that is where it (and any trailing words) goes.
    if out.len() <= start {
        return Err(Bail::Unsupported("redirected_statement"));
    }
    let seg = out.last_mut().ok_or(Bail::Syntax)?;
    seg.redirects.extend(redirects);
    for (v, d) in trailing {
        seg.argv.push(v);
        seg.dynamic.push(d);
    }
    Ok(())
}

/// Words that tree-sitter files under a redirect (`cat > o x` puts `x` inside
/// the `file_redirect`): `(value, dynamic)`, in order. They are command args.
type TrailingWords = Vec<(String, bool)>;

/// One redirect child: `file_redirect`, or the bails for here-docs/strings.
fn redirect(node: Node, src: &str) -> Result<(Redirect, TrailingWords), Bail> {
    if node.kind() != "file_redirect" {
        return Err(bail_for(node));
    }
    let mut fd = None;
    let mut op = None;
    let mut target = None;
    let mut trailing = Vec::new();
    let mut prev_end = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            op = Some(match child.kind() {
                ">" => (RedirOp::OutTrunc, None),
                ">>" => (RedirOp::OutAppend, None),
                "<" => (RedirOp::In, None),
                ">&" => (RedirOp::OutDup, None),
                "<&" => (RedirOp::InDup, None),
                "&>" => (RedirOp::OutAll, None),
                "&>>" => (RedirOp::OutAllAppend, None),
                ">|" => (RedirOp::OutClobber, None),
                ">&-" => (RedirOp::OutDup, Some("-")),
                "<&-" => (RedirOp::InDup, Some("-")),
                _ => return Err(unsupported(child)),
            });
        } else if child.kind() == "file_descriptor" {
            fd = Some(text(child, src)?.parse::<u32>().map_err(|_| Bail::Syntax)?);
        } else if is_word_kind(child.kind()) {
            let fixed_target = op.is_some_and(|(_, fixed)| fixed.is_some());
            if target.is_none() && !fixed_target {
                let (value, dynamic) = word_value(child, src)?;
                // A dynamic target keeps its source text; the engine rejects it.
                // A literal one must not look like an expansion (`'$TMPDIR'/x`),
                // or it would be indistinguishable from the dynamic spelling.
                if !dynamic && value.contains(['$', '`']) {
                    return Err(Bail::Unsupported("redirect_target"));
                }
                target = Some(value);
            } else {
                // A word glued to the previous one by `\<newline>` would need
                // joining; anything else is a separate argument.
                let gap = src.get(prev_end..child.start_byte()).ok_or(Bail::Syntax)?;
                if continuation_only(gap) {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                trailing.push(word_value(child, src)?);
            }
        } else {
            return Err(bail_for(child));
        }
        prev_end = child.end_byte();
    }
    let (op, fixed) = op.ok_or(Bail::Syntax)?;
    let target = match (target, fixed) {
        (Some(t), None) => t,
        (None, Some(t)) => t.to_string(),
        _ => return Err(Bail::Syntax),
    };
    Ok((Redirect { fd, op, target }, trailing))
}

fn empty_segment() -> Segment {
    Segment {
        assigns: Vec::new(),
        argv: Vec::new(),
        dynamic: Vec::new(),
        redirects: Vec::new(),
        follows: Separator::End,
    }
}

fn assignment(node: Node, src: &str) -> Result<(String, String), Bail> {
    let name = match node.child_by_field_name("name") {
        Some(n) => text(n, src)?.to_string(),
        None => return Err(Bail::Syntax),
    };
    let value = match node.child_by_field_name("value") {
        None => String::new(),
        Some(v) if v.kind() == "array" => return Err(Bail::ArrayLiteral),
        // A dynamic value keeps its source text, so `$` stays visible.
        Some(v) => word_value(v, src)?.0,
    };
    Ok((name, value))
}

fn command(node: Node, src: &str) -> Result<Segment, Bail> {
    let mut seg = empty_segment();
    let mut prev: Option<(usize, bool)> = None; // (end byte, was a word)
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            return Err(unsupported(child));
        }
        let joins = match prev {
            Some((end, _)) => {
                continuation_only(src.get(end..child.start_byte()).ok_or(Bail::Syntax)?)
            }
            None => false,
        };
        match child.kind() {
            "variable_assignment" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                seg.assigns.push(assignment(child, src)?);
                prev = Some((child.end_byte(), false));
            }
            "command_name" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                let inner = child.named_child(0).ok_or(Bail::Syntax)?;
                if reserved_at_command_position(&seg, inner, src)? {
                    return Err(Bail::Unsupported("reserved_word"));
                }
                let (v, d) = word_value(inner, src)?;
                seg.argv.push(v);
                seg.dynamic.push(d);
                prev = Some((child.end_byte(), true));
            }
            kind if is_word_kind(kind) => {
                if reserved_at_command_position(&seg, child, src)? {
                    return Err(Bail::Unsupported("reserved_word"));
                }
                let (v, d) = word_value(child, src)?;
                match (joins, prev) {
                    // `a\<newline>b` is the single word `ab`.
                    (true, Some((_, true))) => {
                        if let (Some(last), Some(dynamic)) =
                            (seg.argv.last_mut(), seg.dynamic.last_mut())
                        {
                            // Brace expansion could now span the join
                            // (`{-delete\<nl>,}`), so refuse braces.
                            let braces = |t: &str| t.contains(['{', '}']);
                            if braces(last) || braces(&v) {
                                return Err(Bail::Unsupported("line_continuation"));
                            }
                            last.push_str(&v);
                            *dynamic |= d;
                        }
                    }
                    (true, _) => return Err(Bail::Unsupported("line_continuation")),
                    _ => {
                        seg.argv.push(v);
                        seg.dynamic.push(d);
                    }
                }
                prev = Some((child.end_byte(), true));
            }
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                if joins {
                    return Err(Bail::Unsupported("line_continuation"));
                }
                let (r, words) = redirect(child, src)?;
                seg.redirects.push(r);
                let trailing = !words.is_empty();
                for (v, d) in words {
                    seg.argv.push(v);
                    seg.dynamic.push(d);
                }
                prev = Some((child.end_byte(), trailing));
            }
            _ => return Err(bail_for(child)),
        }
    }
    Ok(seg)
}
