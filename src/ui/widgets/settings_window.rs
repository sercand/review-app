//! The settings modal window (Task 53, spec §6).
//!
//! A floating [`egui::Window`], not a fourth screen: it edits a working copy
//! of the settings ([`SettingsVm`]) and hands the result back as a
//! [`SettingsAction`] for the app to persist. It is also the only way to
//! **sign out** — the button rides the app's existing sign-out flow.

use eframe::egui;

use crate::github::search::Scope;
use crate::persist::settings::Settings;
use crate::ui::theme::Theme;

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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsVm {
    /// The GitHub host.
    pub host: String,
    /// The OAuth App client ID override, empty for "not set".
    pub client_id: String,
    /// The scopes the PR list starts with.
    pub default_scopes: Vec<Scope>,
    /// The theme preference.
    pub theme: Theme,
}

/// The working copy the window starts from.
pub fn settings_vm(s: &Settings) -> SettingsVm {
    SettingsVm {
        host: s.host.clone(),
        client_id: s.client_id.clone().unwrap_or_default(),
        default_scopes: s.default_scopes.clone(),
        theme: s.theme,
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

/// Draw the window over the current screen, pushing what the user asked for
/// onto `actions`.
pub fn render(
    ctx: &egui::Context,
    state: &mut SettingsWindowState,
    signed_in: bool,
    actions: &mut Vec<SettingsAction>,
) {
    if !state.open {
        return;
    }
    let mut close = false;
    egui::Window::new("Settings")
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            ui.set_width(360.0);

            ui.horizontal(|ui| {
                ui.label("GitHub host");
                ui.add(egui::TextEdit::singleline(&mut state.vm.host).desired_width(220.0));
            });
            ui.horizontal(|ui| {
                ui.label("OAuth client ID");
                ui.add(
                    egui::TextEdit::singleline(&mut state.vm.client_id)
                        .desired_width(220.0)
                        .hint_text("default"),
                );
            });

            ui.separator();
            ui.label("Default scopes");
            for scope in ALL_SCOPES {
                let mut selected = state.vm.default_scopes.contains(&scope);
                if ui.checkbox(&mut selected, scope_label(&scope)).changed() {
                    if selected {
                        state.vm.default_scopes.push(scope);
                    } else {
                        state.vm.default_scopes.retain(|s| s != &scope);
                    }
                }
            }

            ui.separator();
            ui.label("Theme");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut state.vm.theme, Theme::System, "System");
                ui.selectable_value(&mut state.vm.theme, Theme::Light, "Light");
                ui.selectable_value(&mut state.vm.theme, Theme::Dark, "Dark");
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    actions.push(SettingsAction::Apply(state.vm.clone()));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                if ui
                    .add_enabled(signed_in, egui::Button::new("Sign out"))
                    .clicked()
                {
                    actions.push(SettingsAction::SignOut);
                    close = true;
                }
            });
        });
    if close {
        state.open = false;
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
            render(ui.ctx(), &mut state, true, &mut actions);
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
