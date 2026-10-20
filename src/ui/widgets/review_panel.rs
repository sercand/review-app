//! The review panel (spec §7.2, Review Focus #6).
//!
//! The panel is the review draft's surface: the pending-comment list with
//! edit/delete, the verdict selector, the summary body, and Submit/Discard.
//! All its rules live in the pure [`review_panel_vm`] — the PR author gets no
//! Approve/Request-changes, GitHub's per-event body rules decide whether
//! Submit is enabled, a server-side pending review blocks a local submit with
//! an explicit reason (Review Focus #6), and a draft whose commit was pushed
//! past keeps a banner naming the commit it was written against (Review
//! Focus #7). [`render`] only draws the view-model and reports controls as
//! [`ReviewAction`]s; the app folds them into the session.

use eframe::egui::{self, RichText, Sense};

use crate::github::reviews::{Review, ReviewState};
use crate::model::review::{PendingReview, ReviewEvent};
use crate::model::thread::ReviewThread;
use crate::ui::widgets::markdown::{self, MarkdownDoc};
use crate::ui::{file_icons, icons, theme};

/// The note shown while the viewer has a pending review on GitHub *and*
/// comments in the local draft (Review Focus #6): the popover finishes the
/// GitHub review, and the local comments can only go out after it.
const LOCAL_DRAFT_WAITS: &str =
    "Your local draft comments go out after this GitHub review is finished";

/// Everything [`review_panel_vm`] reads; kept flat so the rules are testable
/// without a session.
pub struct PanelInputs<'a> {
    /// The live draft.
    pub pending: &'a PendingReview,
    /// Whether the signed-in user wrote the PR (they may not approve or
    /// request changes on it).
    pub viewer_is_author: bool,
    /// Whether the viewer has a pending review on the server.
    pub server_pending: bool,
    /// Whether the head moved since the draft pinned its commit.
    pub head_moved_since_pin: bool,
    /// Why a rate limit blocks writes right now, when one does (spec §13):
    /// the label doubles as the Submit tooltip.
    pub write_blocked: Option<String>,
    /// The PR's review threads; the pending ones are the GitHub review's
    /// comments.
    pub threads: &'a [ReviewThread],
}

/// One comment of the viewer's pending review on GitHub, for the popover.
#[derive(Clone, Debug, PartialEq)]
pub struct GithubItemVm {
    /// The thread the click jumps to.
    pub thread_id: String,
    /// The full path, for the file icon.
    pub path: String,
    /// The file's name only.
    pub name: String,
    /// `"R12"`, `"L3"`, or `"File"` for a file-level comment.
    pub line: String,
    /// The body's first line, cut to 60 characters with an ellipsis.
    pub excerpt: String,
}

/// What the panel may offer this frame.
pub struct ReviewPanelVm {
    /// How many comments the draft holds.
    pub pending_count: usize,
    /// Whether Approve is selectable.
    pub can_approve: bool,
    /// Whether Request changes is selectable.
    pub can_request_changes: bool,
    /// Whether Submit may post the draft as it stands.
    pub submit_enabled: bool,
    /// Why Submit is blocked by something outside the draft, when it is.
    pub blocked_reason: Option<&'static str>,
    /// The pinned-commit banner, when the head moved under the draft.
    pub banner: Option<String>,
    /// The rate-limit tooltip on the disabled Submit, when writes are
    /// blocked (spec §13).
    pub write_blocked: Option<String>,
    /// Whether Submit and Discard act on the viewer's pending review on
    /// GitHub rather than the local draft.
    pub github: bool,
    /// The GitHub review's comments.
    pub github_items: Vec<GithubItemVm>,
}

