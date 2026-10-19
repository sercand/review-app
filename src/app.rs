//! The application shell: screen routing, the egui event loop, and startup.
//!
//! The UI thread never blocks on I/O. It drains [`Event`]s once per frame,
//! routes them into the current screen, and renders. Everything network-shaped
//! goes to the worker (see [`crate::runtime::execute`]), which wakes the UI via
//! `EventSender::with_repaint`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use eframe::egui;

use crate::auth::device_flow::{self, AuthFailure, AuthState};
use crate::auth::session::{complete_login, handle_unauthorized, restore_token, sign_out};
use crate::auth::store::{KeyringStore, TokenStore};
use crate::auth::DEFAULT_CLIENT_ID;
use crate::diff::highlight::{HighlightCache, SyntectHighlighter};
use crate::diff::row_model::RowRef;
use crate::github::client::{GithubClient, Me};
use crate::github::error::GhError;
use crate::model::comment::CommentTarget;
use crate::model::review::ReviewEvent;
use crate::model::RepoRef;
use crate::persist::config_root;
use crate::persist::session::DebouncedWriter;
use crate::persist::settings::Settings;
use crate::runtime::{
    event_channel, execute, spawn_worker, Command, Event, EventReceiver, ExecCtx, SessionId, Worker,
};
use crate::state::composer::{self, ComposerAction};
use crate::state::pr_detail::{start_submit, viewer_is_author, PrSession};
use crate::state::pr_list::{load_more, reduce, PrListMsg, PrListState};
use crate::ui::command::{self, next_index, KeyPress};
use crate::ui::screens::login::{self, LoginAction, LoginForm};
use crate::ui::screens::pr_detail::{self, PrDetailAction, PrDetailState};
use crate::ui::screens::pr_list::{self, PrListAction, PrListForm};
use crate::ui::theme;
use crate::ui::widgets::command_palette::{self, PaletteState};
use crate::ui::widgets::diff_view::{DiffAction, DiffMode};
use crate::ui::widgets::review_panel::ReviewAction;
use crate::ui::widgets::settings_window::{self, SettingsAction, SettingsWindowState};
use crate::ui::widgets::thread_view::ThreadAction;
use crate::ui::widgets::toast::Toasts;

/// The three screens. Settings is a modal window, not a fourth screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenKind {
    /// Sign in.
    Login,
    /// The list of open PRs.
    PrList,
    /// One open PR.
    PrDetail,
}

/// The screen to show at startup.
///
/// A restored token means the app can go straight to the PR list; otherwise the
/// user signs in first.
pub fn initial_screen(has_token: bool) -> ScreenKind {
    if has_token {
        ScreenKind::PrList
    } else {
        ScreenKind::Login
    }
}

/// Fold an event into the current screen.
///
/// Only auth-shaped events move the user between screens: a 401 anywhere drops
/// back to the login screen, and a successful sign-in goes to the PR list.
/// Everything else leaves the current screen alone.
pub fn route(current: ScreenKind, event: &Event) -> ScreenKind {
    match event {
        Event::Failed {
            error: GhError::Unauthorized,
            ..
        } => ScreenKind::Login,
        Event::PrSearchFailed {
            error: GhError::Unauthorized,
            ..
        } => ScreenKind::Login,
        Event::CommentFailed {
            error: GhError::Unauthorized,
            ..
        } => ScreenKind::Login,
        Event::ReviewSubmitFailed {
            error: GhError::Unauthorized,
            ..
        } => ScreenKind::Login,
        Event::LoggedIn { .. } => ScreenKind::PrList,
        _ => current,
    }
}

/// Whether the global key path may dispatch `action` while a text edit has
/// keyboard focus.
///
/// `submit_review` is the gated one. `resolve_key` deliberately resolves
/// `Cmd+Enter` even with text focus — the review panel's own scoped handler
/// (gated on focus in one of its text edits) depends on that — so without
/// this gate the global path fires alongside the panel's handler and posts
/// the review twice in one frame, and `Cmd+Enter` while typing a thread
/// reply or a composer body submits the draft review by accident. Every
/// other action either needs no focus (`palette`, `close`) or is already
/// suppressed by `resolve_key` under text focus.
fn global_key_allowed(action: &str, text_focused: bool) -> bool {
    action != "submit_review" || !text_focused
}

/// Whether `action` may run while `screen` is showing.
///
/// The palette, help, close, and settings actions are global. Everything
/// else in the registry acts on the open PR — and after Back the session
/// stays loaded for a fast return, so running a detail action from the list
/// would edit (and persist marks for) a PR the reviewer cannot see.
fn action_allowed(action: &str, screen: ScreenKind) -> bool {
    match action {
        "palette" | "settings" | "help" | "close" => true,
        _ => screen == ScreenKind::PrDetail,
    }
}

/// Whether a PR-scoped 404 means the *open* session is dead and must close
/// (spec §13: "PR/repo gone or inaccessible; close the session with a
/// toast"). A 404 for a session the app no longer holds — or one that was
/// never PR-scoped — closes nothing.
fn pr_is_gone(event: &Event, open: Option<SessionId>) -> bool {
    match event {
        Event::Failed {
            session: Some(session),
            error: GhError::NotFound,
        } => Some(*session) == open,
        _ => false,
    }
}

/// The write-blocked state for one frame (spec §13: "disable write actions
/// until reset"): `Some(label)` while a rate limit's reset is still in the
/// future — the label is the button tooltip — and `None` once it has passed,
/// which also clears the flag for good.
fn write_blocked_label(
    until: Option<SystemTime>,
    now: SystemTime,
) -> (Option<String>, Option<SystemTime>) {
    let Some(reset_at) = until else {
        return (None, None);
    };
    match reset_at.duration_since(now) {
        Ok(remaining) => (
            Some(format!(
                "GitHub is rate-limiting writes — try again in ~{}s",
                remaining.as_secs()
            )),
            until,
        ),
        Err(_) => (None, None),
    }
}

/// The error an event carries, if it carries one. Every such event toasts
/// it, so this is where the app notices a write-blocking failure.
fn event_error(event: &Event) -> Option<&GhError> {
    match event {
        Event::Failed { error, .. }
        | Event::PrSearchFailed { error, .. }
        | Event::CommentFailed { error, .. }
        | Event::ReviewSubmitFailed { error, .. } => Some(error),
        _ => None,
    }
}

/// The login of the cached account, or the empty string when signed out.
fn account_login(settings: &Settings) -> String {
    settings
        .account
        .as_ref()
        .map(|account| account.login.clone())
        .unwrap_or_default()
}

