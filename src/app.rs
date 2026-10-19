//! The application shell: screen routing, the egui event loop, and startup.
//!
//! The UI thread never blocks on I/O. It drains [`Event`]s once per frame,
//! routes them into the current screen, and renders. Everything network-shaped
//! goes to the worker (see [`crate::runtime::execute`]), which wakes the UI via
//! `EventSender::with_repaint`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;

use crate::auth::device_flow::{self, AuthFailure, AuthState};
use crate::auth::session::{complete_login, handle_unauthorized, restore_token, sign_out};
use crate::auth::store::{KeyringStore, TokenStore};
use crate::auth::DEFAULT_CLIENT_ID;
use crate::github::client::{GithubClient, Me};
use crate::github::error::GhError;
use crate::model::RepoRef;
use crate::persist::config_root;
use crate::persist::settings::Settings;
use crate::runtime::{
    event_channel, execute, spawn_worker, Command, Event, EventReceiver, ExecCtx, SessionId, Worker,
};
use crate::state::pr_detail::PrSession;
use crate::state::pr_list::{load_more, reduce, PrListMsg, PrListState};
use crate::ui::screens::login::{self, LoginAction, LoginForm};
use crate::ui::screens::pr_list::{self, PrListAction, PrListForm};
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
        Event::LoggedIn { .. } => ScreenKind::PrList,
        _ => current,
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
    session: Option<PrSession>,
    next_session: u64,
    toasts: Toasts,
    settings: Settings,
    config_dir: PathBuf,
    store: Arc<dyn TokenStore>,
    exec: Arc<ExecCtx>,
    worker: Worker,
    events: EventReceiver,
}

impl ReviewApp {
    /// Build the app: load settings, restore any token, and start the worker.
    pub fn new(cc: &eframe::CreationContext<'_>) -> ReviewApp {
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
            session: None,
            next_session: 1,
            toasts: Toasts::default(),
            settings,
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
            Event::Failed { error, .. } => self.toasts.push(error.to_string(), now),
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
            // Folded into the open session by `apply_session_event` above.
            Event::PrFilesPage { .. } => {}
            Event::PrMetadataLoaded { .. } => {}
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
    fn reset_pr_list(&mut self) {
        self.pr_list = PrListState::default();
        self.pr_list.scopes = self.settings.default_scopes.clone();
        self.pr_list_form = PrListForm::default();
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
    fn perform_sign_out(&mut self, now: Instant) {
        match sign_out(self.store.as_ref(), &mut self.settings) {
            Ok(()) => {
                self.exec.clear_client();
                self.save_settings(now);
                self.auth_state = AuthState::Idle;
                self.screen = ScreenKind::Login;
                self.reset_pr_list();
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
    fn render_screen(&mut self, ui: &mut egui::Ui, now: Instant) {
        match self.screen {
            ScreenKind::Login => self.render_login(ui, now),
            ScreenKind::PrList => self.render_pr_list(ui),
            ScreenKind::PrDetail => self.render_placeholder(ui, "Pull request", now),
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
            }
        }
    }

    /// Open one PR: allocate a session id, start its load, and show the detail
    /// screen.
    fn open_pr(&mut self, repo: RepoRef, number: u64) {
        let id = SessionId(self.next_session);
        self.next_session += 1;

        let (session, commands) = PrSession::open(id, self.settings.host.clone(), repo, number);
        self.session = Some(session);
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

    /// A stand-in screen until Task 23 lands.
    fn render_placeholder(&mut self, ui: &mut egui::Ui, title: &str, now: Instant) {
        let mut sign_out = false;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(title);
            ui.label("this screen is implemented in a later task");
            ui.separator();
            if ui.button("Sign out").clicked() {
                sign_out = true;
            }
        });
        if sign_out {
            self.perform_sign_out(now);
        }
    }
}

impl eframe::App for ReviewApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        for event in self.events.drain() {
            self.handle_event(event, now);
        }
        self.render_screen(ui, now);
        self.toasts.render(ui, now);
    }
}

/// Initialize `tracing`, install the panic hook, and start the GUI.
pub fn run() -> eframe::Result<()> {
    init_tracing();
    install_panic_hook();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
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
}
