//! AI review building blocks (spec §10).
//!
//! Compiled only on Unix: the sandboxed agent shell this feature grows around
//! (`review-sandbox`) has no Windows backend yet, so the whole AI surface is
//! gated the same way.

pub mod agent;
pub mod chat;
pub mod checklist;
pub mod checkout;
pub mod context;
pub mod events;
pub mod hub;
pub mod incremental;
pub mod interaction;
pub mod llm;
pub mod model;
pub mod pipeline;
pub mod presets;
pub mod prompts;
pub mod repo_link;
pub mod review;
pub mod snapshot;
pub mod stages;
pub mod store;
#[cfg(any(test, feature = "ai-testing"))]
pub mod testing;
pub mod tools;
pub mod ui_interaction;

pub use interaction::{ApprovalDecision, Interaction, NoInteraction, Question};
pub use tools::{AgentRef, AgentRole};
