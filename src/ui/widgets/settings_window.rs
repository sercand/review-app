//! The settings modal window (Task 53, restyled by redesign Task 14).
//!
//! A floating [`egui::Window`], not a fourth screen: it edits a working copy
//! of the settings ([`SettingsVm`]) and hands the result back as a
//! [`SettingsAction`] for the app to persist. It is also the only way to
//! **sign out** — the button rides the app's existing sign-out flow.
//!
//! The AI tab (spec §10) is different: its rows are edited and saved one at a
//! time by their own dialogs, so nothing from it is part of [`SettingsVm`]
//! and saving General/Appearance never validates or rewrites AI settings.

use std::collections::BTreeMap;

use eframe::egui::{self, Frame, Margin, RichText};

use crate::github::search::Scope;
use crate::model::diff::DiffMode;
use crate::persist::settings::{AiSettings, ModelSettings, ProviderSettings, Settings};
use crate::ui::components::{Btn, BtnKind, Field, Seg, elided_label, segmented, toggle_chip};
use crate::ui::theme::Theme;
#[cfg(unix)]
use crate::ui::widgets::settings_ai_tab::{self, AiTabAction};
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

/// The Settings window's tabs. The AI tab is Unix-only with the LLM stack
/// (spec §10); Windows offers the other two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    /// Account, scopes, host.
    #[default]
    General,
    /// Theme, diff layout, code font size.
    Appearance,
    /// AI providers (spec §10).
    #[cfg(unix)]
    Ai,
}

impl SettingsTab {
    /// The tab's label.
    fn label(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Appearance => "Appearance",
            #[cfg(unix)]
            SettingsTab::Ai => "AI",
        }
    }
}

/// Every tab this platform offers, in display order.
fn all_tabs() -> Vec<SettingsTab> {
    #[cfg(unix)]
    {
        vec![
            SettingsTab::General,
            SettingsTab::Appearance,
            SettingsTab::Ai,
        ]
    }
    #[cfg(not(unix))]
    {
        vec![SettingsTab::General, SettingsTab::Appearance]
    }
}

/// Whether the selected tab saves through [`SettingsAction::Apply`] (General
/// and Appearance do; an AI row saves from its own dialog).
fn has_save_footer(tab: SettingsTab) -> bool {
    #[cfg(unix)]
    {
        tab != SettingsTab::Ai
    }
    #[cfg(not(unix))]
    {
        let _ = tab;
        true
    }
}

/// The window's editable view of the settings.
///
/// `client_id` flattens the settings' `Option<String>`: an empty box means
/// "use the built-in client id" and applies back as `None`.
///
/// The AI settings are deliberately absent: their rows are saved by the AI
/// tab's dialogs, not by this window's Save button (spec §10).
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
    ///
    /// Only the General and Appearance fields are written; the AI table
    /// (`s.ai`) is never touched here, so an invalid AI row cannot block
    /// saving the rest (spec §10).
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
    /// The selected tab.
    pub tab: SettingsTab,
    /// The working copy being edited (General and Appearance).
    pub vm: SettingsVm,
    /// The AI tab's dialog and latest test results (spec §10).
    #[cfg(unix)]
    pub ai_tab: settings_ai_tab::AiTabState,
}

