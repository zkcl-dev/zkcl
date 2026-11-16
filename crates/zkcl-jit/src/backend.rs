//! Background compilation, revision publication, and cancellation.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_runtime::native::{
    JitBackend, JitCompilationRequest, JitCompletions, JitFunction, JitOsrRequest, NativeEntry,
};

use crate::{CodeCache, JitError, MappedJitFunction};

// Bound queued snapshots as well as completed compilations awaiting the mutator's poll.
// Installed functions stay mapped only while a runtime holds the revision or a host
// keeps an explicit `MappedJitFunction` handle.
const MAX_PENDING_REQUESTS: usize = 64;
const MAX_COMPILER_THREADS: usize = 2;
// Leave one pending slot for an explicit request during a burst of hot calls.
const AUTOMATIC_ADMISSION_LIMIT: usize = MAX_PENDING_REQUESTS - 1;
// At least one in four worker turns goes to automatic work while both queues have requests.
const MAX_CONSECUTIVE_EXPLICIT_REQUESTS: usize = 3;

/// Requests waiting for a worker, ordered by what their submitter is waiting on.
///
/// Explicit work blocks the submitting runtime. Automatic work serves hot calls
/// and running loops asynchronously. Automatic admission pauses while explicit
/// work waits, with one worker turn in four reserved for queued automatic work.
#[derive(Default)]
struct PendingQueue {
    queued_automatic: VecDeque<QueuedCompilation>,
    queued_explicit: VecDeque<QueuedCompilation>,
    consecutive_explicit_requests: usize,
    compiling_revisions: FxHashSet<u64>,
    pending_code_ids: FxHashMap<u64, usize>,
    completed_revisions: FxHashMap<u64, CompilationOutcome>,
    installed_revisions: FxHashMap<u64, MappedJitFunction>,
    shutting_down: bool,
}

impl PendingQueue {
    /// Whether a speculative request may join the queue now.
    ///
    /// Hot calls stop adding work while a submitter waits, so a foreground burst
    /// is not refilled behind by the calls that triggered it.
    fn admits_automatic(&self) -> bool {
        self.queued_explicit.is_empty() && self.pending_code_ids.len() < AUTOMATIC_ADMISSION_LIMIT
    }

    /// Take the next request, giving explicit work at least one turn in four.
    fn take_next(&mut self) -> Option<(QueuedCompilation, CompilationKind)> {
        let automatic_waiting = !self.queued_automatic.is_empty();
        let explicit_waiting = !self.queued_explicit.is_empty();
        let explicit_first = explicit_waiting
            && (!automatic_waiting
                || self.consecutive_explicit_requests < MAX_CONSECUTIVE_EXPLICIT_REQUESTS);
        let (queued, kind) = if explicit_first {
            (&mut self.queued_explicit, CompilationKind::Explicit)
        } else {
            (&mut self.queued_automatic, CompilationKind::Automatic)
        };
        let request = queued.pop_front()?;
        // Count only turns where automatic work could also have run.
        self.consecutive_explicit_requests = match (kind, automatic_waiting) {
            (CompilationKind::Explicit, true) => self.consecutive_explicit_requests + 1,
            _ => 0,
        };
        Some((request, kind))
    }

    /// Drop queued requests for `revision`; a worker that already claimed it
    /// discards the result when it publishes.
    fn drop_queued(&mut self, revision: u64) {
        self.queued_explicit
            .retain(|queued| queued.request.revision != revision);
        self.queued_automatic
            .retain(|queued| queued.request.revision != revision);
    }
}

struct QueuedCompilation {
    request: JitCompilationRequest,
    osr_pc: Option<usize>,
}

impl From<JitCompilationRequest> for QueuedCompilation {
    fn from(request: JitCompilationRequest) -> Self {
        Self {
            request,
            osr_pc: None,
        }
    }
}

/// Whether a request blocks its submitter (explicit) or only speculates (automatic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompilationKind {
    Automatic,
    Explicit,
}

struct CompilationOutcome {
    code_id: usize,
    is_osr: bool,
    compiled: Option<CompiledRevision>,
}

pub(crate) struct CompiledRevision {
    pub(crate) mapping: MappedJitFunction,
    pub(crate) function: JitFunction,
}

/// Stop-point activity of one compilation queue, recorded on its worker threads.
///
/// `research/backend-optimization-and-feedback.md` asks for the current global-epoch
/// recheck count before the epoch is replaced with per-consumer invalidation. These
/// two counts are that measurement, not a cache.
#[cfg(feature = "counters")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueStopPointCounters {
    /// Stop points answered by the epoch fast path, without taking the queue lock.
    pub fast_path: u64,
    /// Stop points that took the queue lock because the epoch had moved.
    pub rechecks: u64,
}

