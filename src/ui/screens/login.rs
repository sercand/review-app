//! The login screen: a centred 420 pt column with the device flow and a PAT
//! fallback (redesign Task 16).
//!
//! All widget logic lives in the pure [`login_vm`] and the small helpers here
//! so it can be unit-tested; [`render`] only paints that view-model and
//! records what the user asked for in a list of [`LoginAction`]s, which the
//! app shell turns into commands.

use std::time::Duration;

use eframe::egui;
use egui::{Align, Align2, Layout, Pos2, Rect, RichText, Stroke, UiBuilder, Vec2};

use crate::auth::device_flow::{AuthFailure, AuthState};
use crate::persist::settings::Settings;
use crate::ui::{icons, theme};

/// The width of the centred login column (Task 16).
const COLUMN_W: f32 = 420.0;

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
    /// Start (or restart) the OAuth device flow.
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
    /// Open the settings window.
    OpenSettings,
}

/// Which of the two underline tabs is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoginTab {
    /// The OAuth device flow.
    #[default]
    Browser,
    /// The personal-access-token fallback.
    Pat,
}

/// The login screen's editable fields, owned across frames by the app.
#[derive(Clone, Debug)]
pub struct LoginForm {
    /// The host text field.
    pub host_input: String,
    /// The OAuth App client ID text field.
    pub client_id_input: String,
    /// The personal access token text field.
    pub pat_input: String,
    /// Whether the Enterprise Server segment of the host picker is active.
    pub enterprise: bool,
    /// The open underline tab.
    pub tab: LoginTab,
}

impl Default for LoginForm {
    fn default() -> LoginForm {
        LoginForm {
            host_input: Settings::default().host,
            client_id_input: String::new(),
            pat_input: String::new(),
            enterprise: false,
            tab: LoginTab::default(),
        }
    }
}

impl LoginForm {
    /// The form with its host and client-ID fields seeded from `settings`.
    pub fn for_settings(settings: &Settings) -> LoginForm {
        LoginForm {
            host_input: settings.host.clone(),
            client_id_input: settings.client_id.clone().unwrap_or_default(),
            pat_input: String::new(),
            enterprise: settings.host != "github.com",
            tab: LoginTab::default(),
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

/// Split a device code on `-` into its character groups (Task 16):
/// `"8F3A-21KD"` → `[[8,F,3,A],[2,1,K,D]]`; a dashless code is one group.
pub fn code_groups(user_code: &str) -> Vec<Vec<char>> {
    user_code
        .split('-')
        .map(|group| group.chars().collect())
        .collect()
}

/// The countdown line under the shown code; fixed copy at zero.
pub fn expiry_label(remaining: Duration) -> String {
    if remaining.is_zero() {
        return "The code expired. Start over to get a new one.".to_owned();
    }
    let secs = remaining.as_secs();
    format!(
        "Waiting for you to approve. Code expires in {}:{:02}.",
        secs / 60,
        secs % 60
    )
}

/// Paint the login screen, appending whatever the user asked for to `actions`.
/// `remaining` is the device code's countdown while one is showing.
pub fn render(
    ui: &mut egui::Ui,
    vm: &LoginVm,
    form: &mut LoginForm,
    actions: &mut Vec<LoginAction>,
    remaining: Option<Duration>,
) {
    let pal = theme::palette_of(ui);
    let area = ui.available_rect_before_wrap();
    let x = (area.center().x - COLUMN_W / 2.0).max(area.min.x + 12.0);
    let column = Rect::from_min_max(
        Pos2::new(x, area.min.y + 36.0),
        Pos2::new(x + COLUMN_W, area.max.y),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(column)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.set_width(COLUMN_W);

            // The 44 pt accent mark.
            let (logo, _) = ui.allocate_exact_size(Vec2::new(44.0, 44.0), egui::Sense::hover());
            ui.painter().rect_filled(logo, 10.0, pal.accent);
            ui.painter().text(
                logo.center(),
                Align2::CENTER_CENTER,
                icons::GIT_PULL_REQUEST,
                egui::FontId::proportional(24.0),
                pal.accent_ink,
            );

            ui.add_space(14.0);
            ui.label(
                RichText::new("Sign in to review pull requests")
                    .size(22.0)
                    .strong()
                    .color(pal.text),
            );
            ui.label(
                RichText::new(
                    "Your token stays in the system keychain. Drafts and viewed files are \
                     kept on this machine, even after you sign out.",
                )
                .size(12.0)
                .color(pal.muted),
            );

            ui.add_space(14.0);
            egui::Frame::new()
                .fill(pal.surface)
                .stroke(Stroke::new(1.0, pal.line))
                .corner_radius(10.0)
                .inner_margin(18.0)
                .show(ui, |ui| {
                    ui.set_width(COLUMN_W - 36.0);
                    host_section(ui, form, actions);
                    ui.add_space(10.0);
                    tabs(ui, form);
                    ui.add_space(10.0);
                    match form.tab {
                        LoginTab::Browser => browser_tab(ui, vm, actions, remaining),
                        LoginTab::Pat => pat_tab(ui, vm, form, actions),
                    }
                    if let Some(error) = &vm.error {
                        ui.add_space(8.0);
                        ui.label(RichText::new(error).size(12.0).color(pal.bad));
                    }
                });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Requests scopes repo and read:org")
                        .size(11.0)
                        .color(pal.faint),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Settings").size(11.5).color(pal.accent),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        actions.push(LoginAction::OpenSettings);
                    }
                });
            });
        },
    );
}

