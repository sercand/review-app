//! The CI checks list (Task 50, restyled by redesign Task 13): the rollup
//! line, then one row per run — state icon, name, and detail in `faint`.
//! A row with a URL opens that run in the browser.

use eframe::egui::{self, RichText};

use crate::github::checks::{ci_state, CheckRun, CombinedStatus};
use crate::model::pr::CiState;
use crate::ui::{icons, theme};

/// Draw the CI rollup and one row per check run.
///
/// `combined` is `None` while the status is still loading; the rollup then
/// reads it as "no signal" rather than guessing.
pub fn render(ui: &mut egui::Ui, runs: &[CheckRun], combined: Option<&CombinedStatus>) {
    let p = theme::palette_of(ui);
    let empty = CombinedStatus {
        state: String::new(),
        total_count: 0,
    };
    match ci_state(runs, combined.unwrap_or(&empty)) {
        Some(CiState::Success) => {
            ui.label(
                RichText::new(icons::icon_text(icons::CIRCLE_CHECK, "Checks passing")).color(p.ok),
            );
        }
        Some(CiState::Failure) => {
            ui.label(
                RichText::new(icons::icon_text(icons::CIRCLE_X, "Checks failing")).color(p.bad),
            );
        }
        Some(CiState::Pending) => {
            ui.label(
                RichText::new(icons::icon_text(icons::CIRCLE_DOT_DASHED, "Checks running"))
                    .color(p.warn),
            );
        }
        None => {
            ui.weak("No checks");
        }
    }

    for run in runs {
        ui.horizontal(|ui| {
            let (glyph, color) = if run.status != "completed" {
                (icons::CIRCLE_DOT_DASHED, p.warn)
            } else {
                match run.conclusion.as_deref() {
                    Some("success") => (icons::CIRCLE_CHECK, p.ok),
                    Some("failure")
                    | Some("timed_out")
                    | Some("cancelled")
                    | Some("action_required") => (icons::CIRCLE_X, p.bad),
                    _ => (icons::CIRCLE, p.faint),
                }
            };
            ui.label(RichText::new(glyph).color(color));
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
                ui.colored_label(p.faint, detail);
            }
        });
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
