//! What the bash approval engine needs beyond the command itself.
//!
//! One struct on [`crate::ai::tools::ToolCx`] so later additions extend it
//! without touching every `ToolCx { … }` literal. One [`SharedApproval`] is
//! built per run and shared, so the model's [`DecisionCache`] lives as long as
//! the run, and a settings save reaches the run's next command.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use review_approval::Policy;
use review_approval::model_stage::{CommandClassifier, DecisionCache};
use review_approval::policies::{
    CARGO_LOCAL, GIT_ADD, GIT_COMMIT, GIT_RESTORE_STAGED, GIT_TAG, HELP_BYPASS,
};

use crate::persist::settings::{ApprovalMode, AutoApproveSettings};

/// Configuration for `review_approval::classify`, shared by every agent.
#[derive(Clone)]
pub struct ApprovalSetup {
    /// Path-component deny patterns the user added on top of
    /// [`review_approval::default_deny_paths`].
    pub deny_paths: Vec<String>,
    /// What a command nothing vouched for gets: refused (`Auto`), asked
    /// (`Ask`), or run like every other command (`Trust`, checked before
    /// anything else; see [`crate::ai::tools::bash`]).
    pub mode: ApprovalMode,
    /// Opt-in rule policies, consulted before the model.
    pub capabilities: Vec<&'static Policy>,
    /// The local classifier, when one is installed and switched on.
    pub classifier: Option<Arc<dyn CommandClassifier>>,
    /// The classifier's outcomes for this run.
    pub cache: Arc<DecisionCache>,
    /// How long the classifier may take before the command asks instead.
    pub timeout: Duration,
    /// Set while a model classification is running on a blocking thread. A
    /// timed-out call cannot be cancelled, so while it lingers later commands
    /// are decided without the model rather than stacking more threads.
    pub in_flight: Arc<AtomicBool>,
}

/// Holds [`ApprovalSetup::in_flight`]; releases it on drop, also on panic.
pub struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Default for ApprovalSetup {
    fn default() -> ApprovalSetup {
        ApprovalSetup::from_settings(&AutoApproveSettings::default(), None)
    }
}

/// The policy a capability name in `settings.toml` stands for.
///
/// A closed set: `git:fetch/pull/push` and `cargo:crates-*` are deliberately
/// not offered.
fn capability_policy(name: &str) -> Option<&'static Policy> {
    Some(match name {
        "git:add" => &GIT_ADD,
        "git:commit" => &GIT_COMMIT,
        "git:restore-staged" => &GIT_RESTORE_STAGED,
        "git:tag" => &GIT_TAG,
        "cargo:local" => &CARGO_LOCAL,
        "help-bypass" => &HELP_BYPASS,
        _ => return None,
    })
}

