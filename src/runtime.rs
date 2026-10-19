//! The tokio runtime and the `Command`/`Event` bridge between the UI and the
//! network.
//!
//! The UI thread never blocks on I/O: it sends [`Command`]s to a [`Worker`] and
//! drains [`Event`]s once per frame. Everything network-shaped happens on the
//! worker's own multi-thread tokio runtime.

use std::future::Future;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::task::JoinError;

use crate::auth::device_flow::{AuthFailure, DeviceCode};
use crate::auth::device_http::{run_device_flow, HttpDeviceFlow};
use crate::auth::store::TokenStore;
use crate::auth::DEFAULT_SCOPE;
use crate::github::client::{endpoints_for, GithubClient, Me};
use crate::github::error::GhError;

/// Identifies one open PR's session.
///
/// Assigned when a PR is opened and echoed by every PR-scoped [`Event`], so a
/// slow response for a PR the user has left can be dropped (the stale-event
/// rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(pub u64);

/// A unit of work handed from the UI to the worker.
#[derive(Debug)]
pub enum Command {
    /// Round-trip probe used by the bridge tests.
    Ping(u32),
    /// Ask the worker to stop.
    Shutdown,
    /// Start the OAuth device flow against `host` with `client_id`.
    StartDeviceFlow {
        /// The host to authenticate against (`github.com` or Enterprise).
        host: String,
        /// The public OAuth App client ID to use.
        client_id: String,
    },
    /// Validate a pasted personal access token and sign in with it.
    SubmitPat {
        /// The host to authenticate against.
        host: String,
        /// The token the user pasted.
        token: String,
    },
}

/// A result handed from the worker back to the UI.
#[derive(Debug)]
pub enum Event {
    /// Reply to [`Command::Ping`].
    Pong(u32),
    /// A transient message to show as a toast.
    Toast(String),
    /// A worker task panicked; carries the panic message.
    WorkerPanicked(String),
    /// A command failed. `session` is `None` for commands that are not
    /// PR-scoped.
    Failed {
        /// The session that issued the command, when the command was
        /// PR-scoped.
        session: Option<SessionId>,
        /// The classified failure.
        error: GhError,
    },
    /// The device flow returned a code for the user to enter.
    DeviceCode(DeviceCode),
    /// Sign-in succeeded and `me` was validated with `GET /user`.
    LoggedIn {
        /// The authenticated account.
        me: Me,
        /// The access token (or PAT) that produced it.
        token: String,
    },
    /// Sign-in could not complete.
    AuthFailed(AuthFailure),
}

/// Sends [`Event`]s from worker tasks to the UI thread.
///
/// Cloning is cheap; every clone shares the same channel and repaint callback.
#[derive(Clone)]
pub struct EventSender {
    tx: Sender<Event>,
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EventSender {
    /// Send an event, then request a repaint so the UI picks it up promptly.
    ///
    /// A send failure just means the receiver is gone (the app is shutting
    /// down), so it is ignored.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        if let Some(repaint) = &self.repaint {
            repaint();
        }
    }

    /// Attach the callback invoked after every [`EventSender::send`].
    ///
    /// The app passes `ctx.request_repaint()`; the default (no callback) is
    /// used by tests that poll the channel directly.
    pub fn with_repaint(mut self, repaint: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.repaint = Some(repaint);
        self
    }
}

/// Receives [`Event`]s on the UI thread.
pub struct EventReceiver {
    rx: Receiver<Event>,
}

impl EventReceiver {
    /// Take everything queued since the last call, without blocking.
    ///
    /// Called once per frame by the app.
    pub fn drain(&self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Wait up to `timeout` for a single event.
    ///
    /// Returns `None` on timeout or when every sender has been dropped.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// Create a connected [`EventSender`]/[`EventReceiver`] pair.
pub fn event_channel() -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel();
    (EventSender { tx, repaint: None }, EventReceiver { rx })
}

/// A handle to the background worker.
///
/// Dropping the last `Worker` closes the command channel; the worker thread
/// then finishes and its runtime is torn down.
pub struct Worker {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl Worker {
    /// Queue a command. Silently ignored if the worker has already stopped.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Start the worker: a background thread owning a multi-thread tokio runtime.
///
/// Each command runs as its own task, so a slow command never blocks the ones
/// behind it. A task that panics is turned into [`Event::WorkerPanicked`]
/// rather than taking down the runtime.
pub fn spawn_worker<H, F>(events: EventSender, handler: H) -> Worker
where
    H: Fn(Command, EventSender) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the worker's tokio runtime");

