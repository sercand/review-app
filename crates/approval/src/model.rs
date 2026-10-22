// Adapted from amenocturne/da (MIT), src/shparse/model.rs
//! AST the classifier consumes. Minimal on purpose: everything we don't
//! need to classify safely either stays inside a `Word` (as opaque bytes) or
//! causes the lexer to return `Bail`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub assigns: Vec<(String, String)>,
    pub argv: Vec<String>,
    /// Parallel to `argv`: `true` when the word's final value is not known
    /// from the source text — it contains a `$` expansion (unquoted or in
    /// double quotes) or an unquoted brace expansion. Such a word can turn
    /// into a flag, or into several words, at run time. A missing entry
    /// means "literal".
    pub dynamic: Vec<bool>,
    pub redirects: Vec<Redirect>,
    pub follows: Separator,
}

impl Segment {
    /// Whether `argv[i]` may expand to something other than its source text.
    pub fn is_dynamic(&self, i: usize) -> bool {
        self.dynamic.get(i).copied().unwrap_or(false)
    }

    /// Whether any argv word is dynamic.
    pub fn has_dynamic(&self) -> bool {
        self.dynamic.iter().any(|d| *d)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    End,
    Semi,
    And,
    Or,
    Pipe,
    PipeBoth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub fd: Option<u32>,
    pub op: RedirOp,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirOp {
    /// `>`  write, truncate
    OutTrunc,
    /// `>>` write, append
    OutAppend,
    /// `<`  read
    In,
    /// `>&` dup output fd (target is a digit or `-`)
    OutDup,
    /// `<&` dup input fd
    InDup,
    /// `&>` stdout+stderr to file (truncate)
    OutAll,
    /// `&>>` stdout+stderr to file (append)
    OutAllAppend,
    /// `>|` force truncate (ignore noclobber)
    OutClobber,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bail {
    CommandSubstitution,
    ProcessSubstitution,
    Heredoc,
    HereString,
    Arithmetic,
    Subshell,
    BraceGroup,
    /// Compound command reserved word at command position: if/while/for/case/...
    CompoundCommand,
    /// Array literal: VAR=( ... )
    ArrayLiteral,
    UnterminatedQuote,
    UnexpectedToken,
    /// Tree-sitter reported an ERROR or MISSING node (or failed to parse).
    Syntax,
    /// Background execution: `cmd &`.
    Background,
    /// Function definition.
    FunctionDef,
    /// A node kind the walker does not handle; carries the kind name.
    Unsupported(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(dynamic: Vec<bool>) -> Segment {
        Segment {
            assigns: vec![],
            argv: vec!["a".into(), "b".into()],
            dynamic,
            redirects: vec![],
            follows: Separator::End,
        }
    }

    #[test]
    fn segment_dynamic_flags_default_literal() {
        let s = seg(vec![]);
        assert!(!s.is_dynamic(3));
        assert!(!s.has_dynamic());
        let s = seg(vec![false, true]);
        assert!(s.has_dynamic());
        assert!(s.is_dynamic(1));
        assert!(!s.is_dynamic(0));
    }
}
