//! The right-pane details panel (Task 51, tabbed by redesign Task 13):
//! Overview, Conversation, Commits, and Checks behind a tab strip.
//! Everything here is read-only — the review draft is edited in the review
//! popover in the top bar (Task 12).

use eframe::egui::{self, Color32, Frame, Margin, RichText};

use crate::github::checks::{ci_state, CheckRun, CombinedStatus};
use crate::github::commits::Commit;
use crate::github::conversation::{referenced_issues, ConversationComment, IssueRef};
use crate::github::metadata::PrMetadata;
use crate::github::reviews::{Review, ReviewState};
use crate::model::pr::CiState;
use crate::model::RepoRef;
use crate::state::pr_detail::PrSession;
use crate::ui::widgets::review_panel::{reviews_vm, sha7, ReviewVm};
use crate::ui::widgets::thread_view::{avatar, relative_time};
use crate::ui::widgets::{checks, commit_list, markdown};
use crate::ui::{fonts, icons, theme};

/// Which details tab the pane shows (Task 13); persisted in `Layout`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DetailsTab {
    /// Title, author, branches, labels, linked issues, description, reviewers.
    #[default]
    Overview,
    /// The read-only timeline.
    Conversation,
    /// The commits list.
    Commits,
    /// The checks list.
    Checks,
}

/// Per-tab counts for the tab strip, in [`DetailsTab`] order. `None` means
/// the tab never shows a count; `Some(0)` is drawn as no count either.
pub fn tab_counts(session: &PrSession) -> [Option<usize>; 4] {
    [
        None,
        Some(timeline(&session.reviews, &session.conversation).len()),
        Some(session.commits.len()),
        Some(session.check_runs.len()),
    ]
}

/// One entry in the read-only timeline.
pub enum TimelineItem {
    /// A submitted review. A server-side pending review is not one — it
    /// has its own banner in the review popover (spec §7.2).
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
    /// The PR's reviews, for the timeline and the reviewer pills.
    pub reviews: &'a [Review],
    /// The conversation comments, for the timeline.
    pub conversation: &'a [ConversationComment],
    /// The host's web base, e.g. `https://github.com`; every link is built
    /// from it so Enterprise hosts link to their own web UI.
    pub web: &'a str,
    /// The repository the PR lives in.
    pub repo: &'a RepoRef,
}

/// Draw the tabbed pane: the strip on top, the selected tab below.
pub fn render(
    ui: &mut egui::Ui,
    inputs: PanelInputs<'_>,
    tab: &mut DetailsTab,
    counts: [Option<usize>; 4],
) {
    render_tab_strip(ui, tab, counts);
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("metadata-panel")
        .auto_shrink([false, false])
        .show(ui, |ui| match *tab {
            DetailsTab::Overview => render_overview(ui, &inputs),
            DetailsTab::Conversation => render_conversation(ui, &inputs),
            DetailsTab::Commits => render_commits(ui, &inputs),
            DetailsTab::Checks => render_checks(ui, &inputs),
        });
}

/// The tab strip: Overview by name, the rest as icon tabs with faint counts.
fn render_tab_strip(ui: &mut egui::Ui, tab: &mut DetailsTab, counts: [Option<usize>; 4]) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (index, (icon, label, value)) in [
            (icons::FILE_TEXT, "Overview", DetailsTab::Overview),
            (
                icons::MESSAGES_SQUARE,
                "Conversation",
                DetailsTab::Conversation,
            ),
            (icons::GIT_COMMIT_HORIZONTAL, "Commits", DetailsTab::Commits),
            (icons::CIRCLE_CHECK, "Checks", DetailsTab::Checks),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = *tab == value;
            let text = if index == 0 {
                RichText::new(icons::icon_text(icon, label))
            } else {
                RichText::new(icon.to_string())
            };
            let button = egui::Button::new(text.color(if selected { p.text } else { p.muted }))
                .fill(if selected {
                    p.hover
                } else {
                    Color32::TRANSPARENT
                })
                .corner_radius(6.0);
            let response = ui.add(button).on_hover_text(label);
            if let Some(Some(count)) = counts.get(index).copied()
                && count > 0 {
                    ui.label(RichText::new(count.to_string()).color(p.faint).size(11.0));
                }
            if response.clicked() {
                *tab = value;
            }
        }
    });
}

