//! The Settings window's AI tab: the providers table and its edit dialog
//! (spec §10, Task 5).
//!
//! Like the other widgets this never mutates the app's settings: a dialog
//! reports a [`AiTabAction`] and `app.rs` applies it (patching the settings,
//! persisting them, and moving the keychain entry on the worker). The API key
//! itself only ever rides the Save/Test action; it is never stored in this
//! module's state beyond the dialog's frame.
//!
//! Unix-only with the rest of the AI code.

use std::collections::BTreeMap;

use eframe::egui::{self, Frame, Margin, RichText};

use crate::persist::settings::{AiSettings, LlmApi, ModelSettings, ProviderSettings};
use crate::ui::components::{Btn, BtnKind, Field, elided_galley};
use crate::ui::{fonts, icons, theme};

/// Whether the keychain holds a key for a provider, keyed by provider name.
///
/// The app caches this (a keychain read is blocking, so it never happens in a
/// frame); a name missing from the map is "unknown", not "no key".
pub type KeyPresence = BTreeMap<String, bool>;

/// The Key column's state for one provider row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyState {
    /// The app has not probed this name (yet).
    #[default]
    Unknown,
    /// The keychain holds a key.
    Stored,
    /// No key is stored.
    Missing,
}

/// The Status column's state for one provider row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RowStatus {
    /// A connection test has not run since the window opened.
    #[default]
    NotTested,
    /// A test answered; the payload is the model that served it.
    Connected(String),
    /// A test failed; the payload is a display-safe reason.
    Failed(String),
    /// There is no key to test with.
    NoKey,
}

/// One row of the providers table, decided purely from the settings, the
/// cached key presence and the latest test results.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRowVm {
    /// The provider name (the row's keychain account suffix).
    pub name: String,
    /// The API's display name.
    pub api: String,
    /// The base URL, or `default` when the vendor's endpoint is used.
    pub base_url: String,
    /// Whether a key is stored.
    pub key: KeyState,
    /// The latest test outcome.
    pub status: RowStatus,
}

/// The rows the table shows, in settings order.
pub fn rows(
    ai: &AiSettings,
    keys: &KeyPresence,
    test_results: &BTreeMap<String, Result<String, String>>,
) -> Vec<ProviderRowVm> {
    ai.providers
        .iter()
        .map(|provider| {
            let key = match keys.get(&provider.name) {
                Some(true) => KeyState::Stored,
                Some(false) => KeyState::Missing,
                None => KeyState::Unknown,
            };
            let status = match test_results.get(&provider.name) {
                Some(Ok(model)) => RowStatus::Connected(model.clone()),
                Some(Err(error)) => RowStatus::Failed(error.clone()),
                None if key == KeyState::Missing => RowStatus::NoKey,
                None => RowStatus::NotTested,
            };
            ProviderRowVm {
                name: provider.name.clone(),
                api: api_label(provider.api).to_owned(),
                base_url: if provider.base_url.trim().is_empty() {
                    "default".to_owned()
                } else {
                    provider.base_url.clone()
                },
                key,
                status,
            }
        })
        .collect()
}

/// The display name of one API protocol.
pub fn api_label(api: LlmApi) -> &'static str {
    match api {
        LlmApi::AnthropicMessages => "Anthropic",
        LlmApi::OpenaiResponses => "OpenAI",
        LlmApi::Unsupported => "Unsupported",
    }
}

/// Reject what the settings write cannot key: the name is the keychain
/// account suffix (`llm:<name>`) and the models' routing, so it must be
/// non-empty, stored exactly as typed, and unique.
///
/// `original` is the name the row had before the dialog opened; on a rename
/// the row's own old name does not count as a duplicate.
pub fn validate_provider(
    existing: &[ProviderSettings],
    original: Option<&str>,
    candidate: &ProviderSettings,
) -> Result<(), String> {
    let name = &candidate.name;
    if name.trim().is_empty() {
        return Err("provider name cannot be empty".to_owned());
    }
    if name != name.trim() {
        return Err("provider names cannot start or end with spaces".to_owned());
    }
    if existing
        .iter()
        .filter(|provider| Some(provider.name.as_str()) != original)
        .any(|provider| provider.name == *name)
    {
        return Err(format!(
            "provider names must be unique: {name} is used twice"
        ));
    }
    Ok(())
}

