//! The settings modal window (Task 53, restyled by redesign Task 14).
//!
//! A floating [`egui::Window`], not a fourth screen: it edits a working copy
//! of the settings ([`SettingsVm`]) and hands the result back as a
//! [`SettingsAction`] for the app to persist. It is also the only way to
//! **sign out** — the button rides the app's existing sign-out flow.

use std::collections::BTreeMap;

use eframe::egui::{self, Frame, Margin, RichText};

use crate::ui::components::{Btn, BtnKind, Field, Seg, elided_label, segmented, toggle_chip};
#[cfg(unix)]
use crate::ui::components::{Tone, pill};
#[cfg(unix)]
use eframe::egui::{Popup, PopupCloseBehavior};

use crate::github::search::Scope;
use crate::model::diff::DiffMode;
use crate::persist::settings::{ModelSettings, ProviderSettings, RepoAiSettings, Settings};
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
    /// The `[ai]` table's editable copy (spec §10).
    pub ai: AiVm,
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
            ai: AiVm::default(),
        }
    }
}

/// The Settings window's working copy of the `[ai]` table (spec §10).
///
/// The API keys are the one thing here that is *not* settings: a
/// [`ProviderVm::key_input`] is handed to the app as a
/// [`SettingsAction::SetLlmKey`] and lives in the keychain from then on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiVm {
    /// The configured endpoints, in display order.
    pub providers: Vec<ProviderVm>,
    /// The main review/chat model. Empty provider and model mean "not
    /// configured"; applying then clears `models.default`.
    pub default_model: ModelSettings,
    /// The per-repository AI settings.
    pub repos: Vec<RepoAiSettings>,
    /// The latest "Test connection" outcome per provider name.
    pub test_results: BTreeMap<String, Result<String, String>>,
}

impl AiVm {
    /// The provider named `name`, if it is configured.
    #[cfg(unix)]
    fn provider(&self, name: &str) -> Option<&ProviderSettings> {
        self.providers
            .iter()
            .map(|provider| &provider.settings)
            .find(|provider| provider.name == name)
    }

    /// Reject what [`SettingsVm::apply`] cannot write: provider names key the
    /// keychain entry and the model's routing, so they must be non-empty and
    /// unique.
    fn validate(&self) -> Result<(), String> {
        for (index, provider) in self.providers.iter().enumerate() {
            let name = &provider.settings.name;
            if name.trim().is_empty() {
                return Err(format!("provider {} has an empty name", index + 1));
            }
            // The name keys the keychain account (`llm:<name>`) and the
            // model's routing, so it must be stored exactly as typed.
            if name != name.trim() {
                return Err("provider names cannot start or end with spaces".to_owned());
            }
            if self.providers[..index]
                .iter()
                .any(|other| other.settings.name == *name)
            {
                return Err(format!(
                    "provider names must be unique: {name} is used twice"
                ));
            }
        }
        Ok(())
    }
}

/// One provider row: its persisted settings plus the key being typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderVm {
    /// The provider as it will be written back to `settings.toml`.
    pub settings: ProviderSettings,
    /// A key the user typed this session. Never written to `settings.toml`;
    /// non-empty on Save means "store it in the keychain".
    pub key_input: String,
    /// Whether the keychain already holds a key for `settings.name`. A pure
    /// probe: the key itself never reaches the VM.
    pub key_stored: bool,
}

/// The one-per-line text areas of one repository row.
///
/// The buffers live beside [`AiVm::repos`] rather than inside it so a
/// half-typed line — a trailing newline, say — survives the frame instead of
/// being re-split out of [`RepoAiSettings::commands`] every time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoDraft {
    /// The `commands` box, one command per line.
    pub commands: String,
    /// The `prepare` box, one command per line.
    pub prepare: String,
}

impl RepoDraft {
    /// The draft for `repo`: its lists joined one per line.
    fn for_repo(repo: &RepoAiSettings) -> RepoDraft {
        RepoDraft {
            commands: repo.commands.join("\n"),
            prepare: repo.prepare.join("\n"),
        }
    }

