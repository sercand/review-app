//! The settings modal window (Task 53, restyled by redesign Task 14).
//!
//! A floating [`egui::Window`], not a fourth screen: it edits a working copy
//! of the settings ([`SettingsVm`]) and hands the result back as a
//! [`SettingsAction`] for the app to persist. It is also the only way to
//! **sign out** — the button rides the app's existing sign-out flow.

use eframe::egui::{self, Frame, Margin, RichText};

use crate::github::search::Scope;
use crate::model::diff::DiffMode;
use crate::persist::settings::Settings;
use crate::ui::theme::Theme;
use crate::ui::widgets::thread_view::avatar;
use crate::ui::{fonts, icons, theme};

/// Every scope the settings window offers, in display order.
const ALL_SCOPES: [Scope; 5] = [
    Scope::ReviewRequested,
    Scope::Assigned,
    Scope::Authored,
    Scope::Involves,
    Scope::Mentions,
];

/// The window's editable view of the settings.
///
/// `client_id` flattens the settings' `Option<String>`: an empty box means
/// "use the built-in client id" and applies back as `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsVm {
    /// The GitHub host.
    pub host: String,
    /// The OAuth App client ID override, empty for "not set".
    pub client_id: String,
    /// The scopes the PR list starts with.
    pub default_scopes: Vec<Scope>,
    /// The theme preference.
    pub theme: Theme,
    /// The diff layout a new PR session starts in (Task 14).
    pub diff_mode: DiffMode,
    /// The code font size: 12, 13 or 14 (Task 14).
    pub code_font_size: f32,
}

impl Default for SettingsVm {
    fn default() -> Self {
        SettingsVm {
            host: String::new(),
            client_id: String::new(),
            default_scopes: Vec::new(),
            theme: Theme::default(),
            diff_mode: DiffMode::Split,
            code_font_size: 13.0,
        }
    }
}

/// The working copy the window starts from.
pub fn settings_vm(s: &Settings) -> SettingsVm {
    SettingsVm {
        host: s.host.clone(),
        client_id: s.client_id.clone().unwrap_or_default(),
        default_scopes: s.default_scopes.clone(),
        theme: s.theme,
        diff_mode: s.diff.mode,
        code_font_size: s.code_font_size,
    }
}

impl SettingsVm {
    /// Write the edited values back into `s`.
    ///
    /// Returns `true` when the host changed: the stored token belongs to the
    /// old host, so the app must end the session and ask for sign-in again.
    pub fn apply(self, s: &mut Settings) -> bool {
        let host_changed = self.host != s.host;
        s.host = self.host;
        s.client_id = (!self.client_id.is_empty()).then_some(self.client_id);
        s.default_scopes = self.default_scopes;
        s.theme = self.theme;
        s.diff.mode = self.diff_mode;
        s.code_font_size = self.code_font_size;
        host_changed
    }
}

/// The window's state across frames, owned by the app.
#[derive(Clone, Debug, Default)]
pub struct SettingsWindowState {
    /// Whether the window is showing.
    pub open: bool,
    /// The working copy being edited.
    pub vm: SettingsVm,
}

impl SettingsWindowState {
    /// Open the window on a fresh copy of `settings`.
    pub fn open_with(&mut self, settings: &Settings) {
        self.vm = settings_vm(settings);
        self.open = true;
    }

    /// Dismiss the window, discarding unapplied edits.
    pub fn close(&mut self) {
        self.open = false;
    }
}

/// What the window asked the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsAction {
    /// Persist these edited settings (the app saves; a host change ends the
    /// session).
    Apply(SettingsVm),
    /// Sign out through the app's existing sign-out flow.
    SignOut,
}

/// The signed-in account, for the window's Account row.
pub struct AccountInfo<'a> {
    /// The login.
    pub login: &'a str,
    /// The host the token belongs to.
    pub host: &'a str,
    /// The granted scopes, as GitHub reported them.
    pub scopes: &'a [String],
}