/// Overview: title, key–value list, description, reviewers (Task 13).
fn render_overview(ui: &mut egui::Ui, inputs: &PanelInputs<'_>) {
    let p = theme::palette_of(ui);
    let Some(metadata) = inputs.metadata else {
        ui.spinner();
        return;
    };
    let repo_web = format!("{}/{}/{}", inputs.web, inputs.repo.owner, inputs.repo.name);

    ui.label(RichText::new(&metadata.title).font(fonts::semibold(15.0)));
    ui.add_space(4.0);

    key_value(ui, "Author", |ui| {
        avatar(ui, &metadata.author, 20.0);
        ui.label(&metadata.author);
    });
    key_value(ui, "Branch", |ui| {
        branch_chip(ui, &metadata.head_ref);
        ui.label(RichText::new("→").color(p.faint));
        branch_chip(ui, &metadata.base_ref);
    });
    if !metadata.labels.is_empty() {
        key_value(ui, "Labels", |ui| {
            ui.horizontal_wrapped(|ui| {
                for label in &metadata.labels {
                    label_pill(ui, label);
                }
            });
        });
    }
    let issues = referenced_issues(&metadata.body);
    if !issues.is_empty() {
        key_value(ui, "Linked", |ui| {
            ui.vertical(|ui| {
                for issue in &issues {
                    render_issue_link(ui, inputs.web, &repo_web, issue);
                }
            });
        });
    }

    ui.separator();
    markdown::render(ui, &markdown::to_doc(&metadata.body));

    ui.add_space(4.0);
    key_value(ui, "Reviewers", |ui| {
        ui.vertical(|ui| {
            for review in inputs.reviews {
                if review.state == ReviewState::Pending {
                    continue;
                }
                ui.horizontal(|ui| {
                    avatar(ui, &review.author, 18.0);
                    ui.label(&review.author);
                    reviewer_pill(ui, &review.state);
                });
            }
            for login in &metadata.requested_reviewers {
                ui.horizontal(|ui| {
                    ui.label(login);
                    reviewer_pill(ui, &ReviewState::Pending);
                });
            }
        });
    });
}

/// One "Key   value" row of the overview list.
fn key_value(ui: &mut egui::Ui, key: &str, value: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.add_sized(
            [56.0, 0.0],
            egui::Label::new(RichText::new(key).color(p.faint).size(11.5)).selectable(false),
        );
        value(ui);
    });
}

/// A mono branch chip.
fn branch_chip(ui: &mut egui::Ui, name: &str) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.raised)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(name).monospace().size(11.5).color(p.text));
        });
}

/// A label pill.
fn label_pill(ui: &mut egui::Ui, name: &str) {
    let p = theme::palette_of(ui);
    Frame::new()
        .fill(p.raised)
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(name).size(11.5).color(p.text));
        });
}

/// A reviewer's state pill: Requested `warn` + eye, Approved `ok`,
/// Changes requested `bad`, Commented `muted`.
fn reviewer_pill(ui: &mut egui::Ui, state: &ReviewState) {
    let p = theme::palette_of(ui);
    let (icon, label, color) = match state {
        ReviewState::Approved => (icons::CIRCLE_CHECK, "Approved", p.ok),
        ReviewState::ChangesRequested => (icons::CIRCLE_X, "Changes requested", p.bad),
        ReviewState::Commented | ReviewState::Dismissed => {
            (icons::MESSAGE_SQUARE, "Commented", p.muted)
        }
        ReviewState::Pending => (icons::EYE, "Requested", p.warn),
    };
    ui.label(
        RichText::new(icons::icon_text(icon, label))
            .color(color)
            .font(fonts::semibold(11.0)),
    );
}

