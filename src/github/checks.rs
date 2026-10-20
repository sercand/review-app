//! CI for one commit: check runs and the combined status (Task 50).
//!
//! Two endpoints describe one commit's CI — `check-runs` for Actions-style
//! checks and `status` for the legacy combined status — and each alone can
//! report "nothing" while the other carries the signal (GitHub sends the
//! combined `state` as `pending` even when no statuses exist, which is why
//! [`ci_state`] only trusts a pending combined status with `total_count > 0`).
//! The executor loads both and folds them into one rollup.

use serde::Deserialize;

use crate::github::client::GithubClient;
use crate::github::error::GhError;
use crate::model::RepoRef;
use crate::model::pr::CiState;

/// One check run, as the check-runs endpoint lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckRun {
    /// The run's database id, which keys the annotations endpoint.
    pub id: u64,
    /// The check's name, as its app reports it.
    pub name: String,
    /// `queued`, `in_progress`, or `completed`.
    pub status: String,
    /// How it finished; `None` while it has not finished.
    pub conclusion: Option<String>,
    /// Its web URL, when GitHub has one.
    pub html_url: Option<String>,
    /// `output.summary`, when GitHub sends one.
    pub output_summary: Option<String>,
    /// `output.text`, when GitHub sends one.
    pub output_text: Option<String>,
}

/// One annotation a check run attached to a file (the AI review reads these
/// off failing runs — spec §10).
#[derive(Clone, Debug, PartialEq)]
pub struct CheckAnnotation {
    /// The file the annotation points at.
    pub path: String,
    /// The first line it covers.
    pub start_line: u32,
    /// The last line it covers; GitHub sends the start line for a single-line
    /// annotation, and `0` when it has no line at all.
    pub end_line: u32,
    /// `notice`, `warning`, or `failure` (`annotation_level`).
    pub level: String,
    /// The annotation's title, when it has one.
    pub title: Option<String>,
    /// The annotation's message.
    pub message: String,
}

/// The legacy combined status of a commit.
#[derive(Clone, Debug, PartialEq)]
pub struct CombinedStatus {
    /// `success`, `failure`, `error`, or `pending`.
    pub state: String,
    /// How many individual statuses were combined.
    pub total_count: u32,
}

/// Parse a `GET /repos/{o}/{r}/commits/{sha}/check-runs` page body.
pub fn parse_check_runs(body: &str) -> Result<Vec<CheckRun>, GhError> {
    let page: RawCheckRunsPage = serde_json::from_str(body).map_err(decode)?;
    Ok(page.check_runs.into_iter().map(to_run).collect())
}

/// Parse a `GET /repos/{o}/{r}/commits/{sha}/status` body. GitHub sends
/// `"state": null` when there are no statuses; that reads as the empty
/// string, which is no signal at all.
pub fn parse_combined_status(body: &str) -> Result<CombinedStatus, GhError> {
    let raw: RawCombined = serde_json::from_str(body).map_err(decode)?;
    Ok(to_combined(raw))
}

/// The conclusions that mean the commit did not pass.
const FAILING_CONCLUSIONS: [&str; 4] = ["failure", "timed_out", "cancelled", "action_required"];

/// The rollup of both CI signals, or `None` when the commit has no CI at all.
///
/// A failing conclusion or a failing/erroring combined status wins over
/// anything still running; anything not completed (or a combined `pending`
/// that actually has statuses) is pending; any other signal is a success.
/// GitHub reports the combined state as `pending` even with no statuses, so
/// that alone is no signal — hence `None` for no runs and `total_count == 0`.
pub fn ci_state(runs: &[CheckRun], combined: &CombinedStatus) -> Option<CiState> {
    let failing = runs.iter().any(|run| {
        run.conclusion
            .as_deref()
            .is_some_and(|c| FAILING_CONCLUSIONS.contains(&c))
    }) || combined.state == "failure"
        || combined.state == "error";
    if failing {
        return Some(CiState::Failure);
    }
    let running = runs.iter().any(|run| run.status != "completed")
        || (combined.state == "pending" && combined.total_count > 0);
    if running {
        return Some(CiState::Pending);
    }
    if !runs.is_empty() || combined.total_count > 0 {
        return Some(CiState::Success);
    }
    None
}

