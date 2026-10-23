//! The on-disk model store under `<data dir>/models`, filled by
//! `shiso_decision::ensure_model` from the registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use shiso_decision::ensure_model;

use super::loader::{LoadError, load_error};

/// Owns the on-disk model store under `<data dir>/models`.
pub struct WeightsManager {
    home: PathBuf,
    /// Models this process has already ensured, by name: shiso has no
    /// offline "is it installed" probe, so this is what lets a re-enable skip
    /// the download states.
    ensured: Mutex<HashMap<String, PathBuf>>,
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
            ensured: Mutex::new(HashMap::new()),
        }
    }

    /// The install of `model` this process has ensured before, without
    /// touching the network or the disk.
    pub fn installed_dir(&self, model: &str) -> Option<PathBuf> {
        self.ensured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(model)
            .cloned()
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
        let installed = ensure_model(model, &self.home, progress).map_err(load_error)?;
        self.ensured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.to_owned(), installed.dir.clone());
        Ok(installed.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_is_the_models_directory() {
        let m = WeightsManager::new(Path::new("/cfg"));
        assert_eq!(m.home, Path::new("/cfg/models"));
        assert!(m.installed_dir("d1-3b").is_none());
    }

    #[test]
    fn a_build_without_a_backend_refuses_before_any_download() {
        // With no backend feature the call refuses before touching the
        // network; with one it asks the registry, which this test must not.
        if cfg!(any(
            feature = "local-classifier",
            feature = "local-classifier-cuda"
        )) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let m = WeightsManager::with_home(dir.path().to_owned());
        let got = m.ensure_blocking("d1-3b", &mut |_, _| true);
        assert!(matches!(got, Err(LoadError::Unavailable(_))), "{got:?}");
        assert!(m.installed_dir("d1-3b").is_none());
    }

    /// Downloads the real model (about 3.5 GB) into a temporary home.
    #[test]
    #[ignore = "downloads about 3.5 GB from the model registry"]
    fn real_pull_d1_3b() {
        let home = tempfile::tempdir().unwrap();
        let m = WeightsManager::with_home(home.path().to_owned());
        let mut last = (0, 0);
        let dir = m
            .ensure_blocking("d1-3b", &mut |done, total| {
                last = (done, total);
                true
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
        eprintln!("installed at {} after {last:?}", dir.display());
        assert!(dir.is_dir());
        assert!(last.1 > 0 && last.0 == last.1, "{last:?}");
        assert_eq!(m.installed_dir("d1-3b"), Some(dir.clone()));
        // A second call is answered from the store, without progress.
        let again = m.ensure_blocking("d1-3b", &mut |_, _| true).unwrap();
        assert_eq!(again, dir);
    }
}
