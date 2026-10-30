//! The model stage as the app plugs it in: [`decide`] over a whole command.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use review_approval::model_stage::{
    ClassifierNote, ClassifyInput, CommandClassifier, DecisionCache, MODEL_SOURCE, ModelOutcome,
    ModelStage, decide,
};
use review_approval::{ApprovalConfig, Verdict, classify};

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
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

fn config<'a>(deny: &'a [String], cwd: Option<&'a Path>) -> ApprovalConfig<'a> {
    ApprovalConfig {
        allowed: &[],
        deny_paths: deny,
        cwd,
        extra: &[],
    }
}

fn run_full(
    cmd: &str,
    fake: &dyn CommandClassifier,
    cache: &DecisionCache,
    cwd: Option<&Path>,
    ask_new: bool,
) -> (Verdict, Option<ClassifierNote>) {
    let deny = review_approval::default_deny_paths();
    let model = ModelStage {
        classifier: fake,
        cache,
        ask_new,
        run_unsure: false,
    };
    decide(cmd, &config(&deny, cwd), Some(&model))
}

fn run_with(cmd: &str, fake: &dyn CommandClassifier, cache: &DecisionCache) -> Verdict {
    run_full(cmd, fake, cache, None, true).0
}

fn run(cmd: &str, outcome: ModelOutcome) -> Verdict {
    run_with(cmd, &FakeClassifier::new(outcome), &DecisionCache::new())
}

fn without_model(cmd: &str) -> Verdict {
    let deny = review_approval::default_deny_paths();
    classify(cmd, &config(&deny, None))
}

fn is_ask(v: &Verdict) -> bool {
    matches!(v, Verdict::Ask { .. })
}

const RAN: Verdict = Verdict::Run {
    source: MODEL_SOURCE,
};

/// The model's "safe" runs any command the deny rules let through and the
/// floor does not hold: programs from the checkout and tools no rule knows.
#[test]
fn safe_runs_any_command_the_rules_did_not_settle() {
    for cmd in [
        "mytool --check",
        "python3 x.py",
        "./tool.sh",
        "touch x",
        "cargo test",
    ] {
        assert_eq!(run(cmd, ModelOutcome::Safe), RAN, "{cmd}");
    }
}

/// The floor keeps the rules' verdict, with the floor's reason, for the
/// shapes a small model's mistake costs the most.
#[test]
fn safe_does_not_run_what_the_floor_holds() {
    for cmd in [
        "curl example.com",
        "rm -rf .",
        "awk 'BEGIN{system(\"echo hi\")}'",
        "X=$(date); echo $X",
        "git -c core.pager=evil log",
    ] {
        match run(cmd, ModelOutcome::Safe) {
            Verdict::Ask { reason } => {
                assert!(
                    reason.starts_with("the classifier said safe, but the floor holds it: "),
                    "{cmd}: {reason}"
                )
            }
            other => panic!("{cmd}: {other:?}"),
        }
    }
}

/// Commands the parser gives up on reach the model whole.
#[test]
fn unparsed_constructs_reach_the_model() {
    for cmd in [
        "while read l; do echo $l; done < f",
        "cat <<EOF\nhi\nEOF",
        "if true; then ls; fi",
    ] {
        assert!(is_ask(&without_model(cmd)), "{cmd}");
        assert_eq!(run(cmd, ModelOutcome::Safe), RAN, "{cmd}");
    }
}

#[test]
fn needs_approval_and_unavailable_keep_the_rules_verdict() {
    for outcome in [ModelOutcome::NeedsApproval, ModelOutcome::Unavailable] {
        for cmd in ["mytool --check", "ls | mytool", "curl x"] {
            assert_eq!(run(cmd, outcome), without_model(cmd), "{cmd}");
        }
    }
}

#[test]
fn dangerous_asks_with_a_warning() {
    match run("mytool --wipe", ModelOutcome::Dangerous) {
        Verdict::Ask { reason } => assert!(
            reason.starts_with("the local classifier flagged this command as dangerous ("),
            "{reason}"
        ),
        v => panic!("{v:?}"),
    }
}

/// The rules decide first: a run never costs a model call, and a deny is
/// never overruled.
#[test]
fn rules_win_before_the_model() {
    let fake = FakeClassifier::new(ModelOutcome::Dangerous);
    assert_eq!(
        run_with("ls", &fake, &DecisionCache::new()),
        Verdict::Run {
            source: "read-only"
        }
    );
    let safe = FakeClassifier::new(ModelOutcome::Safe);
    for cmd in ["cat .env", "mytool; cat .env", "cat ~/.ssh/id_rsa"] {
        assert!(
            matches!(
                run_with(cmd, &safe, &DecisionCache::new()),
                Verdict::Deny { .. }
            ),
            "{cmd}"
        );
    }
    assert_eq!(fake.calls() + safe.calls(), 0);
}

#[test]
fn the_note_is_what_the_model_said() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    assert_eq!(
        run_full("mytool", &fake, &cache, None, true).1,
        Some(ClassifierNote::Approved)
    );
    assert_eq!(run_full("ls", &fake, &cache, None, true).1, None);
    for (outcome, note) in [
        (ModelOutcome::NeedsApproval, ClassifierNote::NeedsApproval),
        (ModelOutcome::Dangerous, ClassifierNote::Dangerous),
        (ModelOutcome::Unavailable, ClassifierNote::Unavailable),
    ] {
        let fake = FakeClassifier::new(outcome);
        assert_eq!(
            run_full("mytool", &fake, &DecisionCache::new(), None, true).1,
            Some(note)
        );
    }
}