/// The host segment: "github.com" versus "Enterprise Server", with the
/// enterprise fields underneath while the latter is active.
fn host_section(ui: &mut egui::Ui, form: &mut LoginForm, actions: &mut Vec<LoginAction>) {
    let pal = theme::palette_of(ui);
    ui.label(RichText::new("Host").size(12.0).color(pal.muted));
    let picked = crate::ui::components::segmented(
        ui,
        form.enterprise,
        &[
            crate::ui::components::Seg::icon_text(false, icons::GITHUB, "github.com"),
            crate::ui::components::Seg::icon_text(true, icons::BUILDING_2, "Enterprise Server"),
        ],
    );
    if picked == Some(false) && form.enterprise {
        form.enterprise = false;
        form.host_input = "github.com".to_owned();
        actions.push(LoginAction::SetHost("github.com".to_owned()));
    }
    if picked == Some(true) && !form.enterprise {
        form.enterprise = true;
    }

    if form.enterprise {
        ui.add_space(8.0);
        field_label(ui, "Host name");
        let host = crate::ui::components::Field::new(&mut form.host_input)
            .icon(icons::GLOBE)
            .hint("github.acme.corp")
            .show(ui);
        let entered = host.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (host.lost_focus() || entered) && !form.host_input.trim().is_empty() {
            let host = form.host_input.trim().to_owned();
            if host != crate::persist::settings::Settings::default().host {
                actions.push(LoginAction::SetHost(host));
            }
        }
        field_label(ui, "OAuth client ID");
        let client = crate::ui::components::Field::new(&mut form.client_id_input)
            .icon(icons::KEY_ROUND)
            .hint("Iv1.…")
            .show(ui);
        if client.lost_focus() && !form.client_id_input.trim().is_empty() {
            actions.push(LoginAction::SetClientId(
                form.client_id_input.trim().to_owned(),
            ));
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "The built-in client ID only works on github.com. Use your own OAuth app, \
                 or a personal access token.",
            )
            .size(11.0)
            .color(pal.pending),
        );
    }
}

/// The two underline tabs.
fn tabs(ui: &mut egui::Ui, form: &mut LoginForm) {
    let pal = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        for (tab, label) in [
            (LoginTab::Browser, "Browser sign-in"),
            (LoginTab::Pat, "Access token"),
        ] {
            let on = form.tab == tab;
            let response = ui.add(
                egui::Button::new(RichText::new(label).size(13.0).color(if on {
                    pal.text
                } else {
                    pal.muted
                }))
                .frame(false),
            );
            if on {
                let rect = response.rect;
                ui.painter().hline(
                    rect.x_range(),
                    rect.max.y + 2.0,
                    Stroke::new(2.0, pal.accent),
                );
            }
            if response.clicked() {
                form.tab = tab;
            }
        }
    });
}

/// The device-flow tab: the primary button, or the code once it is shown.
fn browser_tab(
    ui: &mut egui::Ui,
    vm: &LoginVm,
    actions: &mut Vec<LoginAction>,
    remaining: Option<Duration>,
) {
    let pal = theme::palette_of(ui);
    match (&vm.user_code, &vm.verification_uri) {
        (Some(user_code), Some(verification_uri)) => {
            ui.label(
                RichText::new(format!(
                    "Enter this code at {}",
                    verification_uri
                        .trim_start_matches("https://")
                        .trim_end_matches('/')
                ))
                .size(12.5)
                .color(pal.text),
            );
            ui.add_space(8.0);
            code_boxes(ui, user_code);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if icons::icon_button(ui, icons::COPY, "Copy the code")
                    .on_hover_text("Copy code")
                    .clicked()
                {
                    actions.push(LoginAction::CopyCode);
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        primary(pal, icons::EXTERNAL_LINK, "Open GitHub"),
                    )
                    .clicked()
                {
                    actions.push(LoginAction::OpenBrowser);
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if vm.spinner {
                    ui.spinner();
                }
                let text = remaining
                    .map(expiry_label)
                    .unwrap_or_else(|| "Waiting for you to approve.".to_owned());
                ui.label(RichText::new(text).size(11.5).color(pal.muted));
            });
            ui.add_space(4.0);
            if ui
                .add(ghost(pal, icons::ROTATE_CCW, "Start over"))
                .clicked()
            {
                actions.push(LoginAction::StartDeviceFlow);
            }
        }
        _ => {
            let sized = ui.available_width();
            let button = ui.add_enabled(
                !vm.needs_client_id,
                primary(pal, icons::LOG_IN, "Sign in with GitHub").min_size(Vec2::new(sized, 32.0)),
            );
            if button.clicked() {
                actions.push(LoginAction::StartDeviceFlow);
            }
            ui.label(
                RichText::new("We'll show a one-time code to enter on github.com.")
                    .size(11.0)
                    .color(pal.faint),
            );
            if vm.spinner {
                ui.spinner();
            }
        }
    }
}

