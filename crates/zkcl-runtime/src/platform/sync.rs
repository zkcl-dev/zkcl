use std::sync::Mutex;
use std::task::Waker;
use std::time::Duration;

use crate::EvalError;

pub(crate) use imp::SleepSignal;

#[derive(Default)]
struct WakeState {
    generation: u64,
    waker: Option<Waker>,
}

/// Synchronization primitive used to notify and wait on host completions and cancellations.
#[derive(Default)]
pub(crate) struct ExecutionWake {
    state: Mutex<WakeState>,
    inner: imp::WakeCondition,
}

impl ExecutionWake {
    pub(crate) fn set_waker(&self, waker: Option<&Waker>) {
        let waker = waker.cloned();
        let previous = std::mem::replace(&mut self.state.lock().unwrap().waker, waker);
        drop(previous);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    pub(crate) fn notify(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.generation = state.generation.wrapping_add(1);
            self.inner.notify_all();
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn wait(&self, generation: u64, delay: Option<Duration>) -> Result<(), EvalError> {
        self.inner.wait(&self.state, generation, delay)
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    use super::WakeState;
    use crate::{EvalError, ExecutionStop};

    /// Condition-variable-backed timeout wait on threaded targets.
    #[derive(Debug, Default)]
    pub(crate) struct SleepSignal {
        lock: Mutex<()>,
        wake: Condvar,
    }

    impl SleepSignal {
        pub(crate) fn notify_all(&self) {
            let _guard = self.lock.lock().unwrap();
            self.wake.notify_all();
        }

        /// Wait up to `duration` while `poll_stop()` returns `None`.
        pub(crate) fn wait_timeout_while(
            &self,
            duration: Duration,
            mut poll_stop: impl FnMut() -> Option<ExecutionStop>,
        ) -> Result<bool, ExecutionStop> {
            let guard = self.lock.lock().unwrap();
            let mut stop = None;
            let _wait = self
                .wake
                .wait_timeout_while(guard, duration, |_| {
                    stop = poll_stop();
                    stop.is_none()
                })
                .unwrap();
            match stop {
                Some(stop) => Err(stop),
                None => Ok(true),
            }
        }

        /// Block up to `duration`, returning `true` when handled natively.
        pub(crate) fn park_timeout_while(
            &self,
            duration: Duration,
            poll_stop: impl FnMut() -> Option<ExecutionStop>,
        ) -> bool {
            let _ = self.wait_timeout_while(duration, poll_stop);
            true
        }
    }

    #[derive(Default)]
    pub(super) struct WakeCondition(Condvar);

    impl WakeCondition {
        pub(super) fn notify_all(&self) {
            self.0.notify_all();
        }

        pub(super) fn wait(
            &self,
            state: &Mutex<WakeState>,
            generation: u64,
            delay: Option<Duration>,
        ) -> Result<(), EvalError> {
            let guard = state.lock().unwrap();
            if let Some(delay) = delay {
                drop(
                    self.0
                        .wait_timeout_while(guard, delay, |s| s.generation == generation)
                        .unwrap(),
                );
            } else {
                drop(
                    self.0
                        .wait_while(guard, |s| s.generation == generation)
                        .unwrap(),
                );
            }
            Ok(())
        }
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::WakeState;
    use crate::{EvalError, ExecutionStop};

    /// No-op timeout signal on WebAssembly; callers yield via host sleep.
    #[derive(Debug, Default)]
    pub(crate) struct SleepSignal;

    impl SleepSignal {
        pub(crate) fn notify_all(&self) {}

        pub(crate) fn wait_timeout_while(
            &self,
            _duration: Duration,
            mut poll_stop: impl FnMut() -> Option<ExecutionStop>,
        ) -> Result<bool, ExecutionStop> {
            match poll_stop() {
                Some(stop) => Err(stop),
                None => Ok(false),
            }
        }

        pub(crate) fn park_timeout_while(
            &self,
            _duration: Duration,
            _poll_stop: impl FnMut() -> Option<ExecutionStop>,
        ) -> bool {
            false
        }
    }

    #[derive(Default)]
    pub(super) struct WakeCondition;

    impl WakeCondition {
        pub(super) fn notify_all(&self) {}

        pub(super) fn wait(
            &self,
            _state: &Mutex<WakeState>,
            _generation: u64,
            _delay: Option<Duration>,
        ) -> Result<(), EvalError> {
            Err(EvalError::new(
                0,
                "pending host work requires incremental execution on Wasm",
            ))
        }
    }
}