    /// Split a text area back into list entries: trimmed, empty lines dropped.
    #[cfg(unix)]
    fn lines(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// The working copy the window starts from.
///
/// `has_key` is the pure keychain probe the AI section uses to show "stored":
/// it answers whether a key exists, never what it is.
pub fn settings_vm(s: &Settings, has_key: &dyn Fn(&str) -> bool) -> SettingsVm {
    SettingsVm {
        host: s.host.clone(),
        client_id: s.client_id.clone().unwrap_or_default(),
        default_scopes: s.default_scopes.clone(),
        theme: s.theme,
        diff_mode: s.diff.mode,
        code_font_size: s.code_font_size,
        ai: AiVm {
            providers: s
                .ai
                .providers
                .iter()
                .map(|provider| ProviderVm {
                    settings: provider.clone(),
                    key_input: String::new(),
                    key_stored: has_key(&provider.name),
                })
                .collect(),
            default_model: s.ai.models.default.clone().unwrap_or_default(),
            repos: s.ai.repos.clone(),
            test_results: BTreeMap::new(),
        },
    }
}

impl SettingsVm {
    /// Write the edited values back into `s`.
    ///
    /// Returns `true` when the host changed: the stored token belongs to the
    /// old host, so the app must end the session and ask for sign-in again.
    ///
    /// Errors when the AI section cannot be written (see
    /// [`AiVm::validate`]); nothing is written in that case. The API keys in
    /// [`ProviderVm::key_input`] never reach `s` — they ride
    /// [`SettingsAction::SetLlmKey`] into the keychain instead.
    ///
    /// A default model whose provider is not among the written providers is
    /// dropped (`models.default = None`), so the file never names an endpoint
    /// that does not exist.
    pub fn apply(self, s: &mut Settings) -> Result<bool, String> {
        self.ai.validate()?;
        let host_changed = self.host != s.host;
        s.host = self.host;
        s.client_id = (!self.client_id.is_empty()).then_some(self.client_id);
        s.default_scopes = self.default_scopes;
        s.theme = self.theme;
        s.diff.mode = self.diff_mode;
        s.code_font_size = self.code_font_size;
        s.ai.providers = self
            .ai
            .providers
            .into_iter()
            .map(|provider| provider.settings)
            .collect();
        // Spec §10: a default model is only configured when it names an
        // endpoint that exists. Removing the last provider must not leave
        // `[ai.models.default]` naming it, and an all-empty model is "not
        // configured" too, so a default settings file never grows an empty
        // table.
        let model = self.ai.default_model;
        let configured = !model.provider.trim().is_empty()
            && s.ai
                .providers
                .iter()
                .any(|provider| provider.name == model.provider);
        s.ai.models.default = configured.then_some(model);
        s.ai.repos = self.ai.repos;
        Ok(host_changed)
    }
}

/// The provider names `before` had and `after` does not: the keychain
/// accounts (`llm:<name>`) a successful apply leaves orphaned. The app clears
/// them so re-adding the name cannot silently reuse an old credential.
pub fn removed_provider_names(before: &[String], after: &[String]) -> Vec<String> {
    before
        .iter()
        .filter(|name| !after.contains(name))
        .cloned()
        .collect()
}

/// The actions Save emits: the apply first, then one [`SettingsAction::SetLlmKey`]
/// per typed key. Every `key_input` is cleared in `vm` before it is cloned, so
/// the key rides only in the emitted actions — never in the apply's settings.
fn save_actions(vm: &mut SettingsVm) -> Vec<SettingsAction> {
    let keys: Vec<(String, String)> = vm
        .ai
        .providers
        .iter()
        .filter(|provider| !provider.key_input.is_empty())
        .map(|provider| (provider.settings.name.clone(), provider.key_input.clone()))
        .collect();
    for provider in &mut vm.ai.providers {
        provider.key_input.clear();
    }
    let mut actions = vec![SettingsAction::Apply(vm.clone())];
    actions.extend(
        keys.into_iter()
            .map(|(provider, key)| SettingsAction::SetLlmKey { provider, key }),
    );
    actions
}

/// The window's state across frames, owned by the app.
#[derive(Clone, Debug, Default)]
pub struct SettingsWindowState {
    /// Whether the window is showing.
    pub open: bool,
    /// The working copy being edited.
    pub vm: SettingsVm,
    /// The repo text areas' buffers, one per `vm.ai.repos` entry, in order.
    pub drafts: Vec<RepoDraft>,
}

impl SettingsWindowState {
    /// Open the window on a fresh copy of `settings`, with `has_key` probing
    /// the keychain for the AI section's "stored" marks.
    pub fn open_with(&mut self, settings: &Settings, has_key: &dyn Fn(&str) -> bool) {
        self.vm = settings_vm(settings, has_key);
        self.drafts = self.vm.ai.repos.iter().map(RepoDraft::for_repo).collect();
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
    /// session). Errors when the AI section is invalid.
    Apply(SettingsVm),
    /// Store `key` for `provider` in the keychain, never in the settings file.
    SetLlmKey {
        /// The provider name the key belongs to.
        provider: String,
        /// The key as typed, in this action only.
        key: String,
    },
    /// Run the "Test connection" probe against `provider` with `model`.
    TestProvider {
        /// The endpoint to dial.
        provider: ProviderSettings,
        /// The model to probe with.
        model: ModelSettings,
    },
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

            setting_row(ui, "Account", |ui| {
                let Some(account) = &account else {
                    ui.label(RichText::new("Not signed in").color(p.muted));
                    return;
                };
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let out = Btn::new(BtnKind::Danger, "Sign out")
                            .icon(icons::LOG_OUT)
                            .show(ui);
                        if out.clicked() {
                            sign_out = true;
                            close = true;
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            avatar(ui, account.login, 28.0);
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.label(RichText::new(account.login).font(fonts::semibold(13.0)));
                                let summary = account_summary(account.host, account.scopes);
                                let width = ui.available_width();
                                let line = elided_label(
                                    ui,
                                    &summary,
                                    egui::FontId::proportional(12.0),
                                    p.muted,
                                    width,
                                );
                                if !account.scopes.is_empty() {
                                    line.on_hover_text(account.scopes.join(", "));
                                }
                            });
                        });
                    });
                });
            });

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

            // Spec §10: the AI section is Unix-only for now (the LLM stack
            // is); elsewhere the VM keeps `ai` untouched and applies it back
            // verbatim.
            #[cfg(unix)]
            {
                rule(ui);
                ai_section(ui, &mut state.vm.ai, &mut state.drafts, actions);
            }

            rule(ui);
            let valid = state.vm.ai.validate().is_ok();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = Btn::new(BtnKind::Primary, "Save").enabled(valid).show(ui);
                let save = if valid {
                    save
                } else {
                    save.on_hover_text("fix the AI providers before saving")
                };
                if save.clicked() {
                    // Apply comes first (the app stops on a rejected apply,
                    // so a key is never stored for unsaved settings), then
                    // the typed keys, which ride only in these actions.
                    actions.extend(save_actions(&mut state.vm));
                    close = true;
                }
                if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
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

