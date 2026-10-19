//! The right-pane CI checks list (Task 50).
//!
//! The rollup line comes from [`ci_state`], which weighs the check runs
//! against the legacy combined status; each run then gets its own row, and
//! a row with a URL opens that run in the browser.

use eframe::egui;

use crate::github::checks::{ci_state, CheckRun, CombinedStatus};
use crate::model::pr::CiState;

/// Draw the CI rollup and one row per check run.
///
/// `combined` is `None` while the status is still loading; the rollup then
/// reads it as "no signal" rather than guessing.
pub fn render(ui: &mut egui::Ui, runs: &[CheckRun], combined: Option<&CombinedStatus>) {
    let empty = CombinedStatus {
        state: String::new(),
        total_count: 0,
    };
    match ci_state(runs, combined.unwrap_or(&empty)) {
        Some(CiState::Success) => ui.label("✓ Checks passing"),
        Some(CiState::Failure) => ui.colored_label(ui.visuals().warn_fg_color, "✗ Checks failing"),
        Some(CiState::Pending) => ui.weak("… Checks running"),
        None => ui.weak("No checks"),
    };

    for run in runs {
        ui.horizontal(|ui| {
            ui.label(glyph(run));
            match &run.html_url {
                Some(url) => {
                    if ui.link(&run.name).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                    }
                }
                None => {
                    ui.label(&run.name);
                }
            }
            let detail = if run.status != "completed" {
                run.status.as_str()
            } else {
                run.conclusion.as_deref().unwrap_or_default()
            };
            if !detail.is_empty() {
                ui.weak(detail);
            }
        });
    }
}

/// The row's state mark: running, passed, failed, or neither.
fn glyph(run: &CheckRun) -> &'static str {
    if run.status != "completed" {
        return "…";
    }
    match run.conclusion.as_deref() {
        Some("success") => "✓",
        Some("failure") | Some("timed_out") | Some("cancelled") | Some("action_required") => "✗",
        _ => "–",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::github::checks::parse_check_runs;

    /// The loaded list, the rollup-only case, and the empty case must all
    /// paint without panicking.
    #[test]
    fn render_paints_the_rollup_and_the_rows() {
        let runs = parse_check_runs(fixture!("check_runs.json")).unwrap();
        assert!(!runs.is_empty());
        let combined = CombinedStatus {
            state: "failure".into(),
            total_count: 2,
        };
        egui::__run_test_ui(|ui| {
            render(ui, &runs, Some(&combined));
            render(ui, &[], Some(&combined));
            render(ui, &[], None);
        });
    }
}