/// Decide what the panel may offer (spec §7.2, Review Focus #6 and #7).
///
/// GitHub's rules: `REQUEST_CHANGES` needs a body, `COMMENT` needs a body or
/// a comment, `APPROVE` needs nothing; the PR author may not pick the latter
/// two; a server-side pending review blocks the local submit entirely; a
/// rate limit blocks every write until its reset (spec §13).
pub fn review_panel_vm(i: &PanelInputs) -> ReviewPanelVm {
    let has_body = !i.pending.body.trim().is_empty();
    let github = i.server_pending;
    let github_items = github_items(i.threads);
    // With a review pending on GitHub, Submit finishes that one: its
    // comments are what a COMMENT needs, not the local draft's.
    let has_comments = if github {
        !github_items.is_empty()
    } else {
        !i.pending.comments.is_empty()
    };
    let event_ok = match i.pending.event {
        ReviewEvent::Approve => true,
        ReviewEvent::RequestChanges => has_body,
        ReviewEvent::Comment => has_body || has_comments,
    };
    let blocked_reason = if github && !i.pending.comments.is_empty() {
        Some(LOCAL_DRAFT_WAITS)
    } else {
        None
    };
    let banner = match (i.head_moved_since_pin, &i.pending.commit_id) {
        (true, Some(pinned)) => Some(format!("Your draft was written against {}", sha7(pinned))),
        _ => None,
    };
    ReviewPanelVm {
        pending_count: i.pending.comments.len(),
        can_approve: !i.viewer_is_author,
        can_request_changes: !i.viewer_is_author,
        submit_enabled: event_ok && i.write_blocked.is_none(),
        blocked_reason,
        banner,
        write_blocked: i.write_blocked.clone(),
        github,
        github_items,
    }
}

/// The pending comments of the viewer's GitHub review, one per thread.
fn github_items(threads: &[ReviewThread]) -> Vec<GithubItemVm> {
    threads
        .iter()
        .filter(|thread| thread.is_pending())
        .map(|thread| GithubItemVm {
            thread_id: thread.id.clone(),
            path: thread.path.clone(),
            name: thread
                .path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned(),
            line: match (thread.side, thread.line) {
                (Some(crate::model::diff::Side::Left), Some(line)) => format!("L{line}"),
                (_, Some(line)) => format!("R{line}"),
                _ => "File".to_owned(),
            },
            excerpt: excerpt(thread.comments.first().map_or("", |c| c.body.as_str())),
        })
        .collect()
}

/// A body's first line, cut to 60 characters with an ellipsis.
fn excerpt(body: &str) -> String {
    let first = body.lines().next().unwrap_or_default();
    if first.chars().count() > 60 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_owned()
    }
}

/// Whether a popover action is the end of the reviewer's business with it:
/// the review was sent or thrown away, or they jumped to a comment.
pub fn closes_popover(action: &ReviewAction) -> bool {
    matches!(
        action,
        ReviewAction::Submit
            | ReviewAction::Discard
            | ReviewAction::SubmitServerPending(..)
            | ReviewAction::DiscardServerPending
            | ReviewAction::LocatePending(_)
            | ReviewAction::LocateThread(_)
    )
}

/// One submitted review, ready to draw.
pub struct ReviewVm {
    /// The reviewer's login.
    pub author: String,
    /// The state the review was submitted in.
    pub state: ReviewState,
    /// The parsed review body.
    pub body_doc: MarkdownDoc,
    /// When it was submitted, as GitHub reported it; empty while pending.
    pub submitted_at: String,
}

/// The submitted reviews to list: a server-side pending review is not one —
/// it has its own banner (spec §7.2).
pub fn reviews_vm(reviews: &[Review]) -> Vec<ReviewVm> {
    reviews
        .iter()
        .filter(|review| review.state != ReviewState::Pending)
        .map(|review| ReviewVm {
            author: review.author.clone(),
            state: review.state.clone(),
            body_doc: markdown::to_doc(&review.body),
            submitted_at: review.submitted_at.clone().unwrap_or_default(),
        })
        .collect()
}

