//! The PR detail top bar (redesign Task 12, mockup 02): back, state pill,
//! repo/title, CI chip with its popover, the tool icons, and the primary
//! Review button whose popover replaces the old bottom review panel.

use eframe::egui::{self, Layout, Popup, PopupCloseBehavior, RichText};

use crate::ui::components::{Btn, BtnKind};

use crate::github::checks::{ci_state, CheckRun, CombinedStatus};
use crate::model::pr::CiState;
use crate::state::pr_detail::PrSession;
use crate::ui::widgets::checks;
use crate::ui::{fonts, icons, theme};

/// What the bar shows, pulled from the session each frame.
pub struct TopBarVm {
    /// Whether the PR is a draft.
    pub draft: bool,
    /// `"owner/repo"`.
    pub repo: String,
    /// The PR number.
    pub number: u64,
    /// The title, once the metadata has loaded.
    pub title: Option<String>,
    /// The CI rollup, `None` until the combined status has loaded.
    pub ci: Option<CiState>,
    /// The check runs, for the CI popover.
    pub runs: Vec<CheckRun>,
    /// The combined status, for the CI popover.
    pub combined: Option<CombinedStatus>,
    /// Pending comments, for the Review button's count pill.
    pub pending: usize,
}

/// Pull the bar's view-model out of a session.
pub fn top_bar_vm(session: &PrSession) -> TopBarVm {
    let ci = session
        .combined_status
        .as_ref()
        .and_then(|combined| ci_state(&session.check_runs, combined));
    TopBarVm {
        draft: session.metadata.as_ref().is_some_and(|m| m.draft),
        repo: session.repo.to_string(),
        number: session.number,
        title: session.metadata.as_ref().map(|m| m.title.clone()),
        ci,
        runs: session.check_runs.clone(),
        combined: session.combined_status.clone(),
        // The local draft plus the comments in a pending review on GitHub.
        pending: session.pending.comments.len()
            + session.threads.iter().filter(|t| t.is_pending()).count(),
    }
}

/// A control the top bar reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopBarAction {
    /// Back to the PR list (`Esc`).
    Back,
    /// Open the command palette (`⌘K`).
    Palette,
    /// Re-fetch the PR.
    Refresh,
    /// Cycle the theme (Task 12).
    ToggleTheme,
    /// Open settings.
    Settings,
    /// Show or hide the details pane (`O`).
    ToggleDetails,
    /// Open the pull request on GitHub.
    OpenInBrowser,
}

/// The bar's window-level state: which theme glyph to show, and whether the
/// details pane is open.
pub struct TopBarSettings {
    /// The configured theme.
    pub theme: theme::Theme,
    /// Whether the OS is in dark mode right now.
    pub system_dark: bool,
    /// Whether the details pane is open.
    pub details_open: bool,
}

/// The theme to switch to: Dark ↔ Light, and System jumps to the opposite
/// of what it currently resolves to (Task 12).
pub fn next_theme(current: theme::Theme, system_dark: bool) -> theme::Theme {
    match current {
        theme::Theme::Dark => theme::Theme::Light,
        theme::Theme::Light => theme::Theme::Dark,
        theme::Theme::System => {
            if current.resolve(system_dark) == egui::Theme::Dark {
                theme::Theme::Light
            } else {
                theme::Theme::Dark
            }
        }
    }
}

