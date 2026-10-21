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

use crate::auth::DEFAULT_CLIENT_ID;
use crate::auth::device_flow::{self, AuthFailure, AuthState};
use crate::auth::session::{complete_login, handle_unauthorized, restore_token, sign_out};
use crate::auth::store::{KeyringStore, TokenStore};
use crate::diff::highlight::{HighlightCache, SyntectHighlighter};
use crate::diff::row_model::RowRef;
use crate::github::client::{GithubClient, Me};
use crate::github::error::GhError;
use crate::model::RepoRef;
use crate::model::comment::CommentTarget;
use crate::model::review::ReviewEvent;
use crate::persist::config_root;
use crate::persist::session::DebouncedWriter;
#[cfg(unix)]
use crate::persist::settings::RepoAiSettings;
use crate::persist::settings::Settings;
use crate::runtime::{
    Command, Event, EventReceiver, ExecCtx, SessionId, Worker, event_channel, execute, spawn_worker,
};
use crate::state::composer::{self, ComposerAction};
use crate::state::pr_detail::{PrSession, start_submit, viewer_is_author};
use crate::state::pr_list::{PrListMsg, PrListState, load_more, reduce};
use crate::ui::command::{self, KeyPress, next_index};
use crate::ui::screens::login::{self, LoginAction, LoginForm};
use crate::ui::screens::pr_detail::{self, PrDetailAction, PrDetailState};
use crate::ui::screens::pr_list::{self, PrListAction, PrListForm};
use crate::ui::theme;
use crate::ui::widgets::command_palette::{self, PaletteState};
use crate::ui::widgets::diff_view::{DiffAction, DiffMode, DiffViewState};
use crate::ui::widgets::file_header::{FileHeaderAction, FooterAction as FileFooterAction};
use crate::ui::widgets::merge_panel;
use crate::ui::widgets::review_panel::ReviewAction;
use crate::ui::widgets::settings_window::{self, SettingsAction, SettingsWindowState};
use crate::ui::widgets::thread_view::ThreadAction;
use crate::ui::widgets::title_bar;
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

/// Who the AI run should act on, taken from the session's loaded metadata.
#[cfg(unix)]
fn ai_target(session: &PrSession) -> Option<crate::ai::checkout::PrTarget> {
    let meta = session.metadata.as_ref()?;
    Some(crate::ai::checkout::PrTarget {
        host: session.host.clone(),
        owner: session.repo.owner.clone(),
        name: session.repo.name.clone(),
        number: session.number,
        head_sha: meta.head_sha.clone(),
        head_branch: meta.head_ref.clone(),
    })
}

/// The store key for the open PR's AI rows, lowercased like the store's own
/// session identity.
#[cfg(unix)]
fn pr_key(session: &PrSession) -> crate::ai::store::PrKey {
    crate::ai::store::PrKey {
        host: session.host.to_lowercase(),
        owner: session.repo.owner.to_lowercase(),
        repo: session.repo.name.to_lowercase(),
        number: session.number,
    }
}

/// Whether the repo's AI settings already name commands. The scout only
/// proposes commands while the repo has none (spec §4 step 1), so once one
/// is configured the tab's suggested-commands card retires.
#[cfg(unix)]
fn repo_has_commands(repo: Option<&RepoAiSettings>) -> bool {
    repo.is_some_and(|repo| !repo.commands.is_empty() || !repo.prepare.is_empty())
}

