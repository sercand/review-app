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
}

/// One rule in the stack. First policy with an opinion wins.
pub trait SegmentPolicy: Send + Sync {
    /// Stable id (e.g. `"git:read"`), reported as the approval's source.
    fn name(&self) -> &'static str;
    /// Inspect one segment: `Some(Approve)` vouches, `Some(Deny)` rejects,
    /// `None` abstains and lets later policies speak.
    fn verify(&self, seg: &Segment, cwd: Option<&Path>) -> Option<PolicyVerdict>;
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
