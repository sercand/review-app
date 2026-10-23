//! What the Settings window and the review run are told about the local
//! classifier: where it is on its way from "switched on" to "ready".

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use review_approval::model_stage::CommandClassifier;
use shiso_decision::error::DecideError;
use shiso_decision::{Decider, OpenDesc};

use super::decision::DecisionClassifier;
use super::schema::{SchemaSource, load_schema};
use super::weights::WeightsManager;

pub use crate::state::classifier::ClassifierState;

/// Where the weights come from; a fake in tests.
pub trait WeightsSource: Send + Sync {
    /// The install of `model` known to be complete, without touching the
    /// network. `None` does not mean it is absent, only that this source
    /// cannot tell cheaply: `ensure` then returns at once if it is there.
    fn installed_dir(&self, model: &str) -> Option<PathBuf>;
    /// Blocking download + verify + install; call from a job thread.
    /// `progress(done, total)` returns `false` to cancel: the download stops
    /// (keeping partial files for resume) and `ensure` returns an error.
    fn ensure(
        &self,
        model: &str,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, LoadError>;
}

impl WeightsSource for WeightsManager {
    fn installed_dir(&self, model: &str) -> Option<PathBuf> {
        WeightsManager::installed_dir(self, model)
    }
    fn ensure(
        &self,
        model: &str,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, LoadError> {
        self.ensure_blocking(model, progress)
    }
}

/// Why the engine could not produce a classifier.
#[derive(Debug)]
pub enum LoadError {
    /// The device cannot be used (backend creation failed), or the model's
    /// schema is unusable.
    Unavailable(String),
    /// The download or the model load failed.
    Failed(String),
}

/// Maps a shiso error to the state it should show. A missing device is
/// `Unavailable`; everything else is retryable.
pub(super) fn load_error(error: DecideError) -> LoadError {
    match error {
        DecideError::NoDevice(why) => LoadError::Unavailable(format!("no usable GPU: {why}")),
        other => LoadError::Failed(other.to_string()),
    }
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
    ) -> Result<(Arc<dyn CommandClassifier>, SchemaSource), LoadError>;
}

/// The real engine: the model's schema, then a `shiso_decision::Decider`.
///
/// There is no cheap device probe in shiso, so the check before the download
/// is only whether this build has a backend at all. On a machine whose
/// device is unusable the download therefore still happens and the failure
/// shows at open time (`NoDevice` becomes `Unavailable`).
struct ShisoEngine;

impl Engine for ShisoEngine {
    fn gpu_available(&self) -> bool {
        cfg!(any(
            feature = "local-classifier",
            feature = "local-classifier-cuda"
        ))
    }

    fn load(
        &self,
        dir: &Path,
        on_trip: Box<dyn Fn() + Send + Sync>,
    ) -> Result<(Arc<dyn CommandClassifier>, SchemaSource), LoadError> {
        // An unreadable schema is never replaced by the default: the model
        // was shipped with a question this build cannot ask.
        let (schema, source) = load_schema(dir).map_err(LoadError::Unavailable)?;
        let decider = Decider::open(&OpenDesc::new(dir)).map_err(load_error)?;
        let classifier = DecisionClassifier::new(Arc::new(decider), schema, on_trip);
        classifier.check_schema().map_err(LoadError::Unavailable)?;
        Ok((Arc::new(classifier), source))
    }
}

type OnState = Arc<dyn Fn(ClassifierState) + Send + Sync>;

struct Inner {
    enabled: bool,
    /// The registry name of the model wanted now.
    model: String,
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
    /// A loader for the registry model `model`.
    pub fn new(manager: WeightsManager, model: &str, on_state: OnState) -> Loader {
        Loader::with_parts(Arc::new(manager), Arc::new(ShisoEngine), model, on_state)
    }

