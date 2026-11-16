//! Background compilation, revision publication, and cancellation.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_runtime::native::{
    JitBackend, JitCompilationRequest, JitCompletions, JitFunction, NativeEntry,
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

#[derive(Default)]
struct PendingQueue {
    queued_automatic: VecDeque<JitCompilationRequest>,
    queued_explicit: VecDeque<JitCompilationRequest>,
    consecutive_explicit_requests: usize,
    compiling_revisions: FxHashSet<u64>,
    pending_code_ids: FxHashMap<u64, usize>,
    completed_revisions: FxHashMap<u64, CompilationOutcome>,
    installed_revisions: FxHashMap<u64, MappedJitFunction>,
    shutting_down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompilationKind {
    Automatic,
    Explicit,
}

struct CompilationOutcome {
    code_id: usize,
    compiled: Option<CompiledRevision>,
}

pub(crate) struct CompiledRevision {
    pub(crate) mapping: MappedJitFunction,
    pub(crate) function: JitFunction,
}

#[derive(Default)]
struct CompilationQueue {
    state: Mutex<PendingQueue>,
    request_available: Condvar,
    has_completions: AtomicBool,
    cancellation_generation: AtomicU64,
}

impl CompilationQueue {
    fn enqueue(
        &self,
        mut state: MutexGuard<'_, PendingQueue>,
        request: JitCompilationRequest,
        kind: CompilationKind,
    ) -> bool {
        let pending_limit = match kind {
            CompilationKind::Automatic => AUTOMATIC_ADMISSION_LIMIT,
            CompilationKind::Explicit => MAX_PENDING_REQUESTS,
        };
        if state.pending_code_ids.len() >= pending_limit
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
            CompilationKind::Automatic => state.queued_automatic.push_back(request),
            CompilationKind::Explicit => state.queued_explicit.push_back(request),
        }
        drop(state);
        self.request_available.notify_one();
        true
    }

    fn wait_for_request(&self) -> Option<(JitCompilationRequest, CompilationKind, u64)> {
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
        let automatic_waiting = !state.queued_automatic.is_empty();
        let (request, kind) = if !state.queued_explicit.is_empty()
            && (!automatic_waiting
                || state.consecutive_explicit_requests < MAX_CONSECUTIVE_EXPLICIT_REQUESTS)
        {
            if automatic_waiting {
                state.consecutive_explicit_requests += 1;
            } else {
                state.consecutive_explicit_requests = 0;
            }
            (state.queued_explicit.pop_front(), CompilationKind::Explicit)
        } else {
            state.consecutive_explicit_requests = 0;
            (
                state.queued_automatic.pop_front(),
                CompilationKind::Automatic,
            )
        };
        let request = request.expect("one queued request");
        state.compiling_revisions.insert(request.revision);
        Some((
            request,
            kind,
            self.cancellation_generation.load(Ordering::Relaxed),
        ))
    }

    fn shutdown(&self) {
        self.state.lock().unwrap().shutting_down = true;
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
            cache.track_runtime(&compiled.mapping);
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
            self.cancellation_generation.fetch_add(1, Ordering::Release);
            if let Some(completed) = state.completed_revisions.remove(&revision) {
                self.has_completions
                    .store(!state.completed_revisions.is_empty(), Ordering::Release);
                return completed.compiled.map(|compiled| compiled.mapping);
            }
            // A worker that already dequeued this revision will discard it in `publish`.
            state
                .queued_explicit
                .retain(|request| request.revision != revision);
            state
                .queued_automatic
                .retain(|request| request.revision != revision);
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
        create_compiler: fn(T) -> F,
    ) -> Result<Self, JitError>
    where
        T: Clone + Send + 'static,
        F: FnMut(
                JitCompilationRequest,
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
            .is_ok_and(|state| state.pending_code_ids.len() < AUTOMATIC_ADMISSION_LIMIT)
    }

    unsafe fn try_submit(&self, request: JitCompilationRequest) -> bool {
        let Some(state) = self.queue.state.try_lock().ok() else {
            return false;
        };
        self.queue
            .enqueue(state, request, CompilationKind::Automatic)
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
        if let Some(index) = state
            .queued_automatic
            .iter()
            .position(|request| request.code_id == code_id && request.revision == revision)
        {
            let request = state.queued_automatic.remove(index).unwrap();
            state.queued_explicit.push_back(request);
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
        bool,
        &CodeCache,
        &dyn Fn() -> bool,
    ) -> Option<CompiledRevision>,
    cache: &CodeCache,
    queue: &CompilationQueue,
    integer_equality_saturation: bool,
) {
    while let Some((request, kind, generation)) = queue.wait_for_request() {
        let revision = request.revision;
        let code_id = request.code_id;
        let generation = Cell::new(generation);
        let is_pending = || {
            if queue.cancellation_generation.load(Ordering::Acquire) == generation.get() {
                return true;
            }
            let state = queue.state.lock().unwrap();
            let pending =
                !state.shutting_down && state.pending_code_ids.get(&revision) == Some(&code_id);
            if pending {
                generation.set(queue.cancellation_generation.load(Ordering::Relaxed));
            }
            pending
        };
        let saturation = integer_equality_saturation && kind == CompilationKind::Explicit;
        let compiled = compile(request, saturation, cache, &is_pending);
        queue.publish(cache, revision, CompilationOutcome { code_id, compiled });
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
                    |request, _, _, is_pending| {
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
        assert_eq!(queued.revision, explicit_revision);
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
            assert_eq!(queued.revision, expected);
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
        unsafe {
            assert!(backend.try_submit(request(automatic)));
            assert!(backend.submit_explicit(request(explicit)));
        }
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(27)
        );
        let (code_id, revision) = {
            let state = backend.queue.state.lock().unwrap();
            assert_eq!(state.queued_automatic.len(), 2);
            let queued = state.queued_automatic.back().unwrap();
            (queued.code_id, queued.revision)
        };

        backend.prioritize_explicit(code_id.wrapping_add(1), revision);
        assert_eq!(
            backend.queue.state.lock().unwrap().queued_automatic.len(),
            2
        );
        let result = runtime
            .call(&compile, &[&runtime.nil(), &function])
            .unwrap();
        assert!(result[1].is_nil() && result[2].is_nil());
        assert!(!runtime.submit_jit_now(code_id));
        assert_eq!(
            backend.queue.state.lock().unwrap().pending_code_ids.len(),
            3
        );

        for (expected_revision, expected_kind) in [
            (explicit, CompilationKind::Explicit),
            (revision, CompilationKind::Explicit),
            (automatic, CompilationKind::Automatic),
        ] {
            let (queued, kind, _) = backend.queue.wait_for_request().unwrap();
            assert_eq!((queued.revision, kind), (expected_revision, expected_kind));
        }
    }

    #[test]
    fn prioritizing_a_claimed_revision_preserves_its_completion() {
        let backend = CraneliftJitBackend::start(0, true).unwrap();
        assert!(submit_standalone_automatic(&backend, 1));
        let (active, kind, _) = backend.queue.wait_for_request().unwrap();
        assert_eq!((active.revision, kind), (1, CompilationKind::Automatic));
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
        assert_eq!(queued.revision, 1);
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
}
