//! The Settings window's AI tab: the providers table and its edit dialog, and
//! the Models table with its model dialog (spec §10, Tasks 5–6).
//!
//! Like the other widgets this never mutates the app's settings: a dialog
//! reports a [`AiTabAction`] and `app.rs` applies it (patching the settings,
//! persisting them, and moving the keychain entry on the worker). The API key
//! itself only ever rides the Save/Test action; it is never stored in this
//! module's state beyond the dialog's frame. Model edits never touch the
//! keychain: a model only names a provider the user already configured.
//!
//! Unix-only with the rest of the AI code.

use std::collections::BTreeMap;

use eframe::egui::{self, Frame, Margin, RichText};

use crate::ai::presets::{ModelPreset, presets as model_presets};
use crate::persist::settings::{
    AiSettings, LlmApi, ModelSettings, ProviderSettings, ReasoningLevel,
};
use crate::ui::components::{Btn, BtnKind, Field, Seg, elided_galley, segmented};
use crate::ui::widgets::settings_window::ModelRole;
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

/// The context-window bounds the old settings UI's DragValue enforced; a model
/// outside them is almost certainly a typo (0, or a raw byte count).
pub const CONTEXT_RANGE: std::ops::RangeInclusive<u32> = 1_000..=2_000_000;

/// The per-response output cap bounds the old settings UI's DragValue enforced.
pub const MAX_TOKENS_RANGE: std::ops::RangeInclusive<u32> = 1..=200_000;