/// The error an event carries, if it carries one. Every such event toasts
/// it, so this is where the app notices a write-blocking failure.
fn event_error(event: &Event) -> Option<&GhError> {
    match event {
        Event::Failed { error, .. }
        | Event::PrSearchFailed { error, .. }
        | Event::CommentFailed { error, .. }
        | Event::ReviewSubmitFailed { error, .. }
        | Event::MergeFailed { error, .. } => Some(error),
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
    /// When the current device-flow code was shown, for the expiry countdown
    /// (redesign Task 16).
    login_started: Option<std::time::Instant>,
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
    /// The macOS menu bar; `None` elsewhere.
    menu: Option<crate::ui::menu::NativeMenu>,
    /// The AI dialog open over the screens (Task 4); `None` shows none.
    #[cfg(unix)]
    ai_dialog: Option<crate::ui::widgets::ai_dialogs::AiDialog>,
    /// The folder-validation error the pick dialog shows (Task 4).
    #[cfg(unix)]
    ai_dialog_error: Option<String>,
    /// The in-flight OS folder picker's answer, polled once per frame
    /// (Task 4); `None` when no picker is open.
    #[cfg(unix)]
    folder_picker: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
}

impl ReviewApp {
    /// Build the app: load settings, restore any token, and start the worker.
    pub fn new(cc: &eframe::CreationContext<'_>) -> ReviewApp {
        // The image diff viewer draws through `egui::Image` (Task 49); the
        // supported formats are the ones enabled on the `image` dependency.
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Plex + JetBrains Mono + the Lucide icon font, before the first frame.
        cc.egui_ctx.set_fonts(crate::ui::fonts::font_definitions());
        let config_dir = config_root();
        let settings = Settings::load_or_default(&config_dir);
        // Both themes' full styles — visuals inside — so `apply_theme`'s
        // `ctx.set_theme` switches between the mockup's dark and light looks.
        // They carry the user's code font size, hence after the settings load.
        theme::install(&cc.egui_ctx, settings.code_font_size);
        let store: Arc<dyn TokenStore> = Arc::new(KeyringStore);

        let restored = match restore_token(store.as_ref(), &settings) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(%error, "could not read the stored token");
                None
            }
        };

        // The AI hub opens the persisted AI database beside the settings;
        // its settings snapshot is replaced whenever the user saves (Task 1).
        #[cfg(unix)]
        let exec = {
            let ai = crate::ai::hub::AiHub::open(&config_dir, store.clone());
            ai.set_settings(settings.ai.clone());
            Arc::new(ExecCtx::with_ai(store.clone(), ai))
        };
        #[cfg(not(unix))]
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
        pr_list.recent_constraints = settings.recent_constraints.clone();
        pr_list.group_by_repo = settings.group_by_repo;
        if restored.is_some() {
            let login = account_login(&settings);
            for command in reduce(&mut pr_list, PrListMsg::Refresh, &login) {
                worker.send(command);
            }
        }

        ReviewApp {
            screen: initial_screen(restored.is_some()),
            auth_state: AuthState::Idle,
            login_started: None,
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
            menu: crate::ui::menu::NativeMenu::install(&cc.egui_ctx),
            #[cfg(unix)]
            ai_dialog: None,
            #[cfg(unix)]
            ai_dialog_error: None,
            #[cfg(unix)]
            folder_picker: None,
        }
    }

    /// Apply one event to the app state.
    fn handle_event(&mut self, event: Event, now: Instant) {
        let previous = self.screen;
        self.screen = route(self.screen, &event);
        self.apply_session_event(&event);
        // The tab's suggested-commands card is a first-run aid: once the
        // repo names commands of its own, it retires (spec §4 step 1).
        #[cfg(unix)]
        self.retire_ai_suggestions();
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
            Event::DeviceCode(code) => {
                self.auth_state = device_flow::start(code);
                self.login_started = Some(now);
            }
            Event::AuthFailed(failure) => {
                self.auth_state = AuthState::Failed(failure);
                self.login_started = None;
            }
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
            } => self.apply_pr_list(
                PrListMsg::PageLoaded {
                    request,
                    query,
                    page,
                },
                now,
            ),
            Event::PrSearchFailed { request, error } => {
                let unauthorized = matches!(error, GhError::Unauthorized);
                self.apply_pr_list(PrListMsg::LoadFailed { request, error }, now);
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
            // The session stores the allow flags through
            // `apply_session_event`; the Merge popover reads them from there.
            Event::RepoSettingsLoaded { .. } => {}
            // The session clears its in-flight flag and reloads metadata and
            // reviews through `apply_session_event`; the toast confirms.
            Event::PrMerged { .. } => self.toasts.push("Pull request merged.".to_owned(), now),
            Event::MergeFailed { error, .. } => {
                let unauthorized = matches!(error, GhError::Unauthorized);
                self.toasts.push(error.to_string(), now);
                if unauthorized {
                    // Spec §6: a 401 anywhere ends the session.
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
            // Spec §10: the Settings window's AI section shows the outcome.
            // The VM outlives a closed window, so the result lands there
            // without stealing the window back over whatever screen the user
            // moved on to; a closed window gets a toast instead.
            Event::AiProviderTested { provider, result } => {
                let window_open = self.settings_window.open;
                if !window_open {
                    let outcome = match &result {
                        Ok(model) => format!("connected — {model} answered"),
                        Err(error) => error.clone(),
                    };
                    self.toasts.push(format!("{provider} test: {outcome}"), now);
                }
                self.settings_window
                    .vm
                    .ai
                    .test_results
                    .insert(provider, result);
            }
            // The AI tab's events fold into the open session's reducer
            // through `apply_session_event` (the stale-event rule lives in
            // `PrSession::on_event`); a failed start also toasts (Task 1).
            // The dialogs (Task 4) open only for the PR the user is looking
            // at — the same stale-event rule for a surface the app owns —
            // and close when the flow answers them. `AiUnrestored` is
            // app-wide.
            #[cfg(unix)]
            Event::AiCheckoutFailed { session, message } => {
                // The failure toasts for any PR (the run may outlive the
                // screen), and ends that PR's flow's dialog; the app-wide
                // recovery dialog belongs to no PR and stays.
                if self.ai_screen_is(session) && !self.recovery_dialog_open() {
                    self.ai_dialog = None;
                }
                self.toasts.push(message, now);
            }
            #[cfg(unix)]
            Event::AiCheckoutPlanned { session, plan }
                if self.ai_screen_is(session) && !self.recovery_dialog_open() =>
            {
                self.ai_dialog = Some(crate::ui::widgets::ai_dialogs::AiDialog::ConfirmCheckout(
                    plan,
                ));
                self.ai_dialog_error = None;
            }
            #[cfg(unix)]
            Event::AiCheckoutHeld {
                session,
                pr_number,
                restore,
            } if self.ai_screen_is(session) && !self.recovery_dialog_open() => {
                self.ai_dialog =
                    Some(crate::ui::widgets::ai_dialogs::AiDialog::Held { pr_number, restore });
            }
            #[cfg(unix)]
            Event::AiRestoreNeedsDecision { session, changes }
                if self.ai_screen_is(session) && !self.recovery_dialog_open() =>
            {
                self.ai_dialog =
                    Some(crate::ui::widgets::ai_dialogs::AiDialog::Restore { lines: changes });
            }
            #[cfg(unix)]
            Event::AiRestored { session, .. } if self.ai_screen_is(session) => {
                // A recovery restore (from the app-wide dialog) has no
                // session and never lands here.
                if !self.recovery_dialog_open() {
                    self.ai_dialog = None;
                }
            }
            #[cfg(unix)]
            Event::AiUnrestored(records) => {
                if !records.is_empty() {
                    self.ai_dialog =
                        Some(crate::ui::widgets::ai_dialogs::AiDialog::Recovery(records));
                }
            }
            #[cfg(unix)]
            Event::AiSnapshot { .. }
            | Event::AiCheckoutPlanned { .. }
            | Event::AiCheckoutHeld { .. }
            | Event::AiNotice { .. }
            | Event::AiPrepareOutput { .. }
            | Event::AiPipeline { .. }
            | Event::AiQuestion { .. }
            | Event::AiApprovalRequest { .. }
            | Event::AiChat { .. }
            | Event::AiRestoreNeedsDecision { .. }
            | Event::AiRestored { .. } => {}
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
            self.apply_pr_list(PrListMsg::Refresh, now);
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
    fn apply_pr_list(&mut self, msg: PrListMsg, now: Instant) {
        // Task 15: the list's filter choices are durable preferences.
        let durable = matches!(
            &msg,
            PrListMsg::SetScopes(_) | PrListMsg::SetGroupByRepo(_) | PrListMsg::PushRecent(_)
        );
        match msg.clone() {
            PrListMsg::SetScopes(scopes) => self.settings.default_scopes = scopes,
            PrListMsg::SetGroupByRepo(group) => self.settings.group_by_repo = group,
            PrListMsg::PushRecent(entry) => self.settings.push_recent(entry),
            _ => {}
        }
        let login = self.login();
        for command in reduce(&mut self.pr_list, msg, &login) {
            self.worker.send(command);
        }
        if durable {
            self.save_settings(now);
        }
    }

    /// Drop the loaded rows, so one account's PRs never show under another's.
    ///
    /// Sign-out and a settings host change call this.
    fn reset_pr_list(&mut self) {
        self.pr_list = PrListState::default();
        self.pr_list.scopes = self.settings.default_scopes.clone();
        self.pr_list.recent_constraints = self.settings.recent_constraints.clone();
        self.pr_list.group_by_repo = self.settings.group_by_repo;
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
    /// The PR-scoped AI dialogs go with it — a Confirm dialog left floating
    /// over the list would run `ConfirmCheckout` against the PR just left
    /// (Task 4 review finding 1); the recovery list is app-wide and stays.
    fn close_detail(&mut self) {
        self.flush_session();
        #[cfg(unix)]
        self.close_ai_flow_dialog();
        self.screen = ScreenKind::PrList;
    }

    /// Close the PR-scoped AI dialogs. The crash-recovery dialog belongs to
    /// no PR (its records name their own repositories), so it survives
    /// navigation the way it survives `open_pr`.
    #[cfg(unix)]
    fn close_ai_flow_dialog(&mut self) {
        if !self
            .ai_dialog
            .as_ref()
            .is_some_and(crate::ui::widgets::ai_dialogs::survives_navigation)
        {
            self.ai_dialog = None;
            self.ai_dialog_error = None;
        }
    }

    /// Store the token and cache the account for the next launch.
    fn finish_login(&mut self, me: &Me, token: &str, now: Instant) {
        self.login_started = None;
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
        self.login_started = None;
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
        // The worker's AI hub reads its settings from its own copy (Task 1).
        #[cfg(unix)]
        self.exec.ai.set_settings(self.settings.ai.clone());
    }

    /// Open the settings window on a fresh copy of the current settings.
    ///
    /// The AI section's "stored" marks come from a pure keychain probe:
    /// whether a key exists, never what it is.
    fn open_settings(&mut self) {
        let store = self.store.clone();
        self.settings_window.open_with(&self.settings, &|provider| {
            crate::auth::store::llm_key(store.as_ref(), provider)
                .ok()
                .flatten()
                .is_some()
        });
    }

    /// Fold the settings window's actions in: Save persists (and a host
    /// change ends the session, since the stored token belongs to the old
    /// host), a typed AI key goes to the keychain, Test connection asks the
    /// worker, and Sign out rides the existing sign-out flow (RULING R10).
    ///
    /// Save is emitted before any key action: a rejected apply stops the loop,
    /// so a key is never stored for settings that were not saved. A successful
    /// apply also clears the keys of removed/renamed providers.
    fn apply_settings_actions(
        &mut self,
        actions: Vec<SettingsAction>,
        ctx: &egui::Context,
        now: Instant,
    ) {
        for action in actions {
            match action {
                SettingsAction::Apply(vm) => {
                    // A removed or renamed provider must not leave its key
                    // behind in the keychain (§10), or re-adding the name
                    // would silently reuse a stale credential.
                    let old_names: Vec<String> = self
                        .settings
                        .ai
                        .providers
                        .iter()
                        .map(|provider| provider.name.clone())
                        .collect();
                    let host_changed = match vm.apply(&mut self.settings) {
                        Ok(host_changed) => host_changed,
                        Err(error) => {
                            // The window keeps Save disabled on invalid input,
                            // so this is the defensive path; still say why
                            // nothing was saved. Stop here: nothing is
                            // persisted, so no key in a later action may be
                            // stored either.
                            self.toasts
                                .push(format!("settings not saved: {error}"), now);
                            return;
                        }
                    };
                    self.save_settings(now);
                    let new_names: Vec<String> = self
                        .settings
                        .ai
                        .providers
                        .iter()
                        .map(|provider| provider.name.clone())
                        .collect();
                    // Best-effort, exactly like storing a key: the settings
                    // are already saved, so a keychain failure is a toast.
                    for name in settings_window::removed_provider_names(&old_names, &new_names) {
                        if let Err(error) =
                            crate::auth::store::clear_llm_key(self.store.as_ref(), &name)
                        {
                            tracing::warn!(
                                %error,
                                %name,
                                "could not clear the removed LLM API key"
                            );
                            self.toasts
                                .push(format!("could not clear the key for {name}: {error}"), now);
                        }
                    }
                    // The code font size lives in the styles; re-register
                    // both so the next frame picks it up (Task 14).
                    theme::install(ctx, self.settings.code_font_size);
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
                // Spec §10: API keys live only in the keychain. The key is
                // never logged and never written to `settings.toml`.
                SettingsAction::SetLlmKey { provider, key } => {
                    if let Err(error) =
                        crate::auth::store::set_llm_key(self.store.as_ref(), &provider, &key)
                    {
                        tracing::warn!(%error, %provider, "could not store the LLM API key");
                        self.toasts.push(
                            format!("could not store the key for {provider}: {error}"),
                            now,
                        );
                    }
                }
                SettingsAction::TestProvider { provider, model } => {
                    self.worker
                        .send(Command::AiTestProvider { provider, model });
                }
                SettingsAction::SignOut => self.perform_sign_out(now),
            }
        }
    }

    /// Flip between dark and light from a title bar's theme button, and
    /// remember the choice.
    fn toggle_theme(&mut self, ctx: &egui::Context, now: Instant) {
        let system_dark = ctx.system_theme() == Some(egui::Theme::Dark);
        self.settings.theme =
            crate::ui::widgets::top_bar::next_theme(self.settings.theme, system_dark);
        self.save_settings(now);
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
            // The diff layout choices are global preferences: mirror them
            // out of the live session and state (Task 7).
            if let Some(session) = &self.session {
                if session.mode != self.settings.diff.mode {
                    self.settings.diff.mode = session.mode;
                    dirty = true;
                }
                if session.hide_whitespace != self.settings.diff.hide_whitespace {
                    self.settings.diff.hide_whitespace = session.hide_whitespace;
                    dirty = true;
                }
            }
            if self.pr_detail.diff.wrap != self.settings.diff.wrap {
                self.settings.diff.wrap = self.pr_detail.diff.wrap;
                dirty = true;
            }
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
            if self.pr_detail.details_tab != self.settings.layout.details_tab {
                self.settings.layout.details_tab = self.pr_detail.details_tab;
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
                LoginAction::OpenSettings => self.open_settings(),
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
            ScreenKind::PrList => self.render_pr_list(ui, now),
            ScreenKind::PrDetail => self.render_pr_detail(ui, now, write_blocked),
        }
    }

    /// Draw the PR list and apply whatever the user asked for.
    fn render_pr_list(&mut self, ui: &mut egui::Ui, now: Instant) {
        let mut msgs = Vec::new();
        let mut actions = Vec::new();
        let wall = crate::ui::widgets::pr_row::now_iso();
        pr_list::render_title_bar(ui, &mut msgs, &mut actions);
        egui::CentralPanel::default().show(ui, |ui| {
            pr_list::render(
                ui,
                &mut self.pr_list,
                &mut self.pr_list_form,
                &mut msgs,
                &mut actions,
                &wall,
            );
        });

        for msg in msgs {
            self.apply_pr_list(msg, now);
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
                PrListAction::Palette => self.palette.open_now(),
                PrListAction::ToggleTheme => self.toggle_theme(ui.ctx(), now),
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

        let (mut session, commands) = PrSession::open(id, self.settings.host.clone(), repo, number);
        // The AI dialogs belong to the PR being left (Task 4); the
        // crash-recovery dialog is app-wide and stays open.
        #[cfg(unix)]
        self.close_ai_flow_dialog();
        // The diff layout is a global preference (Task 7): every PR opens
        // the way the reviewer left the last one.
        let prefs = self.settings.diff;
        session.mode = prefs.mode;
        session.set_hide_whitespace(prefs.hide_whitespace);
        self.session = Some(session);
        // The detail screen's per-PR state — the diff layout, reply drafts,
        // and expanded threads — belongs to the PR being left, not this one.
        // The pane geometry is the user's global layout, restored from
        // settings (Task 53).
        self.pr_detail = PrDetailState {
            diff: DiffViewState {
                wrap: prefs.wrap,
                ..DiffViewState::default()
            },
            left_pane: self.settings.layout.left_pane,
            right_pane: self.settings.layout.right_pane,
            right_open: self.settings.layout.right_open,
            // The AI tab is unix-only; a saved `Ai` loads as `Overview` on
            // Windows (Plan 3).
            details_tab: self.settings.layout.details_tab.for_platform(),
            merge_form: merge_panel::MergeForm::with_method(self.settings.merge_method),
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
        let remaining = match (&self.auth_state, self.login_started) {
            (AuthState::Polling { code, .. }, Some(started)) => {
                Some(code.expires_in.saturating_sub(now.duration_since(started)))
            }
            _ => None,
        };
        let mut toggle_theme = false;
        title_bar::show(ui, "login-title", |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let glyph = if ui.visuals().dark_mode {
                    crate::ui::icons::SUN
                } else {
                    crate::ui::icons::MOON
                };
                toggle_theme = crate::ui::icons::icon_button(ui, glyph, "Toggle theme").clicked();
            });
        });
        if toggle_theme {
            self.toggle_theme(ui.ctx(), now);
        }
        egui::CentralPanel::default().show(ui, |ui| {
            let vm = login::login_vm(&self.auth_state, &self.settings);
            login::render(ui, &vm, &mut self.login_form, &mut actions, remaining);
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
        let system_dark = ui.ctx().system_theme() == Some(egui::Theme::Dark);
        let actions = pr_detail::render(
            ui,
            session,
            &mut self.pr_detail,
            &mut self.highlight_cache,
            &mut diff_actions,
            pr_detail::ScreenInputs {
                login: &login,
                write_blocked,
                theme: self.settings.theme,
                system_dark,
                // The ✦ AI review button appears once a model is configured
                // (spec §4 step 1); the folder is picked on first use.
                ai_configured: self.settings.ai.models.default.is_some(),
            },
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
                PrDetailAction::Merge(merge_panel::MergeAction::Merge) => {
                    // Spec §13: the popover disables Merge while a rate limit
                    // is in force; this is the same gate behind the button.
                    let write_blocked = self
                        .write_blocked_until
                        .is_some_and(|until| until > SystemTime::now());
                    if write_blocked {
                        continue;
                    }
                    let method = self.pr_detail.merge_form.method;
                    let title = self.pr_detail.merge_form.title.clone();
                    let message = self.pr_detail.merge_form.message.clone();
                    // The method the reviewer picked becomes the next PR's
                    // starting point.
                    if self.settings.merge_method != method {
                        self.settings.merge_method = method;
                        self.save_settings(now);
                    }
                    if let Some(session) = &mut self.session
                        && let Some(command) =
                            crate::state::pr_detail::start_merge(session, method, title, message)
                    {
                        // The popover's in-flight half: the button rests
                        // until GitHub answers.
                        session.merging = true;
                        self.worker.send(command);
                    }
                }
                PrDetailAction::OpenSettings => self.open_settings(),
                PrDetailAction::OpenInBrowser => {
                    if let Some(metadata) = self
                        .session
                        .as_ref()
                        .and_then(|session| session.metadata.as_ref())
                    {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(metadata.html_url.clone()));
                    }
                }
                PrDetailAction::Palette => self.palette.open_now(),
                PrDetailAction::ToggleTheme => self.toggle_theme(ui.ctx(), now),
                PrDetailAction::FileHeader(action) => {
                    self.apply_file_header_action(action, ui.ctx(), now);
                }
                PrDetailAction::FileFooter(action) => match action {
                    FileFooterAction::Prev => self.step_file(false, now),
                    FileFooterAction::Next => self.step_file(true, now),
                    FileFooterAction::ViewedAndNext => {
                        self.set_viewed_current(Some(true), now);
                        self.step_file(true, now);
                    }
                },
                #[cfg(unix)]
                PrDetailAction::Ai(action) => self.apply_ai_action(action, now),
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
            ReviewAction::LocateThread(thread_id) => {
                let anchor = session
                    .threads
                    .iter()
                    .find(|thread| thread.id == thread_id)
                    .map(|thread| (thread.path.clone(), thread.side, thread.line));
                if let Some((path, side, line)) = anchor
                    && let Some(index) = session.files.iter().position(|l| l.file.path == path)
                {
                    session.select_file(index);
                    let row = side.zip(line).and_then(|(side, line)| {
                        session.row_model.row_of_line(&session.files, side, line)
                    });
                    // A file-level thread sits at the top of the file.
                    self.pr_detail.diff.scroll_to_row = Some(row.unwrap_or(0));
                }
                Vec::new()
            }
            ReviewAction::LocatePending(local_id) => {
                // Task 12: jump the diff to the comment's anchor.
                let anchor = session
                    .pending
                    .comments
                    .iter()
                    .find(|comment| comment.local_id == local_id)
                    .map(|comment| comment.anchor.clone());
                if let Some(anchor) = anchor
                    && let Some(index) = session
                        .files
                        .iter()
                        .position(|l| l.file.path == anchor.path)
                {
                    session.select_file(index);
                    if let Some(row) =
                        session
                            .row_model
                            .row_of_line(&session.files, anchor.side, anchor.line)
                    {
                        self.pr_detail.diff.scroll_to_row = Some(row);
                    }
                }
                Vec::new()
            }
            ReviewAction::Discard => {
                // With a review pending on GitHub, the popover's Discard
                // throws that one away (after its confirmation row).
                if session.server_pending.is_some() {
                    return self.apply_review_action(
                        ReviewAction::DiscardServerPending,
                        login,
                        now,
                    );
                }
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
                // With a review pending on GitHub, Submit finishes that one
                // with the verdict and summary picked in the popover.
                if session.server_pending.is_some() {
                    let event = session.pending.event;
                    let body = session.pending.body.clone();
                    return self.apply_review_action(
                        ReviewAction::SubmitServerPending(event, body),
                        login,
                        now,
                    );
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

    /// Whether `session` names the PR currently open: the stale-event rule
    /// for surfaces the app owns, like the AI dialogs (Task 4).
    fn session_is(&self, session: SessionId) -> bool {
        self.session.as_ref().is_some_and(|open| open.id == session)
    }

    /// Whether the user is looking at `session`'s PR: the session is the open
    /// one *and* the detail screen is showing. The AI dialogs open on this
    /// rule, so a plan that lands after Back does not float over the list.
    #[cfg(unix)]
    fn ai_screen_is(&self, session: SessionId) -> bool {
        self.screen == ScreenKind::PrDetail && self.session_is(session)
    }

    /// Whether the app-wide crash-recovery dialog is open. While the user is
    /// answering it, no PR-scoped flow dialog may replace it — the list would
    /// be lost until the next launch (Task 4 review finding 3).
    #[cfg(unix)]
    fn recovery_dialog_open(&self) -> bool {
        matches!(
            self.ai_dialog,
            Some(crate::ui::widgets::ai_dialogs::AiDialog::Recovery(_))
        )
    }

    /// Drop the tab's suggested-commands card once the repo's settings name
    /// commands: the scout's proposals are for repos that have none (spec
    /// §4 step 1), and a stored brief keeps carrying them.
    #[cfg(unix)]
    fn retire_ai_suggestions(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        if session.ai.suggestions.is_none() {
            return;
        }
        let repo = self
            .settings
            .ai
            .repo(&session.host, &session.repo.owner, &session.repo.name);
        if repo_has_commands(repo) {
            session.ai.suggestions = None;
        }
    }

    /// Fold one ✦ AI tab control into the open session and send its worker
    /// command (spec §12).
    ///
    /// Commands carry the open session's id so the stale-event rule keeps
    /// working while a run outlives the PR screen; the local half of each
    /// answer/decision is recorded on the reducer immediately, because the
    /// hub emits no "answered" event.
    #[cfg(unix)]
    fn apply_ai_action(
        &mut self,
        action: crate::ui::widgets::ai_panel::AiPanelAction,
        now: Instant,
    ) {
        use crate::ai::hub::SnapshotInputs;
        use crate::ai::pipeline::RunMode;
        use crate::ai::store::PrKey;
        use crate::ui::widgets::ai_panel::{self, AiPanelAction};

        let Some(session) = &mut self.session else {
            return;
        };
        // The store keys checkouts and runs by the lowercased repo identity.
        let key = PrKey {
            host: session.host.to_lowercase(),
            owner: session.repo.owner.to_lowercase(),
            repo: session.repo.name.to_lowercase(),
            number: session.number,
        };
        match action {
            AiPanelAction::Start => {
                // No folder for this repo: ask for one now (Task 4). The
                // pick dialog validates the choice, saves it, and plans the
                // checkout right away (spec §4 step 2).
                if self
                    .settings
                    .ai
                    .repo_with_folder(&session.host, &session.repo.owner, &session.repo.name)
                    .is_none()
                {
                    self.ai_dialog =
                        Some(crate::ui::widgets::ai_dialogs::AiDialog::PickFolder { key });
                    self.ai_dialog_error = None;
                    return;
                }
                let Some(pr) = ai_target(session) else {
                    // The button is live before the metadata arrives; the
                    // head SHA is what the plan needs.
                    self.toasts.push(
                        "the pull request is still loading — try again in a moment".to_owned(),
                        now,
                    );
                    return;
                };
                self.worker.send(Command::AiPlanCheckout {
                    session: session.id,
                    key,
                    pr,
                });
            }
            AiPanelAction::Continue => {
                if self
                    .settings
                    .ai
                    .repo_with_folder(&session.host, &session.repo.owner, &session.repo.name)
                    .is_none()
                {
                    self.toasts.push(
                        "no AI checkout folder for this repository — pick one in Settings → AI"
                            .to_owned(),
                        now,
                    );
                    return;
                }
                // The tab offers Continue whenever a completed review
                // exists; the head comparison behind it (`can_continue`)
                // is the app's gate, so a stale click gets a toast instead
                // of a redundant run.
                if let Some(head) = session.metadata.as_ref().map(|meta| &meta.head_sha)
                    && !session.ai.can_continue(head)
                {
                    self.toasts.push(
                        "this head is already reviewed or a review is in flight".to_owned(),
                        now,
                    );
                    return;
                }
                // An incremental re-review runs against the head the last
                // completed review saw (spec §5.8); the tab only offers
                // Continue when that head exists.
                let (Some(pr), Some(since_sha), Some(snapshot_inputs)) = (
                    ai_target(session),
                    session.ai.last_reviewed_sha.clone(),
                    session.metadata.as_ref().map(|meta| SnapshotInputs {
                        files: session.files.clone(),
                        metadata: meta.clone(),
                    }),
                ) else {
                    self.toasts.push(
                        "the pull request is still loading — try again in a moment".to_owned(),
                        now,
                    );
                    return;
                };
                self.worker.send(Command::AiStart {
                    session: session.id,
                    key,
                    pr,
                    snapshot_inputs,
                    mode: RunMode::Incremental { since_sha },
                });
            }
            AiPanelAction::Answer { id, answer } => {
                self.worker.send(Command::AiAnswer {
                    session: session.id,
                    key,
                    question_id: id,
                    answer: answer.clone(),
                });
                session.ai.record_answer(id, answer);
            }
            AiPanelAction::Approve { id, decision } => {
                self.worker.send(Command::AiApprove {
                    session: session.id,
                    key,
                    approval_id: id,
                    decision: decision.clone(),
                });
                session
                    .ai
                    .record_approval(id, ai_panel::decision_text(&decision).to_owned());
            }
            AiPanelAction::ApprovePrepare { id, ok } => {
                self.worker.send(Command::AiApprovePrepare {
                    session: session.id,
                    key,
                    approval_id: id,
                    ok,
                });
                session
                    .ai
                    .record_approval(id, if ok { "Run" } else { "No" }.to_owned());
            }
            AiPanelAction::SendChat(text) => {
                self.worker.send(Command::AiUserMessage {
                    session: session.id,
                    key,
                    text: text.clone(),
                });
                session.ai.push_chat_user(text);
            }
            AiPanelAction::Pause => {
                self.worker.send(Command::AiPause {
                    session: session.id,
                    key,
                });
                session.ai.set_paused(true);
            }
            AiPanelAction::Resume => {
                self.worker.send(Command::AiResume {
                    session: session.id,
                    key,
                });
                session.ai.set_paused(false);
            }
            // A cancelled plan has no run to cancel in the worker: the tab's
            // own `Planning` is cleared here. That is the backstop when a
            // plan's dialog never reached the screen (Task 4 review
            // findings 1 and 3).
            AiPanelAction::Cancel
                if session.ai.phase == crate::state::ai_review::AiPhase::Planning =>
            {
                session.ai.cancel_planning();
            }
            AiPanelAction::Cancel => self.worker.send(Command::AiCancel {
                session: session.id,
                key,
            }),
            AiPanelAction::Finish => self.worker.send(Command::AiFinish {
                session: session.id,
                key,
                discard: None,
            }),
            // Scroll the diff to the finding's file and line (or its file's
            // top for a summary item), the same jump threads use.
            AiPanelAction::JumpToFinding(id) => {
                use crate::ai::review::FindingAnchor;

                let Some(anchor) = session.ai.findings.get(&id).map(|f| f.anchor.clone()) else {
                    return;
                };
                let Some(index) = session
                    .files
                    .iter()
                    .position(|loaded| loaded.file.path == anchor.path())
                else {
                    return;
                };
                session.select_file(index);
                let row = match &anchor {
                    FindingAnchor::Line(anchor) => {
                        session
                            .row_model
                            .row_of_line(&session.files, anchor.side, anchor.line)
                    }
                    FindingAnchor::Summary { .. } => session
                        .row_model
                        .file_ranges
                        .get(index)
                        .map(|range| range.start),
                };
                if let Some(row) = row {
                    self.pr_detail.diff.scroll_to_row = Some(row);
                }
            }
            AiPanelAction::ShowDropped(show) => {
                session.ai.show_dropped = show;
                // The diff's inline rows follow the toggle (AI Task 5).
                session.rebuild_rows();
            }
            AiPanelAction::AddSuggestedCommands => {
                // Copy the scout's proposals into the repo's settings and
                // retire the card (Task 4). The run only suggests when the
                // repo entry exists, so this updates rather than creates.
                let Some(suggestions) = session.ai.suggestions.clone() else {
                    return;
                };
                let repo_key = format!(
                    "{}/{}/{}",
                    session.host, session.repo.owner, session.repo.name
                )
                .to_lowercase();
                match self
                    .settings
                    .ai
                    .repos
                    .iter_mut()
                    .find(|repo| repo.repo.to_lowercase() == repo_key)
                {
                    Some(repo) => {
                        repo.commands = suggestions.commands;
                        repo.prepare = suggestions.prepare;
                    }
                    None => self.settings.ai.repos.push(RepoAiSettings {
                        repo: repo_key,
                        commands: suggestions.commands,
                        prepare: suggestions.prepare,
                        ..Default::default()
                    }),
                }
                session.ai.suggestions = None;
                self.save_settings(now);
            }
        }
    }

    /// Collect the OS folder picker's answer once per frame (Task 4). The
    /// picker runs on a plain thread because `rfd` blocks; a cancelled
    /// picker leaves the pick dialog open for another try.
    #[cfg(unix)]
    fn poll_folder_picker(&mut self, now: Instant) {
        use std::sync::mpsc::TryRecvError;

        let answer = match &self.folder_picker {
            Some(receiver) => match receiver.try_recv() {
                Ok(answer) => Some(answer),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(None),
            },
            None => return,
        };
        let Some(answer) = answer else { return };
        self.folder_picker = None;
        if let Some(path) = answer {
            self.apply_ai_dialog_action(
                crate::ui::widgets::ai_dialogs::AiDialogAction::FolderChosen(path),
                now,
            );
        }
    }

    /// Paint the open AI dialog over every screen, like the settings
    /// window (Task 4).
    #[cfg(unix)]
    fn render_ai_dialog(&mut self, ctx: &egui::Context, now: Instant) {
        use crate::ui::widgets::ai_dialogs::{self, DialogInputs};

        let Some(dialog) = self.ai_dialog.clone() else {
            return;
        };
        let pr = self.session.as_ref().and_then(ai_target);
        let prepare = self
            .session
            .as_ref()
            .and_then(|session| {
                self.settings.ai.repo_with_folder(
                    &session.host,
                    &session.repo.owner,
                    &session.repo.name,
                )
            })
            .map(|repo| repo.prepare.clone())
            .unwrap_or_default();
        let mut actions = Vec::new();
        let inputs = DialogInputs {
            pr: pr.as_ref(),
            prepare: &prepare,
            error: self.ai_dialog_error.as_deref(),
        };
        ai_dialogs::render(ctx, &dialog, &inputs, &mut actions);
        for action in actions {
            self.apply_ai_dialog_action(action, now);
        }
    }

    /// Apply one AI dialog answer (Task 4): the pick dialog validates and
    /// saves the folder then plans the checkout, the confirm dialog starts
    /// the run, the restore dialog answers the finish, and the recovery
    /// dialog restores stored checkouts one at a time.
    #[cfg(unix)]
    fn apply_ai_dialog_action(
        &mut self,
        action: crate::ui::widgets::ai_dialogs::AiDialogAction,
        now: Instant,
    ) {
        use crate::ai::hub::SnapshotInputs;
        use crate::ai::pipeline::RunMode;
        use crate::ai::store::PrKey;
        use crate::ui::widgets::ai_dialogs::{self, AiDialog, AiDialogAction};

        match action {
            AiDialogAction::PickFolder => {
                if self.folder_picker.is_some() {
                    return; // one picker at a time
                }
                // `rfd` blocks its calling thread (on macOS it dispatches
                // the panel to the main thread and waits), so the GUI
                // thread must not call it; the answer arrives over the
                // channel `poll_folder_picker` reads.
                let (sender, receiver) = std::sync::mpsc::channel();
                self.folder_picker = Some(receiver);
                std::thread::spawn(move || {
                    let _ = sender.send(rfd::FileDialog::new().pick_folder());
                });
            }
            AiDialogAction::FolderChosen(path) => {
                let Some(AiDialog::PickFolder { key }) = self.ai_dialog.clone() else {
                    return;
                };
                if let Some(error) = ai_dialogs::folder_error(&path, &key) {
                    self.ai_dialog_error = Some(error);
                    return;
                }
                ai_dialogs::save_folder(&mut self.settings.ai, &key, path);
                self.save_settings(now);
                self.ai_dialog = None;
                self.ai_dialog_error = None;
                // The pick was asked for by this PR's Start: plan the
                // checkout right away (spec §4 step 2).
                let open_matches = self.session.as_ref().is_some_and(|open| {
                    open.number == key.number
                        && open.host.eq_ignore_ascii_case(&key.host)
                        && open.repo.owner.eq_ignore_ascii_case(&key.owner)
                        && open.repo.name.eq_ignore_ascii_case(&key.repo)
                });
                if !open_matches {
                    return;
                }
                let session = self.session.as_mut().expect("checked above");
                let Some(pr) = ai_target(session) else {
                    return; // no metadata yet: nothing to check out
                };
                self.worker.send(Command::AiPlanCheckout {
                    session: session.id,
                    key,
                    pr,
                });
            }
            AiDialogAction::ConfirmCheckout => {
                self.ai_dialog = None;
                let Some(session) = &mut self.session else {
                    return;
                };
                let key = PrKey {
                    host: session.host.to_lowercase(),
                    owner: session.repo.owner.to_lowercase(),
                    repo: session.repo.name.to_lowercase(),
                    number: session.number,
                };
                let (Some(pr), Some(snapshot_inputs)) = (
                    ai_target(session),
                    session.metadata.as_ref().map(|meta| SnapshotInputs {
                        files: session.files.clone(),
                        metadata: meta.clone(),
                    }),
                ) else {
                    return;
                };
                self.worker.send(Command::AiStart {
                    session: session.id,
                    key,
                    pr,
                    snapshot_inputs,
                    mode: RunMode::Full,
                });
            }
            AiDialogAction::CancelCheckout => {
                // Cancelling a plan awaiting confirmation rests the tab;
                // dismissing the held notice or the pick dialog changes
                // nothing else.
                let was_confirm = matches!(self.ai_dialog, Some(AiDialog::ConfirmCheckout(_)));
                self.ai_dialog = None;
                self.ai_dialog_error = None;
                if was_confirm && let Some(session) = &mut self.session {
                    session.ai.cancel_planning();
                }
            }
            AiDialogAction::RestoreDiscard | AiDialogAction::RestoreKeep => {
                // "Leave as is" is a finish that keeps the checkout: the
                // hub closes the run, marks the record, and answers with
                // `AiNotice` (spec §7.3 step 1).
                let discard = action == AiDialogAction::RestoreDiscard;
                self.ai_dialog = None;
                let Some(session) = &mut self.session else {
                    return;
                };
                let key = PrKey {
                    host: session.host.to_lowercase(),
                    owner: session.repo.owner.to_lowercase(),
                    repo: session.repo.name.to_lowercase(),
                    number: session.number,
                };
                self.worker.send(Command::AiFinish {
                    session: session.id,
                    key,
                    discard: Some(discard),
                });
            }
            AiDialogAction::RecoverRestore(id) => {
                self.worker.send(Command::AiRecoverRestore { id });
                // Drop the restored row from the recovery list (closing it
                // when the last one goes), and close a Held dialog — the
                // restore it offered is on its way and the toast reports
                // the outcome.
                match &mut self.ai_dialog {
                    Some(AiDialog::Recovery(records)) => {
                        records.retain(|record| record.id != id);
                        if records.is_empty() {
                            self.ai_dialog = None;
                        }
                    }
                    Some(AiDialog::Held { .. }) => self.ai_dialog = None,
                    _ => {}
                }
            }
            AiDialogAction::RecoverLater => self.ai_dialog = None,
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
                DiffAction::EditPending { local_id } => {
                    if let Some(session) = &mut self.session {
                        session.edit_pending_via_composer(local_id, now);
                    }
                }
                #[cfg(unix)]
                DiffAction::AiKeep(id) => {
                    use crate::ai::review::FindingState;

                    let Some(session) = &mut self.session else {
                        continue;
                    };
                    match crate::state::pr_detail::keep_draft(session, id) {
                        Ok(kept) => {
                            if let Some(finding) = session.ai.findings.get_mut(&id) {
                                finding.state = FindingState::Kept;
                                finding.pending_local_id = kept.pending_local_id;
                            }
                            session.rebuild_rows();
                            session.mark_session_dirty(now);
                            let command = Command::AiFindingState {
                                session: session.id,
                                key: pr_key(session),
                                id,
                                state: FindingState::Kept,
                                pending_local_id: kept.pending_local_id,
                            };
                            self.worker.send(command);
                        }
                        Err(why) => self.toasts.push(why, now),
                    }
                }
                // The widget already wrote the text into the finding; the
                // store is the durable copy (spec §12).
                #[cfg(unix)]
                DiffAction::AiEdit(id, body) => {
                    let Some(session) = &self.session else {
                        continue;
                    };
                    let command = Command::AiEditFinding {
                        session: session.id,
                        key: pr_key(session),
                        id,
                        body,
                    };
                    self.worker.send(command);
                }
                #[cfg(unix)]
                DiffAction::AiDrop(id) => {
                    use crate::ai::review::FindingState;

                    let Some(session) = &mut self.session else {
                        continue;
                    };
                    if let Some(finding) = session.ai.findings.get_mut(&id) {
                        finding.state = FindingState::DroppedByUser;
                    }
                    session.rebuild_rows();
                    session.mark_session_dirty(now);
                    let command = Command::AiFindingState {
                        session: session.id,
                        key: pr_key(session),
                        id,
                        state: FindingState::DroppedByUser,
                        pending_local_id: None,
                    };
                    self.worker.send(command);
                }
                // Why? asks the chat agent about the finding and brings the
                // ✦ AI tab forward to read the answer (spec §12).
                #[cfg(unix)]
                DiffAction::AiWhy(id) => {
                    let Some(session) = &mut self.session else {
                        continue;
                    };
                    let Some(finding) = session.ai.findings.get(&id).cloned() else {
                        continue;
                    };
                    let text = crate::state::ai_review::why_question(&finding);
                    let command = Command::AiUserMessage {
                        session: session.id,
                        key: pr_key(session),
                        text: text.clone(),
                    };
                    self.worker.send(command);
                    session.ai.push_chat_user(text);
                    self.pr_detail.details_tab = crate::ui::widgets::metadata_panel::DetailsTab::Ai;
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
            if let Some(action) = command::resolve_key(press, text_focused)
                && global_key_allowed(action, text_focused)
            {
                self.dispatch(action, now);
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
            "find" => self.pr_detail.find_open = true,
            "settings" => self.open_settings(),
            "help" => self.help_open = !self.help_open,
            "close" => self.dispatch_close(now),
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
                // Task 12: with the review popover closed the shortcut opens
                // it; with it open the shortcut submits, subject to the same
                // rule and all (Task 44).
                if self.session.is_some() && !self.pr_detail.review_open {
                    self.pr_detail.review_open = true;
                } else {
                    let login = self.login();
                    for command in self.apply_review_action(ReviewAction::Submit, &login, now) {
                        self.worker.send(command);
                    }
                }
            }
            _ => {}
        }
    }

    /// Run a menu bar item: the app-level ones here, the rest through the
    /// same [`ReviewApp::dispatch`] the shortcuts and the palette use.
    fn dispatch_menu(&mut self, id: &str, ctx: &egui::Context, now: Instant) {
        let Some(action) = crate::ui::menu::action_for(id) else {
            return;
        };
        match action {
            "refresh" => match self.screen {
                ScreenKind::PrList => self.apply_pr_list(PrListMsg::Refresh, now),
                ScreenKind::PrDetail => {
                    if let Some(session) = &mut self.session {
                        for command in session.refresh() {
                            self.worker.send(command);
                        }
                    }
                }
                ScreenKind::Login => {}
            },
            "open_github" => {
                if let Some(metadata) = self.session.as_ref().and_then(|s| s.metadata.as_ref()) {
                    ctx.open_url(egui::OpenUrl::new_tab(metadata.html_url.clone()));
                }
            }
            "back" => {
                if self.screen == ScreenKind::PrDetail {
                    self.close_detail();
                }
            }
            "toggle_theme" => self.toggle_theme(ctx, now),
            "sign_out" => self.perform_sign_out(now),
            action => self.dispatch(action, now),
        }
    }

    /// `Esc`: dismiss the topmost surface, then back out of the detail screen.
    ///
    /// The composer cancel belongs to the detail screen: on the list the
    /// hidden session's open composer must not swallow the `Esc` that was
    /// meant for the visible screen. Leaving for the list flushes the
    /// session's debounced writes (spec §11).
    fn dispatch_close(&mut self, now: Instant) {
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
        // An AI dialog floats over the screen and owns `Esc` before it: the
        // key answers the dialog (or is refused by the restore decision) and
        // must never also leave the PR behind it (Task 4 review finding 1).
        #[cfg(unix)]
        if let Some(dialog) = self.ai_dialog.clone() {
            if let Some(action) = crate::ui::widgets::ai_dialogs::dismiss_action(&dialog) {
                self.apply_ai_dialog_action(action, now);
            }
            return;
        }
        if self.screen != ScreenKind::PrDetail {
            return;
        }
        // Popovers and the find popup own `Esc` before the screen does:
        // `handle_shortcuts` runs before `render_screen`, so closing them
        // here is the only path that beats `close_detail` to it (Task 17 QA).
        if self.pr_detail.find_open || self.pr_detail.diff.find_open {
            self.pr_detail.find_open = false;
            self.pr_detail.diff.find_open = false;
            self.pr_detail.diff.clear_find();
            return;
        }
        if self.pr_detail.review_open {
            self.pr_detail.review_open = false;
            return;
        }
        if self.pr_detail.filter_bar.popover_open {
            self.pr_detail.filter_bar.popover_open = false;
            return;
        }
        if let Some(session) = &mut self.session
            && session.composer.target.is_some()
        {
            composer::reduce(&mut session.composer, composer::ComposerMsg::Cancel);
            session.rebuild_rows();
            return;
        }
        self.close_detail();
    }

    /// Fold the file header bar's actions in (Task 7): the file-level ones
    /// act on the current file, the layout ones ride the same paths the
    /// keyboard shortcuts take.
    fn apply_file_header_action(
        &mut self,
        action: FileHeaderAction,
        ctx: &egui::Context,
        now: Instant,
    ) {
        let path = self
            .session
            .as_ref()
            .and_then(|session| session.current_file())
            .map(|(_, loaded)| loaded.file.path.clone());
        match action {
            FileHeaderAction::CopyPath => {
                if let Some(path) = path {
                    ctx.copy_text(path);
                }
            }
            FileHeaderAction::OpenInBrowser => {
                if let Some(url) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.metadata.as_ref())
                    .zip(path)
                    .map(|(metadata, path)| {
                        crate::ui::widgets::file_header::file_url(&metadata.html_url, &path)
                    })
                {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            }
            FileHeaderAction::CommentOnFile => {
                if let (Some(session), Some(path)) = (&mut self.session, path) {
                    session.open_comment(CommentTarget::File { path });
                }
            }
            FileHeaderAction::ToggleViewed => self.set_viewed_current(None, now),
            FileHeaderAction::SetMode(mode) => {
                if let Some(session) = &mut self.session {
                    session.set_mode(mode);
                }
                self.pr_detail.diff.clear_find();
                self.pr_detail.last_layout = None;
            }
            FileHeaderAction::ToggleWrap => {
                self.pr_detail.diff.wrap = !self.pr_detail.diff.wrap;
                // The wrap width changed, so every measured height is stale.
                self.pr_detail.last_layout = None;
            }
            FileHeaderAction::ToggleHideWhitespace => {
                if let Some(session) = &mut self.session {
                    session.toggle_hide_whitespace();
                }
                self.pr_detail.diff.clear_find();
            }
            FileHeaderAction::OpenFind => self.pr_detail.find_open = true,
            FileHeaderAction::SetImageMode(mode) => self.pr_detail.diff.image_mode = mode,
            FileHeaderAction::ToggleRendered => {
                if let Some(session) = &mut self.session
                    && let Some(index) = session.current_file().map(|(index, _)| index as u32)
                {
                    session.toggle_rendered_svg(index);
                }
            }
        }
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
            // Every hunk now has a header bar: that bar is the hunk's first
            // visible row.
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
                    RowRef::HunkHeader { file: f, hunk } if *f as usize == file => {
                        starts.push(index);
                        last_hunk = Some(*hunk);
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
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let now = Instant::now();
        // The traffic lights belong in the middle of the app's title bar;
        // full screen hides the title bar, and them, altogether.
        #[cfg(target_os = "macos")]
        if title_bar::UNDER_TITLE_BAR && !ui.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            crate::ui::traffic_lights::center(frame, f64::from(title_bar::TITLE_BAR_H));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = frame;
        for event in self.events.drain() {
            self.handle_event(event, now);
        }
        // The OS folder picker answers on a background thread (Task 4);
        // collect it once per frame like the worker's events.
        #[cfg(unix)]
        self.poll_folder_picker(now);

        // Spec §13: the rate-limit window is over once its reset instant
        // passes; check every frame so the write surfaces wake up on their
        // own.
        let (write_blocked, until) =
            write_blocked_label(self.write_blocked_until, SystemTime::now());
        self.write_blocked_until = until;

        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);

        // Menu bar clicks run like shortcuts, before the screens draw.
        let clicked = match &mut self.menu {
            Some(menu) => {
                menu.set_pr_open(self.screen == ScreenKind::PrDetail);
                menu.drain()
            }
            None => Vec::new(),
        };
        for id in clicked {
            self.dispatch_menu(&id, &ctx, now);
        }

        // Global shortcuts run before the screens draw, so a file selected
        // by `j`/`k` is rebuilt and drawn on this very frame.
        self.handle_shortcuts(&ctx, now);
        self.render_screen(ui, now, write_blocked.as_deref());
        self.sync_layout(now);

        // The palette, the settings window, and the help overlay float above
        // every screen. The palette runs its selection through the same
        // dispatch as a keybinding.
        let entries = command::registry();
        // Cloning the session's files is only worth it while the palette is
        // actually open (Task 17 review: no per-frame clone otherwise).
        let palette_files: Vec<crate::github::files::PrFile> = if self.palette.open {
            self.session
                .as_ref()
                .map(|s| s.files.iter().map(|l| l.file.clone()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut palette_actions: Vec<command_palette::PaletteSelection> = Vec::new();
        command_palette::render(
            &ctx,
            &mut self.palette,
            &entries,
            &palette_files,
            &mut palette_actions,
        );
        for selection in palette_actions {
            match selection {
                command_palette::PaletteSelection::Action(action) => self.dispatch(action, now),
                command_palette::PaletteSelection::File(index) => {
                    if let Some(session) = &mut self.session {
                        session.select_file(index as usize);
                    }
                }
            }
        }
        let mut settings_actions = Vec::new();
        let account = self
            .settings
            .account
            .as_ref()
            .map(|account| settings_window::AccountInfo {
                login: &account.login,
                host: &self.settings.host,
                scopes: &account.scopes,
            });
        settings_window::render(
            &ctx,
            &mut self.settings_window,
            account,
            &mut settings_actions,
        );
        self.apply_settings_actions(settings_actions, &ctx, now);
        // The AI dialogs float above every screen too (Task 4).
        #[cfg(unix)]
        self.render_ai_dialog(&ctx, now);
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
            .with_min_inner_size([900.0, 600.0])
            // The app's own title bar shares the window's (mockup
            // `.titlebar`): content runs under a transparent title bar and
            // only the traffic lights stay (see `widgets::title_bar`).
            .with_fullsize_content_view(title_bar::UNDER_TITLE_BAR)
            .with_titlebar_shown(!title_bar::UNDER_TITLE_BAR)
            .with_title_shown(!title_bar::UNDER_TITLE_BAR),
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

    /// The scout's suggested commands are a first-run aid only: once the repo
    /// names commands of its own, the tab retires the card (spec §4 step 1).
    #[cfg(unix)]
    #[test]
    fn suggestions_retire_once_the_repo_has_commands() {
        use crate::persist::settings::RepoAiSettings;

        assert!(!repo_has_commands(None), "no repo row, nothing configured");
        assert!(!repo_has_commands(Some(&RepoAiSettings::default())));
        let with_commands = RepoAiSettings {
            commands: vec!["cargo test".to_string()],
            ..Default::default()
        };
        assert!(repo_has_commands(Some(&with_commands)));
        let with_prepare = RepoAiSettings {
            prepare: vec!["cargo fetch".to_string()],
            ..Default::default()
        };
        assert!(repo_has_commands(Some(&with_prepare)));
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
