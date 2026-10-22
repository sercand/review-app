//! Pure bash command classifier for the AI reviewer's auto-approve.
//!
//! Bash is parsed with tree-sitter into a small [`model::Segment`] list; a rule
//! engine then decides run, ask or deny. The crate does no I/O and fails
//! closed: anything it cannot account for is a [`model::Bail`].

pub mod engine;
pub mod model;
pub mod parse;
pub mod policies;
pub mod policy;
pub mod repo;

pub use engine::{Decision, Reason, classify_stack, classify_stack_reason};
pub use model::{Bail, RedirOp, Redirect, Segment, Separator};
pub use parse::parse;
pub use policy::{Policy, PolicyVerdict, SegmentPolicy};
pub use repo::{DenyPaths, RepoAllow, default_deny_paths};

use std::path::Path;

/// What `classify` decided about one command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every segment was vouched for; `source` names the policy that did
    /// (`"read-only"`, `"git:read"`, `"repo-allow"`, `"structural"`,
    /// `"empty"` for a blank command).
    Run { source: &'static str },
    /// The command needs the user's approval.
    Ask { reason: String },
    /// A deny rule matched: the command must not run.
    Deny { reason: String },
}

/// Per-call configuration for [`classify`].
pub struct ApprovalConfig<'a> {
    /// Repo-configured commands: exact line or whole-word prefix, as today.
    pub allowed: &'a [String],
    /// Path-component deny patterns (trailing `*` = prefix), e.g. ".env*", ".ssh".
    pub deny_paths: &'a [String],
    /// Working directory for `mkdir_cwd`; None disables path-relative rules.
    pub cwd: Option<&'a Path>,
    /// Extra policies consulted after the rules (the model stage plugs in here).
    pub extra: &'a [&'a dyn SegmentPolicy],
}

/// Decide whether `command` runs silently, needs approval or is denied.
///
/// The policy stack is `[DenyPaths, RepoAllow, READ_ONLY, GIT_READ,
/// MKDIR_CWD, ..cfg.extra]`; the first policy with an opinion wins, and a
/// deny anywhere in the command (wrapper contents and redirects included)
/// denies the whole command. Anything not vouched for asks.
pub fn classify(command: &str, cfg: &ApprovalConfig<'_>) -> Verdict {
    if command.trim().is_empty() {
        return Verdict::Run { source: "empty" };
    }
    let deny = DenyPaths(cfg.deny_paths);
    let allow = RepoAllow(cfg.allowed);
    let mut stack: Vec<&dyn SegmentPolicy> = vec![
        &deny,
        &allow,
        &policies::READ_ONLY,
        &policies::GIT_READ,
        &policies::MKDIR_CWD,
    ];
    stack.extend_from_slice(cfg.extra);
    let (decision, reason) = classify_stack_reason(command, cfg.cwd, &stack);
    match decision {
        Decision::Approve { source } => Verdict::Run { source },
        Decision::Defer => Verdict::Ask {
            reason: describe(reason.as_ref(), "not in the allowed commands"),
        },
        Decision::Deny => Verdict::Deny {
            reason: describe(reason.as_ref(), "denied by a deny rule (protected path)"),
        },
    }
}

/// Plain-words reason; segment text is followed by `why`.
fn describe(reason: Option<&Reason>, why: &str) -> String {
    match reason {
        Some(Reason::Bail(b)) => bail_text(b),
        Some(Reason::Segment(text)) if !text.is_empty() => format!("{text}: {why}"),
        Some(Reason::Empty) | None | Some(Reason::Segment(_)) => why.to_string(),
        Some(r @ Reason::TooDeep) => r.to_string(),
    }
}

fn bail_text(b: &Bail) -> String {
    match b {
        Bail::CommandSubstitution => "command substitution".into(),
        Bail::ProcessSubstitution => "process substitution".into(),
        Bail::Heredoc => "heredoc".into(),
        Bail::HereString => "here-string".into(),
        Bail::Arithmetic => "arithmetic expansion".into(),
        Bail::Subshell => "subshell".into(),
        Bail::BraceGroup => "brace group".into(),
        Bail::CompoundCommand => "unsupported shell construct: if/for/while".into(),
        Bail::ArrayLiteral => "array assignment".into(),
        Bail::UnterminatedQuote => "unbalanced quote".into(),
        Bail::UnexpectedToken | Bail::Syntax => "syntax error".into(),
        Bail::Background => "background execution (&)".into(),
        Bail::FunctionDef => "function definition".into(),
        Bail::Unsupported(kind) => format!("unsupported shell construct: {kind}"),
    }
}
