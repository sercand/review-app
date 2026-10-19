//! The login screen: device-flow panel with a PAT fallback.
//!
//! All widget logic lives in the pure [`login_vm`] so it can be unit-tested;
//! [`render`] only paints that view-model and records what the user asked for in
//! a list of [`LoginAction`]s, which the app shell turns into commands.

use eframe::egui;

use crate::auth::device_flow::{AuthFailure, AuthState};
use crate::persist::settings::Settings;

/// Everything the login screen needs to draw, derived from auth state.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginVm {
    /// The device-flow code the user types at the verification URI.
    pub user_code: Option<String>,
    /// Where the user enters [`LoginVm::user_code`].
    pub verification_uri: Option<String>,
    /// Whether a sign-in attempt is in flight (draws the spinner).
    pub spinner: bool,
    /// Why the last attempt failed, if it did.
    pub error: Option<String>,
    /// Whether this host has no usable client ID and one must be entered.
    pub needs_client_id: bool,
}

/// What the user asked the login screen to do.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginAction {
    /// Start the OAuth device flow.
    StartDeviceFlow,
    /// Validate and store this personal access token (trimmed).
    SubmitPat(String),
    /// Switch to this host (trimmed, non-empty).
    SetHost(String),
    /// Remember this OAuth App client ID (trimmed, non-empty).
    SetClientId(String),
    /// Copy the shown device-flow code to the clipboard.
    CopyCode,
    /// Open the verification URI in the browser.
    OpenBrowser,
}

/// The login screen's editable fields, owned across frames by the app.
#[derive(Clone, Debug, Default)]
pub struct LoginForm {
    /// The host text field.
    pub host_input: String,
    /// The OAuth App client ID text field.
    pub client_id_input: String,
    /// The personal access token text field.
    pub pat_input: String,
}

impl LoginForm {
    /// The form with its host and client-ID fields seeded from `settings`.
    pub fn for_settings(settings: &Settings) -> LoginForm {
        LoginForm {
            host_input: settings.host.clone(),
            client_id_input: settings.client_id.clone().unwrap_or_default(),
            pat_input: String::new(),
        }
    }
}

/// Derive the login screen's view-model from the auth state and settings.
///
/// `needs_client_id` is the spec §4 prerequisite: a github.com client ID does
/// not work on an Enterprise Server host, so a non-`github.com` host without a
/// `client_id` override cannot start the device flow.
pub fn login_vm(state: &AuthState, settings: &Settings) -> LoginVm {
    let needs_client_id = settings.host != "github.com" && settings.client_id.is_none();

    match state {
        AuthState::Polling { code, .. } => LoginVm {
            user_code: Some(code.user_code.clone()),
            verification_uri: Some(code.verification_uri.clone()),
            spinner: true,
            error: None,
            needs_client_id,
        },
        AuthState::Failed(failure) => LoginVm {
            user_code: None,
            verification_uri: None,
            spinner: false,
            error: Some(failure_message(failure)),
            needs_client_id,
        },
        AuthState::Idle | AuthState::Authenticated(_) => LoginVm {
            user_code: None,
            verification_uri: None,
            spinner: false,
            error: None,
            needs_client_id,
        },
    }
}

/// The user-facing text for an [`AuthFailure`].
///
/// [`AuthFailure::Http`] is used verbatim: besides transport errors it also
/// carries the "session expired" message the shell shows after a 401.
fn failure_message(failure: &AuthFailure) -> String {
    match failure {
        AuthFailure::Expired => "the code expired — start sign-in again".to_owned(),
        AuthFailure::Denied => "sign-in was cancelled".to_owned(),
        AuthFailure::Config(message) => format!("configuration error: {message}"),
        AuthFailure::Http(message) => message.clone(),
    }
}

