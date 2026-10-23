//! The Merge popover: method choice and commit message, GitHub's shape.
//!
//! All decision logic lives in the pure [`merge_gate`] and the default
//! title/message builders so it is testable without a window.

use eframe::egui;

use crate::github::commits::Commit;
use crate::github::merge::{MergeMethod, RepoSettings};
use crate::github::metadata::PrMetadata;

/// GitHub's default commit title for `method`, as the web UI prefills it.
/// Rebase replays the commits unchanged, so it has no custom title.
pub fn default_title(method: MergeMethod, owner: &str, meta: &PrMetadata) -> String {
    match method {
        MergeMethod::Merge => format!(
            "Merge pull request #{} from {}/{}",
            meta.number, owner, meta.head_ref
        ),
        MergeMethod::Squash => format!("{} (#{})", meta.title, meta.number),
        MergeMethod::Rebase => String::new(),
    }
}

/// GitHub's default commit body for `method`: the commit list for a merge
/// commit, the PR body for a squash, nothing for a rebase.
pub fn default_message(method: MergeMethod, meta: &PrMetadata, commits: &[Commit]) -> String {
    match method {
        MergeMethod::Merge => commits
            .iter()
            .filter_map(|c| c.message.lines().next())
            .map(|subject| format!("* {subject}"))
            .collect::<Vec<_>>()
            .join("\n"),
        MergeMethod::Squash => meta.body.clone(),
        MergeMethod::Rebase => String::new(),
    }
}

/// Why the merge cannot happen right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeBlock {
    /// The PR metadata has not loaded yet.
    Loading,
    /// The PR is closed (or already merged).
    Closed,
    /// Draft PRs must be marked ready first.
    Draft,
    /// Branch protection or a required check blocks the merge.
    Blocked,
    /// The branch has conflicts (or GitHub says it is otherwise unmergeable).
    Unmergeable,
    /// The repository does not offer this merge method.
    MethodDisallowed,
    /// A merge request is already in flight.
    Merging,
}

/// Whether the popover's Merge button may fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeGate {
    /// Merge away.
    Allowed,
    /// Disabled, with the reason for the tooltip.
    Blocked(MergeBlock),
}

/// Decide whether PR `meta` can be merged by `method` right now.
///
/// While GitHub is still computing mergeability (`mergeable` is `null`) the
/// merge is allowed: the request itself is the cheapest way to learn the
/// answer, and GitHub's refusal surfaces as a toast.
pub fn merge_gate(
    meta: Option<&PrMetadata>,
    repo: Option<&RepoSettings>,
    merging: bool,
    method: MergeMethod,
) -> MergeGate {
    if merging {
        return MergeGate::Blocked(MergeBlock::Merging);
    }
    let Some(meta) = meta else {
        return MergeGate::Blocked(MergeBlock::Loading);
    };
    if meta.state != "open" {
        return MergeGate::Blocked(MergeBlock::Closed);
    }
    if meta.draft {
        return MergeGate::Blocked(MergeBlock::Draft);
    }
    // `merge_state_status` is the current verdict; `mergeable_state` is the
    // legacy one, used only when the new field is absent (older hosts).
    let status = if meta.merge_state_status.is_empty() {
        meta.mergeable_state.as_str()
    } else {
        meta.merge_state_status.as_str()
    };
    match status {
        "blocked" => return MergeGate::Blocked(MergeBlock::Blocked),
        "dirty" | "behind" | "conflicting" => return MergeGate::Blocked(MergeBlock::Unmergeable),
        _ => {}
    }
    if meta.mergeable == Some(false) {
        return MergeGate::Blocked(MergeBlock::Unmergeable);
    }
    if repo.is_some_and(|repo| !repo.allows(method)) {
        return MergeGate::Blocked(MergeBlock::MethodDisallowed);
    }
    MergeGate::Allowed
}

/// Why the popover's Merge button is disabled, in tooltip words.
pub fn block_reason(block: MergeBlock) -> &'static str {
    match block {
        MergeBlock::Loading => "the pull request is still loading",
        MergeBlock::Closed => "this pull request is closed",
        MergeBlock::Draft => "mark the pull request ready before merging",
        MergeBlock::Blocked => "branch protection or a required check is blocking the merge",
        MergeBlock::Unmergeable => "this branch has conflicts that must be resolved first",
        MergeBlock::MethodDisallowed => "this repository does not allow this merge method",
        MergeBlock::Merging => "a merge is already in progress",
    }
}

/// What the merge popover reported for the app to fold in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeAction {
    /// Merge with the form's current method and message.
    Merge,
}

