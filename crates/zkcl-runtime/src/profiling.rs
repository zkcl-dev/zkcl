//! Function instrumentation and cooperative safe-point sampling.
//!
//! Times are scheduled wall time, including GC and synchronous host calls, but
//! excluding periods when another cooperative task runs or the scheduler waits. Samples
//! are taken only when execution reaches a Lisp VM/native safe point: blocking
//! calls and native code between safe points cannot be sampled asynchronously.
//! Allocation counts are successful, aligned managed-object allocation bytes,
//! including headers, and exclude evacuation copies and Rust/foreign payloads.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use crate::code_store::VmCode;
use crate::condition::Fault;
use crate::heap::Word;
use crate::package::Package;
use crate::platform::Timer;
use crate::{EvalError, HeapStats, Root, Runtime};

mod image;

/// Options for one profiling session.
#[derive(Clone, Copy, Debug)]
pub struct ProfilerConfig {
    /// Minimum wall time between safe-point samples; must be nonzero.
    pub sample_interval: Duration,
}

impl Default for ProfilerConfig {
    fn default() -> Self {
        Self {
            sample_interval: Duration::from_millis(1),
        }
    }
}

/// Aggregate for one compiled definition. Closures sharing code share a row;
/// redefining a function produces another ID, even when its name is unchanged.
#[derive(Clone, Debug, Default)]
pub struct FunctionProfile {
    pub id: u64,
    pub name: String,
    pub calls: u64,
    /// Activations that entered native code; included in [`Self::calls`].
    /// A later deoptimization keeps the activation's original entry kind.
    pub native_calls: u64,
    /// Sum of activation scheduled wall times. Recursive activations overlap deliberately.
    pub total_time: Duration,
    /// Time charged while this definition was the innermost Lisp activation.
    pub self_time: Duration,
    pub samples: u64,
    /// Managed bytes allocated during activations, including callees.
    pub allocated_bytes: u64,
    /// Managed bytes allocated while this was the innermost Lisp activation.
    pub self_allocated_bytes: u64,
}

/// Owned snapshot; remains valid after further evaluation or runtime destruction.
#[derive(Clone, Debug, Default)]
pub struct ProfileReport {
    pub running: bool,
    pub elapsed: Duration,
    pub sampling_mode: &'static str,
    pub samples: u64,
    pub allocated_bytes: u64,
    pub collections: u64,
    pub gc_time: Duration,
    /// Descending self time, then definition ID.
    pub functions: Vec<FunctionProfile>,
}

const SAMPLING_MODE: &str = "safe-point wall-clock";

pub(crate) struct Profiler {
    enabled: Cell<bool>,
    session: RefCell<Option<Rc<RefCell<Session>>>>,
    current_task: Cell<Option<TaskKey>>,
}

type TaskKey = (usize, u64);

impl Default for Profiler {
    fn default() -> Self {
        Self {
            enabled: Cell::new(false),
            session: RefCell::new(None),
            current_task: Cell::new(Some((0, 0))),
        }
    }
}

struct Active {
    token: u64,
    id: u64,
    started: Duration,
    allocated: u64,
    task: TaskKey,
}

#[derive(Default)]
struct TaskTotals {
    elapsed: Duration,
    allocated: u64,
}

struct Session {
    timer: Timer,
    running: bool,
    interval: Duration,
    last_event: Duration,
    last_sample: Duration,
    last_allocated: u64,
    allocation_counter: Rc<Cell<u64>>,
    initial_allocated: u64,
    initial_collections: u64,
    initial_gc_time: Duration,
    next_token: u64,
    samples: u64,
    functions: BTreeMap<u64, FunctionProfile>,
    active: Vec<Active>,
    current_task: Option<TaskKey>,
    tasks: BTreeMap<TaskKey, TaskTotals>,
    stopped: Option<ProfileReport>,
}

impl Session {
    fn now(&self) -> Duration {
        self.timer.elapsed().unwrap_or_default()
    }