/// Why `name` cannot be deleted: the model (by id) that routes through it, or
/// `None` when nothing does.
///
/// Deleting is blocked rather than silently clearing the model (spec §10: the
/// message names the model).
pub fn provider_delete_blocker(ai: &AiSettings, name: &str) -> Option<String> {
    [ai.models.default.as_ref(), ai.models.lookup.as_ref()]
        .into_iter()
        .flatten()
        .find(|model| model.provider == name)
        .map(|model| model.model.clone())
}

/// Write `provider` into `ai`, replacing the row called `original` (a rename
/// or an edit) or appending a new row.
pub fn put_provider(ai: &mut AiSettings, original: Option<&str>, provider: ProviderSettings) {
    match original {
        Some(original) => {
            if let Some(row) = ai
                .providers
                .iter_mut()
                .find(|row| row.name.as_str() == original)
            {
                *row = provider;
            } else {
                ai.providers.push(provider);
            }
        }
        None => ai.providers.push(provider),
    }
}

/// Replace the provider called `from` with `provider`, and repoint every model
/// that routed through the old name (spec §10: a rename updates the models).
pub fn rename_provider(ai: &mut AiSettings, from: &str, provider: ProviderSettings) {
    let to = provider.name.clone();
    put_provider(ai, Some(from), provider);
    for model in [ai.models.default.as_mut(), ai.models.lookup.as_mut()]
        .into_iter()
        .flatten()
    {
        if model.provider == from {
            model.provider = to.clone();
        }
    }
}

/// The model a Test connection probe should use for `provider`: the default
/// model when it routes through it, else the lookup helper.
///
/// `None` means no configured model uses the provider yet, so the dialog's
/// Test button has nothing to ask about (the Models table arrives in Task 6).
pub fn test_model(ai: &AiSettings, provider: &str) -> Option<ModelSettings> {
    [ai.models.default.as_ref(), ai.models.lookup.as_ref()]
        .into_iter()
        .flatten()
        .find(|model| model.provider == provider && !model.model.trim().is_empty())
        .cloned()
}

/// The provider dialog's working copy. `original` is `None` for a new row.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderDialog {
    /// The row's name before the dialog opened, for the rename and the
    /// duplicate check.
    pub original: Option<String>,
    /// The edited name.
    pub name: String,
    /// The edited protocol.
    pub api: LlmApi,
    /// The edited endpoint; empty means the vendor's default.
    pub base_url: String,
    /// Extra headers are not edited here, but an edit must not drop them.
    pub headers: BTreeMap<String, String>,
    /// A key typed this session. Never persisted here; Save hands it to the
    /// app, which stores it in the keychain on the worker.
    pub key_input: String,
    /// Why the last Delete was refused, shown above the buttons.
    pub error: Option<String>,
}

impl ProviderDialog {
    /// A dialog for a brand-new provider called `name`.
    pub fn blank(name: String) -> ProviderDialog {
        ProviderDialog {
            original: None,
            name,
            api: LlmApi::AnthropicMessages,
            base_url: String::new(),
            headers: BTreeMap::new(),
            key_input: String::new(),
            error: None,
        }
    }

    /// A dialog editing `provider`.
    pub fn for_provider(provider: &ProviderSettings) -> ProviderDialog {
        ProviderDialog {
            original: Some(provider.name.clone()),
            name: provider.name.clone(),
            api: provider.api,
            base_url: provider.base_url.clone(),
            headers: provider.headers.clone(),
            key_input: String::new(),
            error: None,
        }
    }

    /// The provider the fields describe.
    pub fn settings(&self) -> ProviderSettings {
        ProviderSettings {
            name: self.name.clone(),
            api: self.api,
            base_url: self.base_url.clone(),
            headers: self.headers.clone(),
        }
    }

    /// The key the user typed, or `None` to keep whatever is stored.
    fn typed_key(&self) -> Option<String> {
        (!self.key_input.is_empty()).then(|| self.key_input.clone())
    }
}

