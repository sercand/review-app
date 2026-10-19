//! Shared helpers for the integration (contract) tests.
//!
//! Integration tests cannot use `src/test_support.rs` (it is `#[cfg(test)]` and
//! therefore absent from the published library), so their helpers live here.
//!
//! Each integration test binary compiles this whole module but uses only part
//! of it, so unused helpers here are expected, not dead code.
#![allow(dead_code)]

use gh_review_desktop::diff::parse::parse_patch;
use gh_review_desktop::github::client::{Endpoints, GithubClient};
use gh_review_desktop::github::files::{FileStatus, LoadedFile, PatchState, PrFile};
use gh_review_desktop::model::diff::FileDiff;

/// A client pointed at a mock server, with a fixed token.
pub fn client(uri: &str) -> GithubClient {
    GithubClient::with_endpoints(
        Endpoints {
            web: uri.to_string(),
            rest: uri.to_string(),
            graphql: format!("{uri}/graphql"),
        },
        "tok",
    )
}

/// Character count of a synthetic minified line (spec §12 stress case).
const MINIFIED_CHARS: usize = 50_000;

/// Every `MINIFIED_EVERY`-th generated line, counted across the whole set,
/// is a minified line.
const MINIFIED_EVERY: usize = 50;

/// `n_files` parsed [`LoadedFile`]s of `rows_per_file` diff lines each, for
/// the large-PR performance guardrail.
///
/// Contents are deterministic — a pure function of a line's global index, no
/// RNG — so runs are comparable: line kinds cycle added/removed/context and
/// every 50th line is a [`MINIFIED_CHARS`]-character minified line, which
/// keeps the display-truncation and highlight-skip paths exercised. Each
/// file carries one hunk starting at line 1 and a `src/` path, so `build`
/// yields `rows_per_file` `Line` rows per file and a `src/*` filter keeps
/// everything.
pub fn synthetic_files(n_files: usize, rows_per_file: usize) -> Vec<LoadedFile> {
    let mut files = Vec::with_capacity(n_files);
    let mut line_no: usize = 0;
    for file_no in 0..n_files {
        let mut patch = format!("@@ -1,{rows_per_file} +1,{rows_per_file} @@\n");
        for _ in 0..rows_per_file {
            line_no += 1;
            patch.push(match line_no % 3 {
                0 => '+',
                1 => '-',
                _ => ' ',
            });
            if line_no.is_multiple_of(MINIFIED_EVERY) {
                patch.push_str(&minified_line(line_no));
            } else {
                patch.push_str(&format!(
                    "let value_{line_no} = {};",
                    line_no.wrapping_mul(7_919)
                ));
            }
            patch.push('\n');
        }

        // Mirror `test_support::loaded()`: keep the patch on the file and the
        // parsed hunks as its diff, failing loudly on a malformed patch.
        let hunks = parse_patch(&patch).expect("synthetic patches parse");
        files.push(LoadedFile {
            file: PrFile {
                path: format!("src/component_{file_no:04}/file_{file_no:04}.rs"),
                previous_path: None,
                status: FileStatus::Modified,
                additions: (rows_per_file / 3) as u32,
                deletions: (rows_per_file / 3) as u32,
                changes: rows_per_file as u32,
                patch: Some(patch),
                patch_state: PatchState::Present,
                blob_sha: format!("sha-{file_no:04}"),
            },
            diff: Some(FileDiff { hunks }),
        });
    }
    files
}

/// A deterministic ASCII minified line of exactly [`MINIFIED_CHARS`] chars.
fn minified_line(line_no: usize) -> String {
    let mut line = format!("var m{line_no}={{");
    while line.len() < MINIFIED_CHARS {
        line.push_str("a:0,");
    }
    line.truncate(MINIFIED_CHARS);
    line
}