    fn account_top(&mut self, now: Duration) {
        let allocated = self.allocation_counter.get();
        let elapsed = now.saturating_sub(self.last_event);
        let bytes = allocated.saturating_sub(self.last_allocated);
        if let Some(task) = self.current_task {
            let totals = self.tasks.entry(task).or_default();
            totals.elapsed = totals.elapsed.saturating_add(elapsed);
            totals.allocated = totals.allocated.saturating_add(bytes);
        }
        if let Some(top) = self
            .active
            .iter()
            .rev()
            .find(|active| Some(active.task) == self.current_task)
        {
            let row = self.functions.get_mut(&top.id).expect("active profile row");
            row.self_time = row.self_time.saturating_add(elapsed);
            row.self_allocated_bytes = row.self_allocated_bytes.saturating_add(bytes);
        }
        self.last_event = now;
        self.last_allocated = allocated;
    }

    fn sample(&mut self, now: Duration) {
        if now.saturating_sub(self.last_sample) < self.interval {
            return;
        }
        // Do not invent catch-up observations for elapsed intervals. A delayed
        // safe point provides exactly one observation of its current activation.
        self.last_sample = now;
        if let Some(top) = self
            .active
            .iter()
            .rev()
            .find(|active| Some(active.task) == self.current_task)
        {
            self.samples = self.samples.saturating_add(1);
            let row = self.functions.get_mut(&top.id).expect("active profile row");
            row.samples = row.samples.saturating_add(1);
        }
    }

    fn snapshot(&mut self, stats: HeapStats) -> ProfileReport {
        if let Some(stopped) = &self.stopped {
            return stopped.clone();
        }
        let now = self.now();
        self.account_top(now);
        let allocated = self.allocation_counter.get();
        let mut functions = self.functions.clone();
        for active in &self.active {
            let totals = self.tasks.get(&active.task).expect("active task totals");
            let row = functions.get_mut(&active.id).expect("active profile row");
            row.total_time = row
                .total_time
                .saturating_add(totals.elapsed.saturating_sub(active.started));
            row.allocated_bytes = row
                .allocated_bytes
                .saturating_add(totals.allocated.saturating_sub(active.allocated));
        }
        let mut functions = functions.into_values().collect::<Vec<_>>();
        functions.sort_by(|left, right| {
            right
                .self_time
                .cmp(&left.self_time)
                .then(left.id.cmp(&right.id))
        });
        ProfileReport {
            running: self.running,
            elapsed: now,
            sampling_mode: SAMPLING_MODE,
            samples: self.samples,
            allocated_bytes: allocated.saturating_sub(self.initial_allocated),
            collections: stats.collections.saturating_sub(self.initial_collections),
            gc_time: stats
                .total_collection_duration
                .unwrap_or_default()
                .saturating_sub(self.initial_gc_time),
            functions,
        }
    }
}

/// Held by an activation, so all normal and non-local frame removals close it.
/// Owns no Lisp words or code and therefore does not retain replaced definitions.
pub(crate) struct ProfileActivation {
    session: Rc<RefCell<Session>>,
    token: u64,
}

impl Drop for ProfileActivation {
    fn drop(&mut self) {
        let mut session = self.session.borrow_mut();
        if !session.running {
            return;
        }
        let now = session.now();
        session.account_top(now);
        let Some(index) = session
            .active
            .iter()
            .rposition(|active| active.token == self.token)
        else {
            return;
        };
        // Vec::truncate can drop older frames before younger ones. Tokens permit
        // either order, including a tail callee entered before its caller drops.
        let active = session.active.remove(index);
        let totals = session.tasks.get(&active.task).expect("active task totals");
        let elapsed = totals.elapsed.saturating_sub(active.started);
        let allocated = totals.allocated.saturating_sub(active.allocated);
        let row = session
            .functions
            .get_mut(&active.id)
            .expect("active profile row");
        row.total_time = row.total_time.saturating_add(elapsed);
        row.allocated_bytes = row.allocated_bytes.saturating_add(allocated);
    }
}

