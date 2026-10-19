//! The right-pane commits list (Task 50).
//!
//! One row per commit — short SHA, subject, author — and each row's SHA is
//! a link that opens the commit in the browser. The commits endpoint is
//! read through the session; this widget only draws what arrived.

use eframe::egui;

use crate::github::commits::Commit;
use crate::ui::widgets::review_panel::sha7;

/// Draw `commits`.
///
/// `repo_web` is the repository's web URL (`https://github.com/{owner}/{name}`);
/// the commit URL is built from it because the list is what the session
/// caches, and it carries no URLs of its own.
pub fn render(ui: &mut egui::Ui, commits: &[Commit], repo_web: &str) {
    if commits.is_empty() {
        ui.weak("No commits.");
        return;
    }
    for commit in commits {
        let subject = commit.message.lines().next().unwrap_or_default();
        ui.horizontal(|ui| {
            if ui.link(sha7(&commit.sha)).clicked() {
                let url = format!("{repo_web}/commit/{}", commit.sha);
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            ui.label(subject);
        });
        ui.horizontal(|ui| {
            ui.weak(format!("by {}", commit.author));
            if !commit.committed_at.is_empty() {
                ui.weak(&commit.committed_at);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::github::commits::parse_commits;

    /// Both the loaded and the empty list must paint without panicking;
    /// nothing here reports actions, so an unclicked list emits none.
    #[test]
    fn render_paints_the_commits_and_the_empty_state() {
        let commits = parse_commits(fixture!("commits_page.json")).unwrap();
        assert!(!commits.is_empty());
        egui::__run_test_ui(|ui| {
            render(ui, &commits, "https://github.com/o/r");
            render(ui, &[], "https://github.com/o/r");
        });
    }
}