/// The device code as 36×46 mono boxes, an en dash between groups.
fn code_boxes(ui: &mut egui::Ui, user_code: &str) {
    let pal = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (i, group) in code_groups(user_code).iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("\u{2013}").size(20.0).color(pal.faint));
            }
            for ch in group {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 46.0), egui::Sense::hover());
                ui.painter().rect_stroke(
                    rect,
                    8.0,
                    Stroke::new(1.0, pal.line),
                    egui::StrokeKind::Inside,
                );
                ui.painter().rect_filled(rect.shrink(1.0), 8.0, pal.raised);
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    *ch,
                    egui::FontId::monospace(20.0),
                    pal.text,
                );
            }
        }
    });
}

/// The personal-access-token tab.
fn pat_tab(ui: &mut egui::Ui, vm: &LoginVm, form: &mut LoginForm, actions: &mut Vec<LoginAction>) {
    let pal = theme::palette_of(ui);
    let field = crate::ui::components::Field::new(&mut form.pat_input)
        .icon(icons::KEY_ROUND)
        .hint("ghp_… or github_pat_…")
        .password(true)
        .show(ui);
    let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if entered && !form.pat_input.trim().is_empty() {
        actions.push(LoginAction::SubmitPat(form.pat_input.trim().to_owned()));
    }
    ui.add_space(8.0);
    if ui
        .add_sized(
            [ui.available_width(), 32.0],
            primary(pal, icons::KEY_ROUND, "Use this token"),
        )
        .clicked()
        && !form.pat_input.trim().is_empty()
    {
        actions.push(LoginAction::SubmitPat(form.pat_input.trim().to_owned()));
    }
    ui.label(
        RichText::new("Needs the repo scope to read private repositories and post comments.")
            .size(11.0)
            .color(pal.faint),
    );
    if vm.spinner {
        ui.spinner();
    }
}

/// A small muted field caption.
fn field_label(ui: &mut egui::Ui, text: &str) {
    let pal = theme::palette_of(ui);
    ui.add_space(6.0);
    ui.label(RichText::new(text).size(11.0).color(pal.faint));
}

/// The accent-filled primary button label style.
fn primary(pal: &'static theme::Palette, icon: char, label: &str) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(format!("{icon}  {label}"))
            .size(13.0)
            .color(pal.accent_ink),
    )
    .fill(pal.accent)
    .corner_radius(6.0)
}

/// A transparent, outlined ghost button.
fn ghost(pal: &'static theme::Palette, icon: char, label: &str) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(format!("{icon} {label}"))
            .size(12.0)
            .color(pal.muted),
    )
    .fill(egui::Color32::TRANSPARENT)
    .stroke(Stroke::new(1.0, pal.line))
    .corner_radius(6.0)
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
        assert!(form.enterprise);
    }

    #[test]
    fn device_code_groups_and_expiry_copy() {
        assert_eq!(
            code_groups("8F3A-21KD"),
            vec![vec!['8', 'F', '3', 'A'], vec!['2', '1', 'K', 'D']]
        );
        assert_eq!(code_groups("ABCD"), vec![vec!['A', 'B', 'C', 'D']]);
        assert_eq!(
            expiry_label(Duration::from_secs(14 * 60 + 32)),
            "Waiting for you to approve. Code expires in 14:32."
        );
        assert_eq!(
            expiry_label(Duration::from_secs(5)),
            "Waiting for you to approve. Code expires in 0:05."
        );
        assert_eq!(
            expiry_label(Duration::ZERO),
            "The code expired. Start over to get a new one."
        );
    }

    #[test]
    fn the_screen_paints_every_state_without_asking_for_anything() {
        for state in [
            AuthState::Idle,
            AuthState::Polling {
                code: DeviceCode {
                    device_code: "dc".into(),
                    user_code: "ABCD-1234".into(),
                    verification_uri: "https://github.com/login/device".into(),
                    interval: Duration::from_secs(5),
                    expires_in: Duration::from_secs(900),
                },
                interval: Duration::from_secs(5),
            },
            AuthState::Failed(AuthFailure::Expired),
        ] {
            let vm = login_vm(&state, &Settings::default());
            let mut form = LoginForm::default();
            let mut actions = Vec::new();
            egui::__run_test_ui(|ui| {
                render(
                    ui,
                    &vm,
                    &mut form,
                    &mut actions,
                    Some(Duration::from_secs(600)),
                );
            });
            assert!(actions.is_empty());
        }
    }
}