/// The popover's live form, owned by the screen state. Transient: unedited
/// fields re-prefill from GitHub's defaults every frame; the whole form is
/// dropped when the PR closes.
#[derive(Clone, Debug, Default)]
pub struct MergeForm {
    /// The chosen method.
    pub method: MergeMethod,
    /// The commit title (first line).
    pub title: String,
    /// The commit body.
    pub message: String,
    /// Whether the user typed over the prefilled title.
    title_edited: bool,
    /// Whether the user typed over the prefilled message.
    message_edited: bool,
}

impl MergeForm {
    /// A form starting with `method`, as the remembered preference says.
    pub fn with_method(method: MergeMethod) -> MergeForm {
        MergeForm {
            method,
            ..MergeForm::default()
        }
    }
}

/// What the popover shows, pulled from the session each frame.
pub struct MergeInputs<'a> {
    /// The repository owner, for the merge commit's default title.
    pub owner: &'a str,
    /// The PR metadata, once loaded.
    pub meta: Option<&'a PrMetadata>,
    /// The repository's allow flags, once loaded.
    pub repo: Option<&'a RepoSettings>,
    /// The PR's commits, for the counts and the merge commit's body.
    pub commits: &'a [Commit],
    /// Whether the merge may fire right now.
    pub gate: MergeGate,
    /// The rate-limit tooltip (spec §13): `Some` while writes are disabled.
    pub write_blocked: Option<&'a str>,
}

