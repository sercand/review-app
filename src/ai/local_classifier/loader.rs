//! What the Settings window and the review run are told about the local
//! classifier: where it is on its way from "switched on" to "ready".

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use review_approval::model_stage::CommandClassifier;

use super::shiso;
use super::weights::WeightsManager;

pub use crate::state::classifier::ClassifierState;

/// Where the weights come from; a fake in tests.
pub trait WeightsSource: Send + Sync {
    /// The verified install, without touching the network.
    fn installed_dir(&self) -> Option<PathBuf>;
    /// Blocking download + verify + install; call from a job thread.
    /// `progress(done, total)` returns `false` to cancel: the download stops
    /// (keeping partial files for resume) and `ensure` returns an error.
    fn ensure(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String>;
}

impl WeightsSource for WeightsManager {
    fn installed_dir(&self) -> Option<PathBuf> {
        WeightsManager::installed_dir(self)
    }
    fn ensure(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String> {
        self.ensure_blocking(progress)
    }
}

/// Why the engine could not produce a classifier.
#[derive(Debug)]
pub enum LoadError {
    /// The device cannot be used (backend creation failed).
    Unavailable(String),
    /// The model failed to load.
    Failed(String),
}

/// The compute side; a fake in tests.
pub trait Engine: Send + Sync {
    /// Cheap check that a GPU backend exists, run before any download.
    fn gpu_available(&self) -> bool;
    /// Opens the backend and the model. Blocking; call from a job thread.
    /// `on_trip` is called (from any thread) if the classifier disables
    /// itself after repeated failures.
    fn load(
        &self,
        dir: &Path,
        on_trip: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<dyn CommandClassifier>, LoadError>;
}

struct ShisoEngine;

impl Engine for ShisoEngine {
    fn gpu_available(&self) -> bool {
        shiso::gpu_available()
    }
    fn load(
        &self,
        dir: &Path,
        on_trip: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<dyn CommandClassifier>, LoadError> {
        let backend = shiso::open_backend().map_err(LoadError::Unavailable)?;
        let classifier = shiso::load_with_backend(backend, dir)
            .map_err(LoadError::Failed)?
            .with_on_trip(on_trip);
        Ok(Arc::new(classifier))
    }
}

type OnState = Arc<dyn Fn(ClassifierState) + Send + Sync>;

struct Inner {
    enabled: bool,
    state: ClassifierState,
    classifier: Option<Arc<dyn CommandClassifier>>,
    /// A job thread is running; at most one at a time.
    job_running: bool,
    /// Bumped by every job; a trip report from an older job's classifier is
    /// ignored.
    generation: u64,
    /// What the running job last reported, even while disabled; shown again
    /// if the setting is switched back on before the job notices.
    phase: ClassifierState,
}

struct Shared {
    inner: Mutex<Inner>,
    source: Arc<dyn WeightsSource>,
    engine: Arc<dyn Engine>,
    on_state: OnState,
}

/// Drives the classifier from "switched on" to `Ready` on background threads
/// (one per job) and reports every transition through `on_state`. The
/// callback is never invoked with the internal lock held.
#[derive(Clone)]
pub struct Loader(Arc<Shared>);

impl Loader {
    pub fn new(manager: WeightsManager, on_state: OnState) -> Loader {
        Loader::with_parts(Arc::new(manager), Arc::new(ShisoEngine), on_state)
    }

