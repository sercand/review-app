//! Persistence: settings, atomic writes, session and draft files.

use std::io::Write;
use std::path::{Path, PathBuf};

pub mod session;
pub mod settings;

/// The application's config directory
/// (`~/Library/Application Support/gh-review-desktop` on macOS, `~/.config/gh-review-desktop`
/// on Linux, `%APPDATA%\gh-review-desktop` on Windows).
pub fn config_root() -> PathBuf {
    directories::ProjectDirs::from("", "", "gh-review-desktop")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".gh-review-desktop"))
}

/// Write `bytes` to `path` so that readers never observe a partial file.
///
/// The bytes go to `<path>.tmp`, are flushed to disk, and the temp file is then
/// renamed over `path` (an atomic replace on every supported platform).
/// Parent directories are created as needed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);

    {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let dir = tempdir();
        let p = dir.path().join("nested/x.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn config_root_is_under_the_application_name() {
        assert!(config_root().ends_with("gh-review-desktop"));
    }
}