/// Keep one text buffer per repo: a missing buffer is filled from that
/// repo's own `commands`/`prepare` — never with an empty draft, which the
/// write-back below would then persist — and buffers for deleted repos are
/// dropped.
#[cfg(unix)]
fn sync_drafts(ai: &AiVm, drafts: &mut Vec<RepoDraft>) {
    while drafts.len() < ai.repos.len() {
        let index = drafts.len();
        drafts.push(RepoDraft::for_repo(&ai.repos[index]));
    }
    drafts.truncate(ai.repos.len());
}

/// The Settings window's AI section (spec §10): providers, the default model,
/// and the per-repository settings. `actions` receives "Test connection" asks;
/// Save emits the keys and the apply (see [`render`]).
#[cfg(unix)]
fn ai_section(
    ui: &mut egui::Ui,
    ai: &mut AiVm,
    drafts: &mut Vec<RepoDraft>,
    actions: &mut Vec<SettingsAction>,
) {
    let p = theme::palette_of(ui);
    sync_drafts(ai, drafts);

    let screen_height = ui.ctx().viewport_rect().height();
    egui::ScrollArea::vertical()
        .max_height((screen_height - 560.0).clamp(140.0, 560.0))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ai_providers(ui, ai);
            if let Err(error) = ai.validate() {
                ui.label(RichText::new(error).size(12.0).color(p.bad));
            }
            ui.add_space(6.0);
            ai_model(ui, ai, actions);
            ui.add_space(6.0);
            ai_repos(ui, ai, drafts);
            // The text areas are authoritative for the lists they edit;
            // applying reads them back through `ai.repos`.
            flush_drafts(ai, drafts);
        });
}

/// Copy the text areas back into `ai.repos`, so `Apply` sees the current
/// command lists rather than what the window opened with.
#[cfg(unix)]
fn flush_drafts(ai: &mut AiVm, drafts: &[RepoDraft]) {
    for (repo, draft) in ai.repos.iter_mut().zip(drafts) {
        repo.commands = RepoDraft::lines(&draft.commands);
        repo.prepare = RepoDraft::lines(&draft.prepare);
    }
}

