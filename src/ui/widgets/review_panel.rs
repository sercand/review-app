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

use eframe::egui;

use crate::github::reviews::{Review, ReviewState};
use crate::model::review::{PendingReview, ReviewEvent};
use crate::ui::widgets::markdown::{self, MarkdownDoc};

/// The blocked-reason shown while the viewer has a pending review on the
/// server (Review Focus #6): it must be submitted or discarded there first.
const SERVER_PENDING_REASON: &str = "Submit or discard your pending review from GitHub first";

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
}

/// Decide what the panel may offer (spec §7.2, Review Focus #6 and #7).
///
/// GitHub's rules: `REQUEST_CHANGES` needs a body, `COMMENT` needs a body or
/// a comment, `APPROVE` needs nothing; the PR author may not pick the latter
/// two; a server-side pending review blocks the local submit entirely.
pub fn review_panel_vm(i: &PanelInputs) -> ReviewPanelVm {
    let has_body = !i.pending.body.trim().is_empty();
    let event_ok = match i.pending.event {
        ReviewEvent::Approve => true,
        ReviewEvent::RequestChanges => has_body,
        ReviewEvent::Comment => has_body || !i.pending.comments.is_empty(),
    };
    let blocked_reason = if i.server_pending {
        Some(SERVER_PENDING_REASON)
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
        submit_enabled: blocked_reason.is_none() && event_ok,
        blocked_reason,
        banner,
    }
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
    /// Submit the server-side pending review with this verdict and body.
    SubmitServerPending(ReviewEvent, String),
    /// Discard the server-side pending review.
    DiscardServerPending,
}

/// The memory key holding the Discard confirmation's open state.
fn discard_confirm_id() -> egui::Id {
    egui::Id::unique("review-panel-discard-confirm")
}

