//! The on-disk model store under `<data dir>/models`.
//!
//! Compile-only stand-in: the decision-model download (`ensure_model`) is
//! wired in by Task B5, which replaces this file's body.

use std::path::{Path, PathBuf};

/// Owns the on-disk model store under `<data dir>/models`.
pub struct WeightsManager;

impl WeightsManager {
    pub fn new(_data_dir: &Path) -> WeightsManager {
        WeightsManager
    }

    /// The verified install, without touching the network. Nothing is
    /// installable until B5.
    pub fn installed_dir(&self) -> Option<PathBuf> {
        None
    }

    /// Blocking download + install; not implemented until B5.
    pub fn ensure_blocking(
        &self,
        _progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, String> {
        Err("model download is not implemented yet".into())
    }
}
