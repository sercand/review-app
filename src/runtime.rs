//! The tokio runtime and the `Command`/`Event` bridge between the UI and the
//! network.
//!
//! The UI thread never blocks on I/O: it sends [`Command`]s to a [`Worker`] and
//! drains [`Event`]s once per frame. Everything network-shaped happens on the
//! worker's own multi-thread tokio runtime.

use std::future::Future;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinError;

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