/// Check a model before it is written: the provider must be one the file
/// names (the router keys endpoints by name), the id non-empty, and the sizes
/// inside the bounds the old settings UI enforced.
///
/// The id itself is not a whitelist: any model the provider serves is allowed,
/// and the presets only fill it in.
pub fn validate_model(ai: &AiSettings, model: &ModelSettings) -> Result<(), String> {
    if model.provider.trim().is_empty() {
        return Err("choose a provider".to_owned());
    }
    if !ai
        .providers
        .iter()
        .any(|provider| provider.name == model.provider)
    {
        return Err(format!("provider {} is not configured", model.provider));
    }
    if model.model.trim().is_empty() {
        return Err("model id cannot be empty".to_owned());
    }
    if !CONTEXT_RANGE.contains(&model.context_window) {
        return Err(format!(
            "context window must be between {} and {} tokens",
            CONTEXT_RANGE.start(),
            CONTEXT_RANGE.end()
        ));
    }
    if !MAX_TOKENS_RANGE.contains(&model.max_tokens) {
        return Err(format!(
            "max output must be between {} and {} tokens",
            MAX_TOKENS_RANGE.start(),
            MAX_TOKENS_RANGE.end()
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

/// The configured model routed through `provider`: the default first, then
/// the lookup helper. Ignores presets.
fn routed_model(ai: &AiSettings, provider: &str) -> Option<ModelSettings> {
    [ai.models.default.as_ref(), ai.models.lookup.as_ref()]
        .into_iter()
        .flatten()
        .find(|model| model.provider == provider && !model.model.trim().is_empty())
        .cloned()
}

/// The model a Test connection probe should use for `provider` when only its
/// name is known: a configured model routed through it, else the first preset
/// its API offers.
///
/// The fallback makes a freshly added provider testable before any model
/// routes through it; `None` means the provider's API has no preset this
/// build knows (a hand-edited unsupported protocol).
pub fn test_model(ai: &AiSettings, provider: &str) -> Option<ModelSettings> {
    if let Some(model) = routed_model(ai, provider) {
        return Some(model);
    }
    let api = ai
        .providers
        .iter()
        .find(|row| row.name == provider)
        .map(|row| row.api)?;
    preset_model(provider, api)
}

/// A model asking for `api`'s first preset through `provider`, sized by that
/// preset; `None` when the API's preset list is empty.
pub fn preset_model(provider: &str, api: LlmApi) -> Option<ModelSettings> {
    let preset = model_presets(api).first()?;
    Some(ModelSettings {
        provider: provider.to_owned(),
        model: preset.id.to_owned(),
        context_window: preset.context_window,
        max_tokens: preset.max_tokens,
        reasoning: None,
    })
}

/// Fill `model`'s id and sizes from `preset`; `None` is the dialog's "Custom"
/// and keeps whatever the user typed (the id stays free text).
pub fn apply_preset(model: &mut ModelSettings, preset: Option<&ModelPreset>) {
    if let Some(preset) = preset {
        model.model = preset.id.to_owned();
        model.context_window = preset.context_window;
        model.max_tokens = preset.max_tokens;
    }
}

/// One row of the Models table: a fixed role and the model it routes to.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRowVm {
    /// The role being described.
    pub role: ModelRole,
    /// The provider name, or `—` when the role is unset.
    pub provider: String,
    /// The model id, or `not configured` when the role is unset.
    pub model: String,
    /// The context window, short (`200k`).
    pub context: String,
    /// The output cap, short (`16k`).
    pub max_out: String,
    /// The reasoning effort; `Default` when the model asks for none.
    pub reasoning: String,
}

/// The two fixed rows of the Models table, filled from `ai`. A role without a
/// model still gets a row (clicking it opens a blank dialog).
pub fn models(ai: &AiSettings) -> Vec<ModelRowVm> {
    [ModelRole::Default, ModelRole::Lookup]
        .into_iter()
        .map(|role| match role.model(ai) {
            Some(model) => ModelRowVm {
                role,
                provider: model.provider.clone(),
                model: model.model.clone(),
                context: format_tokens(model.context_window),
                max_out: format_tokens(model.max_tokens),
                reasoning: reasoning_label(model.reasoning).to_owned(),
            },
            None => ModelRowVm {
                role,
                provider: "—".to_owned(),
                model: "not configured".to_owned(),
                context: "—".to_owned(),
                max_out: "—".to_owned(),
                reasoning: "—".to_owned(),
            },
        })
        .collect()
}

/// A token count the way the mockup writes it: `200k`, `16k`, `1.05M`.
pub fn format_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        let millions = format!("{:.2}", tokens as f64 / 1_000_000.0);
        let millions = millions.trim_end_matches('0').trim_end_matches('.');
        format!("{millions}M")
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// The Reasoning column's text for `level`; `Default` means the model's own
/// effort (no `reasoning` in the settings).
pub fn reasoning_label(level: Option<ReasoningLevel>) -> &'static str {
    match level {
        None => "Default",
        Some(ReasoningLevel::Low) => "Low",
        Some(ReasoningLevel::Medium) => "Medium",
        Some(ReasoningLevel::High) => "High",
    }
}

/// The model dialog's working copy: the fields are the dialog's own, so a
/// preset fill or a typed id never touches the settings until Save.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelDialog {
    /// The role being edited.
    pub role: ModelRole,
    /// The edited provider name.
    pub provider: String,
    /// The edited model id (free text, presets only fill it).
    pub model: String,
    /// The edited context window.
    pub context_window: u32,
    /// The edited output cap.
    pub max_tokens: u32,
    /// The edited reasoning effort; `None` asks for the model's default.
    pub reasoning: Option<ReasoningLevel>,
}

impl ModelDialog {
    /// A dialog for `role`: its configured model, or a blank one routed to the
    /// first provider when the role is unset.
    pub fn new(role: ModelRole, ai: &AiSettings) -> ModelDialog {
        match role.model(ai) {
            Some(model) => ModelDialog {
                role,
                provider: model.provider.clone(),
                model: model.model.clone(),
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                reasoning: model.reasoning,
            },
            None => ModelDialog {
                role,
                provider: ai
                    .providers
                    .first()
                    .map(|provider| provider.name.clone())
                    .unwrap_or_default(),
                model: String::new(),
                context_window: ModelSettings::default().context_window,
                max_tokens: ModelSettings::default().max_tokens,
                reasoning: None,
            },
        }
    }

    /// The model the fields describe.
    pub fn settings(&self) -> ModelSettings {
        ModelSettings {
            provider: self.provider.clone(),
            model: self.model.clone(),
            context_window: self.context_window,
            max_tokens: self.max_tokens,
            reasoning: self.reasoning,
        }
    }

    /// The presets to offer: those of the selected provider's API, or none
    /// when no provider matches (the id and sizes stay free text).
    pub fn presets(&self, ai: &AiSettings) -> &'static [ModelPreset] {
        let api = ai
            .providers
            .iter()
            .find(|provider| provider.name == self.provider)
            .map(|provider| provider.api);
        api.map(model_presets).unwrap_or(&[])
    }

    /// Apply a preset choice; `None` ("Custom") keeps the typed id and sizes.
    pub fn choose_preset(&mut self, preset: Option<&ModelPreset>) {
        let mut model = self.settings();
        apply_preset(&mut model, preset);
        self.model = model.model;
        self.context_window = model.context_window;
        self.max_tokens = model.max_tokens;
    }
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
    /// The model dialog, when one is open.
    pub model_dialog: Option<ModelDialog>,
    /// The latest connection-test outcome per provider name.
    pub test_results: BTreeMap<String, Result<String, String>>,
}