/// A control the review panel reported for the app to fold in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    /// Submit the draft with its current verdict.
    Submit,
    /// Drop the whole draft (already confirmed in the panel).
    Discard,
    /// Pick the verdict the draft submits with.
    SetEvent(ReviewEvent),
    /// Replace the review summary body.
    SetBody(String),
    /// Replace one pending comment's body.
    EditComment(u64, String),
    /// Drop one pending comment.
    DeleteComment(u64),
    /// Jump the diff to one pending comment (Task 12).
    LocatePending(u64),
    /// Submit the server-side pending review with this verdict and body.
    SubmitServerPending(ReviewEvent, String),
    /// Discard the server-side pending review.
    DiscardServerPending,
    /// Jump to a comment of the viewer's pending review on GitHub, by
    /// thread id.
    LocateThread(String),
}

/// One pending comment as the popover lists it (Task 12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingItemVm {
    /// The draft id the click reports back with.
    pub local_id: u64,
    /// The full path, for the file icon.
    pub path: String,
    /// The file's name only.
    pub name: String,
    /// The anchor label, `"R99"` or `"L12"`.
    pub line: String,
    /// The body's first line, cut to 60 characters with an ellipsis.
    pub excerpt: String,
}

/// The popover's pending-comment list.
pub fn pending_items(pending: &PendingReview) -> Vec<PendingItemVm> {
    pending
        .comments
        .iter()
        .map(|comment| {
            let first = comment.body.lines().next().unwrap_or_default();
            let excerpt = if first.chars().count() > 60 {
                format!("{}…", first.chars().take(60).collect::<String>())
            } else {
                first.to_owned()
            };
            PendingItemVm {
                local_id: comment.local_id,
                path: comment.anchor.path.clone(),
                name: comment
                    .anchor
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                line: format!(
                    "{}{}",
                    match comment.anchor.side {
                        crate::model::diff::Side::Right => "R",
                        crate::model::diff::Side::Left => "L",
                    },
                    comment.anchor.line
                ),
                excerpt,
            }
        })
        .collect()
}

/// The memory key holding the Discard confirmation's open state.
fn discard_confirm_id() -> egui::Id {
    egui::Id::unique("review-panel-discard-confirm")
}

/// The server-side pending review banner (Task 12): the REST list does not
/// expose the verdict the reviewer picked in the web UI, so the banner asks
/// for it explicitly (defaulting to Comment) before Submit it or Discard it.
pub fn render_server_pending_banner(ui: &mut egui::Ui, vm: &ReviewPanelVm) -> bool {
    if !vm.github {
        return false;
    }
    let p = theme::palette_of(ui);
    let mut open = false;
    banner_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                p.pending,
                format!(
                    "{} You have a review pending on GitHub with {} comment{}. New comments join it.",
                    icons::MESSAGE_SQUARE_DASHED,
                    vm.github_items.len(),
                    if vm.github_items.len() == 1 { "" } else { "s" }
                ),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                open = crate::ui::components::Btn::new(
                    crate::ui::components::BtnKind::Default,
                    "Finish review",
                )
                .icon(icons::SEND)
                .show(ui)
                .clicked();
            });
        });
    });
    open
}

/// The full-width banner strip style (redesign Task 12): `pending_soft`
/// fill, `pending` text.
pub fn banner_frame(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    egui::Frame::new()
        .fill(p.pending_soft)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.vertical(content);
        });
}

