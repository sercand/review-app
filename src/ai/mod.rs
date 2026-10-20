//! AI review building blocks (spec §10).
//!
//! Compiled only on Unix: the sandboxed agent shell this feature grows around
//! (`review-sandbox`) has no Windows backend yet, so the whole AI surface is
//! gated the same way.

pub mod agent;
pub mod checklist;
pub mod checkout;
pub mod context;
pub mod events;
pub mod interaction;
pub mod llm;
pub mod model;
pub mod review;
pub mod snapshot;
pub mod store;
#[cfg(any(test, feature = "ai-testing"))]
pub mod testing;
pub mod tools;

pub use interaction::{ApprovalDecision, Interaction, NoInteraction, Question};
pub use tools::{AgentRef, AgentRole};