impl AiTabState {
    /// Open the dialog for the provider at `index` in `ai.providers`.
    pub fn edit_provider(&mut self, ai: &AiSettings, index: usize) {
        self.dialog = ai.providers.get(index).map(ProviderDialog::for_provider);
    }

    /// Open the dialog for one model role (an unset role opens blank).
    pub fn edit_model(&mut self, ai: &AiSettings, role: ModelRole) {
        self.model_dialog = Some(ModelDialog::new(role, ai));
    }
}

/// What the AI tab asks the app to do. The app owns the settings write and
/// the keychain op; the widget never touches either.
#[derive(Clone, Debug, PartialEq)]
pub enum AiTabAction {
    /// Commit one model role (spec §10). `None` clears the role (only
    /// meaningful for the optional lookup helper); no keychain is involved.
    SaveModel {
        /// The role the dialog edited.
        role: ModelRole,
        /// The model to store, or `None` to clear the role.
        model: Option<ModelSettings>,
    },
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

/// Draw the providers table, the Models table, and, when open, a dialog.
/// Returns what the user asked for this frame.
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

    let mut open_model = None;
    models_section(ui, ai, &mut open_model);
    if let Some(role) = open_model {
        state.edit_model(ai, role);
    }

    render_dialog(ui.ctx(), state, ai, keys, &mut actions);
    render_model_dialog(ui.ctx(), state, ai, &mut actions);
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

/// The Models table's column widths: Role, Provider, Model, Context, Max out,
/// Reasoning.
const MODEL_COLUMNS: [f32; 6] = [0.23, 0.15, 0.27, 0.11, 0.12, 0.12];

/// The header row plus one clickable row per provider. Returns the row the
/// user clicked.
fn table(ui: &mut egui::Ui, rows: &[ProviderRowVm]) -> Option<usize> {
    let p = theme::palette_of(ui);
    let width = ui.available_width();
    let lefts = column_lefts(width, &COLUMNS);

    header_row(
        ui,
        width,
        &lefts,
        &["Name", "API", "Base URL", "Key", "Status"],
    );

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
fn column_lefts(width: f32, columns: &[f32]) -> Vec<f32> {
    let mut lefts = Vec::with_capacity(columns.len());
    let mut x = 6.0;
    for fraction in columns {
        lefts.push(x);
        x += width * fraction;
    }
    lefts
}

/// Paint a table's header row and its bottom rule.
fn header_row(ui: &mut egui::Ui, width: f32, lefts: &[f32], labels: &[&str]) {
    let p = theme::palette_of(ui);
    let (header, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    for (left, label) in lefts.iter().zip(labels) {
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
}

/// The Models section: a heading and the two fixed role rows. Returns the role
/// the user clicked through `open`.
fn models_section(ui: &mut egui::Ui, ai: &AiSettings, open: &mut Option<ModelRole>) {
    ui.add_space(12.0);
    let p = theme::palette_of(ui);
    ui.label(RichText::new("Models").font(fonts::semibold(13.0)));
    ui.add_space(2.0);

    let rows = models(ai);
    let width = ui.available_width();
    let lefts = column_lefts(width, &MODEL_COLUMNS);
    header_row(
        ui,
        width,
        &lefts,
        &[
            "Role",
            "Provider",
            "Model",
            "Context",
            "Max out",
            "Reasoning",
        ],
    );

    for row in &rows {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::click());
        if response.hovered() {
            ui.painter().rect_filled(rect, 4.0, p.hover);
        }
        let cell_width = |column: usize| width * MODEL_COLUMNS[column] - 8.0;
        cell(
            ui,
            rect,
            lefts[0],
            row.role.label(),
            cell_width(0),
            p.text,
            egui::FontId::proportional(12.5),
        );
        cell(
            ui,
            rect,
            lefts[1],
            &row.provider,
            cell_width(1),
            p.muted,
            egui::FontId::proportional(12.0),
        );
        cell(
            ui,
            rect,
            lefts[2],
            &row.model,
            cell_width(2),
            p.text,
            egui::FontId::proportional(12.0),
        );
        for (column, text) in [(3, &row.context), (4, &row.max_out), (5, &row.reasoning)] {
            cell(
                ui,
                rect,
                lefts[column],
                text,
                cell_width(column),
                p.muted,
                egui::FontId::proportional(12.0),
            );
        }
        ui.painter()
            .hline(rect.x_range(), rect.bottom(), (1.0, p.line_soft));

        let hover = match row.role.model(ai) {
            Some(_) => "edit model",
            None => "configure this model",
        };
        if response.on_hover_text(hover).clicked() {
            *open = Some(row.role);
        }
    }
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
                let name_edit = Field::new(&mut dialog.name).hint("name").show(ui);
                field_label(ui, "API");
                let api_before = dialog.api;
                api_combo(ui, &mut dialog.api);
                let api_changed = dialog.api != api_before;
                field_label(ui, "Base URL (optional)");
                let base_url_edit = Field::new(&mut dialog.base_url)
                    .icon(icons::GLOBE)
                    .hint("vendor default")
                    .show(ui);

                let stored = keys
                    .get(dialog.original.as_deref().unwrap_or(&dialog.name))
                    .copied()
                    .unwrap_or(false);
                field_label(ui, "API key");
                let key_edit = Field::new(&mut dialog.key_input)
                    .icon(icons::KEY_ROUND)
                    .password(true)
                    .hint(if stored {
                        "stored in keychain — type a new key to replace"
                    } else {
                        "stored in the keychain, never in settings"
                    })
                    .show(ui);
                if name_edit.changed()
                    || api_changed
                    || base_url_edit.changed()
                    || key_edit.changed()
                {
                    // The refusal (e.g. Delete blocked by a model) named the
                    // values as they were; any edit makes that message stale.
                    dialog.error = None;
                }

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
                    .and_then(|original| routed_model(ai, original))
                    .or_else(|| routed_model(ai, &dialog.name))
                    // A provider no model routes through yet (or a new row the
                    // settings do not name) is still testable with the API the
                    // dialog is editing, not a stale stored one (Task 6).
                    .or_else(|| preset_model(&dialog.name, dialog.api))
                    .filter(|_| !dialog.name.trim().is_empty());
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
                                dialog.error = None;
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
                                    "this provider's API has no preset model to test"
                                } else if dialog.name.trim().is_empty() {
                                    "name the provider first"
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

/// The model dialog (mockup note: "Models use the same pattern"), shown only
/// while [`AiTabState::model_dialog`] is set.
fn render_model_dialog(
    ctx: &egui::Context,
    state: &mut AiTabState,
    ai: &AiSettings,
    actions: &mut Vec<AiTabAction>,
) {
    if state.model_dialog.is_none() {
        return;
    }
    let p = theme::palette(ctx.theme() == egui::Theme::Dark);
    let mut close = false;
    let mut outcome = None;
    if let Some(dialog) = state.model_dialog.as_mut() {
        let title = format!("{} model", dialog.role.label());
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icons::icon_button(ui, icons::X, "Close  Esc").clicked() {
                            close = true;
                        }
                    });
                });
                rule(ui);

                field_label(ui, "Provider");
                provider_combo(ui, &mut dialog.provider, ai);
                field_label(ui, "Preset");
                preset_combo(ui, dialog, ai);
                field_label(ui, "Model id");
                Field::new(&mut dialog.model)
                    .hint("model id — any the provider serves")
                    .show(ui);
                field_label(ui, "Context window");
                ui.add(
                    egui::DragValue::new(&mut dialog.context_window)
                        .speed(1_000.0)
                        .range(CONTEXT_RANGE),
                );
                field_label(ui, "Max output");
                ui.add(
                    egui::DragValue::new(&mut dialog.max_tokens)
                        .speed(128.0)
                        .range(MAX_TOKENS_RANGE),
                );
                field_label(ui, "Reasoning");
                let levels: [Seg<'_, Option<ReasoningLevel>>; 4] = [
                    Seg::text(None, "Model default"),
                    Seg::text(Some(ReasoningLevel::Low), "Low"),
                    Seg::text(Some(ReasoningLevel::Medium), "Medium"),
                    Seg::text(Some(ReasoningLevel::High), "High"),
                ];
                if let Some(level) = segmented(ui, dialog.reasoning, &levels) {
                    dialog.reasoning = level;
                }

                let candidate = dialog.settings();
                let validation = validate_model(ai, &candidate);
                if let Err(error) = &validation {
                    ui.label(RichText::new(error).size(12.0).color(p.bad));
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    // The lookup helper is optional: Remove clears the role.
                    if dialog.role == ModelRole::Lookup && dialog.role.model(ai).is_some() {
                        let remove = Btn::new(BtnKind::Danger, "Remove")
                            .show(ui)
                            .on_hover_text("the lookup helper is optional");
                        if remove.clicked() {
                            outcome = Some(AiTabAction::SaveModel {
                                role: dialog.role,
                                model: None,
                            });
                            close = true;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let save = Btn::new(BtnKind::Primary, "Save")
                            .enabled(validation.is_ok())
                            .show(ui);
                        let save = if validation.is_ok() {
                            save
                        } else {
                            save.on_hover_text("fix the model before saving")
                        };
                        if save.clicked() {
                            outcome = Some(AiTabAction::SaveModel {
                                role: dialog.role,
                                model: Some(candidate.clone()),
                            });
                            close = true;
                        }
                        if Btn::new(BtnKind::Default, "Cancel").show(ui).clicked() {
                            close = true;
                        }
                    });
                });
            });
    }
    if close {
        state.model_dialog = None;
    }
    if let Some(action) = outcome {
        actions.push(action);
    }
}

