//! The optional local-classifier stage: one more [`SegmentPolicy`], named
//! `"ml"`, consulted only for segments no rule vouched for.
//!
//! Fail closed: a classifier that panics or reports `Unavailable` leaves the
//! segment exactly as the engine would have it with no classifier. A model
//! `Safe` approves only what [`model_may_approve`] allows, and a model
//! `Dangerous` is a deny the engine turns into an ask with a warning.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use crate::model::Segment;
use crate::never_approve::model_may_approve;
use crate::policy::{PolicyVerdict, SegmentPolicy};

/// What the classifier thinks of one segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelOutcome {
    Safe,
    NeedsApproval,
    Dangerous,
    Unavailable,
}

pub trait CommandClassifier: Send + Sync {
    /// `segment` is argv joined by single spaces. The caller enforces the
    /// timeout, but it can only stop waiting, not stop the call: an
    /// implementation must return `Unavailable` promptly when it is busy
    /// (use `try_lock`, never queue behind another call), must never block
    /// indefinitely, and should return `Unavailable` instead of panicking.
    fn classify(&self, segment: &str) -> ModelOutcome;

    /// `false` means the model must not influence decisions right now,
    /// cached outcomes included (for example the user switched it off during
    /// a run). Checked before the cache is read.
    fn active(&self) -> bool {
        true
    }
}

/// Most entries held; the cache is emptied when a new key arrives at the cap.
const CACHE_CAP: usize = 4096;

/// Outcomes by segment text, shared between threads.
#[derive(Default)]
pub struct DecisionCache {
    map: Mutex<HashMap<String, ModelOutcome>>,
}

impl DecisionCache {
    pub fn new() -> DecisionCache {
        DecisionCache::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ModelOutcome>> {
        // A poisoned map is still a valid map of earlier outcomes.
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, k: &str) -> Option<ModelOutcome> {
        self.lock().get(k).copied()
    }

    pub fn put(&self, k: String, v: ModelOutcome) {
        let mut map = self.lock();
        if map.len() >= CACHE_CAP && !map.contains_key(&k) {
            map.clear();
        }
        map.insert(k, v);
    }
}

pub struct ModelPolicy<'a> {
    pub classifier: &'a dyn CommandClassifier,
    pub cache: &'a DecisionCache,
    /// When set, a segment not already cached is not sent to the classifier
    /// once this instant has passed (the caller's timeout for the whole
    /// command), so one slow call is the last for that command.
    pub deadline: Option<Instant>,
}

impl SegmentPolicy for ModelPolicy<'_> {
    fn name(&self) -> &'static str {
        crate::engine::ML_NAME
    }

    fn verify(&self, seg: &Segment, _cwd: Option<&Path>) -> Option<PolicyVerdict> {
        if seg.argv.is_empty() || !self.classifier.active() {
            return None;
        }
        let key = seg.argv.join(" ");
        let outcome = match self.cache.get(&key) {
            Some(o) => o,
            None => {
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    return None;
                }
                match catch_unwind(AssertUnwindSafe(|| self.classifier.classify(&key))) {
                    // Unavailable (still loading, transient timeout) is not
                    // cached, so the next call retries.
                    Ok(o) => {
                        if o != ModelOutcome::Unavailable {
                            self.cache.put(key, o);
                        }
                        o
                    }
                    // A panic counts as unavailable and is not cached either.
                    Err(_) => ModelOutcome::Unavailable,
                }
            }
        };
        match outcome {
            ModelOutcome::Safe => model_may_approve(seg).then_some(PolicyVerdict::Approve),
            ModelOutcome::Dangerous => Some(PolicyVerdict::Deny),
            ModelOutcome::NeedsApproval | ModelOutcome::Unavailable => None,
        }
    }
}