impl Runtime {
    /// Start a fresh session, replacing any previous report. Already active
    /// activations are outside the new session; subsequent calls are instrumented.
    pub fn start_profiling(&self, config: ProfilerConfig) -> Result<(), EvalError> {
        if config.sample_interval.is_zero() {
            return Err(EvalError::new(
                0,
                "profiler sample interval must be nonzero",
            ));
        }
        let timer = Timer::start();
        if timer.elapsed().is_none() {
            return Err(EvalError::new(
                0,
                "profiling requires a host monotonic clock",
            ));
        }
        self.stop_profiling();
        let counter = self.heap.allocation_counter();
        let allocated = counter.get();
        let stats = self.heap_stats();
        *self.profiler.session.borrow_mut() = Some(Rc::new(RefCell::new(Session {
            timer,
            running: true,
            interval: config.sample_interval,
            last_event: Duration::ZERO,
            last_sample: Duration::ZERO,
            last_allocated: allocated,
            allocation_counter: counter,
            initial_allocated: allocated,
            initial_collections: stats.collections,
            initial_gc_time: stats.total_collection_duration.unwrap_or_default(),
            next_token: 0,
            samples: 0,
            functions: BTreeMap::new(),
            active: Vec::new(),
            current_task: self.profiler.current_task.get(),
            tasks: BTreeMap::new(),
            stopped: None,
        })));
        self.profiler.enabled.set(true);
        // Cached/direct native leaf entries can elide activation records. Clear
        // their call caches; native selection checks profiling on every entry.
        self.heap.code_store.invalidate_native_calls();
        Ok(())
    }

    /// Stop at this instant and return a stable snapshot, including open calls.
    pub fn stop_profiling(&self) -> ProfileReport {
        self.profiler.enabled.set(false);
        let session = self.profiler.session.borrow().clone();
        let Some(session) = session else {
            return empty_report();
        };
        let mut session = session.borrow_mut();
        let mut report = session.snapshot(self.heap_stats());
        report.running = false;
        session.running = false;
        session.stopped = Some(report.clone());
        session.active.clear();
        report
    }

    /// Snapshot without stopping. Open calls contribute through this instant.
    pub fn profile_report(&self) -> ProfileReport {
        let session = self.profiler.session.borrow().clone();
        session
            .map(|session| session.borrow_mut().snapshot(self.heap_stats()))
            .unwrap_or_else(empty_report)
    }

    /// Clear a stopped report, or start a fresh session with the current options.
    /// Activations that predate the reset do not contribute to the new session.
    pub fn reset_profiling(&self) {
        let interval = self.profiler.session.borrow().as_ref().and_then(|session| {
            let session = session.borrow();
            session.running.then_some(session.interval)
        });
        if let Some(sample_interval) = interval {
            // These options and host clock were validated by start_profiling.
            self.start_profiling(ProfilerConfig { sample_interval })
                .expect("validated profiler options");
        } else {
            *self.profiler.session.borrow_mut() = None;
        }
    }

    #[inline]
    pub(crate) fn profiling_enabled(&self) -> bool {
        self.profiler.enabled.get()
    }

    #[inline]
    pub(crate) fn profile_activation(&self, code: &VmCode) -> Option<ProfileActivation> {
        self.profile_enter(code, false)
    }

    #[inline]
    pub(crate) fn profile_native_activation(&self, code: &VmCode) -> Option<ProfileActivation> {
        self.profile_enter(code, true)
    }

