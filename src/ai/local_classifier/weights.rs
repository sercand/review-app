//! Fetching and verifying the classifier weights.
//!
//! The weights come straight from Hugging Face, pinned to one commit and to
//! per-file sizes and SHA-256 digests compiled in below. shiso-hub's
//! [`Downloader`] does the transfer (resumable, digest-verified, content
//! addressed under `<home>/blobs/sha256`); this module then hard-links the
//! verified blobs into one directory that `Classifier::open` can read.

use std::fs;
use std::path::{Path, PathBuf};

use shiso_hub::download::{DownloadFileDesc, FileOrigin};
use shiso_hub::recipe::{FileEntry, Role, Sha256};
use shiso_hub::{Downloader, HubConfig, ProgressEvent, probe_free_disk};
use tokio::sync::mpsc;

// To bump the model: upload the new files to the Hugging Face repo, then
// update HF_REPO / HF_REVISION (the full 40-character commit, never a branch)
// and every size and digest in `FILES` (`shasum -a 256 <file>`). The install
// directory is named after the revision, so a bump installs beside the old
// version rather than over it.
pub const HF_REPO: &str = "sercand/da-command-classifier-fp16";
pub const HF_REVISION: &str = "60d071c06ee2efc2c44c37d1a982b388e2a789a9";

/// One pinned file of the model.
pub struct PinnedFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const FILES: [PinnedFile; 3] = [
    PinnedFile {
        name: "model.safetensors",
        size: 134_048_052,
        sha256: "37a2389f90dc87483eca1082ddf54380b7bd1d22924272d57dea41067c8276bc",
    },
    PinnedFile {
        name: "config.json",
        size: 589,
        sha256: "cfadc74846fb8ccddc4aa29e87596c22c1e2301e0c67c684418e0c81fa7149f0",
    },
    PinnedFile {
        name: "tokenizer.json",
        size: 711_661,
        sha256: "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
    },
];

fn total_bytes() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

/// Owns the on-disk model store under `<data dir>/models`.
pub struct WeightsManager {
    config: HubConfig,
}

impl WeightsManager {
    pub fn new(data_dir: &Path) -> WeightsManager {
        WeightsManager::with_config(HubConfig::with_defaults(data_dir.join("models")))
    }

    fn with_config(config: HubConfig) -> WeightsManager {
        WeightsManager { config }
    }

    fn install_dir(&self) -> PathBuf {
        self.config.home.join("classifier").join(HF_REVISION)
    }

    /// The install directory when every file is present at its pinned size,
    /// else `None`. No network and no hashing: digests were checked when the
    /// blobs were downloaded.
    pub fn installed_dir(&self) -> Option<PathBuf> {
        let dir = self.install_dir();
        let complete = FILES
            .iter()
            .all(|f| fs::metadata(dir.join(f.name)).is_ok_and(|m| m.len() == f.size));
        complete.then_some(dir)
    }

    /// Downloads whatever is missing (resuming partial files), verifies every
    /// digest and installs the files. `progress` gets `(done, total)` bytes
    /// and returns `false` to cancel: the fetch is dropped (partial files stay
    /// for the next resume) and an error comes back.
    pub async fn ensure(
        &self,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, String> {
        let total = total_bytes();
        if let Some(dir) = self.installed_dir() {
            progress(total, total);
            return Ok(dir);
        }
        let downloader = Downloader::new(self.config.clone());
        let entries: Vec<FileEntry> = FILES
            .iter()
            .map(|f| {
                Ok(FileEntry {
                    name: f.name.to_string(),
                    size: f.size,
                    sha256: Sha256::parse(f.sha256).map_err(|e| e.to_string())?,
                    binds: None,
                })
            })
            .collect::<Result<_, String>>()?;

        // Bytes already on disk count as done from the start.
        let mut done: Vec<u64> = entries
            .iter()
            .map(|e| {
                if downloader.is_blob_present(&e.sha256, e.size) {
                    e.size
                } else {
                    0
                }
            })
            .collect();
        let missing = total - done.iter().sum::<u64>();
        if missing > 0 {
            fs::create_dir_all(&self.config.home).map_err(|e| e.to_string())?;
            let free = probe_free_disk(&self.config.home).map_err(|e| e.to_string())?;
            if free < missing {
                return Err(format!(
                    "not enough disk space: {missing} bytes needed, {free} free"
                ));
            }
        }
        if !progress(done.iter().sum(), total) {
            return Err(CANCELLED.into());
        }

        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(64);
        let fetch = async {
            let tx = tx;
            for entry in &entries {
                downloader
                    .download_file(DownloadFileDesc {
                        model_name: "da-command-classifier",
                        variant_key: "fp16",
                        role: Role::Base,
                        origin: FileOrigin::HuggingFace {
                            repo: HF_REPO,
                            revision: HF_REVISION,
                        },
                        file: entry,
                        progress_tx: Some(&tx),
                    })
                    .await?;
            }
            Ok::<(), shiso_hub::HubError>(())
        };
        // Resolves to `true` if the caller cancelled, `false` once the fetch
        // has dropped its sender.
        let report = async {
            while let Some(event) = rx.recv().await {
                let (file, bytes) = match event {
                    ProgressEvent::FileProgress { file, bytes, .. } => (file, bytes),
                    ProgressEvent::FileVerified { file, total, .. } => (file, total),
                    _ => continue,
                };
                if let Some(i) = entries.iter().position(|e| e.name == file) {
                    done[i] = bytes;
                    if !progress(done.iter().sum(), total) {
                        return true;
                    }
                }
            }
            false
        };
        let mut fetch = std::pin::pin!(fetch);
        let mut report = std::pin::pin!(report);
        let fetched = tokio::select! {
            r = &mut fetch => {
                // The sender is gone: let the reporter drain what is queued
                // so the last progress call is the final one.
                report.await;
                r
            }
            cancelled = &mut report => {
                if cancelled {
                    return Err(CANCELLED.into());
                }
                fetch.await
            }
        };
        fetched.map_err(|e| e.to_string())?;

        self.install(&downloader, &entries)
    }

    /// Blocking [`ensure`](Self::ensure) on its own current-thread runtime;
    /// call from a dedicated thread, never from the UI or a tokio worker.
    pub fn ensure_blocking(
        &self,
        progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("starting the download runtime: {e}"))?
            .block_on(self.ensure(progress))
    }