    std::thread::Builder::new()
        .name("gh-review-worker".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                while let Some(command) = command_rx.recv().await {
                    let events = events.clone();
                    let task = tokio::spawn(handler(command, events.clone()));
                    tokio::spawn(async move {
                        if let Err(join_error) = task.await {
                            if join_error.is_panic() {
                                events.send(Event::WorkerPanicked(panic_message(join_error)));
                            }
                        }
                    });
                }
            });
        })
        .expect("failed to spawn the worker thread");

    Worker {
        commands: command_tx,
    }
}

/// Extract a printable message from a panicking task's payload.
fn panic_message(error: JoinError) -> String {
    let payload = error.into_panic();
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_owned(),
            Err(_) => "worker task panicked".to_owned(),
        },
    }
}

/// Shared state every executor arm needs: the HTTP client (once signed in) and
/// the token store.
pub struct ExecCtx {
    /// The installed GitHub client, or `None` before sign-in.
    pub client: RwLock<Option<GithubClient>>,
    /// The OS keychain, shared with the UI thread.
    pub store: Arc<dyn TokenStore>,
}

impl ExecCtx {
    /// A context with no client installed yet.
    pub fn new(store: Arc<dyn TokenStore>) -> ExecCtx {
        ExecCtx {
            client: RwLock::new(None),
            store,
        }
    }

    /// Install the client every later command uses.
    pub fn install_client(&self, client: GithubClient) {
        *self.write_client() = Some(client);
    }

    /// Forget the installed client (sign-out, and after a 401).
    pub fn clear_client(&self) {
        *self.write_client() = None;
    }

    /// The client lock for writing, recovered even if a holder panicked.
    fn write_client(&self) -> std::sync::RwLockWriteGuard<'_, Option<GithubClient>> {
        self.client
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// The single place where [`Command`]s become network calls.
///
/// Every arm reports progress and failure through `events`; nothing here
/// touches the UI, the settings file, or drafts.
pub async fn execute(ctx: Arc<ExecCtx>, cmd: Command, events: EventSender) {
    match cmd {
        Command::Ping(n) => events.send(Event::Pong(n)),
        Command::Shutdown => {}
        Command::StartDeviceFlow { host, client_id } => {
            let flow = HttpDeviceFlow::new(&endpoints_for(&host).web);
            let code_events = events.clone();
            let result = run_device_flow(&flow, &client_id, DEFAULT_SCOPE, |code| {
                code_events.send(Event::DeviceCode(code.clone()));
            })
            .await;

            match result {
                Ok(token) => validate_and_sign_in(&ctx, &host, token, &events).await,
                Err(failure) => events.send(Event::AuthFailed(failure)),
            }
        }
        Command::SubmitPat { host, token } => {
            validate_and_sign_in(&ctx, &host, token, &events).await;
        }
    }
}

/// Prove a fresh credential with `GET /user`, then install it and announce it.
///
/// The token is only stored and the client only installed after `me()` succeeds,
/// so a bad credential leaves the app in its previous state. The UI thread still
/// owns the settings bookkeeping ([`crate::auth::session::complete_login`]),
/// which it performs when it receives [`Event::LoggedIn`].
async fn validate_and_sign_in(ctx: &ExecCtx, host: &str, token: String, events: &EventSender) {
    let client = GithubClient::new(host, &token);
    let me = match client.me().await {
        Ok(me) => me,
        Err(error) => {
            events.send(Event::AuthFailed(AuthFailure::Http(error.to_string())));
            return;
        }
    };

    if let Err(error) = ctx.store.set(host, &me.user.login, &token) {
        tracing::warn!(%error, "could not store the token in the keychain");
        events.send(Event::Toast(format!("could not save the token: {error}")));
    }

    ctx.install_client(client);
    events.send(Event::LoggedIn { me, token });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn command_round_trips_through_worker() {
        let (tx, rx) = event_channel();
        let worker = spawn_worker(tx, |cmd, events| async move {
            if let Command::Ping(n) = cmd {
                events.send(Event::Pong(n));
            }
        });
        worker.send(Command::Ping(7));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Some(Event::Pong(7))
        ));
    }

    #[test]
    fn handler_panic_becomes_an_event() {
        let (tx, rx) = event_channel();
        let worker = spawn_worker(tx, |_cmd, _events| async move { panic!("boom") });
        worker.send(Command::Ping(1));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Some(Event::WorkerPanicked(_))
        ));
    }

    #[test]
    fn sending_requests_a_repaint() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let (tx, _rx) = event_channel();
        let tx = tx.with_repaint(Arc::new(move || {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        tx.send(Event::Pong(1));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
