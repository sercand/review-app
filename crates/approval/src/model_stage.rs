//! The optional local-classifier stage: one more [`SegmentPolicy`], named
//! `"ml"`, consulted only for segments no rule vouched for.
//!
//! Fail closed: a classifier that panics or reports `Unavailable` leaves the
//! segment exactly as the engine would have it with no classifier. A model
//! `Safe` approves only what [`model_may_approve`] allows, and a model
//! `Dangerous` is a deny the engine turns into an ask with a warning.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
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

/// What a classifier is asked about.
#[derive(Clone, Copy, Debug)]
pub struct ClassifyInput<'a> {
    /// argv joined by single spaces.
    pub segment: &'a str,
    /// The working directory the command runs in, when known.
    pub cwd: Option<&'a Path>,
}

/// What the model stage concluded about one command, as shown to the user.
///
/// Recorded per consulted segment, then aggregated by the caller into one
/// note per command. A note exists only when the model was actually asked:
/// commands the rules vouched for on their own show nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifierNote {
    /// Every segment the model saw it approved, and the command ran.
    Approved,
    /// The model approved a segment, but a never-auto-run rule (a listed
    /// binary, a repo path) still had the last word; the command asked.
    Blocked,
    /// The model abstained; the command asked.
    NeedsApproval,
    /// The model flagged the command; the command asked with a warning.
    Dangerous,
    /// The model was asked but could not answer (busy, timed out, failed).
    Unavailable,
}

/// Per-segment [`ClassifierNote`]s of one command, filled by [`ModelPolicy`]
/// while the engine consults it, read by the caller afterwards.
#[derive(Default)]
pub struct ClassifierTrace {
    notes: Mutex<Vec<ClassifierNote>>,
}

impl ClassifierTrace {
    pub fn new() -> ClassifierTrace {
        ClassifierTrace::default()
    }

    fn push(&self, note: ClassifierNote) {
        self.notes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(note);
    }

    /// What was recorded, in order.
    pub fn notes(&self) -> Vec<ClassifierNote> {
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The single note for the command: the most informative segment wins,
    /// and a command that ran is approved whatever the segments said along
    /// the way (they cannot disagree with a `Run`: the model is the last
    /// policy, and anything it withheld asks).
    pub fn note(&self, ran: bool) -> Option<ClassifierNote> {
        let notes = self.notes();
        if notes.is_empty() {
            return None;
        }
        let has = |note| notes.contains(&note);
        if ran {
            Some(ClassifierNote::Approved)
        } else if has(ClassifierNote::Dangerous) {
            Some(ClassifierNote::Dangerous)
        } else if has(ClassifierNote::Blocked) {
            Some(ClassifierNote::Blocked)
        } else if has(ClassifierNote::NeedsApproval) {
            Some(ClassifierNote::NeedsApproval)
        } else {
            Some(ClassifierNote::Unavailable)
        }
    }
}

pub trait CommandClassifier: Send + Sync {
    /// `input.segment` is argv joined by single spaces. The caller enforces the
    /// timeout, but it can only stop waiting, not stop the call: an
    /// implementation must return `Unavailable` promptly when it is busy
    /// (use `try_lock`, never queue behind another call), must never block
    /// indefinitely, and should return `Unavailable` instead of panicking.
    fn classify(&self, input: &ClassifyInput<'_>) -> ModelOutcome;

    /// `false` means the model must not influence decisions right now,
    /// cached outcomes included (for example the user switched it off during
    /// a run). Checked before the cache is read.
    fn active(&self) -> bool {
        true
    }

    /// Identifies the model behind this classifier. When it changes, outcomes
    /// cached under the earlier value are dropped before the cache is read,
    /// so a verdict never outlives the model that gave it.
    fn generation(&self) -> u64 {
        0
    }
}

/// Most entries held; the cache is emptied when a new key arrives at the cap.
const CACHE_CAP: usize = 4096;

/// Outcomes by (working directory, segment text), shared between threads.
type CacheKey = (Option<PathBuf>, String);

#[derive(Default)]
pub struct DecisionCache {
    map: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    /// The classifier generation the entries were produced under.
    generation: u64,
    entries: HashMap<CacheKey, ModelOutcome>,
}

impl DecisionCache {
    pub fn new() -> DecisionCache {
        DecisionCache::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        // A poisoned map is still a valid map of earlier outcomes.
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Empties the cache when `generation` is not the one it was filled under.
    pub fn sync_generation(&self, generation: u64) {
        let mut state = self.lock();
        if state.generation != generation {
            state.entries.clear();
            state.generation = generation;
        }
    }

    pub fn get(&self, cwd: Option<&Path>, segment: &str) -> Option<ModelOutcome> {
        self.lock()
            .entries
            .get(&(cwd.map(Path::to_path_buf), segment.to_string()))
            .copied()
    }

    pub fn put(&self, cwd: Option<&Path>, segment: String, v: ModelOutcome) {
        let k = (cwd.map(Path::to_path_buf), segment);
        let mut state = self.lock();
        if state.entries.len() >= CACHE_CAP && !state.entries.contains_key(&k) {
            state.entries.clear();
        }
        state.entries.insert(k, v);
    }
}

pub struct ModelPolicy<'a> {
    pub classifier: &'a dyn CommandClassifier,
    pub cache: &'a DecisionCache,
    /// When set, a segment not already cached is not sent to the classifier
    /// once this instant has passed (the caller's timeout for the whole
    /// command), so one slow call is the last for that command.
    pub deadline: Option<Instant>,
    /// Where to record what the model said about each segment, so the
    /// caller can show the user the classifier's verdict for the command.
    /// `None` records nothing.
    pub trace: Option<&'a ClassifierTrace>,
}

impl SegmentPolicy for ModelPolicy<'_> {
    fn name(&self) -> &'static str {
        crate::engine::ML_NAME
    }