    #[inline]
    fn profile_enter(&self, code: &VmCode, native: bool) -> Option<ProfileActivation> {
        if !self.profiling_enabled() {
            return None;
        }
        let session = self
            .profiler
            .session
            .borrow()
            .as_ref()
            .expect("enabled profiler")
            .clone();
        let token = {
            let mut state = session.borrow_mut();
            let now = state.now();
            state.account_top(now);
            state.sample(now);
            let id = code.id as u64;
            let row = state
                .functions
                .entry(id)
                .or_insert_with(|| FunctionProfile {
                    id,
                    name: code.name.clone().unwrap_or_else(|| "<anonymous>".into()),
                    ..Default::default()
                });
            row.calls = row.calls.saturating_add(1);
            row.native_calls = row.native_calls.saturating_add(u64::from(native));
            let token = state.next_token;
            state.next_token = state
                .next_token
                .checked_add(1)
                .expect("profile activation token overflow");
            // Synchronous nested evaluations keep their caller's task identity.
            let task = state.current_task.unwrap_or((0, 0));
            let totals = state.tasks.entry(task).or_default();
            let started = totals.elapsed;
            let allocated = totals.allocated;
            state.active.push(Active {
                token,
                id,
                started,
                allocated,
                task,
            });
            token
        };
        Some(ProfileActivation { session, token })
    }

    #[inline]
    pub(crate) fn profile_sample(&self) {
        if !self.profiling_enabled() {
            return;
        }
        let session = self
            .profiler
            .session
            .borrow()
            .as_ref()
            .expect("enabled profiler")
            .clone();
        let mut session = session.borrow_mut();
        let now = session.now();
        session.sample(now);
    }

    pub(crate) fn profile_current_task(&self) -> Option<TaskKey> {
        self.profiler.current_task.get()
    }

    /// Scheduler-only scope. None pauses attribution while no Lisp task runs.
    /// Updating the identity also while disabled permits starting a profiler
    /// from inside a previously uninstrumented child task.
    pub(crate) fn profile_task_scope(&self, task: Option<TaskKey>) -> ProfileTaskScope<'_> {
        let previous = self.profiler.current_task.get();
        self.profile_switch_task(task);
        ProfileTaskScope {
            runtime: self,
            previous,
        }
    }

    pub(crate) fn profile_scheduler_scope(&self) -> ProfileTaskScope<'_> {
        // Returning an incremental evaluation to its host must leave its live
        // frames paused. A nested synchronous scheduler instead restores the
        // still-running Lisp caller when it returns.
        let previous = self
            .active_scheduler
            .get()
            .and(self.profiler.current_task.get());
        self.profile_switch_task(None);
        ProfileTaskScope {
            runtime: self,
            previous,
        }
    }

    fn profile_switch_task(&self, task: Option<TaskKey>) {
        self.profiler.current_task.set(task);
        if !self.profiling_enabled() {
            return;
        }
        let session = self
            .profiler
            .session
            .borrow()
            .as_ref()
            .expect("enabled profiler")
            .clone();
        let mut session = session.borrow_mut();
        let now = session.now();
        session.account_top(now);
        session.current_task = task;
    }
}

pub(crate) struct ProfileTaskScope<'rt> {
    runtime: &'rt Runtime,
    previous: Option<TaskKey>,
}

impl Drop for ProfileTaskScope<'_> {
    fn drop(&mut self) {
        self.runtime.profile_switch_task(self.previous);
    }
}

fn empty_report() -> ProfileReport {
    ProfileReport {
        sampling_mode: SAMPLING_MODE,
        ..Default::default()
    }
}

/// Install Lisp extension entry points after the ZK package is initialized.
pub(crate) fn install(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, callback) in [
        ("START-PROFILING", start_callback as ProfileCallback),
        ("STOP-PROFILING", stop_callback as ProfileCallback),
        ("PROFILE-REPORT", report_callback as ProfileCallback),
        ("RESET-PROFILING", reset_callback as ProfileCallback),
    ] {
        let symbol = runtime.intern_package(Package::ZK, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .export(Package::ZK, name, symbol)
            .map_err(|message| EvalError::new(0, message.to_string()))?;
        crate::vm::install_host_function(
            runtime,
            &format!("ZK:{name}"),
            std::sync::Arc::new(callback),
        )?;
    }
    Ok(())
}

type ProfileCallback =
    for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, Fault>;

