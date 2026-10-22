//! Tree walk: statements to [`Segment`]s.
//!
//! Every node kind the walker understands is listed in [`HANDLED`]; anything
//! else is [`Bail::Unsupported`]. Later constructs (lists, pipelines, redirects)
//! extend [`HANDLED`] and [`statement`].

use tree_sitter::Node;

use super::unsupported;
use super::words::{continuation_only, is_word_kind, text, word_value};
use crate::model::{Bail, Segment, Separator};

/// Statement-level node kinds the walker understands. Word kinds are listed in
/// `words::WORD_KINDS`.
pub(super) const HANDLED: &[&str] = &[
    "program",
    "command",
    "command_name",
    "variable_assignment",
    "variable_assignments",
    "comment",
];

/// Reject a node kind with no explicit rule, naming it.
fn ensure_handled(node: Node) -> Result<(), Bail> {
    let kind = node.kind();
    if HANDLED.contains(&kind) || is_word_kind(kind) {
        Ok(())
    } else {
        Err(unsupported(node))
    }
}

pub(super) fn segments(root: Node, src: &str) -> Result<Vec<Segment>, Bail> {
    ensure_handled(root)?;
    let mut out = Vec::new();
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
        out.push(statement(child, src)?);
    }
    // Top-level statements are separated by `;` or a newline; the last ends.
    let last = out.len().saturating_sub(1);
    for seg in out.iter_mut().take(last) {
        seg.follows = Separator::Semi;
    }
    Ok(out)
}

fn statement(node: Node, src: &str) -> Result<Segment, Bail> {
    ensure_handled(node)?;
    match node.kind() {
        "command" => command(node, src),
        "variable_assignment" => {
            let mut seg = empty_segment();
            seg.assigns.push(assignment(node, src)?);
            Ok(seg)
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
            Ok(seg)
        }
        _ => Err(unsupported(node)),
    }
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
                let (v, d) = word_value(inner, src)?;
                seg.argv.push(v);
                seg.dynamic.push(d);
                prev = Some((child.end_byte(), true));
            }
            kind if is_word_kind(kind) => {
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
            _ => return Err(unsupported(child)),
        }
    }
    Ok(seg)
}
