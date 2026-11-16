use std::rc::Rc;
use std::time::Duration;

use super::super::callback::Outcome;
use super::super::control_stack::ContinuationScope;
use super::super::{Continuation, ContinuationStep, ExecState, Resume, Values};
use super::cancellation::BoundarySignal;
use super::groups::TaskGroup;
use super::handles::TaskFailure;
use super::timers::TimerRegistration;
use super::{
    GroupHandle, TaskId, async_error, call_one, deadline, raise_condition, require_suspendable,
    timer_fault, with_scheduler,
};
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, Word};
use crate::{EvalError, Runtime};

pub(super) struct CancelScopeState {
    pub(super) owner: TaskId,
    pub(super) signal: Rc<BoundarySignal>,
}

impl CancelScopeState {
    pub(super) fn is_cancelled(&self) -> bool {
        self.signal.is_triggered() || self.signal.was_unwound()
    }
}

pub(super) struct CancelScopeHandle(pub(super) Rc<CancelScopeState>);

pub(super) struct GroupScope {
    pub(super) body: RootSlot,
    pub(super) group: Option<TaskGroup>,
    pub(super) resume: Resume,
    pub(super) at: usize,
}

impl Drop for GroupScope {
    fn drop(&mut self) {
        if let Some(group) = &self.group {
            group.borrow_mut().close();
        }
    }
}

impl Continuation for GroupScope {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        _returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let result = (|| {
            if let Some(group) = self.group.take() {
                let failure = {
                    let mut state = group.borrow_mut();
                    let failure = state.take_failure();
                    state.close();
                    failure
                };
                return match failure {
                    Some(TaskFailure::Condition(condition)) => {
                        Ok(raise_condition(runtime, condition.get(), self.at))
                    }
                    Some(TaskFailure::Message(message)) => async_error(runtime, &message, self.at),
                    None => Ok(ContinuationStep::KeepValues),
                };
            }
            require_suspendable(runtime, exec, self.at, "establish a task group")?;
            let group = with_scheduler(runtime, |s| s.new_group()).expect("active task");
            let handle = runtime
                .foreign_owned(GroupHandle(Rc::clone(&group)))
                .map_err(|error| EvalError::out_of_memory(self.at, error))?;
            self.group = Some(group);
            Ok(call_one(runtime, self.body.get(), handle.slot.get(), self.at))
        })();
        Outcome::Completed(result)
    }

    fn continuation_scope(&self) -> Option<ContinuationScope> {
        let group = self.group.as_ref()?.borrow();
        // Only the group body has a cancellation boundary. Failure reporting runs outside it.
        group
            .is_open()
            .then(|| ContinuationScope::CancelBoundary(group.boundary()))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

pub(super) struct CancelScope {
    pub(super) body: RootSlot,
    pub(super) fallback: Option<RootSlot>,
    pub(super) running: Option<Rc<BoundarySignal>>,
    pub(super) resume: Resume,
    pub(super) at: usize,
}

impl Drop for CancelScope {
    fn drop(&mut self) {
        if let Some(signal) = &self.running {
            signal.disarm();
        }
    }
}

impl Continuation for CancelScope {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let result = (|| {
            if let Some(signal) = self.running.take() {
                if !signal.was_unwound() {
                    signal.disarm();
                    return Ok(ContinuationStep::KeepValues);
                }
                return match self.fallback {
                    Some(ref fallback) => Ok(ContinuationStep::Call {
                        target: fallback.get(),
                        args: runtime.root_frame(0),
                        at: self.at,
                    }),
                    None => {
                        exec.values.one(Word::NIL);
                        Ok(ContinuationStep::KeepValues)
                    }
                };
            }
            if returned {
                return Ok(ContinuationStep::KeepValues);
            }
            require_suspendable(runtime, exec, self.at, "establish a cancel scope")?;
            let scope = with_scheduler(runtime, |s| s.new_cancel_scope()).expect("active task");
            let signal = Rc::clone(&scope.signal);
            let handle = runtime
                .foreign_owned(CancelScopeHandle(scope))
                .map_err(|error| EvalError::out_of_memory(self.at, error))?;
            self.running = Some(signal);
            Ok(call_one(runtime, self.body.get(), handle.slot.get(), self.at))
        })();
        Outcome::Completed(result)
    }

    fn continuation_scope(&self) -> Option<ContinuationScope> {
        self.running
            .as_ref()
            .map(|signal| ContinuationScope::CancelBoundary(Rc::clone(signal)))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

pub(super) struct ProtectCancel {
    pub(super) thunk: RootSlot,
    pub(super) resume: Resume,
    pub(super) at: usize,
}

impl Continuation for ProtectCancel {
    fn step(
        &mut self,
        _runtime: &Runtime,
        _values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        Ok(if returned {
            ContinuationStep::KeepValues
        } else {
            ContinuationStep::Call {
                target: self.thunk.get(),
                args: RootFrame::empty(),
                at: self.at,
            }
        })
    }

    fn continuation_scope(&self) -> Option<ContinuationScope> {
        Some(ContinuationScope::CancelShield)
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

pub(super) struct WithTimeout {
    pub(super) duration: Duration,
    pub(super) body: RootSlot,
    pub(super) fallback: RootSlot,
    pub(super) running: Option<(Rc<BoundarySignal>, TimerRegistration)>,
    pub(super) resume: Resume,
    pub(super) at: usize,
}

impl Continuation for WithTimeout {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let result = (|| {
            if let Some((signal, timer)) = self.running.take() {
                drop(timer);
                if !signal.was_unwound() {
                    return Ok(ContinuationStep::KeepValues);
                }
                if let Some(error) = signal.take_timer_error() {
                    return Err(timer_fault(self.at, &error));
                }
            } else if returned {
                return Ok(ContinuationStep::KeepValues);
            } else if !self.duration.is_zero() {
                require_suspendable(runtime, exec, self.at, "establish a timeout")?;
                let deadline = deadline(runtime, self.duration, self.at)?;
                self.running = Some(
                    with_scheduler(runtime, |s| s.register_timeout(deadline))
                        .expect("timeout has an active task"),
                );
                return Ok(ContinuationStep::Call {
                    target: self.body.get(),
                    args: runtime.root_frame(0),
                    at: self.at,
                });
            }
            Ok(ContinuationStep::Call {
                target: self.fallback.get(),
                args: runtime.root_frame(0),
                at: self.at,
            })
        })();
        Outcome::Completed(result)
    }

    fn continuation_scope(&self) -> Option<ContinuationScope> {
        self.running
            .as_ref()
            .map(|(signal, _)| ContinuationScope::CancelBoundary(Rc::clone(signal)))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