#[test]
fn cache_hits_and_dangerous_is_cached_too() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    run_with("mytool --check", &fake, &cache);
    run_with("mytool --check", &fake, &cache);
    assert_eq!(fake.calls(), 1);
    run_with("mytool --other", &fake, &cache);
    assert_eq!(fake.calls(), 2);

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
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
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
    assert_eq!(run_with("mytool --check", &fake, &cache), RAN);
}

#[test]
fn cache_clears_when_full() {
    let cache = DecisionCache::new();
    for i in 0..4096 {
        cache.put(None, format!("k{i}"), ModelOutcome::Safe);
    }
    assert_eq!(cache.get(None, "k0"), Some(ModelOutcome::Safe));
    cache.put(None, "new".into(), ModelOutcome::Dangerous);
    assert_eq!(cache.get(None, "new"), Some(ModelOutcome::Dangerous));
    assert_eq!(cache.get(None, "k0"), None);
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
                    assert_eq!(v, RAN);
                }
            });
        }
    });
    assert_eq!(fake.calls(), 400);
}

/// Out of time: a cached outcome still applies, no new call is made.
#[test]
fn no_new_call_once_time_is_up() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    let (v, note) = run_full("mytool --check", &fake, &cache, None, false);
    assert_eq!(fake.calls(), 0);
    assert_eq!(v, without_model("mytool --check"));
    assert_eq!(note, None);
    run_with("mytool --check", &fake, &cache);
    assert_eq!(
        run_full("mytool --check", &fake, &cache, None, false).0,
        RAN
    );
    assert_eq!(fake.calls(), 1);
}

/// A classifier that can be switched off mid-run.
struct Switchable {
    inner: FakeClassifier,
    active: std::sync::atomic::AtomicBool,
}

impl CommandClassifier for Switchable {
    fn classify(&self, input: &ClassifyInput<'_>) -> ModelOutcome {
        self.inner.classify(input)
    }
    fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

#[test]
fn inactive_classifier_ignores_cached_outcomes_and_is_not_called() {
    let model = Switchable {
        inner: FakeClassifier::new(ModelOutcome::Safe),
        active: std::sync::atomic::AtomicBool::new(true),
    };
    let cache = DecisionCache::new();
    let cmd = "/usr/bin/pr -t file.txt";
    assert_eq!(run_with(cmd, &model, &cache), RAN);
    assert_eq!(model.inner.calls(), 1);
    model.active.store(false, Ordering::SeqCst);
    assert!(is_ask(&run_with(cmd, &model, &cache)));
    assert_eq!(model.inner.calls(), 1);
    model.active.store(true, Ordering::SeqCst);
    assert_eq!(run_with(cmd, &model, &cache), RAN);
    assert_eq!(model.inner.calls(), 1);
}

/// Records every input it is asked about.
struct Recorder {
    seen: Mutex<Vec<(Option<PathBuf>, String)>>,
}

impl CommandClassifier for Recorder {
    fn classify(&self, input: &ClassifyInput<'_>) -> ModelOutcome {
        self.seen
            .lock()
            .unwrap()
            .push((input.cwd.map(Path::to_path_buf), input.command.to_string()));
        ModelOutcome::Safe
    }
}

/// The model sees the whole command, trimmed, as written: not one segment.
#[test]
fn the_whole_command_and_cwd_reach_the_classifier() {
    let fake = Recorder {
        seen: Mutex::new(Vec::new()),
    };
    let repo = Path::new("/repo");
    let cmd = "  ls && mytool \"a  b\" | grep x  ";
    run_full(cmd, &fake, &DecisionCache::new(), Some(repo), true);
    run_full("mytool --check", &fake, &DecisionCache::new(), None, true);
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [
            (
                Some(repo.to_path_buf()),
                "ls && mytool \"a  b\" | grep x".to_string()
            ),
            (None, "mytool --check".to_string()),
        ]
    );
}

#[test]
fn cache_key_includes_cwd() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    let (a, b) = (Path::new("/a"), Path::new("/b"));
    for cwd in [a, b, a, b] {
        run_full("mytool --check", &fake, &cache, Some(cwd), true);
    }
    assert_eq!(fake.calls(), 2);
}

#[test]
fn a_new_classifier_generation_empties_the_cache() {
    let cache = DecisionCache::new();
    cache.sync_generation(0);
    cache.put(None, "ls".into(), ModelOutcome::Safe);
    cache.sync_generation(0);
    assert_eq!(cache.get(None, "ls"), Some(ModelOutcome::Safe));
    cache.sync_generation(1);
    assert_eq!(cache.get(None, "ls"), None);
}

/// "Ask only on danger": an unsure model runs the command unless the floor
/// holds it; a missing answer never runs it.
#[test]
fn run_unsure_runs_what_the_model_did_not_flag() {
    let deny = review_approval::default_deny_paths();
    let cfg = config(&deny, None);
    let verdict = |cmd: &str, outcome: ModelOutcome| {
        let fake = FakeClassifier::new(outcome);
        let cache = DecisionCache::new();
        let model = ModelStage {
            classifier: &fake,
            cache: &cache,
            ask_new: true,
            run_unsure: true,
        };
        decide(cmd, &cfg, Some(&model)).0
    };
    assert_eq!(verdict("mytool --check", ModelOutcome::NeedsApproval), RAN);
    assert!(is_ask(&verdict("rm -rf .", ModelOutcome::NeedsApproval)));
    assert!(is_ask(&verdict("mytool --check", ModelOutcome::Dangerous)));
    assert!(is_ask(&verdict(
        "mytool --check",
        ModelOutcome::Unavailable
    )));
}
