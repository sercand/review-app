//! AI review building blocks (spec §10).
//!
//! Compiled only on Unix: the sandboxed agent shell this feature grows around
//! (`review-sandbox`) has no Windows backend yet, so the whole AI surface is
//! gated the same way.

pub mod llm;
