//! The details pane's commits tab (Task 50, restyled by redesign Task 13):
//! message first, the short SHA in accent mono linking to the commit, and a
//! relative time.

use eframe::egui::{self, RichText};

use crate::github::commits::Commit;
use crate::ui::widgets::review_panel::sha7;
use crate::ui::widgets::thread_view::relative_time;
use crate::ui::{fonts, theme};

/// Draw `commits`.
///
/// `repo_web` is the repository's web URL (`https://github.com/{owner}/{name}`);
/// the commit URL is built from it because the list is what the session
/// caches, and it carries no URLs of its own.
pub fn render(ui: &mut egui::Ui, commits: &[Commit], repo_web: &str) {
    let p = theme::palette_of(ui);
    if commits.is_empty() {
        ui.weak("No commits.");
        return;
    }
    for commit in commits {
        let subject = commit.message.lines().next().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(subject).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !commit.committed_at.is_empty() {
                    ui.colored_label(p.faint, relative_time(&commit.committed_at));
                }
                let sha = ui.add(
                    egui::Label::new(
                        RichText::new(sha7(&commit.sha))
                            .color(p.accent)
                            .font(fonts::mono_bold(11.0)),
                    )
                    .selectable(false),
                );
                if sha.on_hover_text("Open in browser").clicked() {
                    let url = format!("{repo_web}/commit/{}", commit.sha);
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            });
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