impl SettingsWindowState {
    /// Open the window on a fresh copy of `settings`, on the General tab.
    pub fn open_with(&mut self, settings: &Settings) {
        self.vm = settings_vm(settings);
        self.tab = SettingsTab::General;
        #[cfg(unix)]
        {
            self.ai_tab = settings_ai_tab::AiTabState::default();
        }
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
    /// Persist these edited General/Appearance settings (the app saves; a host
    /// change ends the session). AI settings are untouched.
    Apply(SettingsVm),
    /// Commit one AI provider row (spec §10): a new row, an edit, or a rename.
    /// `original` is the row's name before the edit; `key` is a key typed in
    /// the dialog, never a stored one.
    SaveProvider {
        /// The row's name before the edit, `None` for a new row.
        original: Option<String>,
        /// The edited row.
        provider: ProviderSettings,
        /// A key typed this session, for the keychain only.
        key: Option<String>,
    },
    /// Delete the provider called `name`; the app blocks it while a model
    /// uses it.
    DeleteProvider {
        /// The provider to remove.
        name: String,
    },
    /// Run the "Test connection" probe against `provider` with `model`.
    TestProvider {
        /// The endpoint to dial.
        provider: ProviderSettings,
        /// The model to probe with.
        model: ModelSettings,
        /// A key typed in the dialog; wins over the stored one.
        key: Option<String>,
    },
    /// Sign out through the app's existing sign-out flow.
    SignOut,
}

#[cfg(unix)]
impl From<AiTabAction> for SettingsAction {
    fn from(action: AiTabAction) -> SettingsAction {
        match action {
            AiTabAction::SaveProvider {
                original,
                provider,
                key,
            } => SettingsAction::SaveProvider {
                original,
                provider,
                key,
            },
            AiTabAction::DeleteProvider { name } => SettingsAction::DeleteProvider { name },
            AiTabAction::TestProvider {
                provider,
                model,
                key,
            } => SettingsAction::TestProvider {
                provider,
                model,
                key,
            },
        }
    }
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
///
/// `ai_settings` is the live `[ai]` table for the AI tab's table and dialog,
/// and `key_presence` is the app's cached "is a key stored" answer (the
/// keychain is never read in a frame).
pub fn render(
    ctx: &egui::Context,
    state: &mut SettingsWindowState,
    account: Option<AccountInfo<'_>>,
    ai_settings: &AiSettings,
    key_presence: &BTreeMap<String, bool>,
    actions: &mut Vec<SettingsAction>,
) {
    if !state.open {
        return;
    }
    #[cfg(not(unix))]
    let _ = (ai_settings, key_presence);
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
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icons::icon_text(icons::SETTINGS, "Settings"))
                        .font(fonts::semibold(14.0)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::icon_button(ui, icons::X, "Close  Esc").clicked() {
                        close = true;
                    }
                });
            });
            rule(ui);

            tab_bar(ui, &mut state.tab);
            rule(ui);

            match state.tab {
                SettingsTab::General => {
                    if let Some(account) = &account {
                        setting_row(ui, "Account", |ui| {
                            ui.horizontal(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let out = Btn::new(BtnKind::Danger, "Sign out")
                                            .icon(icons::LOG_OUT)
                                            .show(ui);
                                        if out.clicked() {
                                            sign_out = true;
                                            close = true;
                                        }
                                        ui.with_layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                avatar(ui, account.login, 28.0);
                                                ui.add_space(4.0);
                                                ui.vertical(|ui| {
                                                    ui.spacing_mut().item_spacing.y = 0.0;
                                                    ui.label(
                                                        RichText::new(account.login)
                                                            .font(fonts::semibold(13.0)),
                                                    );
                                                    let summary = account_summary(
                                                        account.host,
                                                        account.scopes,
                                                    );
                                                    let width = ui.available_width();
                                                    let line = elided_label(
                                                        ui,
                                                        &summary,
                                                        egui::FontId::proportional(12.0),
                                                        p.muted,
                                                        width,
                                                    );
                                                    if !account.scopes.is_empty() {
                                                        line.on_hover_text(
                                                            account.scopes.join(", "),
                                                        );
                                                    }
                                                });
                                            },
                                        );
                                    },
                                );
                            });
                        });
                    } else {
                        setting_row(ui, "Account", |ui| {
                            ui.label(RichText::new("Not signed in").color(p.muted));
                        });
                    }

                    setting_row(ui, "Default scopes", |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for scope in ALL_SCOPES {
                                let selected = state.vm.default_scopes.contains(&scope);
                                let chip = toggle_chip(
                                    ui,
                                    selected,
                                    Some(scope_icon(&scope)),
                                    &scope_label(&scope),
                                    None,
                                );
                                if chip.clicked() {
                                    if selected {
                                        state.vm.default_scopes.retain(|s| s != &scope);
                                    } else {
                                        state.vm.default_scopes.push(scope);
                                    }
                                }
                            }
                        });
                    });

                    setting_row(ui, "Host", |ui| {
                        Field::new(&mut state.vm.host)
                            .icon(icons::GLOBE)
                            .hint("github.com")
                            .show(ui);
                    });

                    setting_row(ui, "OAuth client ID", |ui| {
                        Field::new(&mut state.vm.client_id)
                            .icon(icons::KEY_ROUND)
                            .hint("Built-in")
                            .show(ui);
                    });
                }
                SettingsTab::Appearance => {
                    setting_row(ui, "Theme", |ui| {
                        if let Some(theme) = segmented(
                            ui,
                            state.vm.theme,
                            &[
                                Seg::icon_text(Theme::Dark, icons::MOON, "Dark"),
                                Seg::icon_text(Theme::Light, icons::SUN, "Light"),
                                Seg::text(Theme::System, "System"),
                            ],
                        ) {
                            state.vm.theme = theme;
                        }
                    });

                    setting_row(ui, "Diff layout", |ui| {
                        if let Some(mode) = segmented(
                            ui,
                            state.vm.diff_mode,
                            &[
                                Seg::icon_text(DiffMode::Split, icons::COLUMNS_2, "Split"),
                                Seg::icon_text(DiffMode::Unified, icons::ROWS_2, "Unified"),
                            ],
                        ) {
                            state.vm.diff_mode = mode;
                        }
                    });

                    setting_row(ui, "Code font size", |ui| {
                        if let Some(size) = segmented(
                            ui,
                            state.vm.code_font_size as u32,
                            &[
                                Seg::text(12, "12"),
                                Seg::text(13, "13"),
                                Seg::text(14, "14"),
                            ],
                        ) {
                            state.vm.code_font_size = size as f32;
                        }
                    });
                }
                #[cfg(unix)]
                SettingsTab::Ai => {
                    for action in
                        settings_ai_tab::render(ui, &mut state.ai_tab, ai_settings, key_presence)
                    {
                        actions.push(SettingsAction::from(action));
                    }
                }
            }

            if has_save_footer(state.tab) {
                rule(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if Btn::new(BtnKind::Primary, "Save").show(ui).clicked() {
                        actions.push(SettingsAction::Apply(state.vm.clone()));
                        close = true;
                    }
                    if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
                        close = true;
                    }
                });
            }
        });
    if sign_out {
        actions.push(SettingsAction::SignOut);
    }
    if close {
        state.open = false;
    }
}

