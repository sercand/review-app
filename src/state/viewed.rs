//! The set of files a reviewer has marked viewed, keyed by blob SHA (spec §10.3).
//!
//! A file counts as viewed only while its stored SHA still matches the file's
//! current `blob_sha`. A new push therefore un-views exactly the files whose
//! contents changed — the ones GitHub also un-views — and leaves the rest alone.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::github::files::PrFile;

/// The viewed marks for one `(host, repo, PR)`, as `path → blob_sha when marked`.
///
/// The map is private so no caller can read a mark without comparing it to the
/// file it belongs to; use [`ViewedSet::is_viewed`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewedSet {
    marks: BTreeMap<String, String>,
}

impl ViewedSet {
    /// Whether `f` is viewed: its stored SHA equals its current `blob_sha`.
    pub fn is_viewed(&self, f: &PrFile) -> bool {
        self.marks
            .get(&f.path)
            .is_some_and(|sha| sha == &f.blob_sha)
    }

    /// Mark `f` viewed (storing its current SHA) or clear its mark.
    pub fn set(&mut self, f: &PrFile, viewed: bool) {
        if viewed {
            self.marks.insert(f.path.clone(), f.blob_sha.clone());
        } else {
            self.marks.remove(&f.path);
        }
    }

    /// Apply [`ViewedSet::set`] to each of `files`.
    pub fn set_many(&mut self, files: &[&PrFile], viewed: bool) {
        for f in files {
            self.set(f, viewed);
        }
    }

    /// How many of `files` are currently viewed.
    pub fn viewed_count(&self, files: &[PrFile]) -> usize {
        files.iter().filter(|f| self.is_viewed(f)).count()
    }

    /// How many of `files` were viewed but changed since — the
    /// "N files you had viewed changed since" hint (spec §10.3).
    pub fn invalidated_count(&self, files: &[PrFile]) -> usize {
        files
            .iter()
            .filter(|f| {
                self.marks
                    .get(&f.path)
                    .is_some_and(|sha| sha != &f.blob_sha)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files_at;

    #[test]
    fn only_changed_files_lose_viewed_state() {
        let files = files_at(&["a.rs", "b.rs", "c.rs"]);
        let mut v = ViewedSet::default();
        v.set(&files[0], true);
        v.set_many(&[&files[1], &files[2]], true);
        assert_eq!(v.viewed_count(&files), 3);

        // new push: b.rs changed, a.rs and c.rs did not
        let mut after = files.clone();
        after[1].blob_sha = "new".into();
        assert!(v.is_viewed(&after[0]));
        assert!(!v.is_viewed(&after[1]));
        assert_eq!(v.viewed_count(&after), 2);
        assert_eq!(v.invalidated_count(&after), 1);

        v.set(&after[1], true); // re-marking stores the new SHA
        assert_eq!(v.invalidated_count(&after), 0);
        v.set(&after[0], false);
        assert!(!v.is_viewed(&after[0]));
    }
}
