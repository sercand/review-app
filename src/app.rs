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
use crate::diff::highlight::{HighlightCache, SyntectHighlighter};
use crate::github::client::{GithubClient, Me};
use crate::github::error::GhError;
use crate::model::review::ReviewEvent;
use crate::model::RepoRef;
use crate::persist::config_root;
use crate::persist::settings::Settings;
use crate::runtime::{
    event_channel, execute, spawn_worker, Command, Event, EventReceiver, ExecCtx, SessionId, Worker,
};
use crate::state::composer::{self, ComposerAction};
use crate::state::pr_detail::{start_submit, viewer_is_author, PrSession};
use crate::state::pr_list::{load_more, reduce, PrListMsg, PrListState};
use crate::ui::screens::login::{self, LoginAction, LoginForm};
use crate::ui::screens::pr_detail::{self, PrDetailAction, PrDetailState};
use crate::ui::screens::pr_list::{self, PrListAction, PrListForm};
use crate::ui::widgets::diff_view::DiffAction;
use crate::ui::widgets::review_panel::ReviewAction;
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
        // The image diff viewer draws through `egui::Image` (Task 49); the
        // supported formats are the ones enabled on the `image` dependency.
        egui_extras::install_image_loaders(&cc.egui_ctx);

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
            highlight_cache: HighlightCache::new(2048, Box::new(SyntectHighlighter::new())),
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
    /// Only sign-out calls this today; the settings window (Task 53) is the
    /// next caller, so it is kept rather than inlined.
    #[allow(dead_code)]
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
    ///
    /// Reached from the settings window in Task 53; the temporary placeholder
    /// button it used to back is gone now that the detail screen is real.
    #[allow(dead_code)]
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
            ScreenKind::PrDetail => self.render_pr_detail(ui, now),
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
        // The detail screen's per-PR state — the diff layout, reply drafts,
        // and expanded threads — belongs to the PR being left, not this one.
        self.pr_detail = PrDetailState::default();
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
    fn render_pr_detail(&mut self, ui: &mut egui::Ui, now: Instant) {
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
                PrDetailAction::Back => self.screen = ScreenKind::PrList,
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
                DiffAction::ScrollToFile(_) => {} // Phase D (file tree)
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
        let Some(session) = &mut self.session else {
            return Vec::new();
        };
        match action {
            ComposerAction::Submit(mode) => session.submit_comment(mode, login, now),
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

    /// Flush the open PR's session file before the app exits, regardless of the
    /// debounce (spec §11).
    fn on_exit(&mut self) {
        if let Some(session) = &mut self.session {
            session.flush_session();
        }
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
}