/// The root app: owns settings, the worker bridge, and every piece of UI state.
pub struct ReviewApp {
    screen: ScreenKind,
    auth_state: AuthState,
    login_form: LoginForm,
    pr_list: PrListState,
    pr_list_form: PrListForm,
    pr_detail: PrDetailState,
    highlight_cache: HighlightCache,
    session: Option<PrSession>,
    next_session: u64,
    toasts: Toasts,
    palette: PaletteState,
    help_open: bool,
    settings_window: SettingsWindowState,
    settings_writer: DebouncedWriter,
    applied_theme: Option<egui::Theme>,
    settings: Settings,
    /// Spec §13: while this instant is in the future, GitHub is rate-limiting
    /// the account and the write surfaces (review-panel and composer Submit)
    /// are disabled. Set when a `RateLimited` error surfaces, cleared once
    /// the instant passes (checked every frame).
    write_blocked_until: Option<SystemTime>,
    config_dir: PathBuf,
    store: Arc<dyn TokenStore>,
    exec: Arc<ExecCtx>,
    worker: Worker,
    events: EventReceiver,
}

impl ReviewApp {
    /// Build the app: load settings, restore any token, and start the worker.
    pub fn new(cc: &eframe::CreationContext<'_>) -> ReviewApp {
        // The image diff viewer draws through `egui::Image` (Task 49); the
        // supported formats are the ones enabled on the `image` dependency.
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Plex + JetBrains Mono + the Lucide icon font, before the first frame.
        cc.egui_ctx.set_fonts(crate::ui::fonts::font_definitions());
        // Register both themed visuals and styles once so `apply_theme`'s
        // `ctx.set_theme` switches between the mockup's dark and light looks.
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Dark, theme::visuals(&theme::DARK, true));
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Light, theme::visuals(&theme::LIGHT, false));
        cc.egui_ctx
            .set_style_of(egui::Theme::Dark, theme::style(egui::Style::default()));
        cc.egui_ctx
            .set_style_of(egui::Theme::Light, theme::style(egui::Style::default()));

        let config_dir = config_root();
        let settings = Settings::load_or_default(&config_dir);
        let store: Arc<dyn TokenStore> = Arc::new(KeyringStore);

        let restored = match restore_token(store.as_ref(), &settings) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "could not read the stored token");
                None
            }
        };

        let exec = Arc::new(ExecCtx::new(store.clone()));
        if let Some(token) = &restored {
            exec.install_client(GithubClient::new(&settings.host, token));
        }

        let (events, receiver) = event_channel();
        let egui_ctx = cc.egui_ctx.clone();
        let events = events.with_repaint(Arc::new(move || egui_ctx.request_repaint()));

        let handler_ctx = exec.clone();
        let worker = spawn_worker(events, move |command, events| {
            let ctx = handler_ctx.clone();
            async move { execute(ctx, command, events).await }
        });

        // The list starts on the saved default scopes, and a restored token
        // means its first search can go out before the first frame.
        let mut pr_list = PrListState::default();
        pr_list.scopes = settings.default_scopes.clone();
        if restored.is_some() {
            let login = account_login(&settings);
            for command in reduce(&mut pr_list, PrListMsg::Refresh, &login) {
                worker.send(command);
            }
        }

        ReviewApp {
            screen: initial_screen(restored.is_some()),
            auth_state: AuthState::Idle,
            login_form: LoginForm::for_settings(&settings),
            pr_list,
            pr_list_form: PrListForm::default(),
            pr_detail: PrDetailState::default(),
            highlight_cache: HighlightCache::new(2048, Box::new(SyntectHighlighter::new(true))),
            session: None,
            next_session: 1,
            toasts: Toasts::default(),
            palette: PaletteState::default(),
            help_open: false,
            settings_window: SettingsWindowState::default(),
            settings_writer: DebouncedWriter::default(),
            applied_theme: None,
            settings,
            write_blocked_until: None,
            config_dir,
            store,
            exec,
            worker,
            events: receiver,
        }
    }

    /// Apply one event to the app state.
    fn handle_event(&mut self, event: Event, now: Instant) {
        let previous = self.screen;
        self.screen = route(self.screen, &event);
        self.apply_session_event(&event);
        // Spec §13: a 404 on the open PR means it is gone or inaccessible;
        // the toast below is the notice and the session closes with it.
        let pr_gone = pr_is_gone(&event, self.session.as_ref().map(|session| session.id));
        if let Some(error) = event_error(&event) {
            self.note_write_blocked(error);
        }

        match event {
            Event::Pong(_) => {}
            Event::Toast(message) => self.toasts.push(message, now),
            Event::WorkerPanicked(message) => {
                tracing::error!(%message, "worker task panicked");
                self.toasts.push(format!("internal error: {message}"), now);
            }
            Event::DeviceCode(code) => self.auth_state = device_flow::start(code),
            Event::AuthFailed(failure) => self.auth_state = AuthState::Failed(failure),
            Event::LoggedIn { me, token } => self.finish_login(&me, &token, now),
            Event::Failed {
                error: GhError::Unauthorized,
                ..
            } => self.on_unauthorized(now),
            Event::Failed { error, .. } => {
                self.toasts.push(error.to_string(), now);
                if pr_gone {
                    // Spec §13: the PR is gone; flush what the session still
                    // owes, drop it, and fall back to the list rather than
                    // leave the screen open on a dead PR.
                    self.flush_session();
                    self.session = None;
                    if self.screen == ScreenKind::PrDetail {
                        self.screen = ScreenKind::PrList;
                    }
                }
            }
            Event::PrSearchPage {
                request,
                query,
                page,
            } => self.apply_pr_list(PrListMsg::PageLoaded {
                request,
                query,
                page,
            }),
            Event::PrSearchFailed { request, error } => {
                let unauthorized = matches!(error, GhError::Unauthorized);
                self.apply_pr_list(PrListMsg::LoadFailed { request, error });
                if unauthorized {
                    // Spec §6: a 401 anywhere ends the session.
                    self.on_unauthorized(now);
                }
            }
            // Folded into the open session by `apply_session_event` above:
            // it replaces the optimistic bubble with the real comment,
            // appends replies, and confirms resolved states there.
            Event::PrFilesPage { .. } => {}
            Event::PrMetadataLoaded { .. } => {}
            Event::ThreadsLoaded { .. } => {}
            Event::ReviewsLoaded { .. } => {}
            Event::ServerPendingResolved { .. } => {}
            // The session folds both in through `apply_session_event`:
            // success clears the draft and reloads threads and reviews,
            // failure keeps the draft and records the error. The toast
            // covers the failure surface.
            Event::ReviewSubmitted { .. } => {}
            Event::ReviewSubmitFailed { error, .. } => {
                let unauthorized = matches!(error, GhError::Unauthorized);
                self.toasts.push(error.to_string(), now);
                if unauthorized {
                    // Spec §6: a 401 anywhere ends the session; the draft
                    // survives on disk (Review Focus #5).
                    self.on_unauthorized(now);
                }
            }
            Event::CommentPosted { .. } => {}
            Event::ReplyPosted { .. } => {}
            Event::ThreadResolved { .. } => {}
            // The session caches the blob and applies the waiting expansion
            // through `apply_session_event` above; nothing else to do here.
            Event::HeadBlobLoaded { .. } => {}
            // The session installs the local diff and caches the merge base
            // through `apply_session_event` above.
            Event::LocalDiffReady { .. } => {}
            // The session stores both sides in its image LRU through
            // `apply_session_event` above; evictions are drained after the
            // detail screen paints (Task 49).
            Event::ImagesLoaded { .. } => {}
            // The session caches the commits, check runs, and combined
            // status through `apply_session_event` above (Task 50).
            Event::CommitsLoaded { .. } => {}
            Event::ChecksLoaded { .. } => {}
            // The session folds the conversation into its timeline input
            // through `apply_session_event` above (Task 51).
            Event::ConversationLoaded { .. } => {}
            // The session also rolls a failed post back into the composer;
            // the toast covers the reply case, which has no other surface.
            Event::CommentFailed { error, .. } => {
                let unauthorized = matches!(error, GhError::Unauthorized);
                self.toasts.push(error.to_string(), now);
                if unauthorized {
                    // Spec §6: a 401 anywhere ends the session.
                    self.on_unauthorized(now);
                }
            }
        }

        // Entering the list — a restored token or a fresh sign-in — starts its
        // first search.
        if self.screen == ScreenKind::PrList && previous != ScreenKind::PrList {
            self.apply_pr_list(PrListMsg::Refresh);
        }
    }

    /// Fold a PR-scoped event into the open session and send its commands.
    ///
    /// The session drops events that are not its own, so a reply for a PR the
    /// user has left never reaches the screen now showing.
    fn apply_session_event(&mut self, event: &Event) {
        let Some(session) = &mut self.session else {
            return;
        };
        for command in session.on_event(event) {
            self.worker.send(command);
        }
    }

    /// The signed-in login, or the empty string before sign-in.
    fn login(&self) -> String {
        account_login(&self.settings)
    }

    /// Fold one PR-list message in and send whatever commands it produced.
    fn apply_pr_list(&mut self, msg: PrListMsg) {
        let login = self.login();
        for command in reduce(&mut self.pr_list, msg, &login) {
            self.worker.send(command);
        }
    }

    /// Drop the loaded rows, so one account's PRs never show under another's.
    ///
    /// Sign-out and a settings host change call this.
    fn reset_pr_list(&mut self) {
        self.pr_list = PrListState::default();
        self.pr_list.scopes = self.settings.default_scopes.clone();
        self.pr_list_form = PrListForm::default();
    }

    /// Write any debounced session change before the session is dropped or
    /// replaced (spec §11): a viewed mark, collapse, or composer draft made
    /// inside the debounce window must not die with the session.
    fn flush_session(&mut self) {
        if let Some(session) = &mut self.session {
            session.flush_session();
        }
    }

    /// Spec §13: a rate limit names the instant writes may resume; remember
    /// it so the write surfaces disable themselves. A rate limit without a
    /// reset instant blocks nothing (there is no window to count down), and
    /// a 401 ends the session through the auth path instead of a timer.
    fn note_write_blocked(&mut self, error: &GhError) {
        if !error.is_write_blocking() {
            return;
        }
        if let GhError::RateLimited {
            reset_at: Some(reset_at),
        } = error
        {
            self.write_blocked_until = Some(*reset_at);
        }
    }

    /// Leave the detail screen for the list, flushing the session first:
    /// it stays loaded for a fast return, but its owed writes do not wait.
    fn close_detail(&mut self) {
        self.flush_session();
        self.screen = ScreenKind::PrList;
    }

    /// Store the token and cache the account for the next launch.
    fn finish_login(&mut self, me: &Me, token: &str, now: Instant) {
        match complete_login(self.store.as_ref(), &mut self.settings, me, token) {
            Ok(()) => {
                self.save_settings(now);
                self.toasts
                    .push(format!("signed in as {}", me.user.login), now);
            }
            Err(error) => {
                tracing::warn!(%error, "could not finish login bookkeeping");
                self.toasts
                    .push(format!("could not save the login: {error}"), now);
            }
        }
        self.auth_state = AuthState::Idle;
    }

    /// A 401: drop the token and the client, keep the account for prefill.
    fn on_unauthorized(&mut self, now: Instant) {
        if let Err(error) = handle_unauthorized(self.store.as_ref(), &self.settings) {
            tracing::warn!(%error, "could not clear the stored token");
        }
        self.exec.clear_client();
        self.flush_session();
        self.session = None;
        // `AuthFailure::Http` carries a user-facing message verbatim, which is
        // how the login screen shows the session-expiry notice (spec §6).
        self.auth_state = AuthState::Failed(AuthFailure::Http(
            "your session expired, please sign in again".to_owned(),
        ));
        self.toasts
            .push("your session expired, please sign in again".to_owned(), now);
    }

    /// Sign out: clear the keychain entry and the cached account.
    ///
    /// Reached from the settings window's Sign out button (Task 53).
    fn perform_sign_out(&mut self, now: Instant) {
        match sign_out(self.store.as_ref(), &mut self.settings) {
            Ok(()) => {
                self.exec.clear_client();
                self.save_settings(now);
                self.auth_state = AuthState::Idle;
                self.screen = ScreenKind::Login;
                self.reset_pr_list();
                self.flush_session();
                self.session = None;
                self.toasts.push("signed out".to_owned(), now);
            }
            Err(error) => {
                tracing::warn!(%error, "could not sign out");
                self.toasts
                    .push(format!("could not sign out: {error}"), now);
            }
        }
    }

    /// Persist settings, reporting a failure as a toast.
    fn save_settings(&mut self, now: Instant) {
        if let Err(error) = self.settings.save(&self.config_dir) {
            tracing::warn!(%error, "could not save settings");
            self.toasts
                .push(format!("could not save settings: {error}"), now);
        }
    }

    /// Open the settings window on a fresh copy of the current settings.
    fn open_settings(&mut self) {
        self.settings_window.open_with(&self.settings);
    }

    /// Fold the settings window's actions in: Save persists (and a host
    /// change ends the session, since the stored token belongs to the old
    /// host), and Sign out rides the existing sign-out flow (RULING R10).
    fn apply_settings_actions(&mut self, actions: Vec<SettingsAction>, now: Instant) {
        for action in actions {
            match action {
                SettingsAction::Apply(vm) => {
                    let host_changed = vm.apply(&mut self.settings);
                    self.save_settings(now);
                    if host_changed {
                        self.exec.clear_client();
                        self.flush_session();
                        self.session = None;
                        self.screen = ScreenKind::Login;
                        self.reset_pr_list();
                        // The login screen's fields must show the host the
                        // user just chose, not the one from startup.
                        self.login_form.host_input = self.settings.host.clone();
                        self.login_form.client_id_input =
                            self.settings.client_id.clone().unwrap_or_default();
                        self.toasts
                            .push("host changed, please sign in again".to_owned(), now);
                    }
                }
                SettingsAction::SignOut => self.perform_sign_out(now),
            }
        }
    }

    /// Apply the theme preference against the OS's current light/dark flag.
    ///
    /// `ctx.system_theme()` keeps reporting the OS value after an override,
    /// so `System` follows a mid-session OS switch; the other preferences
    /// ignore it.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let system_dark = ctx.system_theme() == Some(egui::Theme::Dark);
        let desired = self.settings.theme.resolve(system_dark);
        if self.applied_theme != Some(desired) {
            ctx.set_theme(desired);
            self.highlight_cache.set_theme(desired == egui::Theme::Dark);
            self.applied_theme = Some(desired);
        }
    }

    /// Feed the detail screen's pane geometry back into the settings and
    /// persist it through the debounce (Task 53).
    fn sync_layout(&mut self, now: Instant) {
        if self.screen == ScreenKind::PrDetail {
            let mut dirty = false;
            let left = self.pr_detail.left_pane;
            if (left - self.settings.layout.left_pane).abs() > 0.5 {
                self.settings.layout.left_pane = left;
                dirty = true;
            }
            if self.pr_detail.right_open {
                let right = self.pr_detail.right_pane;
                if (right - self.settings.layout.right_pane).abs() > 0.5 {
                    self.settings.layout.right_pane = right;
                    dirty = true;
                }
            }
            if self.pr_detail.right_open != self.settings.layout.right_open {
                self.settings.layout.right_open = self.pr_detail.right_open;
                dirty = true;
            }
            if dirty {
                self.settings_writer.mark_dirty(now);
            }
        }
        if self.settings_writer.should_flush(now) {
            self.settings_writer.flushed();
            self.save_settings(now);
        }
    }

    /// Turn the login screen's actions into commands and settings changes.
    fn apply_login_actions(
        &mut self,
        actions: Vec<LoginAction>,
        ctx: &egui::Context,
        now: Instant,
    ) {
        for action in actions {
            match action {
                LoginAction::StartDeviceFlow => {
                    let host = self.settings.host.clone();
                    let client_id = self
                        .settings
                        .client_id
                        .clone()
                        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
                    self.worker
                        .send(Command::StartDeviceFlow { host, client_id });
                }
                LoginAction::SubmitPat(token) => {
                    if token.is_empty() {
                        self.toasts.push("paste a token first".to_owned(), now);
                    } else {
                        let host = self.settings.host.clone();
                        self.worker.send(Command::SubmitPat { host, token });
                    }
                }
                LoginAction::SetHost(host) => {
                    self.settings.host = host;
                    self.login_form.host_input = self.settings.host.clone();
                    self.save_settings(now);
                }
                LoginAction::SetClientId(client_id) => {
                    self.login_form.client_id_input = client_id.clone();
                    self.settings.client_id = Some(client_id);
                    self.save_settings(now);
                }
                LoginAction::CopyCode => {
                    if let AuthState::Polling { code, .. } = &self.auth_state {
                        ctx.copy_text(code.user_code.clone());
                    }
                }
                LoginAction::OpenBrowser => {
                    if let AuthState::Polling { code, .. } = &self.auth_state {
                        ctx.open_url(egui::OpenUrl::new_tab(code.verification_uri.clone()));
                    }
                }
            }
        }
    }

    /// Draw the current screen.
    ///
    /// `write_blocked` is the rate-limit tooltip (spec §13): `Some` while
    /// writes are disabled, and only the detail screen has write surfaces.
    fn render_screen(&mut self, ui: &mut egui::Ui, now: Instant, write_blocked: Option<&str>) {
        match self.screen {
            ScreenKind::Login => self.render_login(ui, now),
            ScreenKind::PrList => self.render_pr_list(ui),
            ScreenKind::PrDetail => self.render_pr_detail(ui, now, write_blocked),
        }
    }

    /// Draw the PR list and apply whatever the user asked for.
    fn render_pr_list(&mut self, ui: &mut egui::Ui) {
        let mut msgs = Vec::new();
        let mut actions = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            pr_list::render(
                ui,
                &mut self.pr_list,
                &mut self.pr_list_form,
                &mut msgs,
                &mut actions,
            );
        });

        for msg in msgs {
            self.apply_pr_list(msg);
        }
        for action in actions {
            match action {
                PrListAction::LoadMore => {
                    for command in load_more(&self.pr_list) {
                        self.worker.send(command);
                    }
                }
                PrListAction::Open { repo, number } => self.open_pr(repo, number),
                PrListAction::Settings => self.open_settings(),
            }
        }
    }

    /// Open one PR: allocate a session id, start its load, and show the detail
    /// screen.
    fn open_pr(&mut self, repo: RepoRef, number: u64) {
        // The session being replaced may still owe a debounced write
        // (spec §11); it does not survive the new PR.
        self.flush_session();
        let id = SessionId(self.next_session);
        self.next_session += 1;

        let (session, commands) = PrSession::open(id, self.settings.host.clone(), repo, number);
        self.session = Some(session);
        // The detail screen's per-PR state — the diff layout, reply drafts,
        // and expanded threads — belongs to the PR being left, not this one.
        // The pane geometry is the user's global layout, restored from
        // settings (Task 53).
        self.pr_detail = PrDetailState {
            left_pane: self.settings.layout.left_pane,
            right_pane: self.settings.layout.right_pane,
            right_open: self.settings.layout.right_open,
            ..PrDetailState::default()
        };
        for command in commands {
            self.worker.send(command);
        }
        self.screen = ScreenKind::PrDetail;
    }

    /// Draw the login screen and apply whatever the user asked for.
    fn render_login(&mut self, ui: &mut egui::Ui, now: Instant) {
        let mut actions = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            let vm = login::login_vm(&self.auth_state, &self.settings);
            login::render(ui, &vm, &mut self.login_form, &mut actions);
        });

        let ctx = ui.ctx().clone();
        self.apply_login_actions(actions, &ctx, now);
    }

    /// Draw the PR detail screen and apply whatever the user asked for.
    ///
    /// Without an open session there is nothing to draw — a 401 clears it — so
    /// the screen falls back to the list.
    fn render_pr_detail(&mut self, ui: &mut egui::Ui, now: Instant, write_blocked: Option<&str>) {
        let login = self.login();
        let Some(session) = &mut self.session else {
            self.screen = ScreenKind::PrList;
            return;
        };

        // Snapshot the live composer into its persisted draft field (only
        // this path writes that field, so a debounced whole-file write can
        // never carry a stale composer), then flush any debounced session
        // change whose window has elapsed.
        session.sync_composer_draft(now);
        session.tick(now);

        let mut diff_actions = Vec::new();
        let actions = pr_detail::render(
            ui,
            session,
            &mut self.pr_detail,
            &mut self.highlight_cache,
            &mut diff_actions,
            &login,
            write_blocked,
        );

        self.apply_diff_actions(diff_actions, ui.ctx(), now);

        // Blobs the session's image LRU dropped (or a refresh discarded)
        // must have their egui textures released too (Task 49).
        if let Some(session) = &mut self.session {
            let ctx = ui.ctx().clone();
            for uri in session.evicted_images.drain(..) {
                ctx.forget_image(&uri);
            }
        }

        for action in actions {
            match action {
                PrDetailAction::Back => self.close_detail(),
                PrDetailAction::Refresh => {
                    if let Some(session) = &mut self.session {
                        for command in session.refresh() {
                            self.worker.send(command);
                        }
                    }
                }
                PrDetailAction::FileFallback { accept } => {
                    if let Some(session) = &mut self.session {
                        if accept {
                            for command in session.post_fallback(&login, now) {
                                self.worker.send(command);
                            }
                        } else {
                            session.dismiss_fallback();
                        }
                    }
                }
                PrDetailAction::Review(action) => {
                    for command in self.apply_review_action(action, &login, now) {
                        self.worker.send(command);
                    }
                }
                PrDetailAction::OpenSettings => self.open_settings(),
            }
        }
    }

    /// Fold one review-panel control into the open session and return the
    /// commands it produced.
    ///
    /// Draft mutations persist through the session (the draft is never left
    /// waiting on a debounce, spec §11); Submit is gated on the panel's
    /// view-model via [`start_submit`], and on the author rule the view-model
    /// cannot see from the session alone.
    fn apply_review_action(
        &mut self,
        action: ReviewAction,
        login: &str,
        now: Instant,
    ) -> Vec<Command> {
        // Spec §13: the panel and composer disable Submit while a rate
        // limit is in force; this is the same gate for the keyboard and
        // palette paths, which bypass the buttons.
        let write_blocked = self
            .write_blocked_until
            .is_some_and(|until| until > SystemTime::now());
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        match action {
            ReviewAction::SetEvent(event) => {
                session.set_pending_event(event);
                Vec::new()
            }
            ReviewAction::SetBody(body) => {
                session.set_pending_body(body);
                Vec::new()
            }
            ReviewAction::EditComment(local_id, body) => {
                session.edit_pending(local_id, body);
                Vec::new()
            }
            ReviewAction::DeleteComment(local_id) => {
                session.delete_pending(local_id, now);
                Vec::new()
            }
            ReviewAction::Discard => {
                session.discard_pending_review(now);
                Vec::new()
            }
            ReviewAction::Submit => {
                // Spec §13: the panel and composer disable Submit while a
                // rate limit is in force; this is the same gate for the
                // keyboard and palette paths, which bypass the buttons.
                if write_blocked {
                    return Vec::new();
                }
                if viewer_is_author(session, login)
                    && matches!(
                        session.pending.event,
                        ReviewEvent::Approve | ReviewEvent::RequestChanges
                    )
                {
                    self.toasts.push(
                        "you cannot approve or request changes on your own pull request".to_owned(),
                        now,
                    );
                    return Vec::new();
                }
                start_submit(session).into_iter().collect()
            }
            ReviewAction::SubmitServerPending(event, body) => session
                .server_pending
                .as_ref()
                .map(|(review, _)| {
                    vec![Command::SubmitServerPending {
                        session: session.id,
                        repo: session.repo.clone(),
                        number: session.number,
                        review_id: review.id,
                        event,
                        body,
                    }]
                })
                .unwrap_or_default(),
            ReviewAction::DiscardServerPending => session
                .server_pending
                .as_ref()
                .map(|(review, _)| {
                    vec![Command::DeleteServerPending {
                        session: session.id,
                        repo: session.repo.clone(),
                        number: session.number,
                        review_id: review.id,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    /// Fold the diff surface's actions in.
    ///
    /// Comment and thread actions land on the open session (which owns the
    /// composer, the pending list, and the optimistic bookkeeping) or become
    /// commands; the rest are state a later phase owns and are recorded here
    /// rather than dropped so the wiring has one obvious home.
    fn apply_diff_actions(&mut self, actions: Vec<DiffAction>, ctx: &egui::Context, now: Instant) {
        let login = self.login();
        for action in actions {
            match action {
                DiffAction::ToggleViewed(_) => {} // Phase D
                DiffAction::SelectFile(index) => {
                    if let Some(session) = &mut self.session {
                        session.select_file(index as usize);
                        session.mark_session_dirty(now);
                    }
                }
                DiffAction::SetMode(mode) => {
                    if let Some(session) = &mut self.session {
                        session.set_mode(mode);
                    }
                    // Row indices changed, so any find match now points at a
                    // different row.
                    self.pr_detail.diff.clear_find();
                    // The two layouts measure heights at different widths.
                    self.pr_detail.last_layout = None;
                }
                DiffAction::LoadLargeDiff(file) => {
                    if let Some(session) = &mut self.session {
                        for command in session.load_large_diff(file) {
                            self.worker.send(command);
                        }
                    }
                }
                DiffAction::ToggleHideWhitespace => {
                    if let Some(session) = &mut self.session {
                        session.toggle_hide_whitespace();
                    }
                    // Row indices changed, so any find match now points at a
                    // different row (Task 48).
                    self.pr_detail.diff.clear_find();
                }
                DiffAction::LoadImages(file) => {
                    if let Some(session) = &mut self.session {
                        for command in session.load_images(file) {
                            self.worker.send(command);
                        }
                    }
                }
                DiffAction::ToggleRendered(file) => {
                    if let Some(session) = &mut self.session {
                        session.toggle_rendered_svg(file);
                    }
                }
                DiffAction::ExpandGap {
                    file,
                    before_hunk,
                    how,
                } => {
                    if let Some(session) = &mut self.session {
                        for command in session.expand_gap(file, before_hunk, how) {
                            self.worker.send(command);
                        }
                    }
                }
                DiffAction::StartComment(target) => {
                    if let Some(session) = &mut self.session {
                        session.open_comment(target);
                    }
                }
                DiffAction::DeletePending { local_id } => {
                    if let Some(session) = &mut self.session {
                        session.delete_pending(local_id, now);
                    }
                }
                DiffAction::Composer(action) => {
                    let commands = self.apply_composer_action(action, &login, now);
                    for command in commands {
                        self.worker.send(command);
                    }
                }
                DiffAction::Thread(action) => {
                    let Some(session) = &mut self.session else {
                        continue;
                    };
                    let command = match action {
                        ThreadAction::Reply {
                            thread_id,
                            root_comment_id,
                            body,
                        } => Some(Command::ReplyToThread {
                            session: session.id,
                            repo: session.repo.clone(),
                            number: session.number,
                            thread_id,
                            root_comment_id,
                            body,
                        }),
                        ThreadAction::ToggleResolved {
                            thread_id,
                            resolved,
                        } => Some(Command::SetThreadResolved {
                            session: session.id,
                            thread_id,
                            resolved,
                        }),
                        ThreadAction::CopySuggestion(text) => {
                            ctx.copy_text(text);
                            None
                        }
                    };
                    if let Some(command) = command {
                        self.worker.send(command);
                    }
                }
            }
        }
    }

    /// Fold one composer control in and return the commands it produced.
    fn apply_composer_action(
        &mut self,
        action: ComposerAction,
        login: &str,
        now: Instant,
    ) -> Vec<Command> {
        // Spec §13: same rate-limit gate as the review panel; the
        // composer's buttons are disabled while this holds, and this
        // is the choke point behind them.
        let write_blocked = self
            .write_blocked_until
            .is_some_and(|until| until > SystemTime::now());
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        match action {
            ComposerAction::Submit(mode) => {
                if write_blocked {
                    return Vec::new();
                }
                session.submit_comment(mode, login, now)
            }
            ComposerAction::Cancel => {
                composer::reduce(&mut session.composer, composer::ComposerMsg::Cancel);
                session.rebuild_rows();
                Vec::new()
            }
            // The composer row stays put for these, so no re-annotation.
            ComposerAction::SetSide(side) => {
                composer::reduce(&mut session.composer, composer::ComposerMsg::SetSide(side));
                Vec::new()
            }
            ComposerAction::TogglePreview => {
                composer::reduce(&mut session.composer, composer::ComposerMsg::TogglePreview);
                Vec::new()
            }
            ComposerAction::InsertSuggestion => {
                session.insert_suggestion();
                Vec::new()
            }
            // The composer moves from under its line to under the file
            // header, so the annotation has to be rebuilt (Task 47).
            ComposerAction::ToFileLevel => {
                composer::reduce(&mut session.composer, composer::ComposerMsg::ToFileLevel);
                session.rebuild_rows();
                Vec::new()
            }
        }
    }

    /// Read this frame's key presses and dispatch the ones that resolve.
    ///
    /// `?` arrives as `Event::Text` (it is shift + `/` on the common
    /// layouts), so it is picked up here rather than from the key event.
    fn handle_shortcuts(&mut self, ctx: &egui::Context, now: Instant) {
        // `text_edit_focused`, not the broader "anything focused": a clicked
        // button keeps focus, and the shortcuts must survive clicking one —
        // only *typing* must not fire them (Task 52 QA).
        let text_focused = ctx.text_edit_focused();
        let mut presses: Vec<KeyPress> = Vec::new();
        ctx.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let name = key.name();
                        // Letters arrive lowercase unless shifted ("V" for
                        // Shift+V); named keys keep their egui name.
                        let key = if name.chars().count() == 1 && !modifiers.shift {
                            name.to_lowercase()
                        } else {
                            name.to_owned()
                        };
                        presses.push(KeyPress {
                            key,
                            shift: modifiers.shift,
                            command: modifiers.command,
                        });
                    }
                    egui::Event::Text(text) if text == "?" => presses.push(KeyPress {
                        key: "?".to_owned(),
                        shift: false,
                        command: false,
                    }),
                    _ => {}
                }
            }
        });
        for press in &presses {
            if let Some(action) = command::resolve_key(press, text_focused) {
                if global_key_allowed(action, text_focused) {
                    self.dispatch(action, now);
                }
            }
        }
    }

    /// Run one registry action against the current app state.
    ///
    /// Detail-only actions are gated on the screen here, so both the key
    /// path and the palette respect it and a hidden session is never
    /// touched.
    fn dispatch(&mut self, action: &'static str, now: Instant) {
        if !action_allowed(action, self.screen) {
            return;
        }
        match action {
            "palette" => {
                self.palette.open_now();
                self.help_open = false;
            }
            "settings" => self.open_settings(),
            "help" => self.help_open = !self.help_open,
            "close" => self.dispatch_close(),
            "next_file" => self.step_file(true, now),
            "prev_file" => self.step_file(false, now),
            "next_hunk" => self.step_hunk(true),
            "prev_hunk" => self.step_hunk(false),
            "toggle_viewed" => self.set_viewed_current(None, now),
            "mark_viewed_and_next" => {
                self.set_viewed_current(Some(true), now);
                self.step_file(true, now);
            }
            "comment" => self.open_comment_on_current(),
            "reply" => self.expand_first_thread_of_current(),
            "toggle_side_pane" => self.pr_detail.right_open = !self.pr_detail.right_open,
            "split" => {
                if let Some(session) = &mut self.session {
                    session.set_mode(DiffMode::Split);
                }
            }
            "unified" => {
                if let Some(session) = &mut self.session {
                    session.set_mode(DiffMode::Unified);
                }
            }
            // The same path the review panel's Submit button takes, author
            // rule and all (Task 44). Reached from the palette, or from the
            // global key path only while no text edit is focused (see
            // `global_key_allowed`); inside the panel the panel's own
            // scoped ⌘Enter handler owns the shortcut.
            "submit_review" => {
                let login = self.login();
                for command in self.apply_review_action(ReviewAction::Submit, &login, now) {
                    self.worker.send(command);
                }
            }
            _ => {}
        }
    }

    /// `Esc`: dismiss the topmost surface, then back out of the detail screen.
    ///
    /// The composer cancel belongs to the detail screen: on the list the
    /// hidden session's open composer must not swallow the `Esc` that was
    /// meant for the visible screen. Leaving for the list flushes the
    /// session's debounced writes (spec §11).
    fn dispatch_close(&mut self) {
        if self.palette.open {
            self.palette.close();
            return;
        }
        if self.help_open {
            self.help_open = false;
            return;
        }
        if self.settings_window.open {
            self.settings_window.close();
            return;
        }
        if self.screen != ScreenKind::PrDetail {
            return;
        }
        if let Some(session) = &mut self.session {
            if session.composer.target.is_some() {
                composer::reduce(&mut session.composer, composer::ComposerMsg::Cancel);
                session.rebuild_rows();
                return;
            }
        }
        self.close_detail();
    }

    /// `j`/`k`: move the diff to the next/previous visible file in display
    /// order (spec §9.2).
    fn step_file(&mut self, forward: bool, now: Instant) {
        let next = {
            let Some(session) = &self.session else {
                return;
            };
            let order = session.file_order();
            let current = session.current_file().map(|(index, _)| index);
            next_index(&order, current, forward)
        };
        let Some(next) = next else { return };
        // The hunk cursor belongs to the file it was set in.
        self.pr_detail.hunk_row = None;
        if let Some(session) = &mut self.session {
            session.select_file(next);
            session.mark_session_dirty(now);
        }
    }

    /// `n`/`p`: move to the next/previous hunk of the current file and
    /// scroll its first row into view.
    fn step_hunk(&mut self, forward: bool) {
        let starts: Vec<usize> = {
            let Some(session) = &self.session else {
                return;
            };
            let Some(file) = Self::current_file(session) else {
                return;
            };
            let Some(range) = session.row_model.file_ranges.get(file) else {
                return;
            };
            // A hunk starts at its gap row when there is one, else at its
            // first line row; both carry the hunk's first visible row.
            let mut starts = Vec::new();
            let mut last_hunk: Option<u32> = None;
            for (index, row) in session
                .row_model
                .rows
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                match row {
                    RowRef::Gap {
                        file: f,
                        before_hunk,
                    } if *f as usize == file => {
                        starts.push(index);
                        last_hunk = Some(*before_hunk);
                    }
                    RowRef::Line { file: f, hunk, .. }
                        if *f as usize == file && last_hunk != Some(*hunk) =>
                    {
                        starts.push(index);
                        last_hunk = Some(*hunk);
                    }
                    _ => {}
                }
            }
            starts
        };
        let Some(target) = next_index(&starts, self.pr_detail.hunk_row, forward) else {
            return;
        };
        self.pr_detail.diff.scroll_to_row = Some(target);
        self.pr_detail.hunk_row = Some(target);
    }

    /// The file the diff is on (see [`PrSession::current_file`]).
    fn current_file(session: &PrSession) -> Option<usize> {
        session.current_file().map(|(index, _)| index)
    }

    /// `v` / `Shift+V`: flip (or set) the current file's viewed mark.
    fn set_viewed_current(&mut self, wanted: Option<bool>, now: Instant) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(index) = session.current_file().map(|(index, _)| index) else {
            return;
        };
        let currently = session.viewed.is_viewed(&session.files[index].file);
        session
            .viewed
            .set(&session.files[index].file, wanted.unwrap_or(!currently));
        session.mark_session_dirty(now);
    }

    /// `c`: open the composer as a file-level comment on the current file.
    fn open_comment_on_current(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(path) = session
            .current_file()
            .map(|(_, loaded)| loaded.file.path.clone())
        else {
            return;
        };
        session.open_comment(CommentTarget::File { path });
    }

    /// `r`: expand and scroll to the first thread on the current file (or
    /// the first thread at all), so its reply box is on screen. Focusing the
    /// reply text edit itself would need the thread widget's id, which the
    /// app does not have, so the reviewer clicks into it.
    fn expand_first_thread_of_current(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let path = session
            .current_file()
            .map(|(_, loaded)| loaded.file.path.clone());
        let thread = match session
            .threads
            .iter()
            .position(|t| Some(&t.path) == path.as_ref())
        {
            Some(index) => index,
            None if session.threads.is_empty() => return,
            None => 0,
        };
        let Some(row) = session
            .row_model
            .rows
            .iter()
            .position(|r| matches!(r, RowRef::Thread { thread: t } if *t as usize == thread))
        else {
            return;
        };
        let id = session.threads[thread].id.clone();
        self.pr_detail.drafts.expanded.insert(id);
        self.pr_detail.diff.scroll_to_row = Some(row);
    }
}

