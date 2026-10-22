//! The shiso-backed [`CommandClassifier`]: DistilBERT logits on the GPU, then
//! [`calibrate`]. Every failure mode (busy, error, panic, repeated failure)
//! comes out as [`ModelOutcome::Unavailable`], so the engine behaves exactly
//! as it would with no classifier.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use review_approval::model_stage::{CommandClassifier, ModelOutcome};
use shiso_core::Backend;
use shiso_models::distilbert_cls::{Classifier, ClassifierDesc, ClassifyError};

use super::calibrate::calibrate;

/// Consecutive failures after which the classifier stays off for the session.
const MAX_FAILURES: u32 = 3;

/// Where logits come from; a fake in tests.
trait LogitSource: Send + Sync {
    fn logits(&self, text: &str) -> Result<[f32; 3], SourceError>;
}

/// A failed logits call. `kind` is a fixed label that is safe to log at any
/// level; `detail` may echo the command text (a tokenizer error can), so it
/// is only ever logged at debug.
#[derive(Debug)]
struct SourceError {
    kind: &'static str,
    detail: String,
}

impl LogitSource for Classifier {
    fn logits(&self, text: &str) -> Result<[f32; 3], SourceError> {
        Classifier::logits(self, text).map_err(|e| SourceError {
            kind: match &e {
                ClassifyError::BadConfig(_) => "bad config",
                ClassifyError::MissingTensor(_) => "missing tensor",
                ClassifyError::BadTensor { .. } => "bad tensor",
                ClassifyError::Tokenizer(_) => "tokenizer",
                ClassifyError::Device(_) => "device",
                ClassifyError::Io(_) => "io",
            },
            detail: e.to_string(),
        })
    }
}

pub struct ShisoClassifier {
    source: Box<dyn LogitSource>,
    /// Set while a call is running; a second caller does not queue behind it.
    busy: AtomicBool,
    /// Consecutive failed calls; a success resets it. At `MAX_FAILURES` the
    /// classifier answers `Unavailable` for the rest of the session: only a
    /// new `ShisoClassifier` (a reload) clears it.
    failures: AtomicU32,
    /// Called once when the breaker trips, so whoever holds the classifier
    /// can stop reporting it as ready.
    on_trip: Option<Box<dyn Fn() + Send + Sync>>,
}

/// Clears the busy flag however the call ends.
struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl ShisoClassifier {
    fn with_source(source: Box<dyn LogitSource>) -> ShisoClassifier {
        ShisoClassifier {
            source,
            busy: AtomicBool::new(false),
            failures: AtomicU32::new(0),
            on_trip: None,
        }
    }

    /// Calls `f` once when the failure breaker trips.
    pub fn with_on_trip(mut self, f: Box<dyn Fn() + Send + Sync>) -> ShisoClassifier {
        self.on_trip = Some(f);
        self
    }

    fn record_failure(&self) -> ModelOutcome {
        if self.failures.fetch_add(1, Ordering::AcqRel) + 1 == MAX_FAILURES
            && let Some(f) = &self.on_trip
        {
            f();
        }
        ModelOutcome::Unavailable
    }
}

impl CommandClassifier for ShisoClassifier {
    fn classify(&self, segment: &str) -> ModelOutcome {
        if self.failures.load(Ordering::Acquire) >= MAX_FAILURES {
            return ModelOutcome::Unavailable;
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return ModelOutcome::Unavailable;
        }
        let _guard = BusyGuard(&self.busy);
        // Command text is never logged above debug.
        match catch_unwind(AssertUnwindSafe(|| self.source.logits(segment))) {
            // A NaN/inf result is a broken device, not an answer.
            Ok(Ok(logits)) if logits.iter().any(|l| !l.is_finite()) => {
                tracing::warn!("local classifier returned non-finite logits");
                self.record_failure()
            }
            Ok(Ok(logits)) => {
                self.failures.store(0, Ordering::Release);
                calibrate(logits)
            }
            Ok(Err(e)) => {
                tracing::warn!("local classifier call failed: {}", e.kind);
                tracing::debug!("local classifier error detail: {}", e.detail);
                self.record_failure()
            }
            Err(_) => {
                tracing::warn!("local classifier call panicked");
                self.record_failure()
            }
        }
    }
}

/// The compute device: Metal on macOS, CUDA on Linux builds with
/// `local-classifier-cuda`. Created at most once per process and shared. Only
/// a success is cached: a failure (or panic) is returned as `Err` and a later
/// call, say after the user switches the setting off and on, tries again.
pub fn open_backend() -> Result<Arc<dyn Backend>, String> {
    static BACKEND: Mutex<Option<Arc<dyn Backend>>> = Mutex::new(None);
    cached(&BACKEND, || match catch_unwind(create_backend) {
        Ok(r) => r,
        Err(_) => Err("the GPU backend panicked while opening".into()),
    })
}

