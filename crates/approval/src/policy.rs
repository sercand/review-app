// Adapted from amenocturne/da (MIT), src/lib.rs
//! The policy abstraction the engine consults per segment.
//!
//! Rule policies are plain `fn` values wrapped in [`Policy`]; policies that
//! carry per-call data (repo allow-lists, deny paths, the model stage) implement
//! [`SegmentPolicy`] directly.

use std::path::Path;

use crate::model::Segment;

/// What a single policy says about a single segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// This policy vouches for the segment.
    Approve,
    /// This policy actively rejects the segment.
    Deny,
    /// This policy cannot rule the segment out (a glob that might expand to a
    /// protected path) and so no policy may approve it: the engine defers,
    /// whatever the rest of the stack says. Not a deny; a literal deny
    /// anywhere in the command still wins.
    Withhold,
}

/// One rule in the stack. First policy with an opinion wins.
pub trait SegmentPolicy: Send + Sync {
    /// Stable id (e.g. `"git:read"`), reported as the approval's source.
    fn name(&self) -> &'static str;
    /// Inspect one segment: `Some(Approve)` vouches, `Some(Deny)` rejects,
    /// `Some(Withhold)` stops every policy in the stack from approving it
    /// (the engine checks it in its screen pass, before any policy is
    /// consulted), `None` abstains and lets later policies speak.
    fn verify(&self, seg: &Segment, cwd: Option<&Path>) -> Option<PolicyVerdict>;
    /// For a segment this policy denied: what matched, for the reason text
    /// (e.g. `protected path ".env"`).
    fn explain_deny(&self, _seg: &Segment) -> Option<String> {
        None
    }
}

/// A policy made of a name and a plain function; each capability is one
/// `pub static` value.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub name: &'static str,
    pub verify: fn(&Segment, Option<&Path>) -> Option<PolicyVerdict>,
}

impl SegmentPolicy for Policy {
    fn name(&self) -> &'static str {
        self.name
    }

    fn verify(&self, seg: &Segment, cwd: Option<&Path>) -> Option<PolicyVerdict> {
        (self.verify)(seg, cwd)
    }
}
