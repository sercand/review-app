//! The PR snapshot the review agents read (spec §10).
//!
//! The UI already loads PR metadata and files before a run starts, so
//! [`gather_snapshot`] takes those as given and fetches only what the agents
//! additionally need: review threads, the conversation, check runs, and the
//! annotations of runs that did not pass. The result is immutable and shared
//! with every agent of the run through [`crate::ai::tools::ToolCx`], so the
//! `gh_*` tools never hit the network themselves.

use crate::github::checks::{CheckAnnotation, CheckRun};
use crate::github::client::GithubClient;
use crate::github::conversation::ConversationComment;
use crate::github::error::GhError;
use crate::github::files::LoadedFile;
use crate::github::metadata::PrMetadata;
use crate::model::RepoRef;
use crate::model::diff::Side;
use crate::model::thread::ReviewThread;

/// One check run with the annotations the verifier cares about.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckSummary {
    /// The check's name.
    pub name: String,
    /// How it finished; `None` while it has not finished.
    pub conclusion: Option<String>,
    /// `output.summary`, when GitHub sent one.
    pub summary: Option<String>,
    /// `output.text`, when GitHub sent one.
    pub text: Option<String>,
    /// Its annotations; empty unless the conclusion was one that carries
    /// annotations worth reading ([`ANNOTATED_CONCLUSIONS`]).
    pub annotations: Vec<CheckAnnotation>,
}

impl CheckSummary {
    /// The summary of one run; annotations are attached separately because
    /// they cost a request per run and only some conclusions carry them.
    fn new(run: CheckRun, annotations: Vec<CheckAnnotation>) -> CheckSummary {
        CheckSummary {
            name: run.name,
            conclusion: run.conclusion,
            summary: run.output_summary,
            text: run.output_text,
            annotations,
        }
    }
}

/// Everything the agents may ask about one PR, frozen at run start.
#[derive(Clone, Debug, PartialEq)]
pub struct PrSnapshot {
    /// The repository the PR lives in.
    pub repo: RepoRef,
    /// The PR number within the repository.
    pub number: u64,
    /// The PR's metadata: title, author, refs, SHAs, labels, body.
    pub metadata: PrMetadata,
    /// The PR's files, each with its parsed diff when GitHub sent a patch.
    pub files: Vec<LoadedFile>,
    /// The existing review threads.
    pub threads: Vec<ReviewThread>,
    /// The conversation (issue) comments.
    pub conversation: Vec<ConversationComment>,
    /// The head commit's check runs, with annotations for the ones that did
    /// not pass.
    pub checks: Vec<CheckSummary>,
}

/// The conclusions whose runs carry annotations worth reading.
///
/// `success` and `skipped` runs have nothing to learn from, and a run that
/// has not concluded yet has no annotations at all.
const ANNOTATED_CONCLUSIONS: [&str; 5] = [
    "failure",
    "neutral",
    "timed_out",
    "action_required",
    "cancelled",
];

/// Fetch what the snapshot needs on top of `files` and `metadata`, which the
/// UI has already loaded.
///
/// Reads only, so a failure is returned as-is; the run has nothing to undo.
pub async fn gather_snapshot(
    client: &GithubClient,
    repo: &RepoRef,
    number: u64,
    files: Vec<LoadedFile>,
    metadata: PrMetadata,
) -> Result<PrSnapshot, GhError> {
    let threads = client.review_threads(repo, number).await?;
    let conversation = client.list_issue_comments(repo, number).await?;
    let runs = client.list_check_runs(repo, &metadata.head_sha).await?;

    let mut checks = Vec::with_capacity(runs.len());
    for run in runs {
        let annotated = run
            .conclusion
            .as_deref()
            .is_some_and(|c| ANNOTATED_CONCLUSIONS.contains(&c));
        let annotations = if annotated {
            client.check_run_annotations(repo, run.id).await?
        } else {
            Vec::new()
        };
        checks.push(CheckSummary::new(run, annotations));
    }

    Ok(PrSnapshot {
        repo: repo.clone(),
        number,
        metadata,
        files,
        threads,
        conversation,
        checks,
    })
}

impl PrSnapshot {
    /// Every commentable line of `path`'s diff, as `(side, line)`.
    ///
    /// Added and context rows are on the new side, removed rows on the old
    /// side — the same rule [`crate::diff::parse::row_anchor`] uses for
    /// comment defaults (spec §7.3). A path the PR does not touch, or one
    /// with no parsed diff (binary or too large), has no commentable lines.
    pub fn diff_lines(&self, path: &str) -> Vec<(Side, u32)> {
        self.files
            .iter()
            .find(|f| f.file.path == path)
            .and_then(|f| f.diff.as_ref())
            .map(|diff| {
                diff.hunks
                    .iter()
                    .flat_map(|hunk| hunk.rows.iter())
                    .filter_map(crate::diff::parse::row_anchor)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::files::{PatchState, PrFile};
    use crate::model::diff::Side;
    use crate::test_support;

    /// A [`LoadedFile`] whose patch is parsed by [`LoadedFile::parse`] itself.
    fn parsed(path: &str, patch: &str) -> LoadedFile {
        let file = PrFile {
            patch: Some(patch.to_owned()),
            patch_state: PatchState::Present,
            ..test_support::file(path)
        };
        LoadedFile::parse(file)
    }

    fn snapshot_with(files: Vec<LoadedFile>) -> PrSnapshot {
        PrSnapshot {
            repo: test_support::repo(),
            number: 7,
            metadata: test_support::meta("sha_head"),
            files,
            threads: Vec::new(),
            conversation: Vec::new(),
            checks: Vec::new(),
        }
    }

    #[test]
    fn diff_lines_cover_added_context_and_removed_sides() {
        let patch = "@@ -10,4 +12,4 @@\n ctx\n+added\n-removed\n ctx2\n";
        let binary = PrFile {
            patch: None,
            patch_state: PatchState::Binary,
            ..test_support::file("logo.png")
        };
        let snapshot = snapshot_with(vec![parsed("a.rs", patch), LoadedFile::parse(binary)]);

        // Context and added rows are RIGHT at their new number; removed rows
        // are LEFT at their old number. Numbering starts at the hunk header.
        assert_eq!(
            snapshot.diff_lines("a.rs"),
            vec![
                (Side::Right, 12), // ctx
                (Side::Right, 13), // added
                (Side::Left, 11),  // removed
                (Side::Right, 14), // ctx2
            ]
        );

        // A file with no parsed diff, and an unknown path, have no lines.
        assert!(snapshot.diff_lines("logo.png").is_empty());
        assert!(snapshot.diff_lines("missing.rs").is_empty());
    }
}
