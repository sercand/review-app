//! The right-pane metadata panel and conversation timeline (Task 51).
//!
//! The panel is the PR's conversation surface beside the diff: the body
//! rendered as markdown (Task 36), the reviewers/labels/milestone chips,
//! the issues the body references, the commits and checks lists (Task 50),
//! and the read-only timeline merging submitted reviews with conversation
//! comments by time. Everything here is read-only — the review draft is
//! edited in the review panel below the diff (Task 44).

use eframe::egui;

use crate::github::checks::{CheckRun, CombinedStatus};
use crate::github::commits::Commit;
use crate::github::conversation::{referenced_issues, ConversationComment, IssueRef};
use crate::github::metadata::PrMetadata;
use crate::github::reviews::{Review, ReviewState};
use crate::model::RepoRef;
use crate::ui::widgets::review_panel::{reviews_vm, ReviewVm};
use crate::ui::widgets::{checks, commit_list, markdown};

/// One entry in the read-only timeline.
pub enum TimelineItem {
    /// A submitted review. A server-side pending review is not one — it
    /// has its own banner in the review panel (spec §7.2).
    Review(ReviewVm),
    /// A conversation comment.
    Comment(ConversationComment),
}

/// Reviews and conversation comments merged by timestamp.
///
/// GitHub's timestamps are ISO 8601, which sorts lexicographically; the
/// sort is stable, so same-instant items keep the order they arrived in.
pub fn timeline(reviews: &[Review], comments: &[ConversationComment]) -> Vec<TimelineItem> {
    let mut items: Vec<TimelineItem> = reviews_vm(reviews)
        .into_iter()
        .map(TimelineItem::Review)
        .collect();
    items.extend(comments.iter().cloned().map(TimelineItem::Comment));
    items.sort_by_key(|item| match item {
        TimelineItem::Review(vm) => vm.submitted_at.clone(),
        TimelineItem::Comment(comment) => comment.created_at.clone(),
    });
    items
}

/// Everything the panel draws, read from the session each frame.
pub struct PanelInputs<'a> {
    /// The PR's metadata, once loaded.
    pub metadata: Option<&'a PrMetadata>,
    /// The PR's commits (Task 50).
    pub commits: &'a [Commit],
    /// The head commit's check runs (Task 50).
    pub check_runs: &'a [CheckRun],
    /// The head commit's combined status (Task 50).
    pub combined: Option<&'a CombinedStatus>,
    /// The PR's reviews, for the timeline.
    pub reviews: &'a [Review],
    /// The conversation comments, for the timeline.
    pub conversation: &'a [ConversationComment],
    /// The host's web base, e.g. `https://github.com`; every link is built
    /// from it so Enterprise hosts link to their own web UI.
    pub web: &'a str,
    /// The repository the PR lives in.
    pub repo: &'a RepoRef,
}

/// Draw the panel: description, chips, referenced issues, commits, checks,
/// and the timeline.
pub fn render(ui: &mut egui::Ui, inputs: PanelInputs<'_>) {
    egui::ScrollArea::vertical()
        .id_salt("metadata-panel")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let Some(metadata) = inputs.metadata else {
                ui.spinner();
                return;
            };
            let repo_web = format!("{}/{}/{}", inputs.web, inputs.repo.owner, inputs.repo.name);

            ui.heading("Description");
            markdown::render(ui, &markdown::to_doc(&metadata.body));

            render_chips(ui, "Reviewers", &metadata.requested_reviewers);
            render_chips(ui, "Labels", &metadata.labels);
            if let Some(milestone) = &metadata.milestone {
                render_chips(ui, "Milestone", std::slice::from_ref(milestone));
            }

            let issues = referenced_issues(&metadata.body);
            if !issues.is_empty() {
                ui.heading("Referenced issues");
                for issue in &issues {
                    render_issue_link(ui, inputs.web, &repo_web, issue);
                }
            }

            ui.heading("Commits");
            commit_list::render(ui, inputs.commits, &repo_web);

            ui.heading("Checks");
            checks::render(ui, inputs.check_runs, inputs.combined);

            ui.heading("Timeline");
            for item in timeline(inputs.reviews, inputs.conversation) {
                ui.separator();
                match item {
                    TimelineItem::Review(vm) => {
                        ui.horizontal(|ui| {
                            ui.strong(&vm.author);
                            ui.weak(format!("{} · {}", state_label(&vm.state), vm.submitted_at));
                        });
                        markdown::render(ui, &vm.body_doc);
                    }
                    TimelineItem::Comment(comment) => {
                        ui.horizontal(|ui| {
                            ui.strong(&comment.author);
                            ui.weak(&comment.created_at);
                        });
                        markdown::render(ui, &markdown::to_doc(&comment.body));
                    }
                }
            }
        });
}