/// Draw the review popover's content (Task 12): summary, verdict cards,
/// the pending-comment list, and the Submit/Discard footer. The banners and
/// the server-pending picker live in the banner strip above the diff.
///
/// The summary edit binds straight into `pending`; the matching `SetBody`
/// action tells the app the draft changed so it can persist it. `Ctrl`/`⌘`
/// +Enter submits when the view-model allows it and keyboard focus is on the
/// popover's own summary field, and Discard asks for confirmation before it
/// reports anything.
pub fn render(
    ui: &mut egui::Ui,
    vm: &ReviewPanelVm,
    pending: &mut PendingReview,
    actions: &mut Vec<ReviewAction>,
) {
    let p = theme::palette_of(ui);
    ui.set_width(380.0);
    ui.strong("Finish your review");
    if vm.github {
        ui.colored_label(
            p.muted,
            format!(
                "Started on GitHub, {} comment{}",
                vm.github_items.len(),
                if vm.github_items.len() == 1 { "" } else { "s" }
            ),
        );
    }

    let body_response = ui.add(
        egui::TextEdit::multiline(&mut pending.body)
            .hint_text("Leave a summary (optional for Approve)")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    // The edit wrote straight into `pending.body`; report the change so the
    // app persists the draft.
    if body_response.changed() {
        actions.push(ReviewAction::SetBody(pending.body.clone()));
    }
    // The widget the scoped submit shortcut may fire from.
    let owned_ids = [body_response.id];

    for (label, description, event, icon, allowed, reason) in [
        (
            "Comment",
            "Feedback without approving",
            ReviewEvent::Comment,
            icons::MESSAGE_SQUARE,
            true,
            "",
        ),
        (
            "Approve",
            "Ready to merge",
            ReviewEvent::Approve,
            icons::CIRCLE_CHECK,
            vm.can_approve,
            "you are the pull request's author",
        ),
        (
            "Request changes",
            "Needs work before merging; needs a summary",
            ReviewEvent::RequestChanges,
            icons::CIRCLE_X,
            vm.can_request_changes,
            "you are the pull request's author",
        ),
    ] {
        let selected = pending.event == event;
        let frame = egui::Frame::new()
            .fill(if !allowed {
                p.surface
            } else if selected {
                p.accent_soft
            } else {
                p.raised
            })
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(10, 6));
        let card = frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                let radio = if selected {
                    icons::CIRCLE_DOT
                } else {
                    icons::CIRCLE
                };
                let tint = if allowed { p.text } else { p.faint };
                ui.label(RichText::new(radio).color(if selected { p.accent } else { p.faint }));
                ui.label(RichText::new(icon).color(tint));
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).color(tint).strong());
                    ui.colored_label(p.muted, description);
                });
            });
        });
        let response = ui.interact(
            card.response.rect,
            ui.make_persistent_id(("verdict-card", label)),
            Sense::click(),
        );
        if response.clicked() && allowed {
            actions.push(ReviewAction::SetEvent(event));
        }
        if !allowed {
            response.on_hover_text(reason);
        }
    }

    if !vm.github_items.is_empty() {
        ui.label(format!("{} comments on GitHub", vm.github_items.len()));
        for item in &vm.github_items {
            let frame = egui::Frame::new()
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(6, 2));
            let row = frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    file_icons::file_icon(ui, &item.path);
                    ui.label(item.name.clone());
                    ui.colored_label(p.faint, item.line.clone());
                    ui.colored_label(p.muted, item.excerpt.clone());
                });
            });
            let response = ui.interact(
                row.response.rect,
                ui.make_persistent_id(("github-item", item.thread_id.as_str())),
                Sense::click(),
            );
            if response.clicked() {
                actions.push(ReviewAction::LocateThread(item.thread_id.clone()));
            }
        }
    }
    if let Some(reason) = vm.blocked_reason {
        ui.colored_label(p.pending, reason);
    }

    if vm.pending_count > 0 {
        ui.label(format!("{} pending comments", vm.pending_count));
        for item in pending_items(pending) {
            let frame = egui::Frame::new()
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(6, 2));
            let row = frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    file_icons::file_icon(ui, &item.path);
                    ui.label(item.name.clone());
                    ui.colored_label(p.faint, item.line.clone());
                    ui.colored_label(p.muted, item.excerpt.clone());
                });
            });
            let response = ui.interact(
                row.response.rect,
                ui.make_persistent_id(("pending-item", item.local_id)),
                Sense::click(),
            );
            if response.clicked() {
                actions.push(ReviewAction::LocatePending(item.local_id));
            }
        }
    }

    ui.separator();
    if confirm_discard(ui) {
        ui.horizontal(|ui| {
            ui.label(if vm.github {
                "Discard your pending review on GitHub?"
            } else {
                "Discard the whole review?"
            });
            if ui.button("Yes, discard").clicked() {
                actions.push(ReviewAction::Discard);
                set_confirm_discard(ui, false);
            }
            if ui.button("Keep").clicked() {
                set_confirm_discard(ui, false);
            }
        });
    } else {
        ui.horizontal(|ui| {
            let submit = ui.add_enabled(
                vm.submit_enabled,
                egui::Button::new(
                    RichText::new(icons::icon_text(icons::SEND, "Submit review"))
                        .color(p.accent_ink),
                )
                .fill(p.accent),
            );
            if submit.clicked() {
                actions.push(ReviewAction::Submit);
            }
            match &vm.write_blocked {
                Some(reason) => submit.on_disabled_hover_text(reason),
                None => submit.on_disabled_hover_text(
                    "a comment needs a body or a comment, request changes needs a body",
                ),
            };
            ui.label(RichText::new(crate::ui::icons::key_hint("⌘↵")).color(p.faint));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let discard =
                    ui.add(egui::Button::new(RichText::new("Discard").color(p.bad)).frame(false));
                if discard.clicked() {
                    set_confirm_discard(ui, true);
                }
            });
        });
    }
    // Scope the shortcut to the popover: it fires only while keyboard focus
    // is on the summary field, so ⌘+Enter in the thread-reply composer (or
    // any other field) can never submit the review by accident.
    let focused = ui.ctx().memory(|m| m.focused());
    let submit_shortcut = ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter))
        && submit_shortcut_allowed(focused, &owned_ids);
    if submit_shortcut && vm.submit_enabled {
        actions.push(ReviewAction::Submit);
    }
}
/// The short form of a SHA used in banners and error messages.
pub fn sha7(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Whether the Discard confirmation row is open.
fn confirm_discard(ui: &egui::Ui) -> bool {
    let id = discard_confirm_id();
    ui.ctx()
        .data_mut(|data| data.get_temp::<bool>(id).unwrap_or(false))
}

/// Open or close the Discard confirmation row.
fn set_confirm_discard(ui: &egui::Ui, on: bool) {
    let id = discard_confirm_id();
    ui.ctx().data_mut(|data| data.insert_temp(id, on));
}

/// Whether the `Ctrl`/`⌘`+Enter submit shortcut may fire: only while keyboard
/// focus is on a widget the panel owns (`focused` is one of `owned`), so no
/// text field elsewhere on the screen can trigger an accidental submit.
fn submit_shortcut_allowed(focused: Option<egui::Id>, owned: &[egui::Id]) -> bool {
    focused.is_some_and(|id| owned.contains(&id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::line_anchor;

    fn inputs(p: &PendingReview) -> PanelInputs<'_> {
        PanelInputs {
            pending: p,
            viewer_is_author: false,
            server_pending: false,
            head_moved_since_pin: false,
            write_blocked: None,
            threads: &[],
        }
    }

    /// A pending file comment in the viewer's GitHub review.
    fn github_pending_thread() -> ReviewThread {
        ReviewThread {
            id: "PRRT_1".into(),
            is_resolved: false,
            is_outdated: false,
            path: "Assets/README.md".into(),
            line: None,
            start_line: None,
            side: None,
            subject_type: crate::model::thread::SubjectType::File,
            comments: vec![crate::model::thread::ThreadComment {
                database_id: 1,
                author: "me".into(),
                avatar_url: None,
                body: "move this to under plugins".into(),
                created_at: String::new(),
                pending: true,
            }],
        }
    }

    #[test]
    fn a_review_pending_on_github_is_finished_from_the_popover() {
        let p = PendingReview::default();
        let threads = [github_pending_thread()];
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            threads: &threads,
            ..inputs(&p)
        });
        assert!(vm.github, "the popover acts on the GitHub review");
        assert!(vm.blocked_reason.is_none());
        assert!(
            vm.submit_enabled,
            "COMMENT with a GitHub comment needs no summary"
        );
        assert_eq!(vm.github_items.len(), 1);
        assert_eq!(vm.github_items[0].thread_id, "PRRT_1");
        assert_eq!(vm.github_items[0].line, "File");

        let mut changes = PendingReview::default();
        changes.set_event(ReviewEvent::RequestChanges);
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            threads: &threads,
            ..inputs(&changes)
        });
        assert!(!vm.submit_enabled, "REQUEST_CHANGES still needs a summary");
    }

    #[test]
    fn finishing_or_jumping_closes_the_popover() {
        assert!(closes_popover(&ReviewAction::Submit));
        assert!(closes_popover(&ReviewAction::Discard));
        assert!(closes_popover(&ReviewAction::SubmitServerPending(
            ReviewEvent::Comment,
            String::new()
        )));
        assert!(closes_popover(&ReviewAction::DiscardServerPending));
        assert!(closes_popover(&ReviewAction::LocatePending(1)));
        assert!(closes_popover(&ReviewAction::LocateThread("PRRT_1".into())));
        assert!(!closes_popover(&ReviewAction::SetBody("x".into())));
        assert!(!closes_popover(&ReviewAction::SetEvent(
            ReviewEvent::Approve
        )));
    }

    #[test]
    fn author_cannot_approve_or_request_changes() {
        let p = PendingReview::default();
        let vm = review_panel_vm(&PanelInputs {
            viewer_is_author: true,
            ..inputs(&p)
        });
        assert!(!vm.can_approve && !vm.can_request_changes);
        let vm = review_panel_vm(&inputs(&p));
        assert!(vm.can_approve && vm.can_request_changes);
    }

    #[test]
    fn submit_rules_follow_github() {
        let mut p = PendingReview::default();
        assert!(!review_panel_vm(&inputs(&p)).submit_enabled); // COMMENT with nothing
        p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
        assert!(review_panel_vm(&inputs(&p)).submit_enabled); // COMMENT with a comment
        p.set_event(ReviewEvent::RequestChanges);
        assert!(!review_panel_vm(&inputs(&p)).submit_enabled); // REQUEST_CHANGES needs a body
        p.set_body("please fix".into());
        assert!(review_panel_vm(&inputs(&p)).submit_enabled);
        p.clear();
        p.set_event(ReviewEvent::Approve);
        assert!(review_panel_vm(&inputs(&p)).submit_enabled); // APPROVE needs nothing
    }

    #[test]
    fn local_draft_comments_wait_while_a_github_review_is_open() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            ..inputs(&p)
        });
        // The GitHub review is what Submit finishes; the local draft cannot
        // ride along, and the popover says so.
        assert!(vm.github && vm.blocked_reason.is_some());
    }

    #[test]
    fn a_rate_limit_blocks_submit_and_carries_its_tooltip() {
        // APPROVE needs nothing, so the rate limit is the only blocker here
        // (spec §13: disable write actions until reset).
        let mut p = PendingReview::default();
        p.set_event(ReviewEvent::Approve);
        let vm = review_panel_vm(&PanelInputs {
            write_blocked: Some("GitHub is rate-limiting writes — try again in ~30s".into()),
            ..inputs(&p)
        });
        assert!(!vm.submit_enabled, "no write may go out mid-limit");
        assert_eq!(
            vm.write_blocked.as_deref(),
            Some("GitHub is rate-limiting writes — try again in ~30s")
        );
        let vm = review_panel_vm(&inputs(&p));
        assert!(
            vm.submit_enabled && vm.write_blocked.is_none(),
            "past the reset, Submit is live again"
        );
    }

    #[test]
    fn moved_head_banners_the_pinned_commit() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "x".into(), "0123456789abcdef")
            .unwrap();
        let vm = review_panel_vm(&PanelInputs {
            head_moved_since_pin: true,
            ..inputs(&p)
        });
        assert_eq!(
            vm.banner.as_deref(),
            Some("Your draft was written against 0123456")
        );
        let vm = review_panel_vm(&inputs(&p));
        assert!(vm.banner.is_none(), "no move, no banner");
    }

    #[test]
    fn reviews_vm_excludes_pending() {
        let review = |state| Review {
            id: 1,
            node_id: String::new(),
            author: "me".into(),
            state,
            body: String::new(),
            submitted_at: None,
        };
        let vms = reviews_vm(&[
            review(ReviewState::Approved),
            review(ReviewState::Pending),
            review(ReviewState::Commented),
        ]);
        assert_eq!(vms.len(), 2);
        assert_eq!(vms[0].state, ReviewState::Approved);
        assert_eq!(vms[1].state, ReviewState::Commented);
    }

    /// The popover must paint a draft, and the banner a server-pending
    /// review, without panicking and without inventing actions.
    #[test]
    fn render_paints_the_panel_without_actions() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "needs work".into(), "sha1")
            .unwrap();
        p.set_event(ReviewEvent::RequestChanges);
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            head_moved_since_pin: true,
            ..inputs(&p)
        });
        let mut actions = Vec::new();
        let mut opened = false;
        egui::__run_test_ui(|ui| {
            opened |= render_server_pending_banner(ui, &vm);
            render(ui, &vm, &mut p, &mut actions);
        });
        assert!(actions.is_empty(), "an untouched panel emits no actions");
        assert!(!opened, "an untouched banner opens nothing");
    }

    #[test]
    fn pending_items_label_lines_and_cut_excerpts() {
        let pending = PendingReview {
            comments: vec![crate::model::review::PendingComment {
                local_id: 1,
                anchor: line_anchor("src/lib/data-tools/push.ts", 99),
                body: "Should the warning include the template id? It makes the toast actionable.\nsecond line"
                    .into(),
            }],
            ..Default::default()
        };
        let items = pending_items(&pending);
        assert_eq!(items[0].name, "push.ts");
        assert_eq!(items[0].line, "R99");
        assert_eq!(items[0].excerpt.chars().count(), 61);
        assert!(items[0].excerpt.ends_with('…'));
    }

    #[test]
    fn submit_shortcut_needs_focus_on_a_panel_owned_widget() {
        let summary = egui::Id::unique("summary");
        let comment = egui::Id::unique("comment");
        let owned = [summary, comment];
        assert!(submit_shortcut_allowed(Some(summary), &owned));
        assert!(submit_shortcut_allowed(Some(comment), &owned));
        assert!(
            !submit_shortcut_allowed(Some(egui::Id::unique("thread-reply")), &owned),
            "a foreign text field must never trigger a submit"
        );
        assert!(
            !submit_shortcut_allowed(None, &owned),
            "no focus, no submit"
        );
    }

    /// The regression the focus gating exists for: ⌘+Enter while a text
    /// field outside the panel (here, the thread-reply composer) has focus
    /// must not submit the draft review.
    #[test]
    fn cmd_enter_does_not_submit_while_a_foreign_field_has_focus() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
        let vm = review_panel_vm(&inputs(&p));
        assert!(vm.submit_enabled, "the draft is submittable");

        let ctx = egui::Context::default();
        ctx.set_fonts(egui::FontDefinitions::empty());
        let foreign = egui::Id::unique("thread-reply");
        let mut actions = Vec::new();
        // Frame 1: paint once and move focus to a widget the panel does not
        // own; `request_focus` takes effect at the start of the next frame.
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().memory_mut(|m| m.request_focus(foreign));
            render(ui, &vm, &mut p, &mut actions);
        })
        .drop_without_applying_deltas();
        // Frame 2: press Cmd/Ctrl+Enter with that focus still in place.
        let mut input = egui::RawInput::default();
        input
            .events
            .push(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        input.events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        ctx.run_ui(input, |ui| {
            assert_eq!(
                ui.ctx().memory(|m| m.focused()),
                Some(foreign),
                "the foreign field still holds focus"
            );
            render(ui, &vm, &mut p, &mut actions);
        })
        .drop_without_applying_deltas();
        assert!(
            !actions.contains(&ReviewAction::Submit),
            "⌘+Enter in a field outside the panel must not submit the review"
        );
    }
}