    fn with_parts(
        source: Arc<dyn WeightsSource>,
        engine: Arc<dyn Engine>,
        on_state: OnState,
    ) -> Loader {
        Loader(Arc::new(Shared {
            inner: Mutex::new(Inner {
                enabled: false,
                state: ClassifierState::Off,
                classifier: None,
                job_running: false,
                generation: 0,
                phase: ClassifierState::Loading,
            }),
            source,
            engine,
            on_state,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Switches the classifier on or off. On, with no classifier yet, starts
    /// the job: GPU check, download if the weights are absent, then load.
    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.lock();
        if inner.enabled == enabled {
            return;
        }
        inner.enabled = enabled;
        if enabled {
            if inner.job_running {
                // A job from before the last disable is still winding down or
                // carrying on: show where it is rather than Off.
                let phase = inner.phase.clone();
                inner.state = phase.clone();
                drop(inner);
                (self.0.on_state)(phase);
            } else {
                self.start_job(inner);
            }
        } else {
            let dropped = inner.classifier.take();
            inner.state = ClassifierState::Off;
            drop(inner);
            (self.0.on_state)(ClassifierState::Off);
            // Freeing the model can be slow; do it off the lock.
            drop(dropped);
        }
    }

    /// Retry after a `Failed` (or `NotInstalled`) state.
    pub fn download(&self) {
        let inner = self.lock();
        let retryable = matches!(
            inner.state,
            ClassifierState::Failed(_) | ClassifierState::NotInstalled
        );
        if inner.enabled && !inner.job_running && retryable {
            self.start_job(inner);
        }
    }

    /// The classifier, only while `Ready`.
    pub fn classifier(&self) -> Option<Arc<dyn CommandClassifier>> {
        self.lock().classifier.clone()
    }

    /// Starts a job thread unless one is running. Takes the guard so the
    /// check and the flag are one critical section.
    fn start_job(&self, mut inner: MutexGuard<'_, Inner>) {
        if inner.job_running {
            return;
        }
        inner.job_running = true;
        inner.generation += 1;
        inner.phase = ClassifierState::Loading;
        drop(inner);
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("classifier-loader".into())
            .spawn(move || this.run_job());
        if let Err(e) = spawned {
            self.finish(Some(Outcome::State(ClassifierState::Failed(format!(
                "could not start the loader thread: {e}"
            )))));
        }
    }

    fn run_job(&self) {
        let outcome = match catch_unwind(AssertUnwindSafe(|| self.job())) {
            Ok(outcome) => outcome,
            Err(_) => Some(Outcome::State(ClassifierState::Failed(
                "the classifier loader panicked".into(),
            ))),
        };
        self.finish(outcome);
    }

    /// Reports an intermediate state; `false` means the setting was switched
    /// off meanwhile and the job should stop.
    fn progress(&self, state: ClassifierState) -> bool {
        let mut inner = self.lock();
        inner.phase = state.clone();
        if !inner.enabled {
            return false;
        }
        inner.state = state.clone();
        drop(inner);
        (self.0.on_state)(state);
        true
    }

    /// `None` means the job was cancelled by disabling.
    fn job(&self) -> Option<Outcome> {
        let shared = &self.0;
        if !shared.engine.gpu_available() {
            return Some(Outcome::State(ClassifierState::Unavailable(
                "no usable GPU".into(),
            )));
        }
        let dir = match shared.source.installed_dir() {
            Some(dir) => dir,
            None => {
                if !self.progress(ClassifierState::NotInstalled)
                    || !self.progress(ClassifierState::Downloading { done: 0, total: 0 })
                {
                    return None;
                }
                let mut cancelled = false;
                let fetched = shared.source.ensure(&mut |done, total| {
                    let go = self.progress(ClassifierState::Downloading { done, total });
                    cancelled |= !go;
                    go
                });
                if cancelled {
                    return None;
                }
                match fetched {
                    Ok(dir) => dir,
                    Err(e) => return Some(Outcome::State(ClassifierState::Failed(e))),
                }
            }
        };
        if !self.progress(ClassifierState::Loading) {
            return None;
        }
        let generation = self.lock().generation;
        let weak = Arc::downgrade(&self.0);
        let on_trip = Box::new(move || {
            if let Some(shared) = weak.upgrade() {
                Loader(shared).tripped(generation);
            }
        });
        Some(match shared.engine.load(&dir, on_trip) {
            Ok(c) => Outcome::Ready(c),
            Err(LoadError::Unavailable(m)) => Outcome::State(ClassifierState::Unavailable(m)),
            Err(LoadError::Failed(m)) => Outcome::State(ClassifierState::Failed(m)),
        })
    }

    /// The classifier of job `generation` disabled itself after repeated
    /// errors: stop handing it out and say so.
    fn tripped(&self, generation: u64) {
        let mut inner = self.lock();
        if inner.generation != generation || inner.classifier.is_none() {
            return;
        }
        let dropped = inner.classifier.take();
        let state = ClassifierState::Unavailable("disabled after repeated errors".into());
        inner.state = state.clone();
        drop(inner);
        (self.0.on_state)(state);
        drop(dropped);
    }

    /// Ends the job and records the outcome. A cancelled job (`None`) or a
    /// result that arrives after a disable is discarded; if the setting was
    /// switched back on meanwhile, a fresh job starts so that enable is not
    /// lost.
    fn finish(&self, outcome: Option<Outcome>) {
        let mut inner = self.lock();
        inner.job_running = false;
        if !inner.enabled {
            drop(inner);
            // Freeing a model can be slow; never under the lock.
            drop(outcome);
            return;
        }
        let state = match outcome {
            None => return self.start_job(inner),
            Some(Outcome::State(state)) => state,
            Some(Outcome::Ready(c)) => {
                inner.classifier = Some(c);
                ClassifierState::Ready
            }
        };
        inner.state = state.clone();
        drop(inner);
        (self.0.on_state)(state);
    }
}

enum Outcome {
    State(ClassifierState),
    Ready(Arc<dyn CommandClassifier>),
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::time::Duration;

    use review_approval::model_stage::{CommandClassifier, ModelOutcome};

    use super::*;

    struct Fake;
    impl CommandClassifier for Fake {
        fn classify(&self, _: &str) -> ModelOutcome {
            ModelOutcome::Safe
        }
    }

    #[derive(Default)]
    struct FakeSource {
        installed: Mutex<Option<PathBuf>>,
        /// Errors handed out by successive `ensure` calls; empty means success.
        ensure_errors: Mutex<Vec<String>>,
        ensure_calls: AtomicUsize,
        /// `ensure` blocks until this receiver yields (when set).
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        /// Set when `progress` told `ensure` to stop.
        told_to_stop: AtomicBool,
        /// On being told to stop, `ensure` signals here, then waits on
        /// `hold` before returning (each used once).
        stop_seen: Mutex<Option<mpsc::Sender<()>>>,
        hold: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl WeightsSource for FakeSource {
        fn installed_dir(&self) -> Option<PathBuf> {
            self.installed.lock().unwrap().clone()
        }
        fn ensure(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf, String> {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().unwrap().take() {
                rx.recv().unwrap();
            }
            if let Some(e) = self.ensure_errors.lock().unwrap().pop() {
                return Err(e);
            }
            for (done, total) in [(1, 4), (4, 4)] {
                if !progress(done, total) {
                    self.told_to_stop.store(true, Ordering::SeqCst);
                    if let Some(tx) = self.stop_seen.lock().unwrap().take() {
                        tx.send(()).unwrap();
                    }
                    if let Some(rx) = self.hold.lock().unwrap().take() {
                        rx.recv().unwrap();
                    }
                    return Err("cancelled".into());
                }
            }
            let dir = PathBuf::from("/fake/model");
            *self.installed.lock().unwrap() = Some(dir.clone());
            Ok(dir)
        }
    }

    struct FakeEngine {
        gpu: bool,
        load_error: Mutex<Option<LoadError>>,
        loads: AtomicUsize,
        delay: Duration,
        /// The trip callback of the latest load.
        trip: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl FakeEngine {
        fn ok() -> FakeEngine {
            FakeEngine {
                gpu: true,
                load_error: Mutex::new(None),
                loads: AtomicUsize::new(0),
                delay: Duration::ZERO,
                trip: Mutex::new(None),
            }
        }
    }

    impl Engine for FakeEngine {
        fn gpu_available(&self) -> bool {
            self.gpu
        }
        fn load(
            &self,
            _: &Path,
            on_trip: Box<dyn Fn() + Send + Sync>,
        ) -> Result<Arc<dyn CommandClassifier>, LoadError> {
            *self.trip.lock().unwrap() = Some(on_trip);
            self.loads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            match self.load_error.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Ok(Arc::new(Fake)),
            }
        }
    }

    struct Rig {
        loader: Loader,
        states: mpsc::Receiver<ClassifierState>,
        source: Arc<FakeSource>,
        engine: Arc<FakeEngine>,
    }

    fn rig(source: FakeSource, engine: FakeEngine) -> Rig {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let source = Arc::new(source);
        let engine = Arc::new(engine);
        let loader = Loader::with_parts(
            source.clone(),
            engine.clone(),
            Arc::new(move |s| {
                let _ = tx.lock().unwrap().send(s);
            }),
        );
        Rig {
            loader,
            states: rx,
            source,
            engine,
        }
    }

    impl Rig {
        /// Every state up to and including the first one `stop` accepts.
        fn until(&self, stop: impl Fn(&ClassifierState) -> bool) -> Vec<ClassifierState> {
            let mut seen = Vec::new();
            loop {
                let s = self
                    .states
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("timed out after {seen:?}"));
                let done = stop(&s);
                seen.push(s);
                if done {
                    return seen;
                }
            }
        }
    }

    fn is_ready(s: &ClassifierState) -> bool {
        *s == ClassifierState::Ready
    }

    fn is_failed(s: &ClassifierState) -> bool {
        matches!(s, ClassifierState::Failed(_))
    }

    use ClassifierState::*;

    #[test]
    fn enable_without_weights_downloads_then_loads() {
        let r = rig(FakeSource::default(), FakeEngine::ok());
        r.loader.set_enabled(true);
        assert_eq!(
            r.until(is_ready),
            [
                NotInstalled,
                Downloading { done: 0, total: 0 },
                Downloading { done: 1, total: 4 },
                Downloading { done: 4, total: 4 },
                Loading,
                Ready
            ]
        );
        assert!(r.loader.classifier().is_some());
    }

    #[test]
    fn enable_with_installed_weights_skips_the_download() {
        let source = FakeSource::default();
        *source.installed.lock().unwrap() = Some("/fake/model".into());
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        assert_eq!(r.until(is_ready), [Loading, Ready]);
        assert_eq!(r.source.ensure_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn download_error_is_failed_and_retryable() {
        let source = FakeSource::default();
        source.ensure_errors.lock().unwrap().push("offline".into());
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        let seen = r.until(is_failed);
        assert_eq!(seen.last(), Some(&Failed("offline".into())));
        assert!(r.loader.classifier().is_none());
        r.loader.download();
        let seen = r.until(is_ready);
        assert!(seen.contains(&Loading));
        assert!(r.loader.classifier().is_some());
    }

    /// M1: when the classifier trips its breaker, Settings stops showing Ready.
    #[test]
    fn a_tripped_classifier_is_reported_unavailable_and_not_handed_out() {
        let source = FakeSource::default();
        *source.installed.lock().unwrap() = Some(PathBuf::from("/fake/model"));
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| *s == Ready);
        assert!(r.loader.classifier().is_some());
        let trip = r.engine.trip.lock().unwrap().take().unwrap();
        trip();
        let seen = r.until(|s| matches!(s, Unavailable(_)));
        assert_eq!(
            seen.last(),
            Some(&Unavailable("disabled after repeated errors".into()))
        );
        assert!(r.loader.classifier().is_none());
        // A second report changes nothing.
        trip();
        assert!(r.states.try_recv().is_err());
    }

    /// A trip from a classifier of an earlier enable must not knock out the
    /// current one.
    #[test]
    fn a_stale_trip_is_ignored() {
        let source = FakeSource::default();
        *source.installed.lock().unwrap() = Some(PathBuf::from("/fake/model"));
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| *s == Ready);
        let stale = r.engine.trip.lock().unwrap().take().unwrap();
        r.loader.set_enabled(false);
        r.until(|s| *s == Off);
        r.loader.set_enabled(true);
        r.until(|s| *s == Ready);
        stale();
        assert!(r.loader.classifier().is_some());
        assert!(r.states.try_recv().is_err());
    }

    #[test]
    fn load_error_is_failed() {
        let engine = FakeEngine::ok();
        *engine.load_error.lock().unwrap() = Some(LoadError::Failed("bad tensor".into()));
        let r = rig(FakeSource::default(), engine);
        r.loader.set_enabled(true);
        let seen = r.until(is_failed);
        assert_eq!(seen.last(), Some(&Failed("bad tensor".into())));
        assert!(r.loader.classifier().is_none());
    }

    #[test]
    fn backend_open_error_is_unavailable() {
        let engine = FakeEngine::ok();
        *engine.load_error.lock().unwrap() = Some(LoadError::Unavailable("Metal: boom".into()));
        let r = rig(FakeSource::default(), engine);
        r.loader.set_enabled(true);
        let seen = r.until(|s| matches!(s, Unavailable(_)));
        assert_eq!(seen.last(), Some(&Unavailable("Metal: boom".into())));
    }

    #[test]
    fn no_gpu_is_unavailable_without_downloading() {
        let mut engine = FakeEngine::ok();
        engine.gpu = false;
        let r = rig(FakeSource::default(), engine);
        r.loader.set_enabled(true);
        assert_eq!(
            r.until(|s| matches!(s, Unavailable(_))),
            [Unavailable("no usable GPU".into())]
        );
        assert_eq!(r.source.ensure_calls.load(Ordering::SeqCst), 0);
        assert!(r.loader.classifier().is_none());
    }

    #[test]
    fn disable_drops_the_classifier() {
        let r = rig(FakeSource::default(), FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(is_ready);
        r.loader.set_enabled(false);
        assert_eq!(r.until(|s| *s == Off), [Off]);
        assert!(r.loader.classifier().is_none());
    }

    #[test]
    fn disabling_during_the_download_discards_the_result() {
        let source = FakeSource::default();
        let (release, gate) = mpsc::channel();
        *source.gate.lock().unwrap() = Some(gate);
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| matches!(s, Downloading { .. }));
        r.loader.set_enabled(false);
        release.send(()).unwrap();
        r.until(|s| *s == Off);
        // Give the job time to finish; it must not resurrect anything.
        std::thread::sleep(Duration::from_millis(200));
        assert!(r.loader.classifier().is_none());
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 0);
        assert!(r.source.told_to_stop.load(Ordering::SeqCst));
        while let Ok(s) = r.states.try_recv() {
            assert_eq!(s, Off, "no state after Off");
        }
    }

    #[test]
    fn rapid_toggle_does_not_double_load() {
        let mut engine = FakeEngine::ok();
        engine.delay = Duration::from_millis(150);
        let source = FakeSource::default();
        *source.installed.lock().unwrap() = Some("/fake/model".into());
        let r = rig(source, engine);
        r.loader.set_enabled(true);
        r.until(|s| *s == Loading);
        r.loader.set_enabled(false);
        r.loader.set_enabled(true);
        r.until(is_ready);
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 1);
        assert!(r.loader.classifier().is_some());
    }

    #[test]
    fn callback_never_called_while_holding_the_lock() {
        let (done_tx, done_rx) = mpsc::channel();
        let done_tx = Mutex::new(done_tx);
        let cell: Arc<OnceLock<Loader>> = Arc::new(OnceLock::new());
        let in_cb = cell.clone();
        let source = Arc::new(FakeSource::default());
        let loader = Loader::with_parts(
            source,
            Arc::new(FakeEngine::ok()),
            Arc::new(move |s| {
                // Re-entering would deadlock if the lock were held.
                let l = in_cb.get().expect("set before enabling");
                let _ = l.classifier();
                l.set_enabled(true);
                if s == Ready {
                    let _ = done_tx.lock().unwrap().send(());
                }
            }),
        );
        cell.set(loader.clone()).ok();
        loader.set_enabled(true);
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("callback re-entry deadlocked");
    }

    #[test]
    fn re_enable_while_the_old_job_is_cancelling_starts_a_new_job() {
        let source = FakeSource::default();
        let (release, gate) = mpsc::channel();
        let (seen_tx, seen_rx) = mpsc::channel();
        let (hold_tx, hold_rx) = mpsc::channel();
        *source.gate.lock().unwrap() = Some(gate);
        *source.stop_seen.lock().unwrap() = Some(seen_tx);
        *source.hold.lock().unwrap() = Some(hold_rx);
        let r = rig(source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| matches!(s, Downloading { .. }));
        r.loader.set_enabled(false);
        r.until(|s| *s == Off);
        // The old job learns it was cancelled, and is held right there.
        release.send(()).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        r.loader.set_enabled(true);
        // The UI is shown the in-flight state, not Off.
        assert_eq!(r.until(|_| true), [Downloading { done: 1, total: 4 }]);
        hold_tx.send(()).unwrap();
        r.until(is_ready);
        assert_eq!(r.source.ensure_calls.load(Ordering::SeqCst), 2);
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 1);
        assert!(r.loader.classifier().is_some());
    }
}
