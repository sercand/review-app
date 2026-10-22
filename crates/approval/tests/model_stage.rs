//! The model stage as the app plugs it in: `ApprovalConfig.extra`.

use std::sync::atomic::{AtomicUsize, Ordering};

use review_approval::model_stage::{CommandClassifier, DecisionCache, ModelOutcome, ModelPolicy};
use review_approval::{ApprovalConfig, SegmentPolicy, Verdict, classify};

struct FakeClassifier {
    outcome: ModelOutcome,
    calls: AtomicUsize,
}

impl FakeClassifier {
    fn new(outcome: ModelOutcome) -> Self {
        FakeClassifier {
            outcome,
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CommandClassifier for FakeClassifier {
    fn classify(&self, _segment: &str) -> ModelOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

fn run_with(cmd: &str, fake: &dyn CommandClassifier, cache: &DecisionCache) -> Verdict {
    let policy = ModelPolicy {
        classifier: fake,
        cache,
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    let cfg = ApprovalConfig {
        allowed: &[],
        deny_paths: &[],
        cwd: None,
        extra: &extra,
    };
    classify(cmd, &cfg)
}

fn run(cmd: &str, outcome: ModelOutcome) -> Verdict {
    run_with(cmd, &FakeClassifier::new(outcome), &DecisionCache::new())
}

fn without_model(cmd: &str) -> Verdict {
    let cfg = ApprovalConfig {
        allowed: &[],
        deny_paths: &[],
        cwd: None,
        extra: &[],
    };
    classify(cmd, &cfg)
}

fn is_ask(v: &Verdict) -> bool {
    matches!(v, Verdict::Ask { .. })
}

#[test]
fn safe_runs_unknown_segment() {
    assert_eq!(
        run("mytool --check", ModelOutcome::Safe),
        Verdict::Run { source: "ml" }
    );
}

#[test]
fn needs_approval_asks() {
    assert!(is_ask(&run("mytool --check", ModelOutcome::NeedsApproval)));
}

#[test]
fn unavailable_asks_same_as_no_model() {
    for cmd in ["mytool --check", "ls | mytool", "curl x"] {
        assert_eq!(
            run(cmd, ModelOutcome::Unavailable),
            without_model(cmd),
            "{cmd}"
        );
    }
}

#[test]
fn dangerous_becomes_ask_with_warning() {
    match run("mytool --wipe", ModelOutcome::Dangerous) {
        Verdict::Ask { reason } => {
            assert_eq!(
                reason,
                "the local classifier flagged this command as dangerous: mytool --wipe"
            );
        }
        v => panic!("{v:?}"),
    }
}

#[test]
fn safe_for_never_approve_still_asks() {
    for cmd in [
        "curl x",
        "python3 x.py",
        "$CMD a",
        "touch x",
        "/usr/bin/curl x",
    ] {
        let v = run(cmd, ModelOutcome::Safe);
        assert_eq!(v, without_model(cmd), "{cmd}");
        assert!(is_ask(&v), "{cmd}");
    }
}

#[test]
fn pipeline_checks_every_segment() {
    assert!(is_ask(&run("ls | curl -T - x", ModelOutcome::Safe)));
    assert!(is_ask(&run("mytool && python3 x", ModelOutcome::Safe)));
}

#[test]
fn rules_win_before_model() {
    let fake = FakeClassifier::new(ModelOutcome::Dangerous);
    let v = run_with("ls", &fake, &DecisionCache::new());
    assert_eq!(
        v,
        Verdict::Run {
            source: "read-only"
        }
    );
    assert_eq!(fake.calls(), 0);
}

#[test]
fn cache_hits() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    run_with("mytool --check", &fake, &cache);
    run_with("mytool --check", &fake, &cache);
    assert_eq!(fake.calls(), 1);
    run_with("mytool --other", &fake, &cache);
    assert_eq!(fake.calls(), 2);
}

#[test]
fn dangerous_is_cached_too() {
    let fake = FakeClassifier::new(ModelOutcome::Dangerous);
    let cache = DecisionCache::new();
    for _ in 0..3 {
        assert!(is_ask(&run_with("mytool --wipe", &fake, &cache)));
    }
    assert_eq!(fake.calls(), 1);
}

#[test]
fn unavailable_is_not_cached() {
    let fake = FakeClassifier::new(ModelOutcome::Unavailable);
    let cache = DecisionCache::new();
    run_with("mytool", &fake, &cache);
    run_with("mytool", &fake, &cache);
    assert_eq!(fake.calls(), 2);
}

struct PanicsOnce {
    panicked: AtomicUsize,
}

impl CommandClassifier for PanicsOnce {
    fn classify(&self, _segment: &str) -> ModelOutcome {
        if self.panicked.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("classifier exploded");
        }
        ModelOutcome::Safe
    }
}

#[test]
fn panic_is_unavailable_and_not_cached() {
    let fake = PanicsOnce {
        panicked: AtomicUsize::new(0),
    };
    let cache = DecisionCache::new();
    assert!(is_ask(&run_with("mytool --check", &fake, &cache)));
    assert_eq!(
        run_with("mytool --check", &fake, &cache),
        Verdict::Run { source: "ml" }
    );
}

#[test]
fn cache_clears_when_full() {
    let cache = DecisionCache::new();
    for i in 0..4096 {
        cache.put(format!("k{i}"), ModelOutcome::Safe);
    }
    assert_eq!(cache.get("k0"), Some(ModelOutcome::Safe));
    cache.put("new".into(), ModelOutcome::Dangerous);
    assert_eq!(cache.get("new"), Some(ModelOutcome::Dangerous));
    assert_eq!(cache.get("k0"), None);
}

fn run_deny_paths(cmd: &str, outcome: ModelOutcome) -> Verdict {
    let fake = FakeClassifier::new(outcome);
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: &fake,
        cache: &cache,
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    let deny = review_approval::default_deny_paths();
    classify(
        cmd,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &deny,
            cwd: None,
            extra: &extra,
        },
    )
}

#[test]
fn withhold_beats_model_safe() {
    assert!(is_ask(&run_deny_paths("mytool .e*", ModelOutcome::Safe)));
}

#[test]
fn hard_deny_beats_model_dangerous() {
    match run_deny_paths("mytool; cat .env", ModelOutcome::Dangerous) {
        Verdict::Deny { reason } => assert!(reason.contains(".env"), "{reason}"),
        v => panic!("{v:?}"),
    }
}

#[test]
fn concurrent_use() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    std::thread::scope(|s| {
        for t in 0..8 {
            let (fake, cache) = (&fake, &cache);
            s.spawn(move || {
                for i in 0..50 {
                    let v = run_with(&format!("mytool{t}x{i} --check"), fake, cache);
                    assert_eq!(v, Verdict::Run { source: "ml" });
                }
            });
        }
    });
    assert_eq!(fake.calls(), 400);
}