/// The tab strip: a muted label per tab, the selected one semibold with an
/// accent underline (mockup `ai-settings-tab.html`).
fn tab_bar(ui: &mut egui::Ui, selected: &mut SettingsTab) {
    ui.horizontal(|ui| {
        for tab in all_tabs() {
            if tab_button(ui, tab.label(), *selected == tab).clicked() {
                *selected = tab;
            }
        }
    });
}

/// One tab label.
fn tab_button(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let p = theme::palette_of(ui);
    let mut color = if active { p.text } else { p.muted };
    let font = if active {
        fonts::semibold(13.0)
    } else {
        egui::FontId::proportional(13.0)
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(label.len() as f32 * 8.0 + 20.0, 28.0),
        egui::Sense::click(),
    );
    if response.hovered() {
        color = p.text;
    }
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    if active {
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            egui::Stroke::new(2.0, p.accent),
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// One settings row (mockup `.settings-grid`): a 130 pt `muted` label
/// beside the control, which keeps its natural width.
fn setting_row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    let p = theme::palette_of(ui);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(130.0, 30.0), egui::Sense::hover());
        ui.painter().text(
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            p.muted,
        );
        ui.vertical(|ui| add(ui));
    });
    ui.add_space(8.0);
}

/// A full-width 1 pt `line` rule with breathing room.
fn rule(ui: &mut egui::Ui) {
    let p = theme::palette_of(ui);
    ui.add_space(4.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, (1.0, p.line));
    ui.add_space(8.0);
}

/// The account's second line: the host and its scopes, named when there are
/// at most three ("github.com, scopes repo and read:org"), counted otherwise.
pub fn account_summary(host: &str, scopes: &[String]) -> String {
    match scopes.len() {
        0 => format!("{host}, no scopes"),
        1..=3 => format!("{host}, scopes {}", scopes.join(" and ")),
        n => format!("{host}, {n} scopes"),
    }
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
    fn account_summary_names_few_scopes_and_counts_many() {
        let few = vec!["repo".to_owned(), "read:org".to_owned()];
        assert_eq!(
            account_summary("github.com", &few),
            "github.com, scopes repo and read:org"
        );
        let many: Vec<String> = (0..8).map(|i| format!("s{i}")).collect();
        assert_eq!(account_summary("github.com", &many), "github.com, 8 scopes");
    }

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
                &settings.ai,
                &BTreeMap::new(),
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
        assert_eq!(state.tab, SettingsTab::General);
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

    /// Saving General/Appearance must not rewrite the `[ai]` table, and an
    /// invalid AI row can no longer block it (spec §10).
    #[test]
    fn general_save_leaves_the_ai_table_untouched() {
        use crate::persist::settings::{LlmApi, ProviderSettings};

        let mut settings = Settings::default();
        settings.ai.providers = vec![ProviderSettings {
            name: "  not a valid name ".into(),
            api: LlmApi::AnthropicMessages,
            ..ProviderSettings::default()
        }];
        let before = settings.ai.clone();
        let mut vm = settings_vm(&settings);
        vm.host = "ghe.example.com".into();
        assert!(vm.apply(&mut settings));
        assert_eq!(
            settings.ai, before,
            "the General save neither validates nor rewrites [ai]"
        );
    }

    #[test]
    fn the_ai_tab_is_offered_only_on_unix() {
        let tabs = all_tabs();
        assert!(tabs.contains(&SettingsTab::General));
        assert!(tabs.contains(&SettingsTab::Appearance));
        #[cfg(unix)]
        assert!(tabs.contains(&SettingsTab::Ai));
        assert!(has_save_footer(SettingsTab::General));
        assert!(has_save_footer(SettingsTab::Appearance));
        #[cfg(unix)]
        assert!(!has_save_footer(SettingsTab::Ai));
    }
}
