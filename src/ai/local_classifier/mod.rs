//! Local command classifier backed by the shiso inference stack.

pub mod calibrate;
pub mod loader;
pub mod shiso;
pub mod weights;

pub use loader::{ClassifierState, Loader};
pub use shiso::{ShisoClassifier, load, open_backend};
pub use weights::WeightsManager;
