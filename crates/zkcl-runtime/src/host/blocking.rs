use std::collections::BTreeMap;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::{Context, Wake, Waker};

use super::operation::{HostCompletion, HostError, HostReply, Materialize};
use crate::condition::Fault;
use crate::{
    CancellationHandle, EvalError, ExecutionStop, NativeResult, Root, Runtime, RuntimeBuilder,
};

/// A bounded queue served by a fixed number of native workers.
///
/// Sharing a handle shares the same workers. When the last handle is dropped,
/// the workers drain accepted work and exit. Use the completion protocol to
/// await the end of individual operations.
#[derive(Clone)]
pub struct BlockingPool(Arc<Pool>);

/// All of a blocking pool's pending slots are occupied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the blocking work queue is full")]
pub struct BlockingPoolFull;

struct Pool {
    queue: Arc<WorkQueue>,
    capacity: usize,
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.queue.state.lock().unwrap().closed = true;
        self.queue.available.notify_all();
    }
}

#[derive(Default)]
struct WorkQueue {
    state: Mutex<QueuedTasks>,
    available: Condvar,
}

#[derive(Default)]
struct QueuedTasks {
    jobs: BTreeMap<u64, BlockingTask>,
    next_id: u64,
    closed: bool,
}

struct BlockingTask {
    // When withdrawn from the queue, release the provider's captures before
    // dropping the completion that acknowledges its termination.
    run: Box<dyn FnOnce(&HostCompletion) -> Materialize + Send>,
    completion: HostCompletion,
}

struct RemoveQueuedJob {
    queue: Weak<WorkQueue>,
    id: u64,
}

impl Wake for RemoveQueuedJob {
    fn wake(self: Arc<Self>) {
        if let Some(queue) = self.queue.upgrade() {
            let job = queue.state.lock().unwrap().jobs.remove(&self.id);
            drop(job);
        }
    }
}

struct CancelRuntime(CancellationHandle);

impl Wake for CancelRuntime {
    fn wake(self: Arc<Self>) {
        self.0.cancel_with_cleanup();
    }
}

#[derive(Debug, thiserror::Error)]
#[error("isolated Lisp work failed ({0})")]
struct IsolatedRuntimeError(#[source] HostError);

impl BlockingPool {
    /// Start `workers` threads with space for up to `queued` jobs waiting to start.
    pub fn new(workers: NonZeroUsize, queued: NonZeroUsize) -> io::Result<Self> {
        let pool = Self(Arc::new(Pool {
            queue: Arc::default(),
            capacity: queued.get(),
        }));
        for index in 0..workers.get() {
            let queue = Arc::clone(&pool.0.queue);
            std::thread::Builder::new()
                .name(format!("zkcl-blocking-{index}"))
                .spawn(move || queue.run())?;
        }
        Ok(pool)
    }

    /// Submit owned work without blocking the runtime thread. The worker can
    /// check cancellation using its completion. `materialize` converts the
    /// owned result on the runtime's thread. A cancelled queued job is removed
    /// before it starts. If the queue is full, this consumes the completion
    /// and returns an error for the starting callback to report.
    pub fn try_submit<T: Send + 'static>(
        &self,
        completion: HostCompletion,
        work: impl FnOnce(&HostCompletion) -> T + Send + 'static,
        materialize: impl for<'rt> FnOnce(&'rt Runtime, T) -> NativeResult<'rt> + Send + 'static,
    ) -> Result<(), BlockingPoolFull> {
        self.try_submit_with_fault(completion, work, move |runtime, result, at| {
            materialize(runtime, result).map_err(|error| EvalError::host_callback(at, error).into())
        })
    }

    /// Run Lisp in a fresh runtime created and destroyed on a worker thread.
    /// The job receives its own cancellation handle. A parent cancellation asks
    /// the worker to unwind, and cooperative group exit waits for the worker's
    /// runtime to be destroyed. Only owned `Send` data crosses between runtimes.
    /// Lisp conditions and execution stops in the worker become host callback
    /// errors with the original error as a cause.
    pub fn try_submit_runtime<T: Send + 'static>(
        &self,
        completion: HostCompletion,
        builder: RuntimeBuilder,
        work: impl FnOnce(&Runtime) -> Result<T, HostError> + Send + 'static,
        materialize: impl for<'rt> FnOnce(&'rt Runtime, T) -> NativeResult<'rt> + Send + 'static,
    ) -> Result<(), BlockingPoolFull> {
        self.try_submit(
            completion,
            move |completion| {
                let cancellation = CancellationHandle::new();
                let waker = Waker::from(Arc::new(CancelRuntime(cancellation.clone())));
                if completion
                    .poll_cancelled(&mut Context::from_waker(&waker))
                    .is_ready()
                {
                    return Err(EvalError::execution_stopped(0, ExecutionStop::Cancelled).into());
                }
                let runtime = builder.cancellation(cancellation).start()?;
                work(&runtime)
            },
            move |runtime, result| {
                let result = result.map_err(IsolatedRuntimeError)?;
                materialize(runtime, result)
            },
        )
    }

    pub(crate) fn try_submit_with_fault<T: Send + 'static>(
        &self,
        completion: HostCompletion,
        work: impl FnOnce(&HostCompletion) -> T + Send + 'static,
        materialize: impl for<'rt> FnOnce(&'rt Runtime, T, usize) -> Result<Vec<Root<'rt>>, Fault>
        + Send
        + 'static,
    ) -> Result<(), BlockingPoolFull> {
        let queue = &self.0.queue;
        let mut state = queue.state.lock().unwrap();
        if state.jobs.len() == self.0.capacity {
            return Err(BlockingPoolFull);
        }

        let id = state.next_id;
        state.next_id = id.checked_add(1).expect("blocking job ID space exhausted");
        let waker = Waker::from(Arc::new(RemoveQueuedJob {
            queue: Arc::downgrade(queue),
            id,
        }));
        let job = BlockingTask {
            run: Box::new(move |completion| {
                let result = work(completion);
                Box::new(move |runtime, at| materialize(runtime, result, at))
            }),
            completion,
        };
        // The queue lock covers registration and publication. If cancellation
        // races with insertion, its waker removes the published job.
        match job.completion.replace_cancel_waker(waker) {
            Ok(previous) => {
                state.jobs.insert(id, job);
                drop(state);
                queue.available.notify_one();
                drop(previous);
            }
            Err(waker) => {
                drop(state);
                drop(job);
                drop(waker);
            }
        }
        Ok(())
    }
}

impl WorkQueue {
    fn run(&self) {
        loop {
            let job = {
                let state = self.state.lock().unwrap();
                let mut state = self
                    .available
                    .wait_while(state, |state| state.jobs.is_empty() && !state.closed)
                    .unwrap();
                let Some((_, job)) = state.jobs.pop_first() else {
                    return;
                };
                job
            };
            // Also keep the worker alive if a host notification panics after
            // a result has been published.
            let _ = catch_unwind(AssertUnwindSafe(|| job.run()));
        }
    }
}

impl BlockingTask {
    fn run(self) {
        let Self {
            run,
            mut completion,
        } = self;
        if completion.is_cancelled() {
            drop(run);
            return;
        }
        let result: Materialize = match catch_unwind(AssertUnwindSafe(|| run(&completion))) {
            Ok(result) => result,
            Err(payload) => {
                let reason = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                let error = io::Error::other(format!("blocking host work panicked: {reason}"));
                Box::new(move |_: &Runtime, at| {
                    Err(EvalError::host_callback(at, Box::new(error)).into())
                })
            }
        };
        completion.publish(HostReply::Complete(result));
    }
}
