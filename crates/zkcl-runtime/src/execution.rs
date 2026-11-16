use std::mem::DropGuard;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Waker;
use std::time::Duration;

pub(crate) use crate::platform::ExecutionWake;
use crate::platform::{self, SleepSignal};
use crate::vm::task_scheduler::{SchedulerStep, TaskExecutor};
use crate::{DebugEvent, EvalError, ExecutionStop, Root, Runtime};

pub(crate) const HOST_RUNNING: u8 = 0;
pub(crate) const HOST_PAUSE: u8 = 1;
pub(crate) const HOST_CANCEL: u8 = 2;
pub(crate) const HOST_ABORT: u8 = 3;

#[derive(Debug, Default)]
struct CancellationState {
    request: AtomicU8,
    listeners: Mutex<Vec<Weak<ExecutionWake>>>,
    sleep: SleepSignal,
}

/// Requests an execution stop from any thread. Requests persist until reset.
#[derive(Clone, Debug, Default)]
pub struct CancellationHandle(Arc<CancellationState>);

impl CancellationHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Abort at an execution checkpoint. Lisp cleanup forms are skipped.
    pub fn cancel(&self) {
        self.0.request.store(HOST_ABORT, Ordering::Release);
        self.wake_sleepers();
    }

    /// Cancel at an execution checkpoint and run active `UNWIND-PROTECT` cleanups.
    ///
    /// Delivery bypasses Lisp condition handlers. Cleanups can execute until
    /// completion, an abort, or exhaustion of the current execution budget.
    /// Blocking host callbacks control their own responsiveness. An outstanding
    /// abort takes precedence over this request.
    pub fn cancel_with_cleanup(&self) {
        self.0.request.fetch_max(HOST_CANCEL, Ordering::Release);
        self.wake_sleepers();
    }

    pub fn reset(&self) {
        self.0.request.store(HOST_RUNNING, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.request(), HOST_CANCEL | HOST_ABORT)
    }

    pub(crate) fn request_pause(&self) {
        let _ = self.0.request.compare_exchange(
            HOST_RUNNING,
            HOST_PAUSE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn clear_pause(&self) {
        let _ = self.0.request.compare_exchange(
            HOST_PAUSE,
            HOST_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn flag_ptr(&self) -> *const AtomicU8 {
        &self.0.request
    }

    #[inline]
    pub(crate) fn request(&self) -> u8 {
        self.0.request.load(Ordering::Acquire)
    }

    fn wake_sleepers(&self) {
        self.0.sleep.notify_all();
        let listeners = {
            let mut listeners = self.0.listeners.lock().unwrap();
            let live: Vec<_> = listeners.iter().filter_map(Weak::upgrade).collect();
            listeners.retain(|listener| listener.strong_count() != 0);
            live
        };
        for listener in listeners {
            listener.notify();
        }
    }

    pub(crate) fn sleep_wait_timeout(
        &self,
        duration: Duration,
        in_cleanup: bool,
    ) -> Result<bool, ExecutionStop> {
        self.0
            .sleep
            .wait_timeout_while(duration, || cancellation_stop(self.request(), in_cleanup))
    }

    pub(crate) fn park_timeout(&self, duration: Duration, in_cleanup: bool) -> bool {
        self.0
            .sleep
            .park_timeout_while(duration, || cancellation_stop(self.request(), in_cleanup))
    }
}

/// The result of driving one slice of a Lisp invocation.
pub enum EvaluationPoll<'rt> {
    Ready(Vec<Root<'rt>>),
    /// There is runnable Lisp work. Schedule another slice.
    Runnable,
    /// Resume on a wake notification or when the timer delay expires.
    /// `None` means there is no pending timer.
    Waiting(Option<Duration>),
    /// The invocation owns a stopped task. Use [`Evaluation::with_debugger`]
    /// to inspect it, request motion, or transfer through a restart.
    Paused(DebugEvent<'rt>),
}

/// An invocation that owns the runtime's execution until it finishes or is dropped.
///
/// Drive it on the runtime's owning thread. Dropping the handle stops Lisp execution
/// immediately and releases its frames and wait registrations. To run Lisp cleanups,
/// request [`CancellationHandle::cancel_with_cleanup`] and drive it to completion.
#[must_use = "drive the evaluation to produce values and run cleanups"]
pub struct Evaluation<'rt> {
    runtime: &'rt Runtime,
    scheduled: Option<TaskExecutor<'rt>>,
}

impl<'rt> Evaluation<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime, scheduled: TaskExecutor<'rt>) -> Self {
        runtime.evaluation_active.set(true);
        Self {
            runtime,
            scheduled: Some(scheduled),
        }
    }

    /// Deliver debugger stops through [`EvaluationPoll::Paused`] for this invocation.
    /// Lisp debugger hooks still run; a registered debugger frontend is bypassed
    /// for stops that can return to this incremental driver.
    pub fn enable_debugging(&mut self) {
        if self.scheduled.is_some() {
            self.runtime.evaluation_debugging.set(true);
        }
    }

    /// Borrow a paused task on its owning thread. Use the runtime's `debug_*`
    /// operations to see the task's dynamic bindings and live frames. Values and
    /// source snapshots can be kept after this borrow; frame indices cannot be
    /// reused after the task resumes. A nonlocal exit returns `Ok(None)` and
    /// retires the pause. `resume` drives the resulting cleanup or continuation.
    pub fn with_debugger<T>(
        &mut self,
        operation: impl FnOnce(&'rt Runtime) -> T,
    ) -> Result<Option<T>, EvalError> {
        if !self.scheduled.as_ref().is_some_and(TaskExecutor::is_paused) {
            return Err(EvalError::new(0, "the evaluation is not paused"));
        }
        let runtime = self.runtime;
        let mut evaluation = DropGuard::new(self, |evaluation| evaluation.release());
        let result = {
            let previous = runtime.eval_depth.replace(1);
            let _restore = DropGuard::new(previous, |depth| runtime.eval_depth.set(depth));
            evaluation
                .scheduled
                .as_mut()
                .unwrap()
                .with_debugger(runtime, operation)
        };
        DropGuard::dismiss(evaluation);
        Ok(result)
    }

    /// Run a slice on the owning thread. The optional `waker` receives notifications
    /// of external completions and cancellation. The caller drives timer deadlines.
    ///
    /// The slice limits VM work at checkpoints. A synchronous callback, compilation
    /// operation or collection can take longer than an ordinary instruction. The
    /// configured execution budget spans all slices of the invocation.
    pub fn resume(&mut self, waker: Option<&Waker>) -> Result<EvaluationPoll<'rt>, EvalError> {
        if self.scheduled.is_none() {
            return Err(EvalError::new(0, "evaluation has already finished"));
        }
        let runtime = self.runtime;
        let mut evaluation = DropGuard::new(self, |evaluation| evaluation.release());
        runtime.execution_wake().set_waker(waker);
        let outcome = {
            let previous = runtime.eval_depth.replace(1);
            let _restore = DropGuard::new(previous, |depth| runtime.eval_depth.set(depth));
            evaluation.scheduled.as_mut().unwrap().drive(runtime, true)
        };
        match outcome {
            Ok(SchedulerStep::Runnable) => {
                DropGuard::dismiss(evaluation);
                Ok(EvaluationPoll::Runnable)
            }
            Ok(SchedulerStep::Parked(wait)) => {
                DropGuard::dismiss(evaluation);
                Ok(EvaluationPoll::Waiting(wait.delay))
            }
            Ok(SchedulerStep::Paused(event)) => {
                DropGuard::dismiss(evaluation);
                Ok(EvaluationPoll::Paused(event))
            }
            Ok(SchedulerStep::Complete(values)) => {
                Ok(EvaluationPoll::Ready(values.roots(runtime).collect()))
            }
            Err(error) => Err(error),
        }
    }

    fn release(&mut self) {
        if let Some(scheduled) = self.scheduled.take() {
            let runtime = self.runtime;
            let _lease = DropGuard::new(runtime, |runtime| {
                runtime.evaluation_debugging.set(false);
                runtime.evaluation_active.set(false);
                runtime.sweep_foreigns_after_execution();
            });
            runtime.execution_wake().set_waker(None);
            drop(scheduled);
        }
    }
}

impl Drop for Evaluation<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) fn cancellation_stop(request: u8, in_cleanup: bool) -> Option<ExecutionStop> {
    match request {
        HOST_ABORT => Some(ExecutionStop::Aborted),
        HOST_CANCEL if !in_cleanup => Some(ExecutionStop::Cancelled),
        _ => None,
    }
}