/// The value in `slot`, else whatever `create` makes, stored only on `Ok`.
/// Creation runs under the lock so two callers never both create.
fn cached<T: Clone>(
    slot: &Mutex<Option<T>>,
    create: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(value) = guard.as_ref() {
        return Ok(value.clone());
    }
    let value = create()?;
    *guard = Some(value.clone());
    Ok(value)
}

#[cfg(target_os = "macos")]
fn create_backend() -> Result<Arc<dyn Backend>, String> {
    if !shiso_metal::metal_available() {
        return Err("no usable GPU".into());
    }
    let backend = shiso_metal::MetalBackend::new().map_err(|e| format!("Metal: {e}"))?;
    Ok(backend)
}

#[cfg(all(target_os = "linux", feature = "local-classifier-cuda"))]
fn create_backend() -> Result<Arc<dyn Backend>, String> {
    if !shiso_cuda::cuda_available() {
        return Err("no usable GPU".into());
    }
    let options = shiso_cuda::CudaOptions::default().with_decode_graphs(false);
    let backend = shiso_cuda::CudaBackend::new(0, options).map_err(|e| format!("CUDA: {e}"))?;
    Ok(backend)
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", feature = "local-classifier-cuda")
)))]
fn create_backend() -> Result<Arc<dyn Backend>, String> {
    Err("no usable GPU".into())
}

/// Whether this build has a GPU backend that can run on this machine. Cheap
/// (no device is created), so callers can check before fetching weights.
pub fn gpu_available() -> bool {
    catch_unwind(probe_gpu).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn probe_gpu() -> bool {
    shiso_metal::metal_available()
}

#[cfg(all(target_os = "linux", feature = "local-classifier-cuda"))]
fn probe_gpu() -> bool {
    shiso_cuda::cuda_available()
}

#[cfg(not(any(
    target_os = "macos",
    all(target_os = "linux", feature = "local-classifier-cuda")
)))]
fn probe_gpu() -> bool {
    false
}

/// Opens the backend and the model in `model_dir`. Blocking (device setup and
/// weight upload); call it from a dedicated thread.
pub fn load(model_dir: &Path) -> Result<ShisoClassifier, String> {
    load_with_backend(open_backend()?, model_dir)
}