/// Draw the review panel: banners, verdict, summary, pending list, and the
/// submitted reviews.
///
/// The summary and comment text edits bind straight into `pending` (like the
/// composer binds into [`crate::state::composer::ComposerState`]); the
/// matching `SetBody`/`EditComment` actions tell the app the draft changed so
/// it can persist it. `Ctrl`/`⌘`+Enter submits when the view-model allows it
/// and keyboard focus is on one of the panel's own text edits, and Discard
/// asks for confirmation before it reports anything.
pub fn render(
    ui: &mut egui::Ui,
    vm: &ReviewPanelVm,
    pending: &mut PendingReview,
    past: &[ReviewVm],
    actions: &mut Vec<ReviewAction>,
) {
    egui::ScrollArea::vertical()
        .id_salt("review-panel")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(banner) = &vm.banner {
                ui.colored_label(ui.visuals().warn_fg_color, banner);
            }
            if let Some(reason) = vm.blocked_reason {
                ui.colored_label(ui.visuals().warn_fg_color, reason);
                // The REST list does not expose the verdict the reviewer
                // picked in the web UI, so the banner asks for it explicitly
                // (defaulting to Comment) instead of silently submitting one.
                let mut choice = server_pending_choice(ui);
                ui.horizontal(|ui| {
                    ui.label("Submit as:");
                    for (label, event, allowed, hover) in [
                        ("Comment", ReviewEvent::Comment, true, ""),
                        (
                            "Approve",
                            ReviewEvent::Approve,
                            vm.can_approve,
                            "you are the pull request's author",
                        ),
                        (
                            "Request changes",
                            ReviewEvent::RequestChanges,
                            vm.can_request_changes,
                            "you are the pull request's author",
                        ),
                    ] {
                        let response = ui.add_enabled(
                            allowed,
                            egui::Button::selectable(choice.event == event, label),
                        );
                        if response.clicked() {
                            choice.event = event;
                            set_server_pending_choice(ui, choice.clone());
                        }
                        if !allowed {
                            response.on_hover_text(hover);
                        }
                    }
                });
                let choice_body = ui.add(
                    egui::TextEdit::multiline(&mut choice.body)
                        .hint_text("Review summary (optional)")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                if choice_body.changed() {
                    set_server_pending_choice(ui, choice.clone());
                }
                ui.horizontal(|ui| {
                    if ui.button("Submit pending review").clicked() {
                        actions.push(server_pending_submit(
                            choice.event,
                            choice.body.clone(),
                            !vm.can_approve,
                        ));
                    }
                    if ui.button("Discard pending review").clicked() {
                        actions.push(ReviewAction::DiscardServerPending);
                    }
                });
            }

            ui.strong(format!("Pending review ({})", vm.pending_count));
            ui.horizontal(|ui| {
                for (label, event, allowed, reason) in [
                    ("Comment", ReviewEvent::Comment, true, ""),
                    (
                        "Approve",
                        ReviewEvent::Approve,
                        vm.can_approve,
                        "you are the pull request's author",
                    ),
                    (
                        "Request changes",
                        ReviewEvent::RequestChanges,
                        vm.can_request_changes,
                        "you are the pull request's author",
                    ),
                ] {
                    let response = ui.add_enabled(
                        allowed,
                        egui::Button::selectable(pending.event == event, label),
                    );
                    if response.clicked() {
                        actions.push(ReviewAction::SetEvent(event));
                    }
                    if !allowed {
                        response.on_hover_text(reason);
                    }
                }
            });

            let body_response = ui.add(
                egui::TextEdit::multiline(&mut pending.body)
                    .hint_text("Leave a review summary")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            // The edit wrote straight into `pending.body`; report the change
            // so the app persists the draft.
            if body_response.changed() {
                actions.push(ReviewAction::SetBody(pending.body.clone()));
            }
            // The widgets the submit shortcut may fire from: the panel's own
            // text edits, so a field elsewhere can never trigger a submit.
            let mut owned_ids = vec![body_response.id];

            for comment in &mut pending.comments {
                ui.horizontal(|ui| {
                    ui.weak(format!("{}:{}", comment.anchor.path, comment.anchor.line));
                    if ui.button("Delete").clicked() {
                        actions.push(ReviewAction::DeleteComment(comment.local_id));
                    }
                });
                let response = ui.add(
                    egui::TextEdit::multiline(&mut comment.body)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                owned_ids.push(response.id);
                if response.changed() {
                    actions.push(ReviewAction::EditComment(
                        comment.local_id,
                        comment.body.clone(),
                    ));
                }
                ui.separator();
            }

            ui.horizontal(|ui| {
                let submit = ui.add_enabled(
                    vm.submit_enabled,
                    egui::Button::new(egui::RichText::new("Submit review").strong()),
                );
                if submit.clicked() {
                    actions.push(ReviewAction::Submit);
                }
                submit.on_disabled_hover_text(
                    "a comment needs a body or a comment, request changes needs a body",
                );
                if ui.button("Discard").clicked() {
                    set_confirm_discard(ui, true);
                }
            });
            if confirm_discard(ui) {
                ui.horizontal(|ui| {
                    ui.label("Discard the whole review?");
                    if ui.button("Yes, discard").clicked() {
                        actions.push(ReviewAction::Discard);
                        set_confirm_discard(ui, false);
                    }
                    if ui.button("Keep").clicked() {
                        set_confirm_discard(ui, false);
                    }
                });
            }
            // Scope the shortcut to the panel: it fires only while keyboard
            // focus is on one of the panel's own text edits, so ⌘+Enter in
            // the thread-reply composer (or any other field) can never
            // submit the review by accident.
            let focused = ui.ctx().memory(|m| m.focused());
            let submit_shortcut = ui
                .input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter))
                && submit_shortcut_allowed(focused, &owned_ids);
            if submit_shortcut && vm.submit_enabled {
                actions.push(ReviewAction::Submit);
            }

            ui.add_space(8.0);
            ui.strong("Reviews");
            for review in past {
                ui.horizontal(|ui| {
                    ui.strong(&review.author);
                    ui.weak(state_label(&review.state));
                    if !review.submitted_at.is_empty() {
                        ui.weak(&review.submitted_at);
                    }
                });
                markdown::render(ui, &review.body_doc);
                ui.separator();
            }
        });
}

