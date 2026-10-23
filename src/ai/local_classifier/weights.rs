//! The on-disk model store under `<data dir>/models`, filled from the shiso
//! registry by `shiso_runtime::hub`.

use std::path::{Path, PathBuf};

use shiso_runtime::hub;
use shiso_runtime::runtime::OpenError;

use super::loader::{LoadError, load_error};

/// Owns the on-disk model store under `<data dir>/models`.
///
/// It keeps no claim on an install: a loaded model holds its own lock through
/// its `Runtime` (see `ShisoEngine`), so the lock lives exactly as long as the
/// classifier that reads the files.
pub struct WeightsManager {
    home: PathBuf,
}

impl WeightsManager {
    /// A store under `<data_dir>/models`.
    pub fn new(data_dir: &Path) -> WeightsManager {
        WeightsManager::with_home(data_dir.join("models"))
    }

    /// A store rooted at `home` itself.
    pub fn with_home(home: PathBuf) -> WeightsManager {
        WeightsManager { home }
    }

    /// The store's root.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The complete install of `model`, found offline (no network, checked on
    /// every call). `None` when it is not installed or the store cannot be
    /// read.
    pub fn installed_dir(&self, model: &str) -> Option<PathBuf> {
        // The claim is dropped at once: only the path is wanted here.
        hub::resolve(model, &hub::config_for_home(&self.home))
            .ok()?
            .map(|found| found.dir.clone())
    }

    /// Blocking: finds `model` in the store or downloads it, verifying and
    /// installing it. Call from a plain thread, never an async task.
    /// `progress(done, total)` returns `false` to cancel; partial files are
    /// kept, so the next call resumes.
    pub fn ensure_blocking(
        &self,
        model: &str,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, LoadError> {
        if let Some(dir) = self.installed_dir(model) {
            return Ok(dir);
        }
        hub::pull(model, &hub::config_for_home(&self.home), progress)
            .map(|pulled| pulled.dir.clone())
            .map_err(|e| load_error(OpenError::Pull(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_is_the_models_directory() {
        let m = WeightsManager::new(Path::new("/cfg"));
        assert_eq!(m.home, Path::new("/cfg/models"));
        assert!(m.installed_dir("smollm2-135m-decision").is_none());
    }

    /// Downloads the real model (about 0.2 GiB) into a temporary home.
    #[test]
    #[ignore = "downloads about 0.2 GiB from the model registry"]
    fn real_pull_smollm2() {
        let home = tempfile::tempdir().unwrap();
        let m = WeightsManager::with_home(home.path().to_owned());
        let mut last = (0, 0);
        let dir = m
            .ensure_blocking("smollm2-135m-decision", &mut |done, total| {
                last = (done, total);
                true
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
        eprintln!("installed at {} after {last:?}", dir.display());
        assert!(dir.is_dir());
        assert!(last.1 > 0 && last.0 == last.1, "{last:?}");
        assert_eq!(m.installed_dir("smollm2-135m-decision"), Some(dir.clone()));
        // A second call is answered from the store, without progress.
        let again = m
            .ensure_blocking("smollm2-135m-decision", &mut |_, _| true)
            .unwrap();
        assert_eq!(again, dir);
    }
}