/// Paint the login screen, appending whatever the user asked for to `actions`.
pub fn render(
    ui: &mut egui::Ui,
    vm: &LoginVm,
    form: &mut LoginForm,
    actions: &mut Vec<LoginAction>,
) {
    ui.heading("Sign in to GitHub");
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("GitHub host:");
        ui.add(egui::TextEdit::singleline(&mut form.host_input).desired_width(200.0));
        if ui.button("Use this host").clicked() {
            let host = form.host_input.trim().to_owned();
            if !host.is_empty() {
                actions.push(LoginAction::SetHost(host));
            }
        }
    });

    if vm.needs_client_id {
        let warn = ui.visuals().warn_fg_color;
        ui.colored_label(
            warn,
            "This host needs its own OAuth App client ID — a github.com client ID does not work \
             here. Register one with Device Flow enabled, or use a personal access token below.",
        );
        ui.horizontal(|ui| {
            ui.label("OAuth App client ID:");
            ui.add(egui::TextEdit::singleline(&mut form.client_id_input).desired_width(260.0));
            if ui.button("Use this client ID").clicked() {
                let client_id = form.client_id_input.trim().to_owned();
                if !client_id.is_empty() {
                    actions.push(LoginAction::SetClientId(client_id));
                }
            }
        });
    }

    ui.separator();

    match (&vm.user_code, &vm.verification_uri) {
        (Some(user_code), Some(verification_uri)) => {
            ui.label("Enter this code on GitHub:");
            ui.monospace(user_code);
            ui.horizontal(|ui| {
                if ui.button("Copy code").clicked() {
                    actions.push(LoginAction::CopyCode);
                }
                if ui.button("Open in browser").clicked() {
                    actions.push(LoginAction::OpenBrowser);
                }
            });
            ui.label(verification_uri);
            if vm.spinner {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("waiting for authorization…");
                });
            }
        }
        _ => {
            let button = ui.add_enabled(
                !vm.needs_client_id,
                egui::Button::new("Sign in with GitHub"),
            );
            if button.clicked() {
                actions.push(LoginAction::StartDeviceFlow);
            }
            if vm.spinner {
                ui.spinner();
            }
        }
    }

    ui.separator();

    ui.label("Or paste a personal access token:");
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut form.pat_input)
                .password(true)
                .hint_text("ghp_…")
                .desired_width(280.0),
        );
        if ui.button("Use token").clicked() {
            actions.push(LoginAction::SubmitPat(form.pat_input.trim().to_owned()));
        }
    });

    if let Some(error) = &vm.error {
        let error_color = ui.visuals().error_fg_color;
        ui.colored_label(error_color, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::device_flow::{AuthState, DeviceCode};
    use crate::persist::settings::Settings;
    use std::time::Duration;

    #[test]
    fn enterprise_without_client_id_asks_for_one() {
        let mut s = Settings {
            host: "ghe.acme.io".into(),
            ..Default::default()
        };
        assert!(login_vm(&AuthState::Idle, &s).needs_client_id);
        s.client_id = Some("Iv1.abc".into());
        assert!(!login_vm(&AuthState::Idle, &s).needs_client_id);
        assert!(!login_vm(&AuthState::Idle, &Settings::default()).needs_client_id);
    }

    #[test]
    fn polling_shows_the_code_and_spinner_and_failures_show_the_message() {
        let state = AuthState::Polling {
            code: DeviceCode {
                device_code: "dc".into(),
                user_code: "ABCD-1234".into(),
                verification_uri: "https://github.com/login/device".into(),
                interval: Duration::from_secs(5),
                expires_in: Duration::from_secs(900),
            },
            interval: Duration::from_secs(5),
        };
        let vm = login_vm(&state, &Settings::default());
        assert_eq!(vm.user_code.as_deref(), Some("ABCD-1234"));
        assert_eq!(
            vm.verification_uri.as_deref(),
            Some("https://github.com/login/device")
        );
        assert!(vm.spinner);
        assert!(vm.error.is_none());

        let failed = login_vm(
            &AuthState::Failed(AuthFailure::Denied),
            &Settings::default(),
        );
        assert!(!failed.spinner);
        assert_eq!(failed.error.as_deref(), Some("sign-in was cancelled"));
    }

    #[test]
    fn the_form_seeds_its_fields_from_settings() {
        let settings = Settings {
            host: "ghe.acme.io".into(),
            client_id: Some("Iv1.abc".into()),
            ..Default::default()
        };
        let form = LoginForm::for_settings(&settings);
        assert_eq!(form.host_input, "ghe.acme.io");
        assert_eq!(form.client_id_input, "Iv1.abc");
        assert!(form.pat_input.is_empty());
    }
}
