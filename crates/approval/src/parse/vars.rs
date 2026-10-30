//! Shell variables whose value is known from the command line itself.
//!
//! `N=crates/x; grep foo $N/a.rs` is common in agent commands. Without help
//! `$N/a.rs` is a run-time word and most rules refuse it. [`Vars`] remembers
//! `NAME=literal` statements (and `for` loop words) that are certain to have
//! run before a later word is expanded, so the parser can put the literal in
//! its place. Anything less certain makes the name unknown again, and an
//! unknown name expands to a dynamic word exactly as before: the persistent
//! shell may hold any value for it.

/// Characters a known value may hold and still be substituted into an
/// unquoted word: no whitespace (word splitting), no glob or extglob
/// characters (pathname expansion), no backslash (glob escapes), no quotes.
fn unquoted_safe(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '/' | ':' | '=' | ',' | '+' | '-' | '@' | '%')
        })
}

/// Whether `value`, written as a literal assignment value, means exactly its
/// text: no expansion (`$`, backtick) and no tilde, which bash expands in an
/// assignment value (`N=~/x`, `N=a:~/x`).
pub(super) fn literal_value(value: &str) -> bool {
    !value.contains(['$', '`', '~'])
}

/// Whether `name` is a plain shell variable name.
pub(super) fn is_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Known variables at one point of the walk, plus what a nested scope
/// changed (so the caller can forget those names when the scope may or may
/// not have run).
#[derive(Debug, Clone, Default)]
pub(super) struct Vars {
    known: Vec<(String, String)>,
    /// Names this scope assigned or made unknown.
    touched: Vec<String>,
    /// This scope ran something that can assign any variable (`read`,
    /// `eval`, a run-time command name): nothing is known afterwards.
    cleared: bool,
}

impl Vars {
    /// The value to put in place of `$name` inside double quotes.
    pub(super) fn quoted(&self, name: &str) -> Option<&str> {
        self.known
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The value to put in place of an unquoted `$name`, when bash would
    /// neither split it nor glob it.
    pub(super) fn unquoted(&self, name: &str) -> Option<&str> {
        self.quoted(name).filter(|v| unquoted_safe(v))
    }

    /// Record `name=value` (`Some`) or forget `name` (`None`).
    pub(super) fn set(&mut self, name: &str, value: Option<String>) {
        self.known.retain(|(n, _)| n != name);
        if let Some(v) = value {
            self.known.push((name.to_string(), v));
        }
        self.touched.push(name.to_string());
    }

    /// Forget every variable.
    pub(super) fn clear(&mut self) {
        self.known.clear();
        self.cleared = true;
    }

    /// A copy to walk a nested construct with; hand it back to
    /// [`Vars::forget_touched`] afterwards.
    pub(super) fn scope(&self) -> Vars {
        Vars {
            known: self.known.clone(),
            touched: Vec::new(),
            cleared: false,
        }
    }

    /// After a nested scope that may or may not have run (a `||` branch, a
    /// pipeline element, a loop body): whatever it assigned is unknown here.
    pub(super) fn forget_touched(&mut self, inner: Vars) {
        if inner.cleared {
            self.clear();
        }
        for name in inner.touched {
            self.set(&name, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquoted_needs_a_plain_value() {
        let mut v = Vars::default();
        v.set("A", Some("crates/x-y.rs".into()));
        v.set("B", Some("a b".into()));
        v.set("C", Some("*.rs".into()));
        v.set("D", Some(String::new()));
        assert_eq!(v.unquoted("A"), Some("crates/x-y.rs"));
        for n in ["B", "C", "D", "E"] {
            assert_eq!(v.unquoted(n), None, "{n}");
        }
        assert_eq!(v.quoted("B"), Some("a b"));
        assert_eq!(v.quoted("D"), Some(""));
    }

    #[test]
    fn scopes_forget_what_they_touched() {
        let mut v = Vars::default();
        v.set("A", Some("1".into()));
        v.set("B", Some("2".into()));
        let mut inner = v.scope();
        inner.set("A", Some("3".into()));
        v.forget_touched(inner);
        assert_eq!(v.quoted("A"), None);
        assert_eq!(v.quoted("B"), Some("2"));
        let mut inner = v.scope();
        inner.clear();
        v.forget_touched(inner);
        assert_eq!(v.quoted("B"), None);
    }
}
