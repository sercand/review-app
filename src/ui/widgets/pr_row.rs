//! One PR row: the badges it carries and the pure view-model behind them.
//!
//! The widget logic lives in [`pr_row_vm`] so it can be unit-tested; the list
//! screen lays rows out in a table and calls [`render_badges`] for the status
//! cell.

use eframe::egui;

use crate::model::pr::{CiState, PrSummary, ReviewDecision};

/// A status badge shown on a PR row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// The PR is a draft.
    Draft,
    /// Checks are still running.
    CiPending,
    /// Checks passed.
    CiSuccess,
    /// At least one check failed.
    CiFailure,
    /// The PR is approved.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Reviews are required but incomplete.
    ReviewRequired,
}

impl Badge {
    /// The badge's one-word label.
    pub fn label(self) -> &'static str {
        match self {
            Badge::Draft => "draft",
            Badge::CiPending => "checks running",
            Badge::CiSuccess => "checks passed",
            Badge::CiFailure => "checks failed",
            Badge::Approved => "approved",
            Badge::ChangesRequested => "changes requested",
            Badge::ReviewRequired => "review required",
        }
    }

    /// The badge's colour, taken from the theme where one fits.
    pub fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            Badge::Draft => visuals.weak_text_color(),
            Badge::CiPending | Badge::ReviewRequired => visuals.warn_fg_color,
            Badge::CiSuccess | Badge::Approved => egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
            Badge::CiFailure | Badge::ChangesRequested => visuals.error_fg_color,
        }
    }
}

/// Everything one PR row draws.
#[derive(Clone, Debug, PartialEq)]
pub struct PrRowVm {
    /// The PR title.
    pub title: String,
    /// `owner/name#number`.
    pub repo_number: String,
    /// The author's login.
    pub author: String,
    /// The badges, in display order: draft, checks, review.
    pub badges: Vec<Badge>,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// How many comments the PR has.
    pub comments: u32,
    /// When the PR was last updated (as GitHub sent it).
    pub updated_at: String,
}

/// Derive a row's view-model from one summary.
pub fn pr_row_vm(pr: &PrSummary) -> PrRowVm {
    let mut badges = Vec::new();
    if pr.draft {
        badges.push(Badge::Draft);
    }
    badges.extend(pr.ci.map(ci_badge));
    badges.extend(pr.review.map(review_badge));

    PrRowVm {
        title: pr.title.clone(),
        repo_number: format!("{}#{}", pr.repo, pr.number),
        author: pr.author.clone(),
        badges,
        additions: pr.additions,
        deletions: pr.deletions,
        comments: pr.comments,
        updated_at: pr.updated_at.clone(),
    }
}

/// The badge for a check state.
fn ci_badge(ci: CiState) -> Badge {
    match ci {
        CiState::Pending => Badge::CiPending,
        CiState::Success => Badge::CiSuccess,
        CiState::Failure => Badge::CiFailure,
    }
}

/// The badge for a review decision.
fn review_badge(review: ReviewDecision) -> Badge {
    match review {
        ReviewDecision::Approved => Badge::Approved,
        ReviewDecision::ChangesRequested => Badge::ChangesRequested,
        ReviewDecision::ReviewRequired => Badge::ReviewRequired,
    }
}

/// Paint `vm`'s badges inline, in order.
pub fn render_badges(ui: &mut egui::Ui, vm: &PrRowVm) {
    for badge in &vm.badges {
        let color = badge.color(ui.visuals());
        ui.colored_label(color, badge.label());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::pr_summary;

    #[test]
    fn row_vm_surfaces_draft_ci_and_review() {
        let pr = PrSummary {
            draft: true,
            ci: Some(CiState::Failure),
            review: Some(ReviewDecision::ChangesRequested),
            ..pr_summary(7, "t", 3)
        };
        let vm = pr_row_vm(&pr);
        assert_eq!(
            vm.badges,
            vec![Badge::Draft, Badge::CiFailure, Badge::ChangesRequested]
        );
        assert_eq!(vm.repo_number, "o/r#7");
    }

    #[test]
    fn a_plain_pr_has_no_badges() {
        let vm = pr_row_vm(&pr_summary(1, "t", 0));
        assert!(vm.badges.is_empty());
        assert_eq!(vm.repo_number, "o/r#1");
    }
}
