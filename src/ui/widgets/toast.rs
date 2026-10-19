//! Transient toast messages.
//!
//! A toast is a short-lived line of text drawn over the app: worker panics,
//! transient failures, and small confirmations ("signed in as …"). Toasts never
//! queue unboundedly — at most [`Toasts::MAX_VISIBLE`] are shown, each for
//! [`Toasts::LIFETIME`].

use std::time::{Duration, Instant};

use eframe::egui;

/// One queued toast.
#[derive(Clone, Debug)]
struct Toast {
    message: String,
    shown_at: Instant,
}

/// The toast queue.
#[derive(Default)]
pub struct Toasts {
    toasts: Vec<Toast>,
}

impl Toasts {
    /// How long each toast stays on screen.
    pub const LIFETIME: Duration = Duration::from_secs(5);
    /// How many toasts are shown at once (the newest ones).
    pub const MAX_VISIBLE: usize = 3;

    /// Queue `msg`, raised at `now`.
    ///
    /// Expired toasts are dropped, and the queue is capped at
    /// [`Toasts::MAX_VISIBLE`] so a burst of messages can never grow it without
    /// bound.
    pub fn push(&mut self, msg: String, now: Instant) {
        self.toasts
            .retain(|toast| now.saturating_duration_since(toast.shown_at) < Self::LIFETIME);
        self.toasts.push(Toast {
            message: msg,
            shown_at: now,
        });
        while self.toasts.len() > Self::MAX_VISIBLE {
            self.toasts.remove(0);
        }
    }

    /// The messages still on screen at `now`, oldest first (at most
    /// [`Toasts::MAX_VISIBLE`]).
    pub fn visible(&self, now: Instant) -> Vec<&str> {
        let live: Vec<&Toast> = self
            .toasts
            .iter()
            .filter(|toast| now.saturating_duration_since(toast.shown_at) < Self::LIFETIME)
            .collect();
        let first = live.len().saturating_sub(Self::MAX_VISIBLE);
        live[first..]
            .iter()
            .map(|toast| toast.message.as_str())
            .collect()
    }

    /// Draw the visible toasts in the bottom-right corner.
    pub fn render(&self, ui: &mut egui::Ui, now: Instant) {
        let visible = self.visible(now);
        if visible.is_empty() {
            return;
        }

        egui::Area::new(egui::Id::unique("gh-review-toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
            .interactable(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(420.0);
                    for message in visible {
                        ui.label(message);
                    }
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn toasts_expire_and_cap() {
        let t0 = Instant::now();
        let mut t = Toasts::default();
        for i in 0..4 {
            t.push(format!("m{i}"), t0);
        }
        assert_eq!(t.visible(t0), vec!["m1", "m2", "m3"]); // newest three
        assert!(t.visible(t0 + Duration::from_secs(6)).is_empty());
    }

    #[test]
    fn an_empty_queue_shows_nothing() {
        let t = Toasts::default();
        assert!(t.visible(Instant::now()).is_empty());
    }
}