/// The AI tab's state across frames: the open dialog and the latest test
/// results (keyed by provider name, like the table's Status column).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiTabState {
    /// The provider dialog, when one is open.
    pub dialog: Option<ProviderDialog>,
    /// The latest connection-test outcome per provider name.
    pub test_results: BTreeMap<String, Result<String, String>>,
}

impl AiTabState {
    /// Open the dialog for the provider at `index` in `ai.providers`.
    pub fn edit_provider(&mut self, ai: &AiSettings, index: usize) {
        self.dialog = ai.providers.get(index).map(ProviderDialog::for_provider);
    }
}

/// What the AI tab asks the app to do. The app owns the settings write and
/// the keychain op; the widget never touches either.
#[derive(Clone, Debug, PartialEq)]
pub enum AiTabAction {
    /// Commit one provider row. `original` is the row's name before the edit;
    /// `key` is a key typed in the dialog, never the stored one.
    SaveProvider {
        /// The row's name before the edit, `None` for a new row.
        original: Option<String>,
        /// The edited row.
        provider: ProviderSettings,
        /// A key typed this session, for the keychain only.
        key: Option<String>,
    },
    /// Delete the provider called `name` (the app blocks it if a model uses
    /// it; the dialog checks first, so this only arrives for unused rows).
    DeleteProvider {
        /// The provider to remove.
        name: String,
    },
    /// Probe `provider` with `model`, using `key` when the user typed one.
    TestProvider {
        /// The endpoint to dial.
        provider: ProviderSettings,
        /// The model id to ask for.
        model: ModelSettings,
        /// A key typed in the dialog; wins over the stored one.
        key: Option<String>,
    },
}

/// Draw the providers table and, when open, its dialog. Returns what the user
/// asked for this frame.
pub fn render(
    ui: &mut egui::Ui,
    state: &mut AiTabState,
    ai: &AiSettings,
    keys: &KeyPresence,
) -> Vec<AiTabAction> {
    let mut actions = Vec::new();
    heading(ui, ai, state);

    let rows = rows(ai, keys, &state.test_results);
    let mut open = None;
    egui::ScrollArea::vertical()
        .max_height((ui.ctx().viewport_rect().height() - 340.0).clamp(120.0, 420.0))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            open = table(ui, &rows);
            if rows.is_empty() {
                let p = theme::palette_of(ui);
                ui.label(
                    RichText::new("No providers yet. Add one to run AI reviews.")
                        .size(12.0)
                        .color(p.muted),
                );
            }
        });
    if let Some(index) = open {
        state.edit_provider(ai, index);
    }

    render_dialog(ui.ctx(), state, ai, keys, &mut actions);
    actions
}

/// The section heading with the "+ Add provider" button.
fn heading(ui: &mut egui::Ui, ai: &AiSettings, state: &mut AiTabState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Providers").font(fonts::semibold(13.0)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if Btn::new(BtnKind::Default, "Add provider")
                .icon(icons::PLUS)
                .show(ui)
                .clicked()
            {
                state.dialog = Some(ProviderDialog::blank(unique_name(&ai.providers)));
            }
        });
    });
}

/// A name no configured provider uses: `provider-<n>`.
pub fn unique_name(providers: &[ProviderSettings]) -> String {
    let mut index = providers.len() + 1;
    loop {
        let name = format!("provider-{index}");
        if !providers.iter().any(|provider| provider.name == name) {
            return name;
        }
        index += 1;
    }
}

/// The table's column widths as fractions of the available width: Name, API,
/// Base URL, Key, Status (mockup widths).
const COLUMNS: [f32; 5] = [0.22, 0.15, 0.31, 0.15, 0.17];