/// One provider box: name, API, base URL, key, Remove. Add provider sits in
/// the heading.
#[cfg(unix)]
fn ai_providers(ui: &mut egui::Ui, ai: &mut AiVm) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Providers").font(fonts::semibold(13.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if Btn::new(BtnKind::Default, "Add provider")
                .icon(icons::PLUS)
                .show(ui)
                .clicked()
            {
                let name = unique_provider_name(&ai.providers);
                // The first endpoint is the obvious default model route.
                if ai.default_model.provider.is_empty() {
                    ai.default_model.provider = name.clone();
                }
                ai.providers.push(ProviderVm {
                    settings: ProviderSettings {
                        name,
                        ..ProviderSettings::default()
                    },
                    ..ProviderVm::default()
                });
            }
        });
    });
    if ai.providers.is_empty() {
        ui.label(
            RichText::new("No endpoints yet. Add one to run AI reviews.")
                .size(12.0)
                .color(p.muted),
        );
    }

    let mut remove = None;
    for (index, provider) in ai.providers.iter_mut().enumerate() {
        Frame::new()
            .fill(p.bg)
            .corner_radius(8.0)
            .stroke((1.0, p.line))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    Field::new(&mut provider.settings.name)
                        .hint("name")
                        .width(140.0)
                        .show(ui);
                    api_combo(ui, &mut provider.settings.api);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Btn::new(BtnKind::Danger, "Remove")
                            .icon(icons::TRASH_2)
                            .show(ui)
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                });
                Field::new(&mut provider.settings.base_url)
                    .icon(icons::GLOBE)
                    .hint("base URL (vendor default when empty)")
                    .width(400.0)
                    .show(ui);
                ui.horizontal(|ui| {
                    Field::new(&mut provider.key_input)
                        .icon(icons::KEY_ROUND)
                        .password(true)
                        .hint("API key (stored in the keychain)")
                        .width(300.0)
                        .show(ui);
                    if provider.key_stored {
                        pill(ui, Some(icons::CHECK), "stored", Tone::Ok);
                    }
                });
            });
        ui.add_space(4.0);
    }
    if let Some(index) = remove {
        let removed = ai.providers.remove(index);
        // A default model that named the removed endpoint has nowhere to
        // route: clear the section here instead of showing a name that is
        // gone (`apply` drops such a model too, as a safety net).
        if ai.default_model.provider == removed.settings.name {
            ai.default_model = ModelSettings::default();
        }
    }
}

/// A name no configured provider uses: `provider-<n>`.
#[cfg(unix)]
fn unique_provider_name(providers: &[ProviderVm]) -> String {
    let mut index = providers.len() + 1;
    loop {
        let name = format!("provider-{index}");
        if !providers.iter().any(|p| p.settings.name == name) {
            return name;
        }
        index += 1;
    }
}

/// The API combo: exactly the two protocols this build speaks, plus the
/// current (possibly unsupported, hand-edited) value as its label.
#[cfg(unix)]
fn api_combo(ui: &mut egui::Ui, api: &mut crate::persist::settings::LlmApi) {
    use crate::persist::settings::LlmApi;
    let button = combo_button(ui, api_label(*api), 180.0);
    Popup::from_toggle_button_response(&button).show(|ui| {
        ui.set_min_width(180.0);
        for candidate in [LlmApi::AnthropicMessages, LlmApi::OpenaiResponses] {
            if ui
                .selectable_label(*api == candidate, api_label(candidate))
                .clicked()
            {
                *api = candidate;
            }
        }
    });
}

/// The display name of one API protocol.
#[cfg(unix)]
fn api_label(api: crate::persist::settings::LlmApi) -> &'static str {
    use crate::persist::settings::LlmApi;
    match api {
        LlmApi::AnthropicMessages => "Anthropic",
        LlmApi::OpenaiResponses => "OpenAI",
        LlmApi::Unsupported => "Unsupported",
    }
}

