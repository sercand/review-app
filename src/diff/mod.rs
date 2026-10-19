//! The diff engine: parsing GitHub's unified patches, anchoring comments, and
//! (in later tasks) context expansion and local diffs.

pub mod anchor;
pub mod highlight;
pub mod parse;
pub mod row_model;
pub mod suggestion;