/// A titled row of chips, when there is anything to show.
fn render_chips(ui: &mut egui::Ui, title: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    ui.weak(title);
    ui.horizontal_wrapped(|ui| {
        for value in values {
            ui.label(value);
            ui.weak("·");
        }
    });
}

/// One referenced issue, linking to its repository's web UI.
fn render_issue_link(ui: &mut egui::Ui, web: &str, repo_web: &str, issue: &IssueRef) {
    let (label, url) = match &issue.repo {
        Some(repo) => (
            format!("{repo}#{}", issue.number),
            format!("{web}/{repo}/issues/{}", issue.number),
        ),
        None => (
            format!("#{}", issue.number),
            format!("{repo_web}/issues/{}", issue.number),
        ),
    };
    if ui.link(label).clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// The timeline's word for a review's state.
fn state_label(state: &ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "commented",
        ReviewState::Dismissed => "dismissed",
        ReviewState::Pending => "pending",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::github::checks::parse_check_runs;
    use crate::github::commits::parse_commits;
    use crate::github::conversation::parse_issue_comments;
    use crate::github::reviews::{Review, ReviewState};
    use crate::test_support::{meta, repo, review};

    #[test]
    fn timeline_merges_reviews_and_comments_by_time() {
        let reviews = vec![Review {
            submitted_at: Some("2026-10-02T00:00:00Z".into()),
            ..review(ReviewState::Approved)
        }];
        let comments = parse_issue_comments(fixture!("issue_comments.json")).unwrap(); // 2026-10-01 and 2026-10-03
        let t = timeline(&reviews, &comments);
        assert!(matches!(t[1], TimelineItem::Review(_)));
        assert_eq!(t.len(), 3);
    }

    /// The full panel and its not-loaded-yet state must both paint without
    /// panicking; nothing here reports actions.
    #[test]
    fn render_paints_the_panel_with_and_without_metadata() {
        let mut metadata = meta("sha1");
        metadata.body = "Fixes #12 and closes acme/api#34".into();
        metadata.requested_reviewers = vec!["alice".into(), "team:core".into()];
        metadata.labels = vec!["bug".into()];
        metadata.milestone = Some("v1.0".into());
        let commits = parse_commits(fixture!("commits_page.json")).unwrap();
        let runs = parse_check_runs(fixture!("check_runs.json")).unwrap();
        let comments = parse_issue_comments(fixture!("issue_comments.json")).unwrap();
        let reviews = vec![review(ReviewState::Approved)];
        let combined = CombinedStatus {
            state: "failure".into(),
            total_count: 2,
        };
        let r = repo();

        egui::__run_test_ui(|ui| {
            render(
                ui,
                PanelInputs {
                    metadata: Some(&metadata),
                    commits: &commits,
                    check_runs: &runs,
                    combined: Some(&combined),
                    reviews: &reviews,
                    conversation: &comments,
                    web: "https://github.com",
                    repo: &r,
                },
            );
            render(
                ui,
                PanelInputs {
                    metadata: None,
                    commits: &[],
                    check_runs: &[],
                    combined: None,
                    reviews: &[],
                    conversation: &[],
                    web: "https://github.com",
                    repo: &r,
                },
            );
        });
    }
}