/// The default model: provider, model id, its limits, the reasoning effort,
/// and the Test connection probe.
#[cfg(unix)]
fn ai_model(ui: &mut egui::Ui, ai: &mut AiVm, actions: &mut Vec<SettingsAction>) {
    use crate::persist::settings::ReasoningLevel;
    let p = theme::palette_of(ui);
    ui.label(RichText::new("Default model").font(fonts::semibold(13.0)));
    let names: Vec<String> = ai
        .providers
        .iter()
        .map(|provider| provider.settings.name.clone())
        .collect();
    ui.horizontal(|ui| {
        let current = if ai.default_model.provider.is_empty() {
            "provider".to_owned()
        } else {
            ai.default_model.provider.clone()
        };
        let button = combo_button(ui, &current, 190.0);
        Popup::from_toggle_button_response(&button).show(|ui| {
            ui.set_min_width(190.0);
            if names.is_empty() {
                ui.label(RichText::new("no providers yet").color(p.muted));
            }
            for name in &names {
                if ui
                    .selectable_label(ai.default_model.provider == *name, name)
                    .clicked()
                {
                    ai.default_model.provider = name.clone();
                }
            }
        });
        Field::new(&mut ai.default_model.model)
            .hint("model id")
            .width(220.0)
            .show(ui);
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Context").size(12.0).color(p.muted));
        ui.add(
            egui::DragValue::new(&mut ai.default_model.context_window)
                .speed(1_000.0)
                .range(1_000..=2_000_000),
        );
        ui.label(RichText::new("Max output").size(12.0).color(p.muted));
        ui.add(
            egui::DragValue::new(&mut ai.default_model.max_tokens)
                .speed(128.0)
                .range(1..=200_000),
        );
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Reasoning").size(12.0).color(p.muted));
        let levels: [Seg<'_, Option<ReasoningLevel>>; 4] = [
            Seg::text(None, "Model default"),
            Seg::text(Some(ReasoningLevel::Low), "Low"),
            Seg::text(Some(ReasoningLevel::Medium), "Medium"),
            Seg::text(Some(ReasoningLevel::High), "High"),
        ];
        if let Some(level) = segmented(ui, ai.default_model.reasoning, &levels) {
            ai.default_model.reasoning = level;
        }
    });
    let current = ai.default_model.provider.clone();
    let provider = ai.provider(&current).cloned();
    let model = ai.default_model.model.clone();
    let key_stored = ai
        .providers
        .iter()
        .any(|p| p.settings.name == current && p.key_stored);
    let key_typed = ai
        .providers
        .iter()
        .any(|p| p.settings.name == current && !p.key_input.is_empty());
    ui.horizontal(|ui| {
        let ready = test_connection_ready(provider.is_some(), &model, key_stored, key_typed);
        let clicked = Btn::new(BtnKind::Default, "Test connection")
            .icon(icons::CHECK_CHECK)
            .enabled(ready)
            .show(ui)
            .clicked();
        if clicked && let Some(provider) = provider {
            actions.push(SettingsAction::TestProvider {
                provider,
                model: ai.default_model.clone(),
            });
        }
        // A typed key is enough to probe, but it is only stored on Save.
        if ready && key_typed && !key_stored {
            ui.label(
                RichText::new("uses the key above without saving")
                    .size(12.0)
                    .color(p.muted),
            );
        }
        match ai.test_results.get(&current) {
            Some(Ok(model)) => {
                ui.label(RichText::new(format!("connected — {model} answered")).color(p.ok));
            }
            Some(Err(error)) => {
                let width = ui.available_width();
                elided_label(ui, error, egui::FontId::proportional(12.0), p.bad, width);
            }
            None => {
                ui.label(RichText::new("not tested").size(12.0).color(p.faint));
            }
        }
    });
}

/// Whether "Test connection" may run: the provider exists, a model id is
/// entered, and there is a key to try — stored in the keychain or typed in
/// this session (a typed key probes without being saved).
#[cfg(unix)]
fn test_connection_ready(
    provider_exists: bool,
    model: &str,
    key_stored: bool,
    key_typed: bool,
) -> bool {
    provider_exists && !model.trim().is_empty() && (key_stored || key_typed)
}