/// The label for a review's state.
fn state_label(state: &ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "commented",
        ReviewState::Dismissed => "dismissed",
        ReviewState::Pending => "pending",
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

/// The memory key holding the server-pending banner's verdict picker.
fn server_pending_choice_id() -> egui::Id {
    egui::Id::unique("review-panel-server-pending-choice")
}

/// The server-pending banner's picker state: the verdict and body its Submit
/// button reports. The REST list does not expose the verdict the reviewer
/// picked in the web UI, so the banner starts at `Comment` and lets the
/// reviewer restate their choice explicitly.
#[derive(Clone, Debug, Default, PartialEq)]
struct ServerPendingChoice {
    event: ReviewEvent,
    body: String,
}

/// The picker state between frames.
fn server_pending_choice(ui: &egui::Ui) -> ServerPendingChoice {
    ui.ctx()
        .data(|data| data.get_temp::<ServerPendingChoice>(server_pending_choice_id()))
        .unwrap_or_default()
}

/// Store the picker state between frames.
fn set_server_pending_choice(ui: &egui::Ui, choice: ServerPendingChoice) {
    ui.ctx()
        .data_mut(|data| data.insert_temp(server_pending_choice_id(), choice));
}

/// The action the banner's Submit button reports for the chosen verdict and
/// body. The PR author may not approve or request changes, so a stale pick
/// (the state lives in memory across pull requests) falls back to `Comment`.
fn server_pending_submit(event: ReviewEvent, body: String, viewer_is_author: bool) -> ReviewAction {
    let event = match event {
        ReviewEvent::Comment => ReviewEvent::Comment,
        _ if viewer_is_author => ReviewEvent::Comment,
        event => event,
    };
    ReviewAction::SubmitServerPending(event, body)
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
        }
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
    fn server_pending_review_blocks_local_submit() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "x".into(), "sha1").unwrap();
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            ..inputs(&p)
        });
        assert!(!vm.submit_enabled && vm.blocked_reason.is_some());
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

    /// The panel must paint a draft, a server-pending banner, and past
    /// reviews without panicking and without inventing actions.
    #[test]
    fn render_paints_the_panel_without_actions() {
        let mut p = PendingReview::default();
        p.add(line_anchor("a.rs", 1), "needs work".into(), "sha1")
            .unwrap();
        p.set_event(ReviewEvent::RequestChanges);
        let past = reviews_vm(&[Review {
            id: 2,
            author: "other".into(),
            state: ReviewState::ChangesRequested,
            body: "fix **this**".into(),
            submitted_at: Some("2026-10-19T00:00:00Z".into()),
        }]);
        let vm = review_panel_vm(&PanelInputs {
            server_pending: true,
            head_moved_since_pin: true,
            ..inputs(&p)
        });
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            render(ui, &vm, &mut p, &past, &mut actions);
        });
        assert!(actions.is_empty(), "an untouched panel emits no actions");
    }

    #[test]
    fn server_pending_submit_carries_the_chosen_verdict() {
        assert_eq!(
            server_pending_submit(ReviewEvent::Comment, String::new(), false),
            ReviewAction::SubmitServerPending(ReviewEvent::Comment, String::new())
        );
        assert_eq!(
            server_pending_submit(ReviewEvent::Approve, "lgtm".into(), false),
            ReviewAction::SubmitServerPending(ReviewEvent::Approve, "lgtm".into())
        );
        assert_eq!(
            server_pending_submit(ReviewEvent::RequestChanges, "fix this".into(), false),
            ReviewAction::SubmitServerPending(ReviewEvent::RequestChanges, "fix this".into())
        );
        assert_eq!(
            ServerPendingChoice::default(),
            ServerPendingChoice {
                event: ReviewEvent::Comment,
                body: String::new(),
            },
            "the picker defaults to Comment"
        );
    }

    #[test]
    fn server_pending_submit_falls_back_to_comment_for_the_author() {
        // A stale Approve pick (kept in memory across pull requests) must
        // never submit on a PR the viewer authored.
        assert_eq!(
            server_pending_submit(ReviewEvent::Approve, String::new(), true),
            ReviewAction::SubmitServerPending(ReviewEvent::Comment, String::new())
        );
        assert_eq!(
            server_pending_submit(ReviewEvent::RequestChanges, "x".into(), true),
            ReviewAction::SubmitServerPending(ReviewEvent::Comment, "x".into())
        );
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
            render(ui, &vm, &mut p, &[], &mut actions);
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
            render(ui, &vm, &mut p, &[], &mut actions);
        })
        .drop_without_applying_deltas();
        assert!(
            !actions.contains(&ReviewAction::Submit),
            "⌘+Enter in a field outside the panel must not submit the review"
        );
    }
}