impl ApprovalSetup {
    /// The setup for one run. The classifier is dropped unless the settings
    /// switch it on; unknown capability names are ignored with a warning.
    pub fn from_settings(
        settings: &AutoApproveSettings,
        classifier: Option<Arc<dyn CommandClassifier>>,
    ) -> ApprovalSetup {
        let mut capabilities: Vec<&'static Policy> = Vec::new();
        for name in &settings.capabilities {
            match capability_policy(name) {
                Some(policy) => {
                    if !capabilities.iter().any(|p| std::ptr::eq(*p, policy)) {
                        capabilities.push(policy);
                    }
                }
                None => tracing::warn!(name, "ignoring unknown auto-approve capability"),
            }
        }
        ApprovalSetup {
            deny_paths: settings.deny_paths.clone(),
            mode: settings.mode,
            capabilities,
            classifier: classifier.filter(|_| settings.local_classifier),
            cache: Arc::new(DecisionCache::new()),
            timeout: settings.timeout(),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Full trust: every command runs, denies and the classifier included.
    pub fn trust_all(&self) -> bool {
        self.mode == ApprovalMode::Trust
    }

    /// Claims the right to run one model classification, or `None` while an
    /// earlier one is still running.
    pub fn try_begin_model(&self) -> Option<InFlightGuard> {
        self.in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| InFlightGuard(self.in_flight.clone()))
    }

    /// The built-in deny patterns followed by the user's, without duplicates.
    pub fn effective_deny_paths(&self) -> Vec<String> {
        let mut paths = review_approval::default_deny_paths();
        for path in &self.deny_paths {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        paths
    }
}

/// A run's [`ApprovalSetup`], replaced when the user saves new settings so
/// the next command of every agent follows them. The model's cache and its
/// in-flight flag carry over: a verdict stays tied to the model that gave it
/// (see [`DecisionCache::sync_generation`]), not to the settings.
pub struct SharedApproval {
    current: RwLock<Arc<ApprovalSetup>>,
    /// The run's classifier whatever the setting says, so switching the
    /// setting on mid-run can hand it back.
    installed: Option<Arc<dyn CommandClassifier>>,
}

impl SharedApproval {
    pub fn new(
        settings: &AutoApproveSettings,
        classifier: Option<Arc<dyn CommandClassifier>>,
    ) -> SharedApproval {
        SharedApproval {
            current: RwLock::new(Arc::new(ApprovalSetup::from_settings(
                settings,
                classifier.clone(),
            ))),
            installed: classifier,
        }
    }

    /// The setup the next command is decided under.
    pub fn current(&self) -> Arc<ApprovalSetup> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Follow new settings from the next command on.
    pub fn apply(&self, settings: &AutoApproveSettings) {
        let mut slot = self.current.write().unwrap_or_else(|e| e.into_inner());
        let mut next = ApprovalSetup::from_settings(settings, self.installed.clone());
        next.cache = slot.cache.clone();
        next.in_flight = slot.in_flight.clone();
        *slot = Arc::new(next);
    }
}

impl Default for SharedApproval {
    fn default() -> SharedApproval {
        ApprovalSetup::default().into()
    }
}

impl From<ApprovalSetup> for SharedApproval {
    fn from(setup: ApprovalSetup) -> SharedApproval {
        SharedApproval {
            installed: setup.classifier.clone(),
            current: RwLock::new(Arc::new(setup)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_deny_paths_extends_defaults_without_duplicates() {
        let defaults = review_approval::default_deny_paths();
        assert_eq!(ApprovalSetup::default().effective_deny_paths(), defaults);

        let setup = ApprovalSetup {
            deny_paths: vec![
                ".ssh".to_string(),
                "secrets".to_string(),
                "secrets".to_string(),
            ],
            ..ApprovalSetup::default()
        };
        let got = setup.effective_deny_paths();
        assert_eq!(got.len(), defaults.len() + 1, "{got:?}");
        assert_eq!(got.iter().filter(|p| *p == ".ssh").count(), 1);
        assert_eq!(got.last().map(String::as_str), Some("secrets"));
    }

    #[test]
    fn unknown_capability_ignored() {
        let settings = AutoApproveSettings {
            capabilities: vec![
                "git:add".into(),
                "git:push".into(),
                "nonsense".into(),
                "git:add".into(),
                "cargo:local".into(),
            ],
            ..AutoApproveSettings::default()
        };
        let setup = ApprovalSetup::from_settings(&settings, None);
        assert_eq!(setup.capabilities.len(), 2);
        assert!(std::ptr::eq(setup.capabilities[0], &GIT_ADD));
        assert!(std::ptr::eq(setup.capabilities[1], &CARGO_LOCAL));
    }

    #[test]
    fn classifier_is_dropped_unless_switched_on() {
        struct Never;
        impl CommandClassifier for Never {
            fn classify(
                &self,
                _: &review_approval::model_stage::ClassifyInput<'_>,
            ) -> review_approval::model_stage::ModelOutcome {
                review_approval::model_stage::ModelOutcome::Safe
            }
        }
        let c: Arc<dyn CommandClassifier> = Arc::new(Never);
        let off = ApprovalSetup::from_settings(&AutoApproveSettings::default(), Some(c.clone()));
        assert!(off.classifier.is_none());
        let on = ApprovalSetup::from_settings(
            &AutoApproveSettings {
                local_classifier: true,
                classifier_timeout_ms: 0,
                ..AutoApproveSettings::default()
            },
            Some(c),
        );
        assert!(on.classifier.is_some());
        assert_eq!(on.timeout, Duration::from_millis(50));
    }

    #[test]
    fn apply_follows_new_settings_and_keeps_the_cache() {
        struct Never;
        impl CommandClassifier for Never {
            fn classify(
                &self,
                _: &review_approval::model_stage::ClassifyInput<'_>,
            ) -> review_approval::model_stage::ModelOutcome {
                review_approval::model_stage::ModelOutcome::Safe
            }
        }
        let shared = SharedApproval::new(&AutoApproveSettings::default(), Some(Arc::new(Never)));
        let before = shared.current();
        assert_eq!(before.mode, ApprovalMode::Auto);
        assert!(before.classifier.is_none(), "the setting is off");

        shared.apply(&AutoApproveSettings {
            mode: ApprovalMode::Trust,
            local_classifier: true,
            ..AutoApproveSettings::default()
        });
        let after = shared.current();
        assert!(after.trust_all());
        assert!(after.classifier.is_some(), "switched on mid-run");
        assert!(Arc::ptr_eq(&before.cache, &after.cache));
        assert!(Arc::ptr_eq(&before.in_flight, &after.in_flight));
    }
}