impl Runtime {
    pub(crate) fn execution_wake(&self) -> &Arc<ExecutionWake> {
        self.execution_wake.get_or_init(|| {
            let event = Arc::new(ExecutionWake::default());
            let mut listeners = self.cancellation.0.listeners.lock().unwrap();
            listeners.retain(|listener| listener.strong_count() != 0);
            listeners.push(Arc::downgrade(&event));
            event
        })
    }

    #[inline]
    pub(crate) fn host_cancel_request(&self) -> u8 {
        if self.installing_builtins.get() {
            HOST_RUNNING
        } else {
            self.cancellation.request()
        }
    }

    #[inline]
    pub(crate) fn check_host_cancellation(&self, at: usize) -> Result<(), EvalError> {
        let request = self.host_cancel_request();
        if request == HOST_CANCEL
            && crate::vm::task_scheduler::observe_host_cancel(self) == Some(false)
        {
            return Ok(());
        }
        if request != HOST_RUNNING
            && let Some(stop) = cancellation_stop(request, self.defers_host_cancellation())
        {
            return Err(EvalError::execution_stopped(at, stop));
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn check_execution_safepoint(&self, at: usize) -> Result<(), EvalError> {
        self.check_host_cancellation(at)?;
        self.charge_execution_step(at)
    }

    #[inline]
    pub(crate) fn charge_execution_step(&self, at: usize) -> Result<(), EvalError> {
        self.heap.poll_safepoint();
        if self.step_budget.get().is_some() && !self.installing_builtins.get() {
            let remaining = self.remaining_steps.get();
            if remaining == 0 {
                return Err(EvalError::execution_stopped(
                    at,
                    ExecutionStop::BudgetExhausted,
                ));
            }
            self.remaining_steps.set(remaining - 1);
        }
        Ok(())
    }

    pub(crate) fn defers_host_cancellation(&self) -> bool {
        self.cleanup_depth.get() != 0 || crate::vm::task_scheduler::host_cancel_is_shielded(self)
    }

    pub(crate) fn sleep_for(&self, duration: Duration, at: usize) -> Result<(), EvalError> {
        let in_cleanup = self.defers_host_cancellation();
        if !self.installing_builtins.get() {
            match self.cancellation.sleep_wait_timeout(duration, in_cleanup) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(stop) => return Err(EvalError::execution_stopped(at, stop)),
            }
        }
        platform::sleep_host(&*self.host_services, duration, at)
    }
}
