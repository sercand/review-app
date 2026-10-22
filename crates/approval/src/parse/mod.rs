//! Tree-sitter front end: bash source to [`Segment`]s.
//!
//! [`parse`] fails closed. Anything tree-sitter flags as `ERROR`/`MISSING`, and
//! any node kind the walker has no explicit rule for, is a [`Bail`].

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

/// `Bail::Unsupported` for `node`. `Node::kind` and `Language::node_kind_for_id`
/// borrow, so the `'static` names come from a table built once per process
/// (a few hundred short strings, leaked on purpose).
fn unsupported(node: tree_sitter::Node) -> Bail {
    static KINDS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    let kinds = KINDS.get_or_init(|| {
        let lang: tree_sitter::Language = tree_sitter_bash::LANGUAGE.into();
        let count = u16::try_from(lang.node_kind_count()).unwrap_or(u16::MAX);
        (0..count)
            .map(|id| {
                let name = lang.node_kind_for_id(id).unwrap_or("unknown");
                &*Box::leak(name.to_owned().into_boxed_str())
            })
            .collect()
    });
    Bail::Unsupported(
        kinds
            .get(usize::from(node.kind_id()))
            .copied()
            .unwrap_or("unknown"),
    )
}