    /// Links the verified blobs into the install directory, staged beside it
    /// and renamed into place so a half-built directory is never visible. The
    /// staging name is unique to this process and call, so two app instances
    /// installing at once cannot trample each other.
    fn install(&self, downloader: &Downloader, entries: &[FileEntry]) -> Result<PathBuf, String> {
        let final_dir = self.install_dir();
        let parent = final_dir.parent().expect("install dir has a parent");
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let staging = parent.join(format!(
            ".{HF_REVISION}.{}-{nonce:x}.staging",
            std::process::id()
        ));
        fs::create_dir(&staging).map_err(|e| e.to_string())?;
        let linked = (|| {
            for e in entries {
                let blob = downloader.blob_path(&e.sha256);
                let target = staging.join(&e.name);
                if fs::hard_link(&blob, &target).is_err() {
                    fs::copy(&blob, &target)
                        .map_err(|err| format!("installing {}: {err}", e.name))?;
                }
            }
            Ok::<(), String>(())
        })();
        if let Err(e) = linked {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        // Another instance may have finished while we staged.
        if self.installed_dir().is_some() {
            let _ = fs::remove_dir_all(&staging);
            return Ok(final_dir);
        }
        let _ = fs::remove_dir_all(&final_dir);
        fs::rename(&staging, &final_dir).map_err(|e| {
            let _ = fs::remove_dir_all(&staging);
            e.to_string()
        })?;
        Ok(final_dir)
    }
}

const CANCELLED: &str = "cancelled";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    /// Sparse blobs of the pinned sizes, named by digest, as the downloader
    /// leaves them: `ensure` must install them without any network.
    fn seed_blobs(m: &WeightsManager) {
        let blobs = m.config.home.join("blobs").join("sha256");
        fs::create_dir_all(&blobs).unwrap();
        for f in &FILES {
            let file = fs::File::create(blobs.join(f.sha256)).unwrap();
            file.set_len(f.size).unwrap();
        }
    }

    #[test]
    fn pinned_digests_parse() {
        for f in &FILES {
            Sha256::parse(f.sha256).unwrap();
        }
        assert_eq!(HF_REVISION.len(), 40);
    }

    #[test]
    fn nothing_installed_in_an_empty_dir() {
        let dir = tempdir();
        assert_eq!(WeightsManager::new(dir.path()).installed_dir(), None);
    }

    #[test]
    fn models_live_under_the_data_dir() {
        let dir = tempdir();
        let m = WeightsManager::new(dir.path());
        assert!(m.install_dir().starts_with(dir.path().join("models")));
    }

    #[test]
    fn ensure_installs_present_blobs_without_network() {
        let dir = tempdir();
        let m = WeightsManager::new(dir.path());
        seed_blobs(&m);
        let mut last = (0, 0);
        let installed = m
            .ensure_blocking(|d, t| {
                last = (d, t);
                true
            })
            .unwrap();
        assert_eq!(m.installed_dir(), Some(installed.clone()));
        assert_eq!(last, (total_bytes(), total_bytes()));
        for f in &FILES {
            assert_eq!(fs::metadata(installed.join(f.name)).unwrap().len(), f.size);
        }
        // A second call is a no-op that still reports completion.
        m.ensure_blocking(|_, _| true).unwrap();
    }

    #[test]
    fn a_missing_or_truncated_file_means_not_installed() {
        let dir = tempdir();
        let m = WeightsManager::new(dir.path());
        seed_blobs(&m);
        let installed = m.ensure_blocking(|_, _| true).unwrap();
        let cfg = installed.join("config.json");
        fs::remove_file(&cfg).unwrap();
        fs::File::create(&cfg).unwrap().set_len(3).unwrap();
        assert_eq!(m.installed_dir(), None);
        fs::remove_file(&cfg).unwrap();
        assert_eq!(m.installed_dir(), None);
    }

    #[test]
    fn cancelling_before_the_fetch_stops_without_network() {
        let dir = tempdir();
        let m = WeightsManager::new(dir.path());
        let err = m.ensure_blocking(|_, _| false).unwrap_err();
        assert_eq!(err, CANCELLED);
        assert_eq!(m.installed_dir(), None);
    }

    #[test]
    #[ignore = "downloads ~135 MB from Hugging Face"]
    fn real_download_installs_and_verifies() {
        let dir = tempdir();
        let m = WeightsManager::new(dir.path());
        let started = std::time::Instant::now();
        let mut last = (0, 0);
        let installed = m
            .ensure_blocking(|d, t| {
                if d == t || d.saturating_sub(last.0) > 20_000_000 {
                    eprintln!("progress {d}/{t}");
                }
                last = (d, t);
                true
            })
            .expect("download");
        eprintln!("downloaded and installed in {:?}", started.elapsed());
        assert_eq!(last, (total_bytes(), total_bytes()));
        assert_eq!(m.installed_dir(), Some(installed.clone()));
        // Independent check of what landed on disk.
        use sha2::{Digest, Sha256 as H};
        for f in &FILES {
            let bytes = fs::read(installed.join(f.name)).unwrap();
            let digest: String = H::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(digest, f.sha256, "{}", f.name);
        }
    }
}