    fn with_parts(
        source: Arc<dyn WeightsSource>,
        engine: Arc<dyn Engine>,
        model: &str,
        on_state: OnState,
    ) -> Loader {
        Loader(Arc::new(Shared {
            inner: Mutex::new(Inner {
                enabled: false,
                model: model.to_owned(),
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
                self.start_job(inner, None);
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

    /// Switches to another registry model. While enabled, a running job for
    /// the old model is cancelled (its download stops) and the new model is
    /// downloaded and loaded; the old classifier is dropped at once and never
    /// handed out again, even by a job that was already finishing. While
    /// disabled the name is only remembered for the next enable.
    pub fn set_model(&self, model: &str) {
        let mut inner = self.lock();
        if inner.model == model {
            return;
        }
        inner.model = model.to_owned();
        if !inner.enabled {
            return;
        }
        let dropped = inner.classifier.take();
        if inner.job_running {
            // The running job sees the new name at its next progress report
            // (or when it finishes) and stops; say at once that the old
            // model's progress no longer applies.
            inner.state = ClassifierState::Loading;
            inner.phase = ClassifierState::Loading;
            drop(inner);
            (self.0.on_state)(ClassifierState::Loading);
        } else {
            self.start_job(inner, Some(ClassifierState::Loading));
        }
        // Freeing the model can be slow; do it off the lock.
        drop(dropped);
    }

    /// Retry after a `Failed` state.
    pub fn download(&self) {
        let inner = self.lock();
        let retryable = matches!(inner.state, ClassifierState::Failed(_));
        if inner.enabled && !inner.job_running && retryable {
            self.start_job(inner, None);
        }
    }

    /// The classifier, only while `Ready`.
    pub fn classifier(&self) -> Option<Arc<dyn CommandClassifier>> {
        self.lock().classifier.clone()
    }

    /// Starts a job thread for the current model unless one is running. Takes
    /// the guard so the check and the flag are one critical section.
    /// `announce` is reported before the thread starts, so the job's own
    /// first report cannot be overtaken by it.
    fn start_job(&self, mut inner: MutexGuard<'_, Inner>, announce: Option<ClassifierState>) {
        if inner.job_running {
            return;
        }
        inner.job_running = true;
        inner.generation += 1;
        inner.phase = ClassifierState::Loading;
        let model = inner.model.clone();
        if let Some(state) = &announce {
            inner.state = state.clone();
        }
        drop(inner);
        if let Some(state) = announce {
            (self.0.on_state)(state);
        }
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("classifier-loader".into())
            .spawn(move || this.run_job(model));
        if let Err(e) = spawned {
            self.finish(
                Some(Outcome::State(ClassifierState::Failed(format!(
                    "could not start the loader thread: {e}"
                )))),
                None,
            );
        }
    }

    fn run_job(&self, model: String) {
        let outcome = match catch_unwind(AssertUnwindSafe(|| self.job(&model))) {
            Ok(outcome) => outcome,
            Err(_) => Some(Outcome::State(ClassifierState::Failed(
                "the classifier loader panicked".into(),
            ))),
        };
        self.finish(outcome, Some(&model));
    }

    /// Reports an intermediate state of the job for `model`; `false` means
    /// the setting was switched off or the model changed meanwhile and the
    /// job should stop.
    fn progress(&self, model: &str, state: ClassifierState) -> bool {
        let mut inner = self.lock();
        if inner.model != model {
            return false;
        }
        inner.phase = state.clone();
        if !inner.enabled {
            return false;
        }
        inner.state = state.clone();
        drop(inner);
        (self.0.on_state)(state);
        true
    }

    /// `None` means the job was cancelled by disabling or a model change.
    fn job(&self, model: &str) -> Option<Outcome> {
        let shared = &self.0;
        if !shared.engine.gpu_available() {
            return Some(Outcome::State(ClassifierState::Unavailable(
                "no usable GPU".into(),
            )));
        }
        let dir = match shared.source.installed_dir(model) {
            Some(dir) => dir,
            None => {
                // Shown for an install too (shiso has no offline probe): the
                // call below then returns at once and `Loading` follows.
                if !self.progress(model, ClassifierState::Downloading { done: 0, total: 0 }) {
                    return None;
                }
                let mut cancelled = false;
                let fetched = shared.source.ensure(model, &mut |done, total| {
                    let go = self.progress(model, ClassifierState::Downloading { done, total });
                    cancelled |= !go;
                    go
                });
                if cancelled {
                    return None;
                }
                match fetched {
                    Ok(dir) => dir,
                    Err(e) => return Some(Outcome::State(e.into())),
                }
            }
        };
        if !self.progress(model, ClassifierState::Loading) {
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
            Ok((c, source)) => Outcome::Ready(c, source),
            Err(e) => Outcome::State(e.into()),
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

    /// Ends the job and records the outcome. A cancelled job (`None`), a
    /// result that arrives after a disable, or one for a model that is no
    /// longer wanted is discarded; if the setting is on, a fresh job starts
    /// (for the current model) so that enable or switch is not lost.
    /// `model` is the job's model (`None` when the thread never started).
    fn finish(&self, outcome: Option<Outcome>, model: Option<&str>) {
        let mut inner = self.lock();
        inner.job_running = false;
        if !inner.enabled {
            drop(inner);
            // Freeing a model can be slow; never under the lock.
            drop(outcome);
            return;
        }
        if model.is_some_and(|m| m != inner.model) {
            // The model was switched while this job ran: whatever it made is
            // for the wrong model.
            // `set_model` usually announced `Loading` already.
            let announce =
                (inner.state != ClassifierState::Loading).then_some(ClassifierState::Loading);
            self.start_job(inner, announce);
            drop(outcome);
            return;
        }
        let state = match outcome {
            None => return self.start_job(inner, None),
            Some(Outcome::State(state)) => state,
            Some(Outcome::Ready(c, source)) => {
                inner.classifier = Some(c);
                ClassifierState::Ready(source)
            }
        };
        inner.state = state.clone();
        drop(inner);
        (self.0.on_state)(state);
    }
}

impl From<LoadError> for ClassifierState {
    fn from(error: LoadError) -> ClassifierState {
        match error {
            LoadError::Unavailable(m) => ClassifierState::Unavailable(m),
            LoadError::Failed(m) => ClassifierState::Failed(m),
        }
    }
}

enum Outcome {
    State(ClassifierState),
    Ready(Arc<dyn CommandClassifier>, SchemaSource),
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::time::Duration;

    use review_approval::model_stage::{ClassifyInput, CommandClassifier, ModelOutcome};

    use super::*;

    /// A classifier that says which model it came from: `Dangerous` for the
    /// model named `old`, `Safe` for any other.
    struct Fake {
        old: bool,
    }
    impl CommandClassifier for Fake {
        fn classify(&self, _: &ClassifyInput<'_>) -> ModelOutcome {
            if self.old {
                ModelOutcome::Dangerous
            } else {
                ModelOutcome::Safe
            }
        }
    }

    fn classify_with(c: &Arc<dyn CommandClassifier>) -> ModelOutcome {
        c.classify(&ClassifyInput {
            segment: "ls",
            cwd: None,
        })
    }

    #[derive(Default)]
    struct FakeSource {
        installed: Mutex<Option<PathBuf>>,
        /// Errors handed out by successive `ensure` calls; empty means success.
        ensure_errors: Mutex<Vec<String>>,
        ensure_calls: AtomicUsize,
        /// The model of each `ensure` call.
        ensure_models: Mutex<Vec<String>>,
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
        fn installed_dir(&self, _: &str) -> Option<PathBuf> {
            self.installed.lock().unwrap().clone()
        }
        fn ensure(
            &self,
            model: &str,
            progress: &mut dyn FnMut(u64, u64) -> bool,
        ) -> Result<PathBuf, LoadError> {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            self.ensure_models.lock().unwrap().push(model.to_owned());
            if let Some(rx) = self.gate.lock().unwrap().take() {
                rx.recv().unwrap();
            }
            if let Some(e) = self.ensure_errors.lock().unwrap().pop() {
                return Err(LoadError::Failed(e));
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
                    return Err(LoadError::Failed("cancelled".into()));
                }
            }
            Ok(PathBuf::from(format!("/fake/{model}")))
        }
    }

    struct FakeEngine {
        gpu: bool,
        load_error: Mutex<Option<LoadError>>,
        loads: AtomicUsize,
        /// The directory of each load.
        loaded: Mutex<Vec<PathBuf>>,
        schema: SchemaSource,
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
                loaded: Mutex::new(Vec::new()),
                schema: SchemaSource::BuiltIn,
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
            dir: &Path,
            on_trip: Box<dyn Fn() + Send + Sync>,
        ) -> Result<(Arc<dyn CommandClassifier>, SchemaSource), LoadError> {
            *self.trip.lock().unwrap() = Some(on_trip);
            self.loaded.lock().unwrap().push(dir.to_owned());
            self.loads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            match self.load_error.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Ok((
                    Arc::new(Fake {
                        old: dir.ends_with("old"),
                    }),
                    self.schema,
                )),
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
        rig_for("d1-3b", source, engine)
    }

    fn rig_for(model: &str, source: FakeSource, engine: FakeEngine) -> Rig {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let source = Arc::new(source);
        let engine = Arc::new(engine);
        let loader = Loader::with_parts(
            source.clone(),
            engine.clone(),
            model,
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
        matches!(s, ClassifierState::Ready(_))
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
                Downloading { done: 0, total: 0 },
                Downloading { done: 1, total: 4 },
                Downloading { done: 4, total: 4 },
                Loading,
                Ready(SchemaSource::BuiltIn)
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
        assert_eq!(r.until(is_ready), [Loading, Ready(SchemaSource::BuiltIn)]);
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
        r.until(|s| *s == Ready(SchemaSource::BuiltIn));
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
        r.until(|s| *s == Ready(SchemaSource::BuiltIn));
        let stale = r.engine.trip.lock().unwrap().take().unwrap();
        r.loader.set_enabled(false);
        r.until(|s| *s == Off);
        r.loader.set_enabled(true);
        r.until(|s| *s == Ready(SchemaSource::BuiltIn));
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
            "d1-3b",
            Arc::new(move |s| {
                // Re-entering would deadlock if the lock were held.
                let l = in_cb.get().expect("set before enabling");
                let _ = l.classifier();
                l.set_enabled(true);
                if s == Ready(SchemaSource::BuiltIn) {
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

    #[test]
    fn ready_reports_schema_source() {
        for source in [SchemaSource::Model, SchemaSource::BuiltIn] {
            let mut engine = FakeEngine::ok();
            engine.schema = source;
            let r = rig(FakeSource::default(), engine);
            r.loader.set_enabled(true);
            assert_eq!(r.until(is_ready).last(), Some(&Ready(source)));
        }
    }

    /// The setting changed while the old model's download was running: that
    /// download stops, the new model is fetched and loaded, and nothing of
    /// the old model is ever used.
    #[test]
    fn model_change_cancels_and_reloads() {
        let source = FakeSource::default();
        let (release, gate) = mpsc::channel();
        *source.gate.lock().unwrap() = Some(gate);
        let r = rig_for("old", source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| matches!(s, Downloading { .. }));
        r.loader.set_model("new");
        release.send(()).unwrap();
        // The old download is told to stop at its next report; nothing from
        // it reaches the UI, and the new job is announced first.
        let seen = r.until(is_ready);
        assert_eq!(
            seen,
            [
                Loading,
                Downloading { done: 0, total: 0 },
                Downloading { done: 1, total: 4 },
                Downloading { done: 4, total: 4 },
                Loading,
                Ready(SchemaSource::BuiltIn)
            ]
        );
        assert!(r.source.told_to_stop.load(Ordering::SeqCst));
        assert_eq!(*r.source.ensure_models.lock().unwrap(), ["old", "new"]);
        assert_eq!(
            *r.engine.loaded.lock().unwrap(),
            [PathBuf::from("/fake/new")]
        );
        let c = r.loader.classifier().unwrap();
        assert_eq!(classify_with(&c), ModelOutcome::Safe, "the new model");
    }

    /// `Loading` is announced on a model switch while the old job is still
    /// blocked, not after it is released.
    #[test]
    fn a_model_switch_announces_loading_before_the_old_job_is_released() {
        let source = FakeSource::default();
        let (release, gate) = mpsc::channel();
        *source.gate.lock().unwrap() = Some(gate);
        let r = rig_for("old", source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(|s| matches!(s, Downloading { .. }));
        r.loader.set_model("new");
        let first = r
            .states
            .recv_timeout(Duration::from_secs(10))
            .expect("Loading is announced while the old job is still gated");
        assert_eq!(first, Loading);
        release.send(()).unwrap();
        r.until(is_ready);
    }

    /// A model that is already loaded is dropped the moment the name changes.
    #[test]
    fn switching_models_drops_the_loaded_classifier() {
        let mut engine = FakeEngine::ok();
        engine.delay = Duration::from_millis(150);
        let r = rig_for("old", FakeSource::default(), engine);
        r.loader.set_enabled(true);
        r.until(is_ready);
        assert_eq!(
            classify_with(&r.loader.classifier().unwrap()),
            ModelOutcome::Dangerous
        );
        r.loader.set_model("new");
        assert!(r.loader.classifier().is_none(), "old one is gone at once");
        assert_eq!(r.until(|_| true), [Loading]);
        assert_eq!(
            r.until(is_ready).last(),
            Some(&Ready(SchemaSource::BuiltIn))
        );
        assert_eq!(
            classify_with(&r.loader.classifier().unwrap()),
            ModelOutcome::Safe
        );
    }

    /// The old model's load finishes after the switch: its result is thrown
    /// away, not published.
    #[test]
    fn a_load_that_finishes_after_the_switch_is_discarded() {
        let mut engine = FakeEngine::ok();
        engine.delay = Duration::from_millis(300);
        let r = rig_for("old", FakeSource::default(), engine);
        r.loader.set_enabled(true);
        // Wait until the old load is in flight.
        while r.engine.loads.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        r.loader.set_model("new");
        let mut states = Vec::new();
        loop {
            // Never the old model, at any moment.
            if let Some(c) = r.loader.classifier() {
                assert_eq!(classify_with(&c), ModelOutcome::Safe);
            }
            match r.states.recv_timeout(Duration::from_millis(20)) {
                Ok(s) => {
                    let done = is_ready(&s);
                    states.push(s);
                    if done {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(
            states.iter().filter(|s| is_ready(s)).count(),
            1,
            "{states:?}"
        );
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 2);
        assert_eq!(
            classify_with(&r.loader.classifier().unwrap()),
            ModelOutcome::Safe
        );
    }

    #[test]
    fn model_change_while_disabled_waits_for_the_next_enable() {
        let r = rig_for("old", FakeSource::default(), FakeEngine::ok());
        r.loader.set_model("new");
        r.loader.set_model("new");
        std::thread::sleep(Duration::from_millis(50));
        assert!(r.states.try_recv().is_err());
        assert_eq!(r.source.ensure_calls.load(Ordering::SeqCst), 0);
        r.loader.set_enabled(true);
        r.until(is_ready);
        assert_eq!(*r.source.ensure_models.lock().unwrap(), ["new"]);
    }

    /// Switching off and then naming another model (what one save that does
    /// both comes to) starts nothing: no `ensure`, no state.
    #[test]
    fn a_model_name_set_after_switching_off_never_downloads() {
        let r = rig_for("old", FakeSource::default(), FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(is_ready);
        r.loader.set_enabled(false);
        r.until(|s| *s == Off);
        r.loader.set_model("new");
        std::thread::sleep(Duration::from_millis(100));
        assert!(r.states.try_recv().is_err());
        assert_eq!(*r.source.ensure_models.lock().unwrap(), ["old"]);
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 1);
        assert!(r.loader.classifier().is_none());
    }

    #[test]
    fn the_same_model_name_changes_nothing() {
        let source = FakeSource::default();
        *source.installed.lock().unwrap() = Some("/fake/model".into());
        let r = rig_for("d1-3b", source, FakeEngine::ok());
        r.loader.set_enabled(true);
        r.until(is_ready);
        r.loader.set_model("d1-3b");
        assert!(r.loader.classifier().is_some());
        assert!(r.states.try_recv().is_err());
        assert_eq!(r.engine.loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shiso_errors_map_to_states() {
        use DecideError::*;
        assert!(matches!(
            load_error(NoDevice("no Metal".into())),
            LoadError::Unavailable(m) if m == "no usable GPU: no Metal"
        ));
        assert!(matches!(
            load_error(Download("offline".into())),
            LoadError::Failed(m) if m.contains("offline")
        ));
        assert!(matches!(
            load_error(UnknownModel("nope".into())),
            LoadError::Failed(m) if m.contains("nope")
        ));
        assert!(matches!(load_error(Cancelled), LoadError::Failed(_)));
    }

    /// An invalid schema next to the weights is `Unavailable`, never the
    /// built-in question, and nothing is opened.
    #[test]
    fn invalid_schema_is_unavailable_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("decision_schema.json"), "{ not json").unwrap();
        let got = ShisoEngine.load(dir.path(), Box::new(|| {}));
        match got {
            Err(LoadError::Unavailable(m)) => {
                assert!(m.starts_with("invalid decision schema"), "{m}")
            }
            Err(LoadError::Failed(m)) => panic!("failed, not unavailable: {m}"),
            Ok(_) => panic!("loaded despite an invalid schema"),
        }
    }

    #[test]
    fn the_real_engine_needs_a_compiled_backend() {
        assert_eq!(
            ShisoEngine.gpu_available(),
            cfg!(any(
                feature = "local-classifier",
                feature = "local-classifier-cuda"
            ))
        );
    }

    /// The whole path on the real engine: `ensure` (offline when the model is
    /// installed in `SHISO_TEST_HOME`), schema, device, an answer.
    #[test]
    #[ignore = "requires a GPU build and SHISO_TEST_HOME with d1-3b installed"]
    fn real_loader_answers_ls_la() {
        let home = std::env::var("SHISO_TEST_HOME")
            .expect("set SHISO_TEST_HOME to a model home that has d1-3b");
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let loader = Loader::with_parts(
            Arc::new(WeightsManager::with_home(PathBuf::from(home))),
            Arc::new(ShisoEngine),
            "d1-3b",
            Arc::new(move |s| {
                eprintln!("state: {s:?}");
                let _ = tx.lock().unwrap().send(s);
            }),
        );
        loader.set_enabled(true);
        loop {
            let s = rx
                .recv_timeout(Duration::from_secs(300))
                .expect("timed out");
            match s {
                Ready(source) => {
                    eprintln!("ready, schema {source:?}");
                    break;
                }
                Failed(_) | Unavailable(_) => panic!("{s:?}"),
                _ => {}
            }
        }
        let c = loader.classifier().expect("ready");
        let ls = c.classify(&ClassifyInput {
            segment: "ls -la",
            cwd: Some(Path::new("/repo")),
        });
        let rm = c.classify(&ClassifyInput {
            segment: "rm -rf /",
            cwd: Some(Path::new("/repo")),
        });
        eprintln!("ls -la -> {ls:?}; rm -rf / -> {rm:?}");
        assert_eq!(ls, ModelOutcome::Safe);
        assert_ne!(rm, ModelOutcome::Safe);
        loader.set_enabled(false);
    }
}
