//! Tree-sitter front end: bash source to [`Segment`]s.
//!
//! [`parse`] fails closed. Anything tree-sitter flags as `ERROR`/`MISSING`, and
//! any node kind the walker has no explicit rule for, is a [`Bail`].

mod vars;
mod walk;
mod words;

#[cfg(test)]
mod tests;

use crate::model::{Bail, Segment};

/// Parse `src` into a flat list of command segments.
///
/// A fresh `Parser` is built per call (cheap). A NUL byte is rejected up front:
/// bash truncates at NUL while tree-sitter does not, so the two would disagree.
pub fn parse(src: &str) -> Result<Vec<Segment>, Bail> {
    if src.contains('\0') {
        return Err(Bail::Syntax);
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|_| Bail::Syntax)?;
    let tree = parser.parse(src, None).ok_or(Bail::Syntax)?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(Bail::Syntax);
    }
    walk::segments(root, src)
}

/// `Bail::Unsupported` for `node`. `Node::kind` borrows from the node, so the
/// `'static` name is located in the grammar's own `NODE_TYPES` JSON; a kind
/// that cannot be found (or needs JSON escaping) is reported as "unknown".
fn unsupported(node: tree_sitter::Node) -> Bail {
    let kind = node.kind();
    let needle = format!("\"type\": \"{kind}\"");
    let name = tree_sitter_bash::NODE_TYPES
        .find(&needle)
        .and_then(|at| {
            let start = at + needle.len() - kind.len() - 1;
            tree_sitter_bash::NODE_TYPES.get(start..start + kind.len())
        })
        .filter(|found| *found == kind)
        .unwrap_or("unknown");
    Bail::Unsupported(name)
}