fn start_callback<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    if args.len() > 1 {
        return Err(EvalError::new(
            at,
            "START-PROFILING accepts an optional interval in microseconds",
        )
        .into());
    }
    let config = if let Some(value) = args.first() {
        let micros = value.as_i64().filter(|value| *value > 0).ok_or_else(|| {
            EvalError::new(
                at,
                "profiler sample interval must be a positive integer in microseconds",
            )
        })?;
        ProfilerConfig {
            sample_interval: Duration::from_micros(micros as u64),
        }
    } else {
        ProfilerConfig::default()
    };
    runtime.start_profiling(config)?;
    Ok(vec![runtime.root(Word::T)])
}

fn stop_callback<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    no_arguments(args, at)?;
    report_value(runtime, &runtime.stop_profiling())
        .map(|value| vec![value])
        .map_err(Fault::from)
}

fn report_callback<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    no_arguments(args, at)?;
    report_value(runtime, &runtime.profile_report())
        .map(|value| vec![value])
        .map_err(Fault::from)
}

fn reset_callback<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    no_arguments(args, at)?;
    runtime.reset_profiling();
    Ok(vec![runtime.nil()])
}

fn no_arguments(args: &[Root<'_>], at: usize) -> Result<(), Fault> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(EvalError::new(at, "profiler operation takes no arguments").into())
    }
}

fn report_value<'rt>(
    runtime: &'rt Runtime,
    report: &ProfileReport,
) -> Result<Root<'rt>, EvalError> {
    let rows = report
        .functions
        .iter()
        .map(|row| {
            plist(
                runtime,
                vec![
                    ("ID", number(runtime, row.id)?),
                    ("NAME", runtime.make_string(&row.name).map_err(oom)?),
                    ("CALLS", number(runtime, row.calls)?),
                    ("NATIVE-CALLS", number(runtime, row.native_calls)?),
                    ("TOTAL-NANOSECONDS", nanos(runtime, row.total_time)?),
                    ("SELF-NANOSECONDS", nanos(runtime, row.self_time)?),
                    ("SAMPLES", number(runtime, row.samples)?),
                    ("ALLOCATED-BYTES", number(runtime, row.allocated_bytes)?),
                    (
                        "SELF-ALLOCATED-BYTES",
                        number(runtime, row.self_allocated_bytes)?,
                    ),
                ],
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut functions = runtime.nil();
    for row in rows.into_iter().rev() {
        functions = runtime.cons(&row, &functions).map_err(oom)?;
    }
    plist(
        runtime,
        vec![
            (
                "RUNNING",
                runtime.root(if report.running { Word::T } else { Word::NIL }),
            ),
            ("ELAPSED-NANOSECONDS", nanos(runtime, report.elapsed)?),
            (
                "SAMPLING-MODE",
                runtime.make_string(report.sampling_mode).map_err(oom)?,
            ),
            ("SAMPLES", number(runtime, report.samples)?),
            ("ALLOCATED-BYTES", number(runtime, report.allocated_bytes)?),
            ("COLLECTIONS", number(runtime, report.collections)?),
            ("GC-NANOSECONDS", nanos(runtime, report.gc_time)?),
            ("FUNCTIONS", functions),
        ],
    )
}

fn oom(error: crate::OutOfMemory) -> EvalError {
    EvalError::out_of_memory(0, error)
}
fn number(runtime: &Runtime, value: u64) -> Result<Root<'_>, EvalError> {
    runtime.integer_from(value).map_err(oom)
}
fn nanos(runtime: &Runtime, value: Duration) -> Result<Root<'_>, EvalError> {
    runtime.integer_from(value.as_nanos()).map_err(oom)
}
fn plist<'rt>(
    runtime: &'rt Runtime,
    pairs: Vec<(&str, Root<'rt>)>,
) -> Result<Root<'rt>, EvalError> {
    let mut list = runtime.nil();
    for (key, value) in pairs.into_iter().rev() {
        let key = runtime.intern_package(Package::KEYWORD, key, 0)?.0;
        let key = runtime.root(key);
        list = runtime.cons(&value, &list).map_err(oom)?;
        list = runtime.cons(&key, &list).map_err(oom)?;
    }
    Ok(list)
}