/// Draw the merge popover's content: the three method cards, the commit
/// title and body (GitHub offers neither for a rebase), and the Merge
/// button with its disabled reason as a tooltip.
pub fn render(
    ui: &mut egui::Ui,
    inputs: &MergeInputs<'_>,
    form: &mut MergeForm,
    actions: &mut Vec<MergeAction>,
) {
    use crate::ui::components::{Btn, BtnKind};
    use eframe::egui::{RichText, Sense};
    let p = crate::ui::theme::palette_of(ui);
    ui.set_width(380.0);
    ui.strong("Merge pull request");

    // Unedited fields re-prefill every frame, so metadata and commits that
    // arrive after the popover opened still land in the defaults, and a
    // method switch recomputes them.
    if let Some(meta) = inputs.meta {
        if !form.title_edited {
            form.title = default_title(form.method, inputs.owner, meta);
        }
        if !form.message_edited {
            form.message = default_message(form.method, meta, inputs.commits);
        }
    }

    let n = inputs.commits.len();
    let commits = if n == 1 {
        "1 commit".to_owned()
    } else {
        format!("{n} commits")
    };
    let cards = [
        (
            "Create a merge commit",
            "All commits from this branch will be added to the base branch via a merge commit."
                .to_owned(),
            MergeMethod::Merge,
        ),
        (
            "Squash and merge",
            format!("The {commits} from this branch will be added to the base branch."),
            MergeMethod::Squash,
        ),
        (
            "Rebase and merge",
            format!("The {commits} from this branch will be rebased and added to the base branch."),
            MergeMethod::Rebase,
        ),
    ];
    for (label, description, method) in cards {
        let allowed = inputs.repo.is_none_or(|repo| repo.allows(method));
        let selected = form.method == method;
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
                    crate::ui::icons::CIRCLE_DOT
                } else {
                    crate::ui::icons::CIRCLE
                };
                let tint = if allowed { p.text } else { p.faint };
                ui.label(RichText::new(radio).color(if selected { p.accent } else { p.faint }));
                ui.vertical(|ui| {
                    ui.label(RichText::new(label).color(tint).strong());
                    ui.colored_label(p.muted, description);
                });
            });
        });
        let response = ui.interact(
            card.response.rect,
            ui.make_persistent_id(("merge-card", label)),
            Sense::click(),
        );
        if response.clicked() && allowed && form.method != method {
            form.method = method;
            form.title_edited = false;
            form.message_edited = false;
        }
        if !allowed {
            response.on_hover_text(block_reason(MergeBlock::MethodDisallowed));
        }
    }

    // GitHub offers no custom message for a rebase: the commits are replayed
    // unchanged.
    if form.method != MergeMethod::Rebase {
        let title = ui.add(
            egui::TextEdit::singleline(&mut form.title)
                .hint_text("Commit title")
                .desired_width(f32::INFINITY),
        );
        form.title_edited |= title.changed();
        // A multiline TextEdit grows to its full text, and a popup measures
        // its content intrinsically — a long squash message (the PR body)
        // would stretch the popover past the bottom of the window. The box
        // scrolls instead, like GitHub's.
        let mut message_changed = false;
        egui::ScrollArea::vertical()
            .id_salt("merge-message")
            .max_height(160.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let message = ui.add(
                    egui::TextEdit::multiline(&mut form.message)
                        .hint_text("Add an optional extended description…")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                message_changed = message.changed();
            });
        form.message_edited |= message_changed;
    }

    ui.separator();
    let blocked = match inputs.gate {
        MergeGate::Allowed => None,
        MergeGate::Blocked(MergeBlock::Closed) if inputs.meta.is_some_and(|meta| meta.merged) => {
            Some("this pull request was already merged".to_owned())
        }
        MergeGate::Blocked(block) => Some(block_reason(block).to_owned()),
    };
    let merging = inputs.gate == MergeGate::Blocked(MergeBlock::Merging);
    let enabled =
        blocked.is_none() && inputs.write_blocked.is_none() && !form.title.trim().is_empty();
    let label = if merging { "Merging…" } else { "Merge" };
    let button = Btn::new(BtnKind::Primary, label)
        .icon(crate::ui::icons::CHECK)
        .enabled(enabled)
        .show(ui);
    let clicked = button.clicked();
    if let Some(reason) = blocked.or_else(|| inputs.write_blocked.map(str::to_owned)) {
        button.on_hover_text(reason);
    }
    if clicked {
        actions.push(MergeAction::Merge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::commits::Commit;
    use crate::github::merge::MergeMethod;
    use crate::github::merge::RepoSettings;
    use crate::test_support::meta;

    fn settings(merge: bool, squash: bool, rebase: bool) -> RepoSettings {
        RepoSettings {
            allow_merge_commit: merge,
            allow_squash_merge: squash,
            allow_rebase_merge: rebase,
        }
    }

    fn commit(message: &str) -> Commit {
        Commit {
            sha: "s".into(),
            message: message.into(),
            author: "a".into(),
            committed_at: String::new(),
        }
    }

    #[test]
    fn merge_defaults_match_githubs_wording() {
        let m = meta("sha_head"); // "a pull request", #7, head "feature", base "main"
        assert_eq!(
            default_title(MergeMethod::Merge, "o", &m),
            "Merge pull request #7 from o/feature"
        );
        assert_eq!(
            default_message(
                MergeMethod::Merge,
                &m,
                &[commit("First one\n\nbody"), commit("Second")]
            ),
            "* First one\n* Second"
        );
    }

    #[test]
    fn squash_defaults_are_pr_title_and_body() {
        let mut m = meta("sha_head");
        m.body = "why".into();
        assert_eq!(
            default_title(MergeMethod::Squash, "o", &m),
            "a pull request (#7)"
        );
        assert_eq!(default_message(MergeMethod::Squash, &m, &[]), "why");
    }

    #[test]
    fn rebase_has_no_custom_message() {
        let m = meta("sha_head");
        assert_eq!(default_title(MergeMethod::Rebase, "o", &m), "");
        assert_eq!(default_message(MergeMethod::Rebase, &m, &[]), "");
    }

    #[test]
    fn gate_allows_a_clean_open_pr() {
        let m = meta("sha_head");
        assert_eq!(
            merge_gate(Some(&m), None, false, MergeMethod::Squash),
            MergeGate::Allowed
        );
    }

    #[test]
    fn gate_blocks_draft_and_closed() {
        let mut draft = meta("sha_head");
        draft.draft = true;
        assert!(matches!(
            merge_gate(Some(&draft), None, false, MergeMethod::Merge),
            MergeGate::Blocked(MergeBlock::Draft)
        ));
        let mut closed = meta("sha_head");
        closed.state = "closed".into();
        assert!(matches!(
            merge_gate(Some(&closed), None, false, MergeMethod::Merge),
            MergeGate::Blocked(MergeBlock::Closed)
        ));
    }

    #[test]
    fn gate_blocks_known_unmergeable_states() {
        let mut m = meta("sha_head");
        m.merge_state_status = "blocked".into();
        assert!(matches!(
            merge_gate(Some(&m), None, false, MergeMethod::Merge),
            MergeGate::Blocked(MergeBlock::Blocked)
        ));
        m.merge_state_status = String::new();
        m.mergeable_state = "dirty".into();
        assert!(matches!(
            merge_gate(Some(&m), None, false, MergeMethod::Merge),
            MergeGate::Blocked(MergeBlock::Unmergeable)
        ));
    }

    #[test]
    fn gate_allows_while_mergeability_is_still_computing() {
        let mut m = meta("sha_head");
        m.mergeable = None;
        m.mergeable_state = "unknown".into();
        m.merge_state_status = String::new();
        assert_eq!(
            merge_gate(Some(&m), None, false, MergeMethod::Merge),
            MergeGate::Allowed
        );
    }

    #[test]
    fn gate_blocks_methods_the_repository_disallows() {
        let m = meta("sha_head");
        assert!(matches!(
            merge_gate(
                Some(&m),
                Some(&settings(true, false, true)),
                false,
                MergeMethod::Squash
            ),
            MergeGate::Blocked(MergeBlock::MethodDisallowed)
        ));
        assert_eq!(
            merge_gate(
                Some(&m),
                Some(&settings(true, false, true)),
                false,
                MergeMethod::Merge
            ),
            MergeGate::Allowed
        );
    }

    /// A popover over a clean PR; the state records the form and the actions.
    fn popover_harness(
        gate: MergeGate,
    ) -> egui_kittest::Harness<'static, (MergeForm, Vec<MergeAction>)> {
        egui_kittest::Harness::builder()
            .with_size([600.0, 600.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, state: &mut (MergeForm, Vec<MergeAction>)| {
                    let m = meta("sha_head");
                    let commits: Vec<Commit> = Vec::new();
                    let inputs = MergeInputs {
                        owner: "o",
                        meta: Some(&m),
                        repo: Some(&settings(true, true, true)),
                        commits: &commits,
                        gate,
                        write_blocked: None,
                    };
                    render(ui, &inputs, &mut state.0, &mut state.1);
                },
                (MergeForm::default(), Vec::new()),
            )
    }

    #[test]
    fn clicking_a_method_card_selects_it_and_reprefills_the_title() {
        use egui_kittest::kittest::Queryable;
        let mut harness = popover_harness(MergeGate::Allowed);
        harness.run();
        assert_eq!(harness.state().0.method, MergeMethod::Merge);
        harness.get_by_label("Squash and merge").click();
        harness.run();
        assert_eq!(harness.state().0.method, MergeMethod::Squash);
        assert_eq!(harness.state().0.title, "a pull request (#7)");
    }

    #[test]
    fn merge_button_reports_the_action_only_when_the_gate_allows_it() {
        use egui_kittest::kittest::Queryable;
        let mut harness = popover_harness(MergeGate::Allowed);
        harness.run();
        harness.get_by_label("Merge").click();
        harness.run();
        assert_eq!(harness.state().1, vec![MergeAction::Merge]);

        let mut blocked = popover_harness(MergeGate::Blocked(MergeBlock::Draft));
        blocked.run();
        blocked.get_by_label("Merge").click();
        blocked.run();
        assert!(blocked.state().1.is_empty(), "a blocked gate must not fire");
    }

    #[test]
    fn a_long_message_keeps_the_merge_button_on_screen() {
        use egui_kittest::kittest::Queryable;
        // A squash prefill is the PR body, which can run to hundreds of
        // lines; the message box must scroll, not stretch the popover and
        // push the Merge button off screen. Driven through the real detail
        // screen, the same path the app paints.
        let long_body = "line\n".repeat(200);
        let (mut session, _cmds) = crate::state::pr_detail::PrSession::open(
            crate::runtime::SessionId(1),
            "github.com".into(),
            crate::test_support::repo(),
            7,
        );
        let mut m = meta("sha_head");
        m.body = long_body;
        session.on_event(&crate::runtime::Event::PrMetadataLoaded {
            session: crate::runtime::SessionId(1),
            meta: m,
        });
        let mut state = crate::ui::screens::pr_detail::PrDetailState {
            merge_open: true,
            ..Default::default()
        };
        state.merge_form = MergeForm::with_method(MergeMethod::Squash);
        let mut cache = crate::diff::highlight::HighlightCache::new(
            16,
            Box::new(crate::diff::highlight::SyntectHighlighter::new(true)),
        );
        let mut harness = egui_kittest::Harness::builder()
            .with_size([1400.0, 900.0])
            .with_context_setup(|ctx| {
                ctx.set_fonts(crate::ui::fonts::font_definitions());
                crate::ui::theme::install(ctx, 13.0);
            })
            .build_ui_state(
                move |ui, _state: &mut ()| {
                    let mut diff_actions = Vec::new();
                    crate::ui::screens::pr_detail::render(
                        ui,
                        &mut session,
                        &mut state,
                        &mut cache,
                        &mut diff_actions,
                        crate::ui::screens::pr_detail::ScreenInputs {
                            login: "viewer",
                            write_blocked: None,
                            theme: crate::ui::theme::Theme::Dark,
                            system_dark: true,
                            ai_configured: false,
                            #[cfg(unix)]
                            local_checkout: None,
                            #[cfg(unix)]
                            repo_ai: None,
                        },
                    );
                },
                (),
            );
        harness.run_steps(6);
        let rects: Vec<_> = harness
            .get_all_by_label("Merge")
            .map(|node| node.rect())
            .collect();
        // One "Merge" is the top bar's button; the popover's is the lowest.
        let popover_button = rects
            .into_iter()
            .filter(|r| r.top() > 60.0)
            .max_by(|a, b| a.top().total_cmp(&b.top()))
            .expect("the popover's Merge button");
        assert!(
            popover_button.bottom() <= 900.0,
            "the popover's Merge button must stay on screen, was at y={}",
            popover_button.bottom()
        );
    }
}
