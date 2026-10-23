//! Local command classifier on a shiso decision model.

pub mod loader;
pub mod schema;
pub mod weights;

pub use loader::{ClassifierState, Loader};
pub use weights::WeightsManager;