#[cfg(feature = "counters")]
#[derive(Default)]
struct StopPointCounters {
    fast_path: AtomicU64,
    rechecks: AtomicU64,
}

#[cfg(feature = "counters")]
impl StopPointCounters {
    fn record_fast_path(&self) {
        self.fast_path.fetch_add(1, Ordering::Relaxed);
    }

    fn record_recheck(&self) {
        self.rechecks.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> QueueStopPointCounters {
        QueueStopPointCounters {
            fast_path: self.fast_path.load(Ordering::Relaxed),
            rechecks: self.rechecks.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct CompilationQueue {
    state: Mutex<PendingQueue>,
    request_available: Condvar,
    has_completions: AtomicBool,
    cancellation_generation: AtomicU64,
    #[cfg(feature = "counters")]
    stop_points: StopPointCounters,
}

impl CompilationQueue {
    fn enqueue(
        &self,
        mut state: MutexGuard<'_, PendingQueue>,
        queued: impl Into<QueuedCompilation>,
        kind: CompilationKind,
    ) -> bool {
        let queued = queued.into();
        let request = &queued.request;
        let has_capacity = match kind {
            CompilationKind::Automatic => state.admits_automatic(),
            CompilationKind::Explicit => state.pending_code_ids.len() < MAX_PENDING_REQUESTS,
        };
        if !has_capacity
            || state.pending_code_ids.contains_key(&request.revision)
            || state.installed_revisions.contains_key(&request.revision)
            || state.compiling_revisions.contains(&request.revision)
        {
            return false;
        }
        state
            .pending_code_ids
            .insert(request.revision, request.code_id);
        match kind {
            CompilationKind::Automatic => state.queued_automatic.push_back(queued),
            CompilationKind::Explicit => state.queued_explicit.push_back(queued),
        }
        drop(state);
        self.request_available.notify_one();
        true
    }

    fn wait_for_request(&self) -> Option<(QueuedCompilation, CompilationKind, u64)> {
        let mut state = self
            .request_available
            .wait_while(self.state.lock().unwrap(), |state| {
                !state.shutting_down
                    && state.queued_automatic.is_empty()
                    && state.queued_explicit.is_empty()
            })
            .unwrap();
        if state.shutting_down {
            return None;
        }
        let (request, kind) = state.take_next().expect("a wake-up leaves one request");
        state.compiling_revisions.insert(request.request.revision);
        Some((
            request,
            kind,
            self.cancellation_generation.load(Ordering::Relaxed),
        ))
    }

    /// Whether a claimed request is still wanted by its submitter.
    ///
    /// A worker records the epoch it claimed at. Releasing a claimed revision or
    /// shutting down bumps that epoch, so an unchanged value proves the claim
    /// still stands without taking the queue lock.
    fn is_still_pending(&self, code_id: usize, revision: u64, observed: &Cell<u64>) -> bool {
        if self.cancellation_generation.load(Ordering::Acquire) == observed.get() {
            #[cfg(feature = "counters")]
            self.stop_points.record_fast_path();
            return true;
        }
        #[cfg(feature = "counters")]
        self.stop_points.record_recheck();
        let state = self.state.lock().unwrap();
        let pending =
            !state.shutting_down && state.pending_code_ids.get(&revision) == Some(&code_id);
        if pending {
            observed.set(self.cancellation_generation.load(Ordering::Relaxed));
        }
        pending
    }

    fn shutdown(&self) {
        // `Drop` runs while a panicked test or worker unwinds with the lock possibly
        // poisoned. Panicking again there aborts the process and hides the first failure.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .shutting_down = true;
        self.cancellation_generation.fetch_add(1, Ordering::Release);
        self.request_available.notify_all();
    }

    fn publish(&self, cache: &CodeCache, revision: u64, outcome: CompilationOutcome) {
        let mut state = self.state.lock().unwrap();
        state.compiling_revisions.remove(&revision);
        if state.shutting_down || state.pending_code_ids.get(&revision) != Some(&outcome.code_id) {
            return;
        }
        // Cancellation shares this lock. Index the mapping before exposing the completion.
        if let Some(compiled) = &outcome.compiled {
            cache.track_runtime(&compiled.mapping, outcome.is_osr);
        }
        state.completed_revisions.insert(revision, outcome);
        self.has_completions.store(true, Ordering::Release);
    }

    fn install_completed(
        &self,
        belongs_to_runtime: &mut dyn FnMut(usize, u64) -> bool,
    ) -> Option<JitCompletions> {
        let mut state = self.state.try_lock().ok()?;
        let PendingQueue {
            pending_code_ids,
            completed_revisions,
            installed_revisions,
            ..
        } = &mut *state;
        let mut installed_functions = Vec::new();
        let mut failed = Vec::new();
        for (revision, completion) in completed_revisions
            .extract_if(|&revision, completion| belongs_to_runtime(completion.code_id, revision))
        {
            pending_code_ids.remove(&revision);
            match completion.compiled {
                Some(compiled) => {
                    installed_functions.push(compiled.function);
                    installed_revisions.insert(revision, compiled.mapping);
                }
                None => failed.push((completion.code_id, revision)),
            }
        }
        self.has_completions
            .store(!completed_revisions.is_empty(), Ordering::Release);
        Some((installed_functions, failed))
    }

    fn release_revision(&self, code_id: usize, revision: u64) -> Option<MappedJitFunction> {
        let mut state = self.state.lock().unwrap();
        if state.pending_code_ids.get(&revision) == Some(&code_id) {
            state.pending_code_ids.remove(&revision);
            // Only a claimed revision has a worker taking the epoch fast path;
            // releasing one that is still queued invalidates nothing.
            if state.compiling_revisions.contains(&revision) {
                self.cancellation_generation.fetch_add(1, Ordering::Release);
            }
            if let Some(completed) = state.completed_revisions.remove(&revision) {
                self.has_completions
                    .store(!state.completed_revisions.is_empty(), Ordering::Release);
                return completed.compiled.map(|compiled| compiled.mapping);
            }
            state.drop_queued(revision);
        } else if state
            .installed_revisions
            .get(&revision)
            .is_some_and(|code| code.code_id == code_id)
        {
            return state.installed_revisions.remove(&revision);
        }
        None
    }
}

/// A compiler target with a bounded queue and revision-scoped executable mappings.
pub struct BackgroundJit<T> {
    pub(crate) target: T,
    cache: Arc<CodeCache>,
    queue: Arc<CompilationQueue>,
    workers: Vec<JoinHandle<()>>,
    pub(crate) integer_equality_saturation: bool,
    supports_osr: bool,
}

/// The Cranelift backend compiles lifted, optimized SSA to native code.
#[cfg(feature = "cranelift")]
pub type CraneliftJitBackend = BackgroundJit<cranelift_codegen::isa::OwnedTargetIsa>;

/// Direct selects call-free scalar leaves; retained DEBUG 3 code uses the shared Cranelift native compiler.
#[cfg(feature = "direct")]
pub type DirectJitBackend = BackgroundJit<zkcl_vop::Target>;

pub(crate) fn worker_count() -> usize {
    std::thread::available_parallelism().map_or(1, |cpus| cpus.get().min(MAX_COMPILER_THREADS))
}

impl<T> BackgroundJit<T> {
    pub(crate) fn start_workers<F>(
        target: T,
        workers: usize,
        integer_equality_saturation: bool,
        supports_osr: bool,
        create_compiler: fn(T) -> F,
    ) -> Result<Self, JitError>
    where
        T: Clone + Send + 'static,
        F: FnMut(
                JitCompilationRequest,
                Option<usize>,
                bool,
                &CodeCache,
                &dyn Fn() -> bool,
            ) -> Option<CompiledRevision>
            + 'static,
    {
        let mut backend = Self {
            target,
            cache: Arc::new(CodeCache::new()),
            queue: Arc::new(CompilationQueue::default()),
            workers: Vec::with_capacity(workers),
            integer_equality_saturation,
            supports_osr,
        };
        for i in 0..workers {
            let target = backend.target.clone();
            let cache = Arc::clone(&backend.cache);
            let queue = Arc::clone(&backend.queue);
            let worker = std::thread::Builder::new()
                .name(format!("zkcl-jit-compiler-{i}"))
                .spawn(move || {
                    compile_queued(
                        create_compiler(target),
                        &cache,
                        &queue,
                        integer_equality_saturation,
                    )
                })
                .map_err(JitError::SpawnWorker)?;
            backend.workers.push(worker);
        }
        Ok(backend)
    }

    /// Access the underlying executable [`CodeCache`].
    pub fn code_cache(&self) -> &CodeCache {
        &self.cache
    }

    /// Stop-point activity of the shared queue, counted only with the `counters` feature.
    #[cfg(feature = "counters")]
    pub fn queue_stop_point_counters(&self) -> QueueStopPointCounters {
        self.queue.stop_points.snapshot()
    }
}

// SAFETY: The built-in compilers lower the submitted snapshots and publish their
// original revisions. Each completion owns its executable mapping independently.
unsafe impl<T: Send + Sync> JitBackend for BackgroundJit<T> {
    fn machine_code(
        &self,
        code_id: usize,
        revision: u64,
    ) -> Option<zkcl_runtime::native::MachineCode> {
        let code = self.cache.get_revision(code_id, revision)?.executable_code;
        Some(zkcl_runtime::native::MachineCode {
            address: code.as_ptr() as usize as u64,
            bytes: code.bytes().to_vec(),
        })
    }

    unsafe fn submit(&self, request: JitCompilationRequest) {
        // SAFETY: The caller guarantees that this request satisfies the submission contract.
        let _ = unsafe { self.try_submit(request) };
    }

    fn can_submit_automatic(&self) -> bool {
        self.queue
            .state
            .try_lock()
            .is_ok_and(|state| state.admits_automatic())
    }

    unsafe fn try_submit(&self, request: JitCompilationRequest) -> bool {
        let Some(state) = self.queue.state.try_lock().ok() else {
            return false;
        };
        self.queue
            .enqueue(state, request, CompilationKind::Automatic)
    }

    fn supports_osr(&self) -> bool {
        self.supports_osr
    }

    unsafe fn try_submit_osr(&self, request: JitOsrRequest) -> bool {
        if !self.supports_osr {
            return false;
        }
        let Some(state) = self.queue.state.try_lock().ok() else {
            return false;
        };
        self.queue.enqueue(
            state,
            QueuedCompilation {
                request: request.compilation,
                osr_pc: Some(request.canonical_pc),
            },
            CompilationKind::Automatic,
        )
    }

    unsafe fn submit_explicit(&self, request: JitCompilationRequest) -> bool {
        self.queue.enqueue(
            self.queue.state.lock().unwrap(),
            request,
            CompilationKind::Explicit,
        )
    }

    fn prioritize_explicit(&self, code_id: usize, revision: u64) {
        let mut state = self.queue.state.lock().unwrap();
        if let Some(index) = state.queued_automatic.iter().position(|queued| {
            queued.osr_pc.is_none()
                && queued.request.code_id == code_id
                && queued.request.revision == revision
        }) {
            let request = state.queued_automatic.remove(index).unwrap();
            // The submitter waits on this revision now, so it precedes other queued
            // explicit work instead of joining the back of that queue.
            state.queued_explicit.push_front(request);
        }
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        self.poll_for(&mut |_, _| true).unwrap_or_default().0
    }

    fn has_completions(&self) -> bool {
        self.queue.has_completions.load(Ordering::Acquire)
    }

    fn poll_for(
        &self,
        belongs_to_runtime: &mut dyn FnMut(usize, u64) -> bool,
    ) -> Option<JitCompletions> {
        if !self.has_completions() {
            return Some(JitCompletions::default());
        }
        self.queue.install_completed(belongs_to_runtime)
    }

    fn release(&self, code_id: usize, revision: u64) {
        if let Some(code) = self.queue.release_revision(code_id, revision) {
            self.cache.remove_runtime(&code);
        }
    }
}

impl<T> Drop for BackgroundJit<T> {
    fn drop(&mut self) {
        self.queue.shutdown();
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn compile_queued(
    mut compile: impl FnMut(
        JitCompilationRequest,
        Option<usize>,
        bool,
        &CodeCache,
        &dyn Fn() -> bool,
    ) -> Option<CompiledRevision>,
    cache: &CodeCache,
    queue: &CompilationQueue,
    integer_equality_saturation: bool,
) {
    while let Some((QueuedCompilation { request, osr_pc }, kind, generation)) =
        queue.wait_for_request()
    {
        let revision = request.revision;
        let code_id = request.code_id;
        let observed_generation = Cell::new(generation);
        let is_pending = || queue.is_still_pending(code_id, revision, &observed_generation);
        let saturation = integer_equality_saturation && kind == CompilationKind::Explicit;
        let compiled = compile(request, osr_pc, saturation, cache, &is_pending);
        queue.publish(
            cache,
            revision,
            CompilationOutcome {
                code_id,
                is_osr: osr_pc.is_some(),
                compiled,
            },
        );
    }
}

pub(crate) fn jit_function_from_mapping(
    code: &MappedJitFunction,
    root_slots: usize,
    native_debug: bool,
) -> JitFunction {
    JitFunction {
        native_debug,
        code_id: code.code_id,
        entry: NativeEntry::from_jit_c(root_slots, code.revision),
        c_entry: Some(code.c_entry),
        resumable: true,
        leaf_entry: None,
        leaf_guards: Box::default(),
        executable_owner: Some(code.executable_code.clone()),
    }
}

#[cfg(all(test, feature = "cranelift"))]
mod tests {
    use super::*;
    use zkcl_bytecode::{Arity, Immediate, Instruction};

    fn request(revision: u64) -> JitCompilationRequest {
        JitCompilationRequest {
            native_debug: false,
            code_id: 0,
            revision,
            arity: Arity::ZERO,
            registers: 1,
            canonical: [
                Instruction::Load(0, Immediate::Fixnum(revision as i64)),
                Instruction::SetValues(0..1),
            ]
            .into(),
            captures: Box::default(),
            notinline: Box::default(),
            vop_calls: Box::default(),
            handlers: Box::default(),
            restarts: Box::default(),
            keywords: Box::default(),
            jump_tables: Box::default(),
            param_facts: Box::default(),
            site_feedback: Vec::new(),
            cl_const_names: Box::default(),
        }
    }

    #[test]
    fn cancellation_checkpoints_follow_the_claimed_revision() {
        let queue = CompilationQueue::default();
        let cache = CodeCache::new();
        let (observed, observations) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        assert!(queue.enqueue(
            queue.state.lock().unwrap(),
            request(1),
            CompilationKind::Explicit
        ));

        std::thread::scope(|scope| {
            let compiler_queue = &queue;
            let compiler_cache = &cache;
            let worker = scope.spawn(move || {
                compile_queued(
                    |request, _, _, _, is_pending| {
                        observed.send((request.revision, is_pending())).unwrap();
                        resumed
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        observed.send((request.revision, is_pending())).unwrap();
                        None
                    },
                    compiler_cache,
                    compiler_queue,
                    false,
                );
            });

            assert_eq!(observations.recv().unwrap(), (1, true));
            // Canceling another revision or a mismatched code ID leaves the active job valid.
            assert!(queue.enqueue(
                queue.state.lock().unwrap(),
                request(2),
                CompilationKind::Explicit
            ));
            queue.release_revision(0, 2);
            queue.release_revision(1, 1);
            resume.send(()).unwrap();
            assert_eq!(observations.recv().unwrap(), (1, true));

            assert!(queue.enqueue(
                queue.state.lock().unwrap(),
                request(3),
                CompilationKind::Explicit
            ));
            assert_eq!(observations.recv().unwrap(), (3, true));
            queue.release_revision(0, 3);
            assert!(queue.enqueue(
                queue.state.lock().unwrap(),
                request(4),
                CompilationKind::Explicit
            ));
            resume.send(()).unwrap();
            assert_eq!(observations.recv().unwrap(), (3, false));
            assert_eq!(observations.recv().unwrap(), (4, true));
            resume.send(()).unwrap();
            assert_eq!(observations.recv().unwrap(), (4, true));

            assert!(queue.enqueue(
                queue.state.lock().unwrap(),
                request(5),
                CompilationKind::Explicit
            ));
            assert_eq!(observations.recv().unwrap(), (5, true));
            queue.shutdown();
            resume.send(()).unwrap();
            assert_eq!(observations.recv().unwrap(), (5, false));
            worker.join().unwrap();
        });
        let state = queue.state.lock().unwrap();
        assert!(!state.completed_revisions.contains_key(&3));
        assert!(state.completed_revisions.contains_key(&4));
        assert!(state.compiling_revisions.is_empty());
    }

    #[test]
    fn releasing_a_queued_revision_leaves_claimed_work_untouched() {
        let queue = CompilationQueue::default();
        for revision in [1, 2] {
            assert!(queue.enqueue(
                queue.state.lock().unwrap(),
                request(revision),
                CompilationKind::Explicit
            ));
        }
        let (claimed, kind, claimed_generation) = queue.wait_for_request().unwrap();
        assert_eq!(
            (claimed.request.revision, kind),
            (1, CompilationKind::Explicit)
        );

        // Revision 2 is still queued, so no worker watches the epoch on its behalf.
        assert!(queue.release_revision(0, 2).is_none());
        assert_eq!(
            queue.cancellation_generation.load(Ordering::Acquire),
            claimed_generation
        );

        // Releasing the claimed revision must invalidate that worker's fast path.
        assert!(queue.release_revision(0, 1).is_none());
        assert_ne!(
            queue.cancellation_generation.load(Ordering::Acquire),
            claimed_generation
        );
    }

    #[cfg(feature = "counters")]
    #[test]
    fn stop_point_counters_separate_fast_paths_from_rechecks() {
        let queue = CompilationQueue::default();
        assert!(queue.enqueue(
            queue.state.lock().unwrap(),
            request(1),
            CompilationKind::Explicit
        ));
        let (claimed, _, generation) = queue.wait_for_request().unwrap();
        let observed = Cell::new(generation);

        assert!(queue.is_still_pending(
            claimed.request.code_id,
            claimed.request.revision,
            &observed
        ));
        queue.release_revision(claimed.request.code_id, claimed.request.revision);
        assert!(!queue.is_still_pending(
            claimed.request.code_id,
            claimed.request.revision,
            &observed
        ));
        assert_eq!(
            queue.stop_points.snapshot(),
            QueueStopPointCounters {
                fast_path: 1,
                rechecks: 1,
            }
        );
    }

    fn submit_standalone_automatic(backend: &CraneliftJitBackend, revision: u64) -> bool {
        // SAFETY: The backend serves only synthetic requests in these queue tests.
        unsafe { backend.try_submit(request(revision)) }
    }

    fn submit_standalone_explicit(backend: &CraneliftJitBackend, revision: u64) -> bool {
        // SAFETY: The backend serves only synthetic requests in these queue tests.
        unsafe { backend.submit_explicit(request(revision)) }
    }

    #[test]
    fn canceled_requests_leave_queue_capacity_available() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        for revision in 1..=(2 * MAX_PENDING_REQUESTS) as u64 {
            assert!(submit_standalone_automatic(&backend, revision));
            backend.release(0, revision);
            assert!(submit_standalone_explicit(&backend, revision));
            backend.release(0, revision);
        }
        let state = backend.queue.state.lock().unwrap();
        assert!(state.queued_automatic.is_empty());
        assert!(state.queued_explicit.is_empty());
        drop(state);

        for revision in 1..=AUTOMATIC_ADMISSION_LIMIT as u64 {
            assert!(submit_standalone_automatic(&backend, revision));
        }
        let explicit_revision = MAX_PENDING_REQUESTS as u64;
        assert!(!backend.can_submit_automatic());
        assert!(!submit_standalone_automatic(&backend, explicit_revision));
        assert!(submit_standalone_explicit(&backend, explicit_revision));
        assert!(!submit_standalone_explicit(&backend, explicit_revision + 1));
        let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
        assert_eq!(queued.request.revision, explicit_revision);
        assert_eq!(kind, CompilationKind::Explicit);
        backend.release(0, explicit_revision);
        assert!(
            !submit_standalone_explicit(&backend, explicit_revision),
            "revision is still compiling"
        );
        backend.queue.publish(
            backend.code_cache(),
            explicit_revision,
            CompilationOutcome {
                code_id: 0,
                is_osr: false,
                compiled: None,
            },
        );
        assert!(!backend.has_completions());
        assert!(submit_standalone_explicit(&backend, explicit_revision + 1));
        backend.release(0, explicit_revision + 1);
        backend.release(0, 1);
        assert!(backend.can_submit_automatic());
        assert!(submit_standalone_automatic(&backend, explicit_revision + 2));
    }

    #[test]
    fn automatic_admission_still_checks_capacity_after_preflight() {
        let backend = CraneliftJitBackend::start(0, false).unwrap();
        for revision in 1..AUTOMATIC_ADMISSION_LIMIT as u64 {
            assert!(submit_standalone_automatic(&backend, revision));
        }
        let last_automatic = AUTOMATIC_ADMISSION_LIMIT as u64;
        let retry_revision = last_automatic + 1;
        let (checked, check) = std::sync::mpsc::channel();
        let (resume, resumed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let backend = &backend;
            let submit = scope.spawn(move || {
                checked.send(backend.can_submit_automatic()).unwrap();
                resumed.recv().unwrap();
                submit_standalone_automatic(backend, retry_revision)
            });
            let was_ready = check.recv().unwrap();
            let took_slot = submit_standalone_automatic(backend, last_automatic);
            resume.send(()).unwrap();
            assert!(was_ready && took_slot);
            assert!(!submit.join().unwrap());
        });
        assert!(submit_standalone_explicit(&backend, retry_revision + 1));
        backend.release(0, retry_revision + 1);
        backend.release(0, 1);
        assert!(submit_standalone_automatic(&backend, retry_revision));
    }

    #[test]
    fn explicit_requests_overtake_automatic_work_with_bounded_preference() {
        let backend = CraneliftJitBackend::start(0, false).unwrap();
        for revision in [1, 2] {
            assert!(submit_standalone_automatic(&backend, revision));
        }
        for revision in 3..=9 {
            assert!(submit_standalone_explicit(&backend, revision));
        }
        for expected in [3, 4, 5, 1, 6, 7, 8, 2, 9] {
            let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
            assert_eq!(queued.request.revision, expected);
            assert_eq!(
                kind,
                if expected <= 2 {
                    CompilationKind::Automatic
                } else {
                    CompilationKind::Explicit
                }
            );
        }
    }

    #[test]
    fn lisp_compile_prioritizes_the_existing_automatic_revision() {
        let runtime = zkcl_runtime::Runtime::builder()
            .jit_threshold(1)
            .start()
            .unwrap();
        runtime.load_str("(defun prioritize-probe () 27)").unwrap();
        let function = runtime.eval("#'prioritize-probe").unwrap().pop().unwrap();
        let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
        let backend = Arc::new(CraneliftJitBackend::start(0, false).unwrap());
        runtime.set_jit_backend(backend.clone());

        let (automatic, explicit) = (u64::MAX, u64::MAX - 1);
        // SAFETY: This backend has no workers. The test dequeues these probes
        // directly without producing any executable completions.
        unsafe { assert!(backend.try_submit(request(automatic))) };
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(27)
        );
        // Automatic admission is open while no submitter waits, so the call queues
        // its own request behind the probe.
        let (queued_count, queued) = {
            let state = backend.queue.state.lock().unwrap();
            let queued = state
                .queued_automatic
                .back()
                .map(|queued| (queued.request.code_id, queued.request.revision));
            (state.queued_automatic.len(), queued)
        };
        assert_eq!(queued_count, 2, "the call queues its automatic request");
        let (code_id, revision) = queued.expect("the call queued its automatic request");

        backend.prioritize_explicit(code_id.wrapping_add(1), revision);
        assert_eq!(
            backend.queue.state.lock().unwrap().queued_automatic.len(),
            2,
            "a mismatched code id leaves the request queued"
        );
        // A later explicit request joins the queue before `COMPILE` promotes the
        // automatic revision, which is the ordering this test pins.
        // SAFETY: Same contract as above: no workers, queue inspection only.
        unsafe { assert!(backend.submit_explicit(request(explicit))) };
        let result = runtime
            .call(&compile, &[&runtime.nil(), &function])
            .unwrap();
        assert!(result[1].is_nil() && result[2].is_nil());
        assert!(!runtime.submit_jit_now(code_id));
        {
            let state = backend.queue.state.lock().unwrap();
            assert!(
                !state
                    .queued_automatic
                    .iter()
                    .any(|queued| queued.request.revision == revision),
                "COMPILE moves the request out of the automatic queue"
            );
            assert_eq!(
                state
                    .queued_explicit
                    .front()
                    .map(|queued| queued.request.revision),
                Some(revision),
                "the promoted request precedes other explicit work"
            );
            assert_eq!(
                state.pending_code_ids.get(&revision),
                Some(&code_id),
                "promotion keeps the lease on the revision"
            );
        }

        for (expected_revision, expected_kind) in [
            (revision, CompilationKind::Explicit),
            (explicit, CompilationKind::Explicit),
            (automatic, CompilationKind::Automatic),
        ] {
            let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
            assert_eq!(
                (queued.request.revision, kind),
                (expected_revision, expected_kind)
            );
        }
    }

    #[test]
    fn prioritizing_a_claimed_revision_preserves_its_completion() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        assert!(submit_standalone_automatic(&backend, 1));
        let (active, kind, _) = backend.queue.wait_for_request().unwrap();
        assert_eq!(
            (active.request.revision, kind),
            (1, CompilationKind::Automatic)
        );
        backend.prioritize_explicit(0, 1);
        assert!(
            backend
                .queue
                .state
                .lock()
                .unwrap()
                .queued_explicit
                .is_empty()
        );

        backend.queue.publish(
            backend.code_cache(),
            1,
            CompilationOutcome {
                code_id: 0,
                is_osr: false,
                compiled: None,
            },
        );
        backend.prioritize_explicit(0, 1);
        assert!(
            backend
                .queue
                .state
                .lock()
                .unwrap()
                .queued_explicit
                .is_empty()
        );
        let (installed, failures) = backend.poll_for(&mut |_, _| true).unwrap();
        assert!(installed.is_empty());
        assert_eq!(failures, [(0, 1)]);
        assert!(!backend.has_completions());
    }

    #[test]
    fn explicit_requests_wait_for_queue_access() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        let queue_access = backend.queue.state.lock().unwrap();
        let (ready, attempted) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let backend = &backend;
            let submit = scope.spawn(move || {
                assert!(!backend.can_submit_automatic());
                assert!(!submit_standalone_automatic(backend, 1));
                ready.send(()).unwrap();
                submit_standalone_explicit(backend, 1)
            });
            let nonblocking = attempted.recv_timeout(std::time::Duration::from_secs(5));
            drop(queue_access);
            nonblocking.unwrap();
            assert!(submit.join().unwrap());
        });
        let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
        assert_eq!(queued.request.revision, 1);
        assert_eq!(kind, CompilationKind::Explicit);
    }

    #[test]
    fn polling_preserves_completions_for_other_runtimes() {
        use std::time::{Duration, Instant};

        let backend = CraneliftJitBackend::start(1, true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        for revision in [1, 2] {
            while !submit_standalone_automatic(&backend, revision) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        let wait = |revision| {
            loop {
                if let Some((mut native, failures)) =
                    backend.poll_for(&mut |code_id, current| code_id == 0 && current == revision)
                {
                    assert!(failures.is_empty());
                    if let Some(function) = native.pop() {
                        assert!(native.is_empty());
                        break function;
                    }
                }
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        };
        // The worker compiles revision 1 first. Requesting revision 2 must preserve it.
        assert_eq!(wait(2).entry.revision, 2);
        assert_eq!(wait(1).entry.revision, 1);
        backend.release(0, 2);
        assert_eq!(backend.code_cache().get(0).unwrap().revision, 1);
        backend.release(0, 1);
        assert!(backend.code_cache().is_empty());
    }

    #[test]
    fn loop_entries_use_revision_leases_without_replacing_the_function_entry() {
        use std::time::{Duration, Instant};

        let backend = CraneliftJitBackend::start(1, false).unwrap();
        assert!(backend.supports_osr());
        let deadline = Instant::now() + Duration::from_secs(5);
        let ordinary = backend.compile_now(&request(1)).unwrap();
        let submit = |pc, revision| {
            let mut compilation = request(revision);
            compilation.arity = Arity {
                required: 1,
                maximum: Some(1),
            };
            compilation.registers = 3;
            compilation.canonical = [
                Instruction::Load(2, Immediate::Fixnum(41)),
                Instruction::Unless(0, 4),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Jump(1),
                Instruction::SetValues(2..3),
            ]
            .into();
            let request = JitOsrRequest {
                canonical_pc: pc,
                compilation,
            };
            // SAFETY: Only this test receives and inspects these standalone completions.
            while !unsafe { backend.try_submit_osr(request.clone()) } {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        };
        let completion = |revision| loop {
            if let Some((successes, failures)) =
                backend.poll_for(&mut |id, rev| id == 0 && rev == revision)
                && !(successes.is_empty() && failures.is_empty())
            {
                break (successes, failures);
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };

        submit(4, 2);
        let (successes, failures) = completion(2);
        assert!(successes.is_empty());
        assert_eq!(failures, [(0, 2)]);

        submit(1, 3);
        let (successes, failures) = completion(3);
        assert!(failures.is_empty());
        assert_eq!(successes.len(), 1);
        assert!(successes[0].resumable);
        assert!(successes[0].leaf_entry.is_none());
        assert_eq!(
            backend.code_cache().get(0).unwrap().revision,
            ordinary.entry.revision
        );
        assert!(backend.machine_code(0, 3).is_some());
        backend.release(0, 3);
        assert!(backend.machine_code(0, 3).is_none());
        assert!(successes[0].executable_owner.is_some());
    }

    #[test]
    fn automatic_admission_pauses_while_explicit_work_waits() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        assert!(submit_standalone_explicit(&backend, 1));
        assert!(!backend.can_submit_automatic());
        assert!(!submit_standalone_automatic(&backend, 2));

        let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
        assert_eq!(
            (queued.request.revision, kind),
            (1, CompilationKind::Explicit)
        );
        assert!(backend.can_submit_automatic());
        assert!(submit_standalone_automatic(&backend, 2));
    }

    #[test]
    fn prioritizing_a_queued_request_precedes_other_explicit_work() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        assert!(submit_standalone_automatic(&backend, 1));
        assert!(submit_standalone_explicit(&backend, 2));
        backend.prioritize_explicit(0, 1);
        for (expected_revision, expected_kind) in [
            (1, CompilationKind::Explicit),
            (2, CompilationKind::Explicit),
        ] {
            let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
            assert_eq!(
                (queued.request.revision, kind),
                (expected_revision, expected_kind)
            );
        }
    }
}