/// The header row plus one clickable row per provider. Returns the row the
/// user clicked.
fn table(ui: &mut egui::Ui, rows: &[ProviderRowVm]) -> Option<usize> {
    let p = theme::palette_of(ui);
    let width = ui.available_width();
    let lefts = column_lefts(width);

    let (header, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    for (left, label) in lefts
        .iter()
        .zip(["Name", "API", "Base URL", "Key", "Status"])
    {
        ui.painter().text(
            egui::pos2(header.left() + left, header.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(11.5),
            p.muted,
        );
    }
    ui.painter()
        .hline(header.x_range(), header.bottom(), (1.0, p.line));

    let mut clicked = None;
    for (index, row) in rows.iter().enumerate() {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::click());
        if response.hovered() {
            ui.painter().rect_filled(rect, 4.0, p.hover);
        }
        let cell_width = |column: usize| width * COLUMNS[column] - 8.0;
        cell(
            ui,
            rect,
            lefts[0],
            &row.name,
            cell_width(0),
            p.text,
            egui::FontId::proportional(12.5),
        );
        cell(
            ui,
            rect,
            lefts[1],
            &row.api,
            cell_width(1),
            p.muted,
            egui::FontId::proportional(12.0),
        );
        cell(
            ui,
            rect,
            lefts[2],
            &row.base_url,
            cell_width(2),
            p.muted,
            egui::FontId::proportional(12.0),
        );
        let (key_text, key_color) = match row.key {
            KeyState::Stored => (format!("{} stored", icons::CHECK), p.ok),
            KeyState::Missing => ("no key".to_owned(), p.bad),
            KeyState::Unknown => ("—".to_owned(), p.faint),
        };
        cell(
            ui,
            rect,
            lefts[3],
            &key_text,
            cell_width(3),
            key_color,
            egui::FontId::proportional(12.0),
        );
        let (status_text, status_color) = match &row.status {
            RowStatus::Connected(_) => (format!("{} connected", icons::CIRCLE), p.ok),
            RowStatus::Failed(_) => ("failed".to_owned(), p.bad),
            RowStatus::NoKey => ("—".to_owned(), p.faint),
            RowStatus::NotTested => ("not tested".to_owned(), p.muted),
        };
        cell(
            ui,
            rect,
            lefts[4],
            &status_text,
            cell_width(4),
            status_color,
            egui::FontId::proportional(12.0),
        );
        ui.painter()
            .hline(rect.x_range(), rect.bottom(), (1.0, p.line_soft));

        let hover = match &row.status {
            RowStatus::Connected(model) => format!("{} answered", model),
            RowStatus::Failed(error) => error.clone(),
            _ => "edit provider".to_owned(),
        };
        let response = response.on_hover_text(hover);
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// One elided table cell starting at `left` in `row`.
fn cell(
    ui: &egui::Ui,
    row: egui::Rect,
    left: f32,
    text: &str,
    max_width: f32,
    color: egui::Color32,
    font: egui::FontId,
) {
    let galley = elided_galley(ui, text, font, color, max_width);
    let y = row.center().y - galley.size().y / 2.0;
    ui.painter()
        .galley(egui::pos2(row.left() + left, y), galley, color);
}

/// The left edge of each column, with a little padding before the first.
fn column_lefts(width: f32) -> [f32; 5] {
    let mut lefts = [0.0; 5];
    let mut x = 6.0;
    for (index, fraction) in COLUMNS.iter().enumerate() {
        lefts[index] = x;
        x += width * fraction;
    }
    lefts
}

/// The provider dialog (mockup: the small panel beside the Settings window),
/// shown only while [`AiTabState::dialog`] is set.
fn render_dialog(
    ctx: &egui::Context,
    state: &mut AiTabState,
    ai: &AiSettings,
    keys: &KeyPresence,
    actions: &mut Vec<AiTabAction>,
) {
    if state.dialog.is_none() {
        return;
    }
    let p = theme::palette(ctx.theme() == egui::Theme::Dark);
    let mut close = false;
    let mut outcome = None;
    let results = &state.test_results;
    if let Some(dialog) = state.dialog.as_mut() {
        let title = match &dialog.original {
            Some(name) => format!("Edit provider \"{name}\""),
            None => "Add provider".to_owned(),
        };
        egui::Window::new(title.clone())
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
                ui.set_width(340.0);
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).font(fonts::semibold(14.0)));
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if icons::icon_button(ui, icons::X, "Close  Esc").clicked() {
                                close = true;
                            }
                        },
                    );
                });
                rule(ui);

                field_label(ui, "Name");
                Field::new(&mut dialog.name)
                    .hint("name")
                    .show(ui);
                field_label(ui, "API");
                api_combo(ui, &mut dialog.api);
                field_label(ui, "Base URL (optional)");
                Field::new(&mut dialog.base_url)
                    .icon(icons::GLOBE)
                    .hint("vendor default")
                    .show(ui);

                let stored = keys
                    .get(dialog.original.as_deref().unwrap_or(&dialog.name))
                    .copied()
                    .unwrap_or(false);
                field_label(ui, "API key");
                Field::new(&mut dialog.key_input)
                    .icon(icons::KEY_ROUND)
                    .password(true)
                    .hint(if stored {
                        "stored in keychain — type a new key to replace"
                    } else {
                        "stored in the keychain, never in settings"
                    })
                    .show(ui);

                // The latest Test result for the name the dialog would save.
                match results.get(&dialog.name) {
                    Some(Ok(model)) => {
                        ui.label(
                            RichText::new(format!(
                                "{} connected — {model} answered",
                                icons::CIRCLE
                            ))
                            .size(12.0)
                            .color(p.ok),
                        );
                    }
                    Some(Err(error)) => {
                        ui.label(RichText::new(error).size(12.0).color(p.bad));
                    }
                    None => {
                        ui.label(RichText::new("not tested").size(12.0).color(p.faint));
                    }
                }

                let candidate = dialog.settings();
                let validation =
                    validate_provider(&ai.providers, dialog.original.as_deref(), &candidate);
                if let Some(error) = dialog
                    .error
                    .clone()
                    .or_else(|| validation.as_ref().err().cloned())
                {
                    ui.label(RichText::new(error).size(12.0).color(p.bad));
                }

                let probe_model = dialog
                    .original
                    .as_deref()
                    .and_then(|original| test_model(ai, original))
                    .or_else(|| test_model(ai, &dialog.name));
                let typed = dialog.typed_key();
                let testable = probe_model.is_some()
                    && (typed.is_some() || stored)
                    && !dialog.name.trim().is_empty();

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if dialog.original.is_some() {
                        let delete = Btn::new(BtnKind::Danger, "Delete")
                            .icon(icons::TRASH_2)
                            .show(ui);
                        if delete.clicked()
                            && let Some(name) = dialog.original.clone()
                        {
                            match provider_delete_blocker(ai, &name) {
                                Some(model) => {
                                    dialog.error = Some(format!(
                                        "{name} is used by the model {model}; change that model first"
                                    ));
                                }
                                None => {
                                    outcome = Some(AiTabAction::DeleteProvider { name });
                                    close = true;
                                }
                            }
                        }
                    }
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let save = Btn::new(BtnKind::Primary, "Save")
                                .enabled(validation.is_ok())
                                .show(ui);
                            let save = if validation.is_ok() {
                                save
                            } else {
                                save.on_hover_text("fix the provider before saving")
                            };
                            if save.clicked() {
                                outcome = Some(AiTabAction::SaveProvider {
                                    original: dialog.original.clone(),
                                    provider: candidate.clone(),
                                    key: dialog.typed_key(),
                                });
                                close = true;
                            }
                            if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
                                close = true;
                            }
                            let mut test = Btn::new(BtnKind::Default, "Test")
                                .icon(icons::CHECK_CHECK)
                                .enabled(testable)
                                .show(ui);
                            if !testable {
                                test = test.on_hover_text(if probe_model.is_none() {
                                    "no model uses this provider yet"
                                } else {
                                    "add an API key first"
                                });
                            }
                            if test.clicked()
                                && let Some(model) = probe_model.clone()
                            {
                                outcome = Some(AiTabAction::TestProvider {
                                    provider: candidate.clone(),
                                    model,
                                    key: typed.clone(),
                                });
                            }
                        },
                    );
                });
            });
    }
    if close {
        state.dialog = None;
    }
    if let Some(action) = outcome {
        actions.push(action);
    }
}

