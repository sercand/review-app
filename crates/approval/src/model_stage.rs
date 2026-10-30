//! The optional local-classifier stage, consulted for a whole command the
//! rules did not approve (spec: model stage).
//!
//! The model sees the command exactly as the agent wrote it, constructs the
//! parser gives up on included. `Safe` runs the command unless it is one of
//! the few shapes in [`crate::floor`] (inline code, environment swaps,
//! deletes, state-changing git, network, installers); `Dangerous` and
//! `NeedsApproval` leave it to the mode (refused or asked). Deny rules run
//! first, so a command touching a secret path never reaches the model. Fail closed: a classifier that panics or reports
//! `Unavailable` leaves the command exactly as it would be with no
//! classifier.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::floor::floor_reason;
use crate::{ApprovalConfig, Verdict, classify};

/// `Verdict::Run.source` for a command the model approved.
pub const MODEL_SOURCE: &str = "classifier";

/// What the classifier thinks of one command.
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
    /// The command as the agent wrote it, trimmed.
    pub command: &'a str,
    /// The working directory the command runs in, when known.
    pub cwd: Option<&'a Path>,
}

/// What the model said about one command, as shown to the user. A note
/// exists only when the model was actually asked: commands the rules
/// decided on their own show nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifierNote {
    /// The model approved the command, and it ran.
    Approved,
    /// The model approved the command, but it is a kind a model's "safe"
    /// never runs ([`crate::floor`]).
    Overruled,
    /// The model abstained.
    NeedsApproval,
    /// The model flagged the command.
    Dangerous,
    /// The model was asked but could not answer (busy, timed out, failed).
    Unavailable,
}

impl From<ModelOutcome> for ClassifierNote {
    fn from(outcome: ModelOutcome) -> ClassifierNote {
        match outcome {
            ModelOutcome::Safe => ClassifierNote::Approved,
            ModelOutcome::NeedsApproval => ClassifierNote::NeedsApproval,
            ModelOutcome::Dangerous => ClassifierNote::Dangerous,
            ModelOutcome::Unavailable => ClassifierNote::Unavailable,
        }
    }
}

pub trait CommandClassifier: Send + Sync {
    /// `input.command` is the whole command line. The caller enforces the
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

/// Outcomes by (working directory, command text), shared between threads.
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

    pub fn get(&self, cwd: Option<&Path>, command: &str) -> Option<ModelOutcome> {
        self.lock()
            .entries
            .get(&(cwd.map(Path::to_path_buf), command.to_string()))
            .copied()
    }

    pub fn put(&self, cwd: Option<&Path>, command: String, v: ModelOutcome) {
        let k = (cwd.map(Path::to_path_buf), command);
        let mut state = self.lock();
        if state.entries.len() >= CACHE_CAP && !state.entries.contains_key(&k) {
            state.entries.clear();
        }
        state.entries.insert(k, v);
    }
}

/// Ask `classifier` about `command` (trimmed), through `cache`.
///
/// Returns `None` when the model must not be used now (switched off) or
/// `ask_new` is false and nothing is cached: that is a skip, not an answer.
/// `Unavailable` (still loading, a transient failure, a panic) is returned
/// but never cached, so the next command retries.
pub fn consult(
    classifier: &dyn CommandClassifier,
    cache: &DecisionCache,
    command: &str,
    cwd: Option<&Path>,
    ask_new: bool,
) -> Option<ModelOutcome> {
    let command = command.trim();
    if command.is_empty() || !classifier.active() {
        return None;
    }
    cache.sync_generation(classifier.generation());
    if let Some(outcome) = cache.get(cwd, command) {
        return Some(outcome);
    }
    if !ask_new {
        return None;
    }
    let input = ClassifyInput { command, cwd };
    let outcome = catch_unwind(AssertUnwindSafe(|| classifier.classify(&input)))
        .unwrap_or(ModelOutcome::Unavailable);
    if outcome != ModelOutcome::Unavailable {
        cache.put(cwd, command.to_string(), outcome);
    }
    Some(outcome)
}

/// The model for one [`decide`] call.
pub struct ModelStage<'a> {
    pub classifier: &'a dyn CommandClassifier,
    pub cache: &'a DecisionCache,
    /// `false` once the caller's time for this command is up: only a cached
    /// outcome applies, no new call is made.
    pub ask_new: bool,
    /// Run a command the model is unsure about (`NeedsApproval`) as if it
    /// had said `Safe`; the floor still applies. `Unavailable` never runs.
    pub run_unsure: bool,
}

/// [`classify`], then the model for a command the rules did not settle.
///
/// A `Run` or `Deny` from the rules is final and the model is not asked. An
/// `Ask` (including a command the parser gave up on) goes to the model as
/// one whole line: `Safe` runs it unless [`floor_reason`] names a reason
/// not to, `Dangerous` asks with a warning, and `NeedsApproval` or no
/// answer keep the rules' reason. The note is what the model said, when it
/// was asked.
pub fn decide(
    command: &str,
    cfg: &ApprovalConfig<'_>,
    model: Option<&ModelStage<'_>>,
) -> (Verdict, Option<ClassifierNote>) {
    let verdict = classify(command, cfg);
    let (Verdict::Ask { reason }, Some(model)) = (&verdict, model) else {
        return (verdict, None);
    };
    let Some(outcome) = consult(
        model.classifier,
        model.cache,
        command,
        cfg.cwd,
        model.ask_new,
    ) else {
        return (verdict, None);
    };
    let verdict = match outcome {
        ModelOutcome::NeedsApproval if model.run_unsure => match floor_reason(command, cfg) {
            None => Verdict::Run {
                source: MODEL_SOURCE,
            },
            Some(floor) => Verdict::Ask {
                reason: format!("the classifier was unsure, and the floor holds it: {floor}"),
            },
        },
        ModelOutcome::Safe => match floor_reason(command, cfg) {
            None => Verdict::Run {
                source: MODEL_SOURCE,
            },
            Some(floor) => {
                return (
                    Verdict::Ask {
                        reason: format!(
                            "the classifier said safe, but the floor holds it: {floor}"
                        ),
                    },
                    Some(ClassifierNote::Overruled),
                );
            }
        },
        ModelOutcome::Dangerous => Verdict::Ask {
            reason: format!("the local classifier flagged this command as dangerous ({reason})"),
        },
        ModelOutcome::NeedsApproval | ModelOutcome::Unavailable => verdict,
    };
    (verdict, Some(outcome.into()))
}