impl GithubClient {
    /// Every check run for `sha`, following pagination.
    ///
    /// The response wraps the list in an object, so this pages with
    /// [`GithubClient::get_json`] rather than [`GithubClient::get_all_pages`].
    pub async fn list_check_runs(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<Vec<CheckRun>, GhError> {
        let mut all = Vec::new();
        let mut next = Some(format!(
            "/repos/{}/{}/commits/{sha}/check-runs?per_page=100",
            repo.owner, repo.name
        ));
        while let Some(path) = next {
            let (page, link) = self.get_json::<RawCheckRunsPage>(&path).await?;
            all.extend(page.check_runs.into_iter().map(to_run));
            next = link;
        }
        Ok(all)
    }

    /// Every annotation attached to one check run, following pagination.
    ///
    /// The AI review reads these off failing runs (spec §10). The response is
    /// a bare array, so this pages with [`GithubClient::get_all_pages`].
    pub async fn check_run_annotations(
        &self,
        repo: &RepoRef,
        check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>, GhError> {
        let path = format!(
            "/repos/{}/{}/check-runs/{check_run_id}/annotations?per_page=100",
            repo.owner, repo.name
        );
        let raws = self.get_all_pages::<RawCheckAnnotation>(&path).await?;
        Ok(raws.into_iter().map(to_annotation).collect())
    }

    /// The legacy combined status for `sha`.
    pub async fn combined_status(
        &self,
        repo: &RepoRef,
        sha: &str,
    ) -> Result<CombinedStatus, GhError> {
        let path = format!("/repos/{}/{}/commits/{sha}/status", repo.owner, repo.name);
        let (raw, _link) = self.get_json::<RawCombined>(&path).await?;
        Ok(to_combined(raw))
    }
}

/// One page of the check-runs list.
#[derive(Deserialize)]
struct RawCheckRunsPage {
    #[serde(default)]
    check_runs: Vec<RawCheckRun>,
}

/// One check run as GitHub sends it.
#[derive(Deserialize)]
struct RawCheckRun {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    output: Option<RawCheckRunOutput>,
}

/// The `output` object of a check run; its fields are `null` unless the app
/// filled them.
#[derive(Deserialize)]
struct RawCheckRunOutput {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// One annotation as the annotations endpoint sends it. `end_line` is absent
/// for a single-line annotation, and `title` is `null` without one.
#[derive(Deserialize)]
struct RawCheckAnnotation {
    #[serde(default)]
    path: String,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
    #[serde(default, rename = "annotation_level")]
    annotation_level: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    message: String,
}

/// Convert one raw check run; a missing `output` reads as no summary or text.
fn to_run(raw: RawCheckRun) -> CheckRun {
    CheckRun {
        id: raw.id,
        name: raw.name,
        status: raw.status,
        conclusion: raw.conclusion,
        html_url: raw.html_url,
        output_summary: raw.output.as_ref().and_then(|o| o.summary.clone()),
        output_text: raw.output.as_ref().and_then(|o| o.text.clone()),
    }
}

/// Convert one raw annotation; a missing line number reads as `0`.
fn to_annotation(raw: RawCheckAnnotation) -> CheckAnnotation {
    let start = raw.start_line.unwrap_or(0);
    CheckAnnotation {
        path: raw.path,
        start_line: start,
        end_line: raw.end_line.unwrap_or(start),
        level: raw.annotation_level,
        title: raw.title,
        message: raw.message,
    }
}

/// The combined-status body; `state` is `null` when there are no statuses.
#[derive(Deserialize)]
struct RawCombined {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    total_count: u32,
}

/// Convert the raw combined status; a `null` state reads as no signal.
fn to_combined(raw: RawCombined) -> CombinedStatus {
    CombinedStatus {
        state: raw.state.unwrap_or_default(),
        total_count: raw.total_count,
    }
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
    fn runs_carry_id_and_output_fields() {
        let runs = parse_check_runs(fixture!("check_runs.json")).unwrap();
        assert_eq!(runs[0].id, 1);
        assert_eq!(
            runs[0].output_summary.as_deref(),
            Some("cargo build exited 101")
        );
        assert_eq!(
            runs[0].output_text.as_deref(),
            Some("error[E0308]: mismatched types")
        );
        // A `null` output field reads as `None`, not the empty string.
        assert_eq!(runs[1].id, 2);
        assert_eq!(runs[1].output_summary, None);
        assert_eq!(runs[1].output_text, None);
    }

    #[test]
    fn ci_state_prefers_failure_then_pending_and_ignores_empty_status() {
        let runs = parse_check_runs(fixture!("check_runs.json")).unwrap();
        let empty = CombinedStatus {
            state: "pending".into(),
            total_count: 0,
        };
        assert_eq!(ci_state(&runs, &empty), Some(CiState::Failure));
        assert_eq!(ci_state(&[], &empty), None); // no CI configured
        assert_eq!(
            ci_state(
                &[],
                &CombinedStatus {
                    state: "pending".into(),
                    total_count: 2
                }
            ),
            Some(CiState::Pending)
        );
        assert_eq!(
            ci_state(
                &[],
                &CombinedStatus {
                    state: "success".into(),
                    total_count: 1
                }
            ),
            Some(CiState::Success)
        );
    }
}