/// The dialog's field caption.
fn field_label(ui: &mut egui::Ui, label: &str) {
    let p = theme::palette_of(ui);
    ui.label(RichText::new(label).size(12.0).color(p.muted));
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

/// The API combo: the two protocols this build speaks. A hand-edited
/// unsupported protocol stays visible (and unchangeable until replaced).
fn api_combo(ui: &mut egui::Ui, api: &mut LlmApi) {
    egui::ComboBox::from_id_salt("provider-api")
        .width(200.0)
        .selected_text(api_label(*api))
        .show_ui(ui, |ui| {
            for candidate in [LlmApi::AnthropicMessages, LlmApi::OpenaiResponses] {
                ui.selectable_value(api, candidate, api_label(candidate));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::settings::{LlmApi, ModelSettings, ProviderSettings};

    fn provider(name: &str) -> ProviderSettings {
        ProviderSettings {
            name: name.to_owned(),
            api: LlmApi::AnthropicMessages,
            ..ProviderSettings::default()
        }
    }

    #[test]
    fn validate_provider_rejects_duplicates_and_edge_spaces() {
        let existing = vec![provider("work")];

        // A new row may not reuse a name.
        let error = validate_provider(&existing, None, &provider("work")).unwrap_err();
        assert!(error.contains("unique"), "unexpected error: {error}");

        // Editing a row keeps its own name.
        assert!(validate_provider(&existing, Some("work"), &provider("work")).is_ok());

        // A rename still may not land on another row's name.
        let two = vec![provider("work"), provider("home")];
        let error = validate_provider(&two, Some("work"), &provider("home")).unwrap_err();
        assert!(error.contains("unique"), "unexpected error: {error}");

        // Edge spaces are rejected even when unique.
        let error = validate_provider(&existing, None, &provider(" work")).unwrap_err();
        assert!(error.contains("spaces"), "unexpected error: {error}");

        // An empty (or all-space) name is rejected.
        let error = validate_provider(&existing, None, &provider("  ")).unwrap_err();
        assert!(error.contains("empty"), "unexpected error: {error}");

        let error = validate_provider(&existing, None, &provider("")).unwrap_err();
        assert!(error.contains("empty"), "unexpected error: {error}");
    }

    #[test]
    fn delete_blocked_when_a_model_uses_the_provider() {
        let mut ai = AiSettings {
            providers: vec![provider("work"), provider("home")],
            models: crate::persist::settings::AiModels {
                default: Some(ModelSettings {
                    provider: "work".into(),
                    model: "model-id".into(),
                    ..ModelSettings::default()
                }),
                ..Default::default()
            },
            ..AiSettings::default()
        };

        let blocker = provider_delete_blocker(&ai, "work").expect("the default model uses it");
        assert!(blocker.contains("model-id"), "names the model: {blocker}");
        assert_eq!(
            provider_delete_blocker(&ai, "home"),
            None,
            "an unused provider is deletable"
        );

        // The lookup helper blocks too.
        ai.models.lookup = Some(ModelSettings {
            provider: "home".into(),
            model: "cheap-id".into(),
            ..ModelSettings::default()
        });
        let blocker = provider_delete_blocker(&ai, "home").expect("the lookup model uses it");
        assert!(blocker.contains("cheap-id"), "names the model: {blocker}");
    }

    #[test]
    fn rename_updates_model_references() {
        let mut ai = AiSettings {
            providers: vec![provider("work")],
            models: crate::persist::settings::AiModels {
                default: Some(ModelSettings {
                    provider: "work".into(),
                    model: "big".into(),
                    ..ModelSettings::default()
                }),
                lookup: Some(ModelSettings {
                    provider: "work".into(),
                    model: "small".into(),
                    ..ModelSettings::default()
                }),
            },
            ..AiSettings::default()
        };

        let mut renamed = provider("office");
        renamed.base_url = "https://office.example.com".into();
        rename_provider(&mut ai, "work", renamed);

        assert_eq!(ai.providers.len(), 1);
        assert_eq!(ai.providers[0].name, "office");
        assert_eq!(ai.providers[0].base_url, "https://office.example.com");
        assert_eq!(
            ai.models.default.as_ref().unwrap().provider,
            "office",
            "the default model follows the rename"
        );
        assert_eq!(
            ai.models.lookup.as_ref().unwrap().provider,
            "office",
            "the lookup model follows the rename"
        );

        // Models on another provider are untouched.
        ai.models.lookup = Some(ModelSettings {
            provider: "other".into(),
            model: "small".into(),
            ..ModelSettings::default()
        });
        rename_provider(&mut ai, "office", provider("work"));
        assert_eq!(
            ai.models.lookup.as_ref().unwrap().provider,
            "other",
            "only models routed through the renamed provider are repointed"
        );
        assert_eq!(ai.models.default.as_ref().unwrap().provider, "work");
    }
}
