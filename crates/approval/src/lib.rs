//! Pure bash command classifier for the AI reviewer's auto-approve.
//!
//! Bash is parsed with tree-sitter into a small [`model::Segment`] list; a rule
//! engine (added in later tasks) then decides run, ask or deny. The crate does
//! no I/O and fails closed: anything it cannot account for is a [`model::Bail`].

pub mod model;

pub use model::{Bail, RedirOp, Redirect, Segment, Separator};