impl eframe::App for ReviewApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        for event in self.events.drain() {
            self.handle_event(event, now);
        }

        // Spec §13: the rate-limit window is over once its reset instant
        // passes; check every frame so the write surfaces wake up on their
        // own.
        let (write_blocked, until) =
            write_blocked_label(self.write_blocked_until, SystemTime::now());
        self.write_blocked_until = until;

        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);

        // Global shortcuts run before the screens draw, so a file selected
        // by `j`/`k` is rebuilt and drawn on this very frame.
        self.handle_shortcuts(&ctx, now);
        self.render_screen(ui, now, write_blocked.as_deref());
        self.sync_layout(now);

        // The palette, the settings window, and the help overlay float above
        // every screen. The palette runs its selection through the same
        // dispatch as a keybinding.
        let entries = command::registry();
        let mut palette_actions: Vec<&'static str> = Vec::new();
        command_palette::render(&ctx, &mut self.palette, &entries, &mut palette_actions);
        for action in palette_actions {
            self.dispatch(action, now);
        }
        let mut settings_actions = Vec::new();
        let signed_in = self.settings.account.is_some();
        settings_window::render(
            &ctx,
            &mut self.settings_window,
            signed_in,
            &mut settings_actions,
        );
        self.apply_settings_actions(settings_actions, now);
        command_palette::render_help(&ctx, &mut self.help_open, &entries);

        self.toasts.render(ui, now);
    }

    /// Flush the open PR's session file before the app exits, regardless of the
    /// debounce (spec §11).
    fn on_exit(&mut self) {
        if let Some(session) = &mut self.session {
            session.flush_session();
        }
        // A pane drag still inside its debounce window must not be lost (Task 53).
        if self.settings_writer.is_dirty() {
            self.settings_writer.flushed();
            self.save_settings(Instant::now());
        }
    }
}

