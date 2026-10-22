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

pub use engine::{Decision, Reason, classify_stack, classify_stack_reason};
pub use model::{Bail, RedirOp, Redirect, Segment, Separator};
pub use parse::parse;
pub use policy::{Policy, PolicyVerdict, SegmentPolicy};