/// The model dialog's provider dropdown: the providers the file names.
fn provider_combo(ui: &mut egui::Ui, provider: &mut String, ai: &AiSettings) {
    let p = theme::palette_of(ui);
    let selected = if provider.trim().is_empty() {
        "provider".to_owned()
    } else {
        provider.clone()
    };
    egui::ComboBox::from_id_salt("model-provider")
        .width(200.0)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            if ai.providers.is_empty() {
                ui.label(RichText::new("no providers yet").color(p.muted));
            }
            for row in &ai.providers {
                if ui
                    .selectable_label(*provider == row.name, &row.name)
                    .clicked()
                {
                    *provider = row.name.clone();
                }
            }
        });
}

/// The preset dropdown: the selected provider's API presets plus "Custom",
/// which keeps the typed id and sizes.
fn preset_combo(ui: &mut egui::Ui, dialog: &mut ModelDialog, ai: &AiSettings) {
    let list = dialog.presets(ai);
    let selected = list.iter().find(|preset| preset.id == dialog.model);
    let label = selected.map(|preset| preset.label).unwrap_or("Custom");
    let mut choice: Option<Option<&'static ModelPreset>> = None;
    egui::ComboBox::from_id_salt("model-preset")
        .width(200.0)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for preset in list {
                let on = selected.map(|chosen| chosen.id) == Some(preset.id);
                if ui.selectable_label(on, preset.label).clicked() {
                    choice = Some(Some(preset));
                }
            }
            if ui.selectable_label(selected.is_none(), "Custom").clicked() {
                choice = Some(None);
            }
        });
    match choice {
        Some(Some(preset)) => dialog.choose_preset(Some(preset)),
        Some(None) => dialog.choose_preset(None),
        None => {}
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
    use crate::ai::presets::presets;
    use crate::persist::settings::{LlmApi, ModelSettings, ProviderSettings, ReasoningLevel};
    use crate::ui::widgets::settings_window::ModelRole;

    fn provider(name: &str) -> ProviderSettings {
        ProviderSettings {
            name: name.to_owned(),
            api: LlmApi::AnthropicMessages,
            ..ProviderSettings::default()
        }
    }

    /// One provider called `work` and nothing else configured.
    fn ai_with_provider() -> AiSettings {
        AiSettings {
            providers: vec![provider("work")],
            ..AiSettings::default()
        }
    }

    /// The table always shows both fixed roles; an unset role keeps its row as
    /// a placeholder, so clicking it can configure it.
    #[test]
    fn models_table_shows_both_roles_and_placeholders() {
        let mut ai = ai_with_provider();
        let rows = models(&ai);
        assert_eq!(rows.len(), 2, "one row per role");
        assert_eq!(rows[0].role, ModelRole::Default);
        assert_eq!(rows[0].role.label(), "Review & chat");
        assert_eq!(rows[0].provider, "—");
        assert_eq!(rows[0].model, "not configured");
        assert_eq!(rows[0].context, "—");
        assert_eq!(rows[0].max_out, "—");
        assert_eq!(rows[0].reasoning, "—");

        ai.models.default = Some(ModelSettings {
            provider: "work".into(),
            model: "claude-sonnet-5-5".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            reasoning: Some(ReasoningLevel::Medium),
        });
        ai.models.lookup = Some(ModelSettings {
            provider: "work".into(),
            model: "claude-haiku-5-5".into(),
            context_window: 1_000_000,
            max_tokens: 8_192,
            reasoning: None,
        });
        let rows = models(&ai);
        assert_eq!(rows[0].provider, "work");
        assert_eq!(rows[0].model, "claude-sonnet-5-5");
        assert_eq!(
            rows[0].context, "200k",
            "sizes read as the mockup writes them"
        );
        assert_eq!(rows[0].max_out, "16k");
        assert_eq!(rows[0].reasoning, "Medium");
        assert_eq!(rows[1].role.label(), "Lookup helper");
        assert_eq!(rows[1].context, "1M");
        assert_eq!(rows[1].max_out, "8k");
        assert_eq!(
            rows[1].reasoning, "Default",
            "unset reasoning means the model's own effort"
        );
    }

    /// Saving one role patches only that role; `None` clears the lookup
    /// helper and leaves the review model alone.
    #[test]
    fn set_model_patches_only_the_chosen_role() {
        let mut ai = ai_with_provider();
        let main = ModelSettings {
            provider: "work".into(),
            model: "big".into(),
            ..ModelSettings::default()
        };
        let helper = ModelSettings {
            provider: "work".into(),
            model: "small".into(),
            ..ModelSettings::default()
        };
        ModelRole::Default.set_model(&mut ai, Some(main.clone()));
        ModelRole::Lookup.set_model(&mut ai, Some(helper.clone()));
        assert_eq!(ai.models.default.as_ref(), Some(&main));
        assert_eq!(ai.models.lookup.as_ref(), Some(&helper));

        ModelRole::Lookup.set_model(&mut ai, None);
        assert_eq!(ai.models.lookup, None);
        assert_eq!(
            ai.models.default.as_ref(),
            Some(&main),
            "clearing the helper leaves the review model alone"
        );
    }

    /// A model must route through a provider the file names: the write keys
    /// the endpoint by name, and the router would have nowhere to send it.
    #[test]
    fn validate_model_requires_an_existing_provider() {
        let ai = ai_with_provider();
        let valid = ModelSettings {
            provider: "work".into(),
            model: "claude-sonnet-5-5".into(),
            ..ModelSettings::default()
        };
        assert!(
            validate_model(&ai, &valid).is_ok(),
            "a routed model is valid"
        );

        let mut unknown = valid.clone();
        unknown.provider = "nowhere".into();
        let error = validate_model(&ai, &unknown).unwrap_err();
        assert!(error.contains("nowhere"), "names the provider: {error}");

        let mut empty_provider = valid.clone();
        empty_provider.provider.clear();
        let error = validate_model(&ai, &empty_provider).unwrap_err();
        assert!(error.contains("provider"), "asks for a provider: {error}");

        let mut no_model = valid.clone();
        no_model.model = "  ".into();
        let error = validate_model(&ai, &no_model).unwrap_err();
        assert!(error.contains("model id"), "asks for a model id: {error}");

        // The size ranges the old settings UI enforced (its DragValues).
        let mut tiny = valid.clone();
        tiny.context_window = 999;
        assert!(
            validate_model(&ai, &tiny).is_err(),
            "a context window below 1k is refused"
        );

        let mut huge = valid.clone();
        huge.context_window = 2_000_001;
        assert!(
            validate_model(&ai, &huge).is_err(),
            "a context window above 2M is refused"
        );

        let mut no_output = valid.clone();
        no_output.max_tokens = 0;
        assert!(
            validate_model(&ai, &no_output).is_err(),
            "an output cap of 0 is refused"
        );

        let mut wide_output = valid.clone();
        wide_output.max_tokens = 200_001;
        assert!(
            validate_model(&ai, &wide_output).is_err(),
            "an output cap above 200k is refused"
        );
    }

    /// A preset is a convenience: it fills the id and the published sizes, but
    /// "Custom" leaves whatever the user typed alone (the id is never a
    /// whitelist).
    #[test]
    fn choosing_a_preset_fills_sizes_and_keeps_custom_free_text() {
        let mut model = ModelSettings {
            provider: "work".into(),
            model: "my-local-finetune".into(),
            context_window: 32_768,
            max_tokens: 4_096,
            reasoning: Some(ReasoningLevel::Low),
        };

        // "Custom" keeps the typed id, sizes and reasoning.
        apply_preset(&mut model, None);
        assert_eq!(model.model, "my-local-finetune");
        assert_eq!(model.context_window, 32_768);
        assert_eq!(model.max_tokens, 4_096);
        assert_eq!(model.reasoning, Some(ReasoningLevel::Low));

        // A preset fills the id and its published sizes.
        let preset = presets(LlmApi::AnthropicMessages)
            .iter()
            .find(|preset| preset.id == "claude-sonnet-5-5")
            .expect("the Anthropic list has Sonnet 5.5");
        apply_preset(&mut model, Some(preset));
        assert_eq!(model.model, "claude-sonnet-5-5");
        assert_eq!(model.context_window, preset.context_window);
        assert_eq!(model.max_tokens, preset.max_tokens);
        assert_eq!(
            model.reasoning,
            Some(ReasoningLevel::Low),
            "a preset is about the id and sizes, not the reasoning effort"
        );

        // The id stays free text: a typed-over id survives "Custom" (the
        // dialog then shows it as Custom, not as the preset it resembles).
        model.model = "claude-sonnet-5-5-20261001".into();
        apply_preset(&mut model, None);
        assert_eq!(model.model, "claude-sonnet-5-5-20261001");

        // The dialog starts blank for an empty role but uses the provider's
        // API presets.
        let ai = ai_with_provider();
        let mut dialog = ModelDialog::new(ModelRole::Lookup, &ai);
        assert_eq!(dialog.provider, "work", "the only provider is preselected");
        assert_eq!(dialog.model, "", "an empty role starts blank");
        assert_eq!(dialog.presets(&ai), presets(LlmApi::AnthropicMessages));
        dialog.choose_preset(Some(preset));
        assert_eq!(dialog.model, "claude-sonnet-5-5");
        assert_eq!(dialog.context_window, preset.context_window);
        assert_eq!(dialog.max_tokens, preset.max_tokens);
    }

    /// A provider no model routes through can still be tested: the probe asks
    /// for the API's first preset (a routed model always wins).
    #[test]
    fn test_model_prefers_a_routed_model_then_a_preset() {
        let mut ai = ai_with_provider();
        let preset = &presets(LlmApi::AnthropicMessages)[0];
        let probe = test_model(&ai, "work").expect("the API has a preset to probe");
        assert_eq!(probe.model, preset.id);
        assert_eq!(probe.provider, "work");

        ai.models.default = Some(ModelSettings {
            provider: "work".into(),
            model: "routed-id".into(),
            ..ModelSettings::default()
        });
        assert_eq!(test_model(&ai, "work").unwrap().model, "routed-id");

        assert_eq!(
            test_model(&ai, "nowhere"),
            None,
            "an unknown provider has nothing to probe"
        );
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