/// The per-repository settings: the checkout path (with a folder picker) and
/// the two command lists as one-per-line text areas.
#[cfg(unix)]
fn ai_repos(ui: &mut egui::Ui, ai: &mut AiVm, drafts: &mut Vec<RepoDraft>) {
    let p = theme::palette_of(ui);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Repositories").font(fonts::semibold(13.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if Btn::new(BtnKind::Default, "Add repository")
                .icon(icons::PLUS)
                .show(ui)
                .clicked()
            {
                ai.repos.push(RepoAiSettings::default());
                drafts.push(RepoDraft::default());
            }
        });
    });
    if ai.repos.is_empty() {
        ui.label(
            RichText::new("No repositories yet; AI review only needs one for checkouts.")
                .size(12.0)
                .color(p.muted),
        );
    }

    let mut remove = None;
    for (index, repo) in ai.repos.iter_mut().enumerate() {
        Frame::new()
            .fill(p.bg)
            .corner_radius(8.0)
            .stroke((1.0, p.line))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    Field::new(&mut repo.repo)
                        .icon(icons::GITHUB)
                        .hint("host/owner/name")
                        .width(300.0)
                        .show(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Btn::new(BtnKind::Danger, "Remove")
                            .icon(icons::TRASH_2)
                            .show(ui)
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                });
                ui.horizontal(|ui| {
                    let mut text = repo.path.display().to_string();
                    let edited = Field::new(&mut text)
                        .icon(icons::FOLDER)
                        .hint("/path/to/checkout")
                        .width(300.0)
                        .show(ui);
                    if edited.changed() {
                        repo.path = std::path::PathBuf::from(text);
                    }
                    folder_picker(ui, &mut repo.path);
                });
                if let Some(draft) = drafts.get_mut(index) {
                    text_area(
                        ui,
                        "Commands (one per line)",
                        &mut draft.commands,
                        "cargo test",
                    );
                    text_area(
                        ui,
                        "Prepare, outside the sandbox (one per line)",
                        &mut draft.prepare,
                        "cargo fetch",
                    );
                }
            });
        ui.add_space(4.0);
    }
    if let Some(index) = remove {
        ai.repos.remove(index);
        if index < drafts.len() {
            drafts.remove(index);
        }
    }
}

/// A one-per-line text area in the window's field style.
#[cfg(unix)]
fn text_area(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str) {
    let p = theme::palette_of(ui);
    ui.label(RichText::new(label).size(12.0).color(p.muted));
    let frame = Frame::new()
        .fill(p.bg)
        .corner_radius(6.0)
        .stroke((1.0, p.line))
        .inner_margin(6);
    ui.add(
        egui::TextEdit::multiline(text)
            .frame(frame)
            .desired_rows(2)
            .desired_width(400.0)
            .hint_text(hint),
    );
}

/// The folder picker: a Browse button whose popup walks the directories. A
/// click on a folder selects it *and* descends into it, so the path field is
/// the picker's whole state; clicking outside closes it.
#[cfg(unix)]
fn folder_picker(ui: &mut egui::Ui, path: &mut std::path::PathBuf) {
    let p = theme::palette_of(ui);
    let button = Btn::new(BtnKind::Default, "Browse")
        .icon(icons::FOLDER)
        .show(ui);
    Popup::from_toggle_button_response(&button)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(320.0);
            let current = if path.is_dir() {
                path.clone()
            } else {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
            };
            if let Some(parent) = current.parent()
                && ui.button(".. up").clicked()
            {
                *path = parent.to_path_buf();
            }
            ui.label(
                RichText::new(current.display().to_string())
                    .size(12.0)
                    .color(p.muted),
            );
            let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&current)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|entry| entry.is_dir())
                .filter(|entry| {
                    !entry
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                })
                .collect();
            entries.sort();
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .show(ui, |ui| {
                    for entry in entries {
                        let name = entry
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        if ui.selectable_label(false, format!("{name}/")).clicked() {
                            *path = entry;
                        }
                    }
                });
        });
}

