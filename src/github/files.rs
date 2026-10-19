//! The files API: `GET /repos/{owner}/{repo}/pulls/{number}/files`.
//!
//! Each entry becomes a [`PrFile`]. GitHub omits `patch` for binary files and
//! for very large text diffs, and does not say which, so [`PrFile::patch_state`]
//! is derived from `changes` (spec §7.5). The executor pages this endpoint and
//! parses every patch on the worker thread ([`LoadedFile::parse`]).

use serde::Deserialize;

use crate::diff::parse::parse_patch;
use crate::github::error::GhError;
use crate::model::diff::FileDiff;

/// How GitHub describes a file's change (`status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileStatus {
    /// The file is new.
    Added,
    /// The file was deleted.
    Removed,
    /// The file's contents changed.
    Modified,
    /// The file was renamed; `previous_path` holds the old name.
    Renamed,
    /// The file was copied; `previous_path` holds the source.
    Copied,
    /// Only the mode changed (no textual patch).
    Changed,
    /// The file is identical to its previous version.
    Unchanged,
}

impl FileStatus {
    /// Map the API's `status` string; `None` for a value this build does not know.
    pub fn from_wire(value: &str) -> Option<FileStatus> {
        match value {
            "added" => Some(FileStatus::Added),
            "removed" => Some(FileStatus::Removed),
            "modified" => Some(FileStatus::Modified),
            "renamed" => Some(FileStatus::Renamed),
            "copied" => Some(FileStatus::Copied),
            "changed" => Some(FileStatus::Changed),
            "unchanged" => Some(FileStatus::Unchanged),
            _ => None,
        }
    }
}

/// Whether GitHub sent a patch for a file, and if not, why (spec §7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchState {
    /// `patch` is present and can be parsed.
    Present,
    /// `patch` is absent and `changes == 0`: a binary file.
    Binary,
    /// `patch` is absent but `changes > 0`: a diff too large to send.
    TooLarge,
}

/// One file in a PR, as GitHub's files API describes it.
///
/// Deserializes from the API's wire shape (`filename`, `previous_filename`,
/// `sha`, …); the derived [`PatchState`] is not a wire field.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(from = "WireFile")]
pub struct PrFile {
    /// The API `filename`: the new path, and the path comments anchor to.
    pub path: String,
    /// The API `previous_filename`, for renames and copies.
    pub previous_path: Option<String>,
    /// What kind of change this is.
    pub status: FileStatus,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// Total changed lines, as GitHub counts them.
    pub changes: u32,
    /// The raw unified diff GitHub sent, when it sent one.
    pub patch: Option<String>,
    /// Whether `patch` is present, and if not, why.
    pub patch_state: PatchState,
    /// The API `sha`: the blob's SHA, which keys viewed state (spec §10.3).
    pub blob_sha: String,
}

/// One page of `/pulls/{n}/files` as GitHub sends it.
#[derive(Deserialize)]
struct WireFile {
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    additions: u32,
    #[serde(default)]
    deletions: u32,
    #[serde(default)]
    changes: u32,
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    sha: String,
}

impl From<WireFile> for PrFile {
    fn from(file: WireFile) -> PrFile {
        let patch_state = match (file.patch.is_some(), file.changes) {
            (true, _) => PatchState::Present,
            (false, 0) => PatchState::Binary,
            (false, _) => PatchState::TooLarge,
        };

        PrFile {
            path: file.filename,
            previous_path: file.previous_filename,
            // GitHub's status set is closed; an unknown value is treated as a
            // modification rather than dropping the file.
            status: FileStatus::from_wire(&file.status).unwrap_or(FileStatus::Modified),
            additions: file.additions,
            deletions: file.deletions,
            changes: file.changes,
            patch: file.patch,
            patch_state,
            blob_sha: file.sha,
        }
    }
}

/// One file plus the diff parsed from its patch.
///
/// `diff` is `None` unless GitHub sent a patch ([`PatchState::Present`]) and it
/// parsed cleanly; a binary or too-large file has no diff to show.
#[derive(Clone, Debug)]
pub struct LoadedFile {
    /// The file, as the files API described it.
    pub file: PrFile,
    /// The parsed diff, when there is one.
    pub diff: Option<FileDiff>,
}

impl LoadedFile {
    /// Attach the parsed diff for a file, on the worker thread.
    pub fn parse(file: PrFile) -> LoadedFile {
        let diff = if file.patch_state == PatchState::Present {
            file.patch
                .as_deref()
                .and_then(|patch| parse_patch(patch).ok())
                .map(|hunks| FileDiff { hunks })
        } else {
            None
        };

        LoadedFile { file, diff }
    }
}

/// Parse one page of the files API.
pub fn parse_files_page(body: &str) -> Result<Vec<PrFile>, GhError> {
    serde_json::from_str(body).map_err(decode)
}

/// The path to show in a file header: `old → new` for a file that moved.
pub fn display_path(f: &PrFile) -> String {
    match &f.previous_path {
        Some(previous) if matches!(f.status, FileStatus::Renamed | FileStatus::Copied) => {
            format!("{previous} → {}", f.path)
        }
        _ => f.path.clone(),
    }
}

/// The path a comment must anchor to: always the API `filename`, never
/// `previous_filename` (spec §7.3).
pub fn anchor_path(f: &PrFile) -> &str {
    &f.path
}

/// Map a JSON decode failure to [`GhError::Decode`].
fn decode(error: serde_json::Error) -> GhError {
    GhError::Decode(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn statuses_paths_and_patch_states() {
        let files = parse_files_page(fixture!("pr_files_page.json")).unwrap();
        let renamed = files
            .iter()
            .find(|f| f.status == FileStatus::Renamed)
            .unwrap();
        assert_eq!(display_path(renamed), "src/old name.rs → src/new name.rs");
        assert_eq!(anchor_path(renamed), "src/new name.rs");

        let deleted = files
            .iter()
            .find(|f| f.status == FileStatus::Removed)
            .unwrap();
        assert_eq!(anchor_path(deleted), "src/gone.rs");

        let binary = files.iter().find(|f| f.path.ends_with(".png")).unwrap();
        assert_eq!(
            (binary.patch.is_none(), binary.patch_state),
            (true, PatchState::Binary)
        );

        let huge = files
            .iter()
            .find(|f| f.path.ends_with("huge.json"))
            .unwrap();
        assert_eq!(huge.patch_state, PatchState::TooLarge);

        let unicode = files.iter().find(|f| f.path.contains("ünïcode")).unwrap();
        assert_eq!(anchor_path(unicode), unicode.path);

        let changed = files.iter().find(|f| f.path == "bin/tool").unwrap();
        assert_eq!(changed.status, FileStatus::Changed); // mode change
    }
}
