//! The on-disk model store under `<data dir>/models`, filled from the shiso
//! registry by `shiso_runtime::hub`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use shiso_runtime::hub::{self, InstalledModel};
use shiso_runtime::runtime::OpenError;

use super::loader::{LoadError, load_error};

/// Owns the on-disk model store under `<data dir>/models`.
pub struct WeightsManager {
    home: PathBuf,
    /// The installs this process has found or pulled, by name. Each entry
    /// holds the install's lock, which is what keeps `shiso rm` and a
    /// reinstall from removing files a loaded model is reading: the model is
    /// opened from the bare directory, so the lock must outlive it.
    installed: Mutex<HashMap<String, InstalledModel>>,
}

impl WeightsManager {
    /// A store under `<data_dir>/models`.
    pub fn new(data_dir: &Path) -> WeightsManager {
        WeightsManager::with_home(data_dir.join("models"))
    }

    /// A store rooted at `home` itself.
    pub fn with_home(home: PathBuf) -> WeightsManager {
        WeightsManager {
            home,
            installed: Mutex::new(HashMap::new()),
        }
    }

    /// The complete install of `model`, found offline (no network). `None`
    /// when it is not installed or the store cannot be read.
    pub fn installed_dir(&self, model: &str) -> Option<PathBuf> {
        let mut installed = self.installed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(found) = installed.get(model) {
            return Some(found.dir.clone());
        }
        let found = hub::resolve(model, &hub::config_for_home(&self.home)).ok()??;
        let dir = found.dir.clone();
        installed.insert(model.to_owned(), found);
        Some(dir)
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
        // Not `pull` for an install already held: reinstalling a variant
        // needs its lock exclusively.
        if let Some(dir) = self.installed_dir(model) {
            return Ok(dir);
        }
        let pulled = hub::pull(model, &hub::config_for_home(&self.home), progress)
            .map_err(|e| load_error(OpenError::Pull(e)))?;
        let dir = pulled.dir.clone();
        self.installed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.to_owned(), pulled);
        Ok(dir)
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