/// Conversation: the timeline with avatars and a connecting line (Task 13).
fn render_conversation(ui: &mut egui::Ui, inputs: &PanelInputs<'_>) {
    let p = theme::palette_of(ui);
    let items = timeline(inputs.reviews, inputs.conversation);
    if items.is_empty() {
        ui.weak("No conversation yet.");
        return;
    }
    let count = items.len();
    for (index, item) in items.into_iter().enumerate() {
        let author = match &item {
            TimelineItem::Review(vm) => vm.author.clone(),
            TimelineItem::Comment(comment) => comment.author.clone(),
        };
        // The avatar column on the left, the entry beside it: the body
        // never runs under the avatar or the connecting line.
        let row = ui.horizontal_top(|ui| {
            let avatar_rect = ui.scope(|ui| avatar(ui, &author, 24.0)).response.rect;
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&author).strong());
                    let when = match &item {
                        TimelineItem::Review(vm) => vm.submitted_at.clone(),
                        TimelineItem::Comment(comment) => comment.created_at.clone(),
                    };
                    ui.colored_label(p.faint, relative_time(&when));
                    if let TimelineItem::Review(vm) = &item {
                        reviewer_pill(ui, &vm.state);
                    }
                });
                match &item {
                    TimelineItem::Review(vm) => markdown::render(ui, &vm.body_doc),
                    TimelineItem::Comment(comment) => {
                        markdown::render(ui, &markdown::to_doc(&comment.body));
                    }
                }
            });
            avatar_rect
        });
        let gap = 12.0;
        // The connecting line runs from under this avatar to the next one,
        // down the avatar column only (mockup `.tl::before`).
        if index + 1 < count {
            let avatar_rect = row.inner;
            ui.painter().vline(
                avatar_rect.center().x,
                egui::Rangef::new(
                    avatar_rect.bottom() + 4.0,
                    row.response.rect.bottom() + gap - 4.0,
                ),
                (1.0, p.line),
            );
        }
        ui.add_space(gap);
    }
    ui.colored_label(
        p.faint,
        "Conversation comments are read-only here. Reply on github.com.",
    );
}

/// Commits: message, accent mono SHA, relative time (Task 13).
fn render_commits(ui: &mut egui::Ui, inputs: &PanelInputs<'_>) {
    let repo_web = format!("{}/{}/{}", inputs.web, inputs.repo.owner, inputs.repo.name);
    commit_list::render(ui, inputs.commits, &repo_web);
}

/// Checks: the rollup sentence over the head SHA, then the rows (Task 13).
fn render_checks(ui: &mut egui::Ui, inputs: &PanelInputs<'_>) {
    let p = theme::palette_of(ui);
    let empty = CombinedStatus {
        state: String::new(),
        total_count: 0,
    };
    let head = inputs.metadata.map(|m| sha7(&m.head_sha));
    match ci_state(inputs.check_runs, inputs.combined.unwrap_or(&empty)) {
        Some(CiState::Success) => {
            let n = inputs.check_runs.len();
            let on = head.map(|h| format!(" on {h}")).unwrap_or_default();
            ui.colored_label(p.ok, format!("All {n} checks passed{on}"));
        }
        Some(CiState::Failure) => {
            let failed = inputs
                .check_runs
                .iter()
                .filter(|run| {
                    run.status == "completed"
                        && run.conclusion.as_deref().is_some_and(|c| {
                            matches!(c, "failure" | "timed_out" | "cancelled" | "action_required")
                        })
                })
                .count();
            let n = inputs.check_runs.len();
            let on = head.map(|h| format!(" on {h}")).unwrap_or_default();
            ui.colored_label(p.bad, format!("{failed} of {n} checks failed{on}"));
        }
        Some(CiState::Pending) => {
            ui.colored_label(p.warn, "Checks are still running");
        }
        None => {
            ui.weak("No checks");
        }
    }
    ui.add_space(4.0);
    checks::render(ui, inputs.check_runs, inputs.combined);
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
    fn tab_counts_follow_the_session() {
        use crate::runtime::{Event, SessionId};
        use crate::state::pr_detail::PrSession;
        use crate::test_support::loaded;

        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        assert_eq!(tab_counts(&s), [None, Some(0), Some(0), Some(0)]);
        s.commits = parse_commits(fixture!("commits_page.json")).unwrap();
        assert_eq!(tab_counts(&s)[2], Some(2));
    }

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

    /// Every tab, and the not-loaded-yet state, must paint without panicking;
    /// nothing here reports actions.
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

        for tab in [
            DetailsTab::Overview,
            DetailsTab::Conversation,
            DetailsTab::Commits,
            DetailsTab::Checks,
        ] {
            let mut tab = tab;
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
                    &mut tab,
                    [None, Some(3), Some(2), Some(2)],
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
                    &mut tab,
                    [None, None, None, None],
                );
            });
        }
    }
}