/// Draw the window over the current screen, pushing what the user asked for
/// onto `actions`.
pub fn render(
    ctx: &egui::Context,
    state: &mut SettingsWindowState,
    account: Option<AccountInfo<'_>>,
    actions: &mut Vec<SettingsAction>,
) {
    if !state.open {
        return;
    }
    let p = theme::palette(ctx.theme() == egui::Theme::Dark);
    let mut close = false;
    let mut sign_out = false;
    egui::Window::new("Settings")
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .frame(
            Frame::new()
                .fill(p.surface)
                .corner_radius(10.0)
                .stroke((1.0, p.line))
                .inner_margin(Margin::symmetric(16, 12)),
        )
        .show(ctx, |ui| {
            ui.set_width(600.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icons::icon_text(icons::SETTINGS, "Settings"))
                        .font(fonts::semibold(14.0)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::icon_button(ui, icons::X, "Close").clicked() {
                        close = true;
                    }
                });
            });
            ui.separator();

            egui::Grid::new("settings-grid")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    label(ui, "Account");
                    ui.horizontal(|ui| {
                        if let Some(account) = &account {
                            avatar(ui, account.login, 24.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(account.login).strong());
                                ui.colored_label(
                                    p.muted,
                                    format!(
                                        "{}, scopes {}",
                                        account.host,
                                        if account.scopes.is_empty() {
                                            "none".to_owned()
                                        } else {
                                            account.scopes.join(" and ")
                                        }
                                    ),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(icons::icon_text(
                                                    icons::LOG_OUT,
                                                    "Sign out",
                                                ))
                                                .color(p.bad),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        sign_out = true;
                                        close = true;
                                    }
                                },
                            );
                        } else {
                            ui.weak("Not signed in");
                        }
                    });
                    ui.end_row();

                    label(ui, "Theme");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut state.vm.theme, Theme::Dark, "Dark");
                        ui.selectable_value(&mut state.vm.theme, Theme::Light, "Light");
                        ui.selectable_value(&mut state.vm.theme, Theme::System, "System");
                    });
                    ui.end_row();

                    label(ui, "Default scopes");
                    ui.horizontal_wrapped(|ui| {
                        for scope in ALL_SCOPES {
                            let selected = state.vm.default_scopes.contains(&scope);
                            let chip = egui::Button::selectable(
                                selected,
                                RichText::new(icons::icon_text(
                                    scope_icon(&scope),
                                    &scope_label(&scope),
                                ))
                                .color(if selected {
                                    p.accent
                                } else {
                                    p.muted
                                }),
                            )
                            .fill(if selected {
                                p.accent_soft
                            } else {
                                p.raised
                            });
                            if ui.add(chip).clicked() {
                                if selected {
                                    state.vm.default_scopes.retain(|s| s != &scope);
                                } else {
                                    state.vm.default_scopes.push(scope);
                                }
                            }
                        }
                    });
                    ui.end_row();

                    label(ui, "Diff layout");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut state.vm.diff_mode, DiffMode::Split, "Split");
                        ui.selectable_value(&mut state.vm.diff_mode, DiffMode::Unified, "Unified");
                    });
                    ui.end_row();

                    label(ui, "Code font size");
                    ui.horizontal(|ui| {
                        for size in [12.0, 13.0, 14.0] {
                            ui.selectable_value(
                                &mut state.vm.code_font_size,
                                size,
                                format!("{size:.0}"),
                            );
                        }
                    });
                    ui.end_row();

                    label(ui, "Host");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.vm.host)
                            .desired_width(ui.available_width()),
                    );
                    ui.end_row();

                    label(ui, "OAuth client ID");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.vm.client_id)
                            .desired_width(ui.available_width())
                            .hint_text("default"),
                    );
                    ui.end_row();
                });

            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Save")
                                .color(p.accent_ink)
                                .font(fonts::semibold(12.5)),
                        )
                        .fill(p.accent),
                    )
                    .clicked()
                {
                    actions.push(SettingsAction::Apply(state.vm.clone()));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if sign_out {
        actions.push(SettingsAction::SignOut);
    }
    if close {
        state.open = false;
    }
}

/// The 130 pt label column of the settings grid.
fn label(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette_of(ui);
    ui.add_sized(
        [130.0, 0.0],
        egui::Label::new(RichText::new(text).color(p.muted)).selectable(false),
    );
}

/// The chip icon for a scope (mockup 05).
fn scope_icon(scope: &Scope) -> char {
    match scope {
        Scope::ReviewRequested => icons::EYE,
        Scope::Assigned => icons::USER_CHECK,
        Scope::Authored => icons::PENCIL_LINE,
        Scope::Involves => icons::USERS,
        Scope::Mentions => icons::AT_SIGN,
    }
}

/// The checkbox label for a scope.
fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Assigned => "Assigned".to_owned(),
        Scope::Authored => "Authored".to_owned(),
        Scope::ReviewRequested => "Review requested".to_owned(),
        Scope::Involves => "Involves".to_owned(),
        Scope::Mentions => "Mentions".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    #[test]
    fn window_paints_an_idle_frame_without_asking_for_anything() {
        let dir = tempdir();
        let settings = Settings::load_or_default(dir.path());
        let mut state = SettingsWindowState::default();
        state.open_with(&settings);
        let mut actions = Vec::new();
        egui::__run_test_ui(|ui| {
            render(
                ui.ctx(),
                &mut state,
                Some(AccountInfo {
                    login: "me",
                    host: "github.com",
                    scopes: &["repo".to_owned()],
                }),
                &mut actions,
            );
        });
        assert!(actions.is_empty(), "an untouched window asks for nothing");
        assert!(state.open, "painting alone keeps it open");
    }

    #[test]
    fn window_starts_from_the_settings_it_was_opened_with() {
        let dir = tempdir();
        let mut settings = Settings::load_or_default(dir.path());
        settings.host = "ghe.example.com".into();
        settings.client_id = Some("cid".into());
        let mut state = SettingsWindowState::default();
        state.open_with(&settings);
        assert_eq!(state.vm.host, "ghe.example.com");
        assert_eq!(state.vm.client_id, "cid");
        assert_eq!(state.vm.theme, Theme::System);
    }

    #[test]
    fn apply_moves_theme_and_scopes_but_keeps_the_account() {
        let mut settings = Settings::default();
        settings.account = Some(crate::persist::settings::Account {
            login: "octocat".into(),
            ..Default::default()
        });
        let mut vm = settings_vm(&settings);
        vm.theme = Theme::Dark;
        vm.default_scopes = vec![Scope::Mentions];
        assert!(!vm.apply(&mut settings));
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.default_scopes, vec![Scope::Mentions]);
        assert!(
            settings.account.is_some(),
            "apply never touches the account"
        );
    }
}