/// Initialize `tracing`, install the panic hook, and start the GUI.
pub fn run() -> eframe::Result<()> {
    init_tracing();
    install_panic_hook();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "gh-review-desktop",
        options,
        Box::new(|cc| Ok(Box::new(ReviewApp::new(cc)))),
    )
}

/// Install the global tracing subscriber (once, at startup).
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
}

/// Log every panic before the previous hook runs, so a crash is never silent.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = ?info, "application panicked");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::client::GitHubUser;
    use crate::github::error::GhError;
    use crate::runtime::Event;

    #[test]
    fn routing_follows_auth_events() {
        assert!(matches!(initial_screen(false), ScreenKind::Login));
        assert!(matches!(initial_screen(true), ScreenKind::PrList));
        let unauthorized = Event::Failed {
            session: None,
            error: GhError::Unauthorized,
        };
        assert!(matches!(
            route(ScreenKind::PrDetail, &unauthorized),
            ScreenKind::Login
        ));
        let other = Event::Failed {
            session: None,
            error: GhError::NotFound,
        };
        assert!(matches!(
            route(ScreenKind::PrDetail, &other),
            ScreenKind::PrDetail
        ));

        // A 401 from a PR search ends the session too (spec §6).
        let search_unauthorized = Event::PrSearchFailed {
            request: 1,
            error: GhError::Unauthorized,
        };
        assert!(matches!(
            route(ScreenKind::PrList, &search_unauthorized),
            ScreenKind::Login
        ));
        let search_failed = Event::PrSearchFailed {
            request: 1,
            error: GhError::NotFound,
        };
        assert!(matches!(
            route(ScreenKind::PrList, &search_failed),
            ScreenKind::PrList
        ));

        // A 401 from a mutation ends the session too (spec §6).
        let comment_unauthorized = Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Unauthorized,
        };
        assert!(matches!(
            route(ScreenKind::PrDetail, &comment_unauthorized),
            ScreenKind::Login
        ));
        let comment_failed = Event::CommentFailed {
            session: SessionId(1),
            optimistic_id: 1,
            error: GhError::Server(500),
        };
        assert!(matches!(
            route(ScreenKind::PrDetail, &comment_failed),
            ScreenKind::PrDetail
        ));
    }

    #[test]
    fn a_successful_login_routes_to_the_pr_list() {
        let me = Me {
            user: GitHubUser {
                login: "octocat".into(),
                name: None,
                avatar_url: None,
            },
            scopes: vec!["repo".into()],
        };
        let logged_in = Event::LoggedIn {
            me,
            token: "tok".into(),
        };
        assert!(matches!(
            route(ScreenKind::Login, &logged_in),
            ScreenKind::PrList
        ));
        assert!(matches!(
            route(ScreenKind::PrList, &Event::Toast("hi".into())),
            ScreenKind::PrList
        ));
    }

    #[test]
    fn detail_actions_only_run_on_the_detail_screen() {
        // Every action that reaches for the session or the detail screen's
        // own state must not run while the session is merely hidden: after
        // Back it stays loaded, and mutating it from the list edits (and
        // persists marks for) a PR the reviewer cannot see.
        for action in [
            "next_file",
            "prev_file",
            "next_hunk",
            "prev_hunk",
            "toggle_viewed",
            "mark_viewed_and_next",
            "comment",
            "reply",
            "toggle_side_pane",
            "split",
            "unified",
            "submit_review",
        ] {
            assert!(
                action_allowed(action, ScreenKind::PrDetail),
                "{action} runs on the detail screen"
            );
            assert!(
                !action_allowed(action, ScreenKind::PrList),
                "{action} must not touch the hidden session"
            );
            assert!(
                !action_allowed(action, ScreenKind::Login),
                "{action} must not run from the login screen"
            );
        }
    }

    #[test]
    fn palette_help_close_and_settings_stay_global() {
        for action in ["palette", "settings", "help", "close"] {
            for screen in [ScreenKind::Login, ScreenKind::PrList, ScreenKind::PrDetail] {
                assert!(
                    action_allowed(action, screen),
                    "{action} is global, even on {screen:?}"
                );
            }
        }
    }

    #[test]
    fn the_global_submit_shortcut_yields_to_text_focus() {
        // With focus on a review-panel text edit the panel's own scoped
        // ⌘Enter handler submits; firing the global path in the same frame
        // would post twice. With focus on any other field — a thread reply,
        // the composer — the global path would submit the review by
        // accident. Either way, text focus means the global path stays out.
        assert!(!global_key_allowed("submit_review", true));
        // With no text focused ⌘Enter is the documented global shortcut.
        assert!(global_key_allowed("submit_review", false));
        // The other always-resolving keys must survive text focus.
        assert!(global_key_allowed("palette", true));
        assert!(global_key_allowed("close", true));
    }

    #[test]
    fn a_pr_scoped_404_closes_the_open_session() {
        let open = Some(SessionId(1));
        let gone = Event::Failed {
            session: Some(SessionId(1)),
            error: GhError::NotFound,
        };
        assert!(pr_is_gone(&gone, open), "the open PR is gone (spec §13)");

        // A stale 404 for a session the app already replaced closes
        // nothing, and neither does one that was never PR-scoped.
        let stale = Event::Failed {
            session: Some(SessionId(2)),
            error: GhError::NotFound,
        };
        assert!(!pr_is_gone(&stale, open));
        let unscoped = Event::Failed {
            session: None,
            error: GhError::NotFound,
        };
        assert!(!pr_is_gone(&unscoped, open));
        assert!(!pr_is_gone(&unscoped, None));

        // Other failures keep the session open (the toast is their face).
        let server = Event::Failed {
            session: Some(SessionId(1)),
            error: GhError::Server(500),
        };
        assert!(!pr_is_gone(&server, open));
        // And the auth path owns the 401, not this rule.
        let unauthorized = Event::Failed {
            session: Some(SessionId(1)),
            error: GhError::Unauthorized,
        };
        assert!(!pr_is_gone(&unauthorized, open));
    }

    #[test]
    fn the_write_block_lives_until_its_reset_instant() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let until = now + std::time::Duration::from_secs(42);
        let (label, kept) = write_blocked_label(Some(until), now);
        let label = label.expect("a reset ahead of now blocks writes");
        assert!(label.contains("~42s"), "{label}");
        assert_eq!(kept, Some(until), "the window survives the frame");

        // Once the instant passes the flag clears for good, and an unset
        // window blocks nothing.
        let past = now + std::time::Duration::from_secs(1);
        let (label, kept) =
            write_blocked_label(Some(past), now + std::time::Duration::from_secs(2));
        assert!(label.is_none() && kept.is_none(), "the reset has passed");
        let (label, kept) = write_blocked_label(None, now);
        assert!(label.is_none() && kept.is_none(), "no window, no block");
    }
}