/// A field-styled combobox button (mockup `.combo`): the current value and an
/// up/down chevron, `width` wide.
#[cfg(unix)]
fn combo_button(ui: &mut egui::Ui, value: &str, width: f32) -> egui::Response {
    let p = theme::palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, crate::ui::components::CONTROL_H),
        egui::Sense::click(),
    );
    let stroke = if response.hovered() { p.muted } else { p.line };
    ui.painter().rect(
        rect,
        6.0,
        p.bg,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    let cy = rect.center().y;
    ui.painter().text(
        egui::pos2(rect.right() - 8.0, cy),
        egui::Align2::RIGHT_CENTER,
        icons::CHEVRONS_UP_DOWN,
        egui::FontId::proportional(14.0),
        p.muted,
    );
    let galley = crate::ui::components::elided_galley(
        ui,
        value,
        egui::FontId::proportional(13.0),
        p.text,
        rect.width() - 34.0,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 8.0, cy - galley.size().y / 2.0),
        galley,
        p.text,
    );
    response
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
    use crate::test_support::tempdir;

    #[test]
    fn window_paints_an_idle_frame_without_asking_for_anything() {
        let dir = tempdir();
        let settings = Settings::load_or_default(dir.path());
        let mut state = SettingsWindowState::default();
        state.open_with(&settings, &|_| false);
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
        state.open_with(&settings, &|_| false);
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
        let mut vm = settings_vm(&settings, &|_| false);
        vm.theme = Theme::Dark;
        vm.default_scopes = vec![Scope::Mentions];
        assert!(!vm.apply(&mut settings).unwrap());
        assert_eq!(settings.theme, Theme::Dark);
        assert_eq!(settings.default_scopes, vec![Scope::Mentions]);
        assert!(
            settings.account.is_some(),
            "apply never touches the account"
        );
    }

    /// A settings value with one provider, one default model and one repo.
    fn ai_settings() -> Settings {
        use crate::persist::settings::{
            LlmApi, ModelSettings, ProviderSettings, ReasoningLevel, RepoAiSettings,
        };
        let mut s = Settings::default();
        s.ai.providers = vec![ProviderSettings {
            name: "work".into(),
            api: LlmApi::AnthropicMessages,
            base_url: "https://llm.example.com".into(),
            headers: BTreeMap::new(),
        }];
        s.ai.models.default = Some(ModelSettings {
            provider: "work".into(),
            model: "model-id".into(),
            context_window: 200_000,
            max_tokens: 8192,
            reasoning: Some(ReasoningLevel::Medium),
        });
        s.ai.repos = vec![RepoAiSettings {
            repo: "github.com/owner/name".into(),
            path: "/Users/me/src/owner/name".into(),
            commands: vec!["cargo test".into(), "cargo clippy".into()],
            prepare: vec!["cargo fetch".into()],
            ..RepoAiSettings::default()
        }];
        s
    }

    #[test]
    fn ai_vm_round_trips_through_apply() {
        let mut settings = ai_settings();
        let before = settings.ai.clone();
        let vm = settings_vm(&settings, &|_| false);
        assert_eq!(vm.ai.providers.len(), 1);
        assert_eq!(vm.ai.repos.len(), 1);
        // Touch nothing: the VM must already describe the same `[ai]` table.
        assert!(!vm.apply(&mut settings).unwrap());
        assert_eq!(settings.ai, before);
    }

    #[test]
    fn apply_emits_no_key_into_settings() {
        let mut settings = ai_settings();
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.providers[0].key_input = "sekrit".into();
        vm.apply(&mut settings).unwrap();
        let raw = toml::to_string(&settings).unwrap();
        assert!(
            !raw.contains("sekrit"),
            "the key never reaches settings.toml"
        );
        assert!(!raw.contains("key_input"));
    }

    #[test]
    fn duplicate_provider_names_are_rejected() {
        let mut settings = ai_settings();
        let mut vm = settings_vm(&settings, &|_| false);
        let duplicate = vm.ai.providers[0].clone();
        vm.ai.providers.push(duplicate);
        let error = vm.clone().apply(&mut settings).unwrap_err();
        assert!(error.contains("unique"), "unexpected error: {error}");
        assert_eq!(
            settings.ai.providers.len(),
            1,
            "a rejected apply writes nothing"
        );

        // An empty name is rejected too.
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.providers[0].settings.name = "  ".into();
        let error = vm.apply(&mut settings).unwrap_err();
        assert!(error.contains("empty"), "unexpected error: {error}");
    }

    #[test]
    fn an_empty_model_clears_the_default() {
        let mut settings = ai_settings();
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.default_model = crate::persist::settings::ModelSettings::default();
        vm.apply(&mut settings).unwrap();
        assert!(
            settings.ai.models.default.is_none(),
            "an unconfigured model stays out of settings.toml"
        );
    }

    /// Regression (review finding 1): removing the provider the default model
    /// routes through must not persist a dangling `[ai.models.default]`.
    ///
    /// `apply` drops it (rather than erroring): the window's Remove button
    /// already clears the section, so this is the last line of defence and
    /// must never block saving unrelated settings.
    #[test]
    fn a_default_model_without_its_provider_is_dropped() {
        let mut settings = ai_settings();
        assert!(settings.ai.models.default.is_some());
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.providers.clear(); // the removed provider still names the model
        assert!(!vm.apply(&mut settings).unwrap());
        assert!(
            settings.ai.models.default.is_none(),
            "the file must not name a provider that is gone"
        );
        assert!(settings.ai.providers.is_empty());
    }

    /// Regression (review finding 2): a missing text buffer is refilled from
    /// the repo itself, so the write-back cannot blank its commands.
    #[cfg(unix)]
    #[test]
    fn a_missing_draft_is_filled_from_the_repo_instead_of_blanking_it() {
        let before = ai_settings();
        let mut ai = settings_vm(&before, &|_| false).ai;
        let mut drafts = Vec::new();
        sync_drafts(&ai, &mut drafts);
        assert_eq!(drafts.len(), ai.repos.len());
        assert_eq!(drafts[0].commands, "cargo test\ncargo clippy");
        assert_eq!(drafts[0].prepare, "cargo fetch");
        flush_drafts(&mut ai, &drafts);
        assert_eq!(ai.repos[0].commands, before.ai.repos[0].commands);
        assert_eq!(ai.repos[0].prepare, before.ai.repos[0].prepare);

        // Extra buffers (a repo was just removed) are dropped, not written.
        drafts.push(RepoDraft::default());
        sync_drafts(&ai, &mut drafts);
        assert_eq!(drafts.len(), ai.repos.len());
    }

    #[test]
    fn removed_provider_names_lists_what_a_rename_or_remove_orphaned() {
        let before = vec!["work".to_string(), "home".to_string()];
        assert_eq!(
            removed_provider_names(&before, &["work".to_string()]),
            vec!["home".to_string()]
        );
        // A rename is a remove plus an add: the old name is cleared.
        assert_eq!(
            removed_provider_names(&["work".to_string()], &["office".to_string()]),
            vec!["work".to_string()]
        );
        assert!(removed_provider_names(&before, &before).is_empty());
        assert!(removed_provider_names(&[], &before).is_empty());
    }

    #[test]
    fn save_emits_the_apply_first_and_clears_the_typed_keys() {
        let settings = ai_settings();
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.providers[0].key_input = "sekrit".into();

        let actions = save_actions(&mut vm);

        assert_eq!(actions.len(), 2, "one apply and one key: {actions:?}");
        match &actions[0] {
            SettingsAction::Apply(applied) => {
                assert_eq!(applied.ai.providers.len(), 1);
                assert!(
                    applied.ai.providers.iter().all(|p| p.key_input.is_empty()),
                    "the apply must not carry the key"
                );
            }
            other => panic!("the apply must come first, got {other:?}"),
        }
        assert_eq!(
            actions[1],
            SettingsAction::SetLlmKey {
                provider: "work".into(),
                key: "sekrit".into(),
            }
        );
        assert!(
            vm.ai.providers[0].key_input.is_empty(),
            "the VM no longer holds the typed key"
        );
    }

    #[test]
    fn provider_names_with_surrounding_spaces_are_rejected() {
        let mut settings = ai_settings();
        let before = settings.ai.clone();
        let mut vm = settings_vm(&settings, &|_| false);
        vm.ai.providers[0].settings.name = " work".into();
        let error = vm.apply(&mut settings).unwrap_err();
        assert!(error.contains("spaces"), "unexpected error: {error}");
        assert_eq!(settings.ai, before, "a rejected apply writes nothing");
    }

    #[cfg(unix)]
    #[test]
    fn test_connection_needs_a_provider_model_and_key() {
        assert!(
            test_connection_ready(true, "model", true, false),
            "stored key"
        );
        assert!(
            test_connection_ready(true, "model", false, true),
            "a typed key may probe without saving"
        );
        assert!(
            !test_connection_ready(true, "model", false, false),
            "no key at all"
        );
        assert!(!test_connection_ready(false, "model", true, true));
        assert!(!test_connection_ready(true, "  ", true, false));
    }

    #[test]
    fn key_stored_reflects_the_keychain_probe() {
        let settings = ai_settings();
        let stored = settings_vm(&settings, &|name| name == "work");
        assert!(stored.ai.providers[0].key_stored);
        let missing = settings_vm(&settings, &|_| false);
        assert!(!missing.ai.providers[0].key_stored);
        assert!(
            missing.ai.providers[0].key_input.is_empty(),
            "the probe never exposes the key itself"
        );
    }
}