/// Draw the bar. `review_open` drives (and is written back by) the review
/// popover, whose content the caller supplies as `review_popover`.
pub fn render(
    ui: &mut egui::Ui,
    vm: &TopBarVm,
    settings: &TopBarSettings,
    review_open: &mut bool,
    actions: &mut Vec<TopBarAction>,
    review_popover: impl FnOnce(&mut egui::Ui),
) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        if icons::icon_button(ui, icons::ARROW_LEFT, "Back to list  Esc").clicked() {
            actions.push(TopBarAction::Back);
        }
        let (icon, label, tone) = if vm.draft {
            (
                icons::GIT_PULL_REQUEST_DRAFT,
                "Draft",
                crate::ui::components::Tone::Neutral,
            )
        } else {
            (
                icons::GIT_PULL_REQUEST,
                "Open",
                crate::ui::components::Tone::Ok,
            )
        };
        crate::ui::components::pill(ui, Some(icon), label, tone);
        if let Some(ci) = vm.ci {
            let (icon, color) = match ci {
                CiState::Success => (icons::CIRCLE_CHECK, p.ok),
                CiState::Failure => (icons::CIRCLE_X, p.bad),
                CiState::Pending => (icons::CIRCLE_DOT_DASHED, p.warn),
            };
            let (rect, button) =
                ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
            if button.hovered() {
                ui.painter().rect_filled(rect, 6.0, p.hover);
            }
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                icon,
                egui::FontId::proportional(16.0),
                color,
            );
            // A toggle popup: a click opens it, the next click (or a click
            // anywhere else) closes it. `from_response` alone stays open.
            Popup::from_toggle_button_response(&button)
                .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                    ui.set_width(320.0);
                    checks::render(ui, &vm.runs, vm.combined.as_ref());
                });
            button.on_hover_text("Checks");
        }
        ui.colored_label(p.muted, vm.repo.clone());
        ui.colored_label(p.faint, format!("#{}", vm.number));
        match &vm.title {
            Some(title) => ui.add(
                egui::Label::new(RichText::new(title).font(fonts::semibold(13.0)))
                    .selectable(false)
                    .truncate(),
            ),
            // Nothing to title the PR with yet.
            None => ui.spinner(),
        };

        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            let review = Btn::new(BtnKind::Primary, "Review")
                .icon(icons::SEND)
                .show(ui);
            // The popover is bound to `review_open`; the button is what
            // flips it (the popover only closes itself on outside clicks).
            if review.clicked() {
                *review_open = !*review_open;
            }
            if vm.pending > 0 {
                let rect = egui::Rect::from_center_size(
                    review.rect.right_top() + egui::vec2(-4.0, 2.0),
                    egui::vec2(16.0, 16.0),
                );
                ui.painter().rect_filled(rect, 8.0, p.pending);
                let galley = ui.painter().layout_no_wrap(
                    vm.pending.to_string(),
                    egui::FontId::proportional(10.0),
                    p.accent_ink,
                );
                ui.painter()
                    .galley(rect.center() - galley.size() / 2.0, galley, p.accent_ink);
            }
            Popup::from_response(&review)
                .open_bool(review_open)
                .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| review_popover(ui));

            ui.separator();
            let mut details_open = settings.details_open;
            if icons::icon_toggle(ui, icons::PANEL_RIGHT, "Details  O", &mut details_open).clicked()
            {
                actions.push(TopBarAction::ToggleDetails);
            }
            if icons::icon_button(ui, icons::SETTINGS, "Settings").clicked() {
                actions.push(TopBarAction::Settings);
            }
            if icons::icon_button(ui, icons::GITHUB, "Open pull request on GitHub").clicked() {
                actions.push(TopBarAction::OpenInBrowser);
            }
            let dark = settings.theme.resolve(settings.system_dark) == egui::Theme::Dark;
            let glyph = if dark { icons::SUN } else { icons::MOON };
            if icons::icon_button(ui, glyph, "Theme").clicked() {
                actions.push(TopBarAction::ToggleTheme);
            }
            if icons::icon_button(ui, icons::REFRESH_CW, "Refresh").clicked() {
                actions.push(TopBarAction::Refresh);
            }
            if icons::icon_button(ui, icons::COMMAND, &icons::key_hint("Commands  ⌘K")).clicked()
            {
                actions.push(TopBarAction::Palette);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the bar once per frame with `open` as the popover state;
    /// the state's flag records whether the popover content was drawn.
    fn bar_harness() -> egui_kittest::Harness<'static, (bool, bool)> {
        let vm = TopBarVm {
            draft: false,
            repo: "o/r".into(),
            number: 7,
            title: Some("t".into()),
            ci: None,
            runs: Vec::new(),
            combined: None,
            pending: 0,
        };
        egui_kittest::Harness::builder()
            .with_size([1200.0, 400.0])
            .with_context_setup(|ctx| ctx.set_fonts(crate::ui::fonts::font_definitions()))
            .build_ui_state(
                move |ui, state: &mut (bool, bool)| {
                    let settings = TopBarSettings {
                        theme: theme::Theme::Dark,
                        system_dark: true,
                        details_open: false,
                    };
                    state.1 = false;
                    let (open, drawn) = (&mut state.0, &mut state.1);
                    render(ui, &vm, &settings, open, &mut Vec::new(), |_| *drawn = true);
                },
                (false, false),
            )
    }

    #[test]
    fn the_review_button_opens_and_closes_its_popover() {
        use egui_kittest::kittest::Queryable;
        let mut harness = bar_harness();
        harness.get_by_label("Review").click();
        harness.run();
        assert_eq!(*harness.state(), (true, true), "a click opens the popover");
        harness.get_by_label("Review").click();
        harness.run();
        assert!(!harness.state().0, "a second click closes it");
    }

    #[test]
    fn theme_toggle_flips_the_resolved_theme() {
        assert_eq!(next_theme(theme::Theme::Dark, false), theme::Theme::Light);
        assert_eq!(next_theme(theme::Theme::Light, true), theme::Theme::Dark);
        assert_eq!(next_theme(theme::Theme::System, true), theme::Theme::Light);
        assert_eq!(next_theme(theme::Theme::System, false), theme::Theme::Dark);
    }
}