    fn verify(&self, seg: &Segment, cwd: Option<&Path>) -> Option<PolicyVerdict> {
        if seg.argv.is_empty() || !self.classifier.active() {
            return None;
        }
        self.cache.sync_generation(self.classifier.generation());
        let key = seg.argv.join(" ");
        let outcome = match self.cache.get(cwd, &key) {
            Some(o) => o,
            None => {
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    // Past the caller's deadline nothing more is asked of
                    // the model; that is a skip, not an answer, so it is
                    // not recorded.
                    return None;
                }
                let input = ClassifyInput { segment: &key, cwd };
                match catch_unwind(AssertUnwindSafe(|| self.classifier.classify(&input))) {
                    // Unavailable (still loading, transient timeout) is not
                    // cached, so the next call retries.
                    Ok(o) => {
                        if o != ModelOutcome::Unavailable {
                            self.cache.put(cwd, key, o);
                        }
                        o
                    }
                    // A panic counts as unavailable and is not cached either.
                    Err(_) => ModelOutcome::Unavailable,
                }
            }
        };
        match outcome {
            ModelOutcome::Safe => {
                if model_may_approve(seg) {
                    self.record(ClassifierNote::Approved);
                    Some(PolicyVerdict::Approve)
                } else {
                    // The model vouched, but a never-auto-run rule may not
                    // hand it out silently; the user still sees the model's
                    // opinion next to the ask.
                    self.record(ClassifierNote::Blocked);
                    None
                }
            }
            ModelOutcome::Dangerous => {
                self.record(ClassifierNote::Dangerous);
                Some(PolicyVerdict::Deny)
            }
            ModelOutcome::NeedsApproval => {
                self.record(ClassifierNote::NeedsApproval);
                None
            }
            ModelOutcome::Unavailable => {
                self.record(ClassifierNote::Unavailable);
                None
            }
        }
    }
}

impl ModelPolicy<'_> {
    fn record(&self, note: ClassifierNote) {
        if let Some(trace) = self.trace {
            trace.push(note);
        }
    }
}
