//! Local command classifier backed by the shiso inference stack.

pub mod calibrate;
pub mod loader;
pub mod shiso;

pub use loader::ClassifierState;
pub use shiso::{ShisoClassifier, load, open_backend};