/// [`load`] on an already opened backend (see [`open_backend`]: one per
/// process).
pub fn load_with_backend(
    backend: Arc<dyn Backend>,
    model_dir: &Path,
) -> Result<ShisoClassifier, String> {
    let classifier = catch_unwind(AssertUnwindSafe(|| {
        Classifier::open(&ClassifierDesc {
            dir: model_dir,
            backend,
        })
    }))
    .map_err(|_| "the classifier panicked while loading".to_string())?
    .map_err(|e| format!("loading the classifier: {e}"))?;
    Ok(ShisoClassifier::with_source(Box::new(classifier)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::super::calibrate::tests::{expected, goldens};
    use super::*;

    type Step = Option<Result<[f32; 3], String>>;

    /// Plays back scripted results; `None` panics.
    struct Script(Mutex<Vec<Step>>);

    impl Script {
        fn classifier(steps: Vec<Step>) -> ShisoClassifier {
            let mut steps = steps;
            steps.reverse();
            ShisoClassifier::with_source(Box::new(Script(Mutex::new(steps))))
        }
    }

    impl LogitSource for Script {
        fn logits(&self, _: &str) -> Result<[f32; 3], SourceError> {
            let step = self.0.lock().unwrap().pop().expect("script exhausted");
            step.unwrap_or_else(|| panic!("device exploded"))
                .map_err(|detail| SourceError {
                    kind: "device",
                    detail,
                })
        }
    }

    const SAFE: [f32; 3] = [6.0, -2.0, -5.0];

    fn err() -> Step {
        Some(Err("device lost".into()))
    }

    #[test]
    fn cached_retries_after_an_error_and_keeps_a_success() {
        let slot = Mutex::new(None);
        assert_eq!(
            cached(&slot, || Err::<u32, _>("boom".into())),
            Err("boom".into())
        );
        assert_eq!(cached(&slot, || Ok(7)), Ok(7));
        // Cached now: the creator is not run again.
        assert_eq!(cached(&slot, || Err("not called".into())), Ok(7));
    }

    #[test]
    fn open_backend_reports_no_gpu_cleanly() {
        match open_backend() {
            Ok(_) => eprintln!("skipped: this machine has a usable GPU"),
            Err(e) => assert!(e.contains("GPU"), "{e}"),
        }
    }

    // Two tests below script a panic inside `classify`; the default panic hook
    // prints it to stderr. That noise is accepted: swapping the process-wide
    // hook would race with every other test running in parallel and could
    // swallow a real failure's message.
    #[test]
    fn classify_maps_errors_to_unavailable() {
        let c = Script::classifier(vec![err(), None, Some(Ok(SAFE))]);
        assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
        assert_eq!(c.classify("ls"), ModelOutcome::Unavailable); // panic
        assert_eq!(c.classify("ls"), ModelOutcome::Safe);
    }

    #[test]
    fn three_failures_disable_for_session() {
        // Only three steps: a fourth call must not reach the source.
        let c = Script::classifier(vec![err(), err(), None]);
        for _ in 0..3 {
            assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
        }
        assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
        assert_eq!(c.failures.load(Ordering::Acquire), 3);
    }

    /// M1: the trip is announced exactly once, when the third failure lands.
    #[test]
    fn tripping_the_breaker_notifies_once() {
        let trips = Arc::new(AtomicU32::new(0));
        let seen = trips.clone();
        let c = Script::classifier(vec![err(), err(), err()]).with_on_trip(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        for n in 1..=5 {
            assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
            assert_eq!(trips.load(Ordering::SeqCst), u32::from(n >= 3), "call {n}");
        }
    }

    /// M1: a NaN/inf result is a failure; it must not reset the counter.
    #[test]
    fn non_finite_logits_count_as_failures() {
        let trips = Arc::new(AtomicU32::new(0));
        let seen = trips.clone();
        let c = Script::classifier(vec![
            err(),
            Some(Ok([f32::NAN, 0.0, 0.0])),
            Some(Ok([0.0, f32::INFINITY, 0.0])),
        ])
        .with_on_trip(Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        for _ in 0..3 {
            assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
        }
        assert_eq!(c.failures.load(Ordering::Acquire), 3);
        assert_eq!(trips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn success_resets_counter() {
        let c = Script::classifier(vec![
            err(),
            err(),
            Some(Ok(SAFE)),
            err(),
            err(),
            Some(Ok(SAFE)),
        ]);
        let got: Vec<_> = (0..6).map(|_| c.classify("ls")).collect();
        use ModelOutcome::*;
        assert_eq!(
            got,
            [
                Unavailable,
                Unavailable,
                Safe,
                Unavailable,
                Unavailable,
                Safe
            ]
        );
    }

    #[test]
    fn busy_call_does_not_queue() {
        struct Gate {
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl LogitSource for Gate {
            fn logits(&self, _: &str) -> Result<[f32; 3], SourceError> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(SAFE)
            }
        }
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::channel();
        let c = Arc::new(ShisoClassifier::with_source(Box::new(Gate {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        })));
        let first = {
            let c = c.clone();
            std::thread::spawn(move || c.classify("ls"))
        };
        entered_rx.recv().unwrap();
        assert_eq!(c.classify("ls"), ModelOutcome::Unavailable);
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), ModelOutcome::Safe);
        // The flag was cleared: the next call runs (and would block, so
        // release it first).
        release_tx.send(()).unwrap();
        assert_eq!(c.classify("ls"), ModelOutcome::Safe);
    }

    #[test]
    #[ignore = "requires GPU + SHISO_TEST_MODEL_DISTILBERT_CLS"]
    fn real_model_matches_fixture_decisions() {
        let dir = std::env::var("SHISO_TEST_MODEL_DISTILBERT_CLS")
            .expect("set SHISO_TEST_MODEL_DISTILBERT_CLS to the converted model dir");
        let t = std::time::Instant::now();
        let c = load(Path::new(&dir)).expect("load");
        let load_time = t.elapsed();
        let rows = goldens();
        let mut agree = 0;
        let mut approve_mismatch = Vec::new();
        let mut lost_approvals = 0;
        for g in &rows {
            let got = c.classify(&g.text);
            let want = expected(&g.decision);
            if got == want {
                agree += 1;
            } else {
                eprintln!(
                    "differs: {:?} got {got:?} want {want:?}; device {:?} golden {:?}",
                    g.text,
                    c.source.logits(&g.text),
                    g.logits
                );
            }
            // Only a new approval is unsafe; a golden approve the device
            // defers on is the fail-closed direction and is just counted.
            if got == ModelOutcome::Safe && want != ModelOutcome::Safe {
                approve_mismatch.push((g.text.clone(), got, want));
            } else if got != ModelOutcome::Safe && want == ModelOutcome::Safe {
                lost_approvals += 1;
            }
        }
        let pct = 100.0 * agree as f64 / rows.len() as f64;
        eprintln!(
            "load {load_time:?}; agreement {agree}/{} = {pct:.1}%; new approvals {}; lost approvals {lost_approvals}",
            rows.len(),
            approve_mismatch.len()
        );
        assert!(approve_mismatch.is_empty(), "{approve_mismatch:?}");
        assert!(pct >= 97.0, "agreement {pct:.1}%");
    }
}
