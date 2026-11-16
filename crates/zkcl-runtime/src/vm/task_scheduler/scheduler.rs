use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::DropGuard;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rustc_hash::FxHashMap;

use super::super::callback::{self, Outcome};
use super::super::debugging::{DebugEvent, PausedEvent};
use super::super::{
    ExecState, Invocation, Pending, PendingContinuation, Resume, StepControl, Values, accept,
    dispatch,
};
use super::TaskId;
use super::cancellation::{BoundarySignal, TaskCancellation};
use super::dependencies::{Dependency, Requirement, WaitGraph};
use super::groups::{ActiveGroups, Group, TaskGroup};
use super::handles::{AwaitRegistration, LiveHandle, Task, TaskFailure};
use super::operations::{
    AcquireSemaphore, Await, AwaitAny, AwaitPromise, ChannelRecv, ChannelSend, Operation, Select,
    SelectBranch, SelectFinished, Spawn, SuspendReason, Suspension, TaskCall, WaitEvent,
};
use super::scopes::CancelScopeState;
use super::sync::{QueueRegistration, SelectClaim, SenderRegistration};
use super::timers::{TimerEvent, TimerRegistration, Timers};
use crate::classes::TaskClassUpdates;
use crate::condition::Fault;
use crate::execution::{HOST_ABORT, HOST_CANCEL};
use crate::heap::Word;
use crate::host::{HostCancellation, HostEvents, HostOperation};
use crate::printer::PrinterTaskState;
use crate::{EvalError, ExecutionStop, OutOfMemory, Runtime};

const ROOT_TASK: TaskId = 0;

struct Membership {
    group: TaskGroup,
    allow_failure: bool,
}

enum Wait {
    Sleep(TimerRegistration),
    Await {
        registration: AwaitRegistration,
        await_task: Await,
        has_deadline: bool,
    },
    AwaitAny {
        registrations: Vec<AwaitRegistration>,
        await_any: AwaitAny,
        has_deadline: bool,
    },
    AwaitPromise {
        registration: QueueRegistration,
        await_promise: AwaitPromise,
    },
    Select {
        claim: Rc<SelectClaim>,
        registrations: SelectRegistrations,
        select: Select,
        has_deadline: bool,
    },
    Group {
        group: TaskGroup,
        unwinding: bool,
    },
    Event {
        registration: QueueRegistration,
        wait_event: WaitEvent,
    },
    Semaphore {
        registration: QueueRegistration,
        acquire: AcquireSemaphore,
    },
    ChannelSend {
        registration: SenderRegistration,
        send: ChannelSend,
    },
    ChannelRecv {
        registration: QueueRegistration,
        recv: ChannelRecv,
    },
    Host(HostOperation),
}

struct SelectRegistrations {
    _queues: Vec<QueueRegistration>,
    _senders: Vec<SenderRegistration>,
    _tasks: Vec<AwaitRegistration>,
    timeout: Option<(usize, TimerRegistration)>,
}

enum Wake<'a> {
    Ready,
    TaskFinished(&'a LiveHandle),
    ClockFault(&'a Rc<str>),
}

enum TaskState<'rt> {
    Ready(Option<PendingContinuation<'rt>>),
    Waiting {
        wait: Wait,
        resume: Resume,
        at: usize,
    },
}

struct TaskSlot<'rt> {
    exec: ExecState<'rt>,
    printer: PrinterTaskState,
    class_updates: TaskClassUpdates,
    handle: Option<LiveHandle>,
    membership: Option<Membership>,
    state: TaskState<'rt>,
}

struct ActiveTask {
    id: TaskId,
    cancellation: Rc<TaskCancellation>,
    handle: Option<LiveHandle>,
}

pub(super) struct Scheduler<'rt> {
    root: Option<TaskSlot<'rt>>,
    children: FxHashMap<TaskId, TaskSlot<'rt>>,
    active: Option<ActiveTask>,
    host_cancel_shield: bool,
    ready: VecDeque<TaskId>,
    groups: ActiveGroups,
    timers: Timers,
    host_events: Option<Arc<HostEvents>>,
    host_waits: usize,
    host_cancellations: VecDeque<std::task::Waker>,
    host_cancelled: bool,
    next_id: TaskId,
    select_cursor: usize,
    wait_graph: WaitGraph,
}

impl<'rt> Scheduler<'rt> {
    fn new(values: Values) -> Self {
        Self {
            root: Some(TaskSlot {
                exec: ExecState::new(values),
                printer: PrinterTaskState::default(),
                class_updates: TaskClassUpdates::default(),
                handle: None,
                membership: None,
                state: TaskState::Ready(None),
            }),
            children: FxHashMap::default(),
            active: None,
            host_cancel_shield: false,
            ready: VecDeque::new(),
            groups: ActiveGroups::default(),
            timers: Timers::default(),
            host_events: None,
            host_waits: 0,
            host_cancellations: VecDeque::new(),
            host_cancelled: false,
            next_id: 1,
            select_cursor: 0,
            wait_graph: WaitGraph::default(),
        }
    }

    pub(super) fn peek_select_cursor(&self) -> usize {
        self.select_cursor
    }

    pub(super) fn next_select_cursor(&mut self) -> usize {
        let current = self.select_cursor;
        self.select_cursor = current.wrapping_add(1);
        current
    }

    pub(super) fn has_ready_tasks(&self) -> bool {
        !self.ready.is_empty()
    }

    pub(super) fn has_concurrent_work(&self) -> bool {
        self.is_child_task() || !self.children.is_empty() || self.timers.next_deadline().is_some()
    }

    pub(super) fn needs_suspendable_calls(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.cancellation.has_cancel_boundary_in_scope())
            || self.has_concurrent_work()
    }

    fn is_child_task(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.id != ROOT_TASK)
    }

    pub(super) fn retains_task_result(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.handle.is_some())
    }

    pub(super) fn active_cancel_pending(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.cancellation.is_pending())
    }

    pub(super) fn new_group(&self) -> TaskGroup {
        let active = self.active.as_ref().expect("group has an active owner");
        Rc::new(RefCell::new(Group::new(
            active.id,
            Rc::clone(&active.cancellation),
        )))
    }

    pub(super) fn owns_group(&self, group: &Group) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| group.belongs_to(active.id, &active.cancellation))
    }

    pub(super) fn register_timeout(
        &mut self,
        deadline: u64,
    ) -> (Rc<BoundarySignal>, TimerRegistration) {
        let active = self.active.as_ref().expect("timeout has an active task");
        let signal = BoundarySignal::for_deadline(Rc::clone(&active.cancellation));
        let timer = self
            .timers
            .register(deadline, TimerEvent::Timeout(active.id, Rc::clone(&signal)));
        (signal, timer)
    }

    fn task(&self, id: TaskId) -> Option<&TaskSlot<'rt>> {
        if id == ROOT_TASK {
            self.root.as_ref()
        } else {
            self.children.get(&id)
        }
    }

    fn task_mut(&mut self, id: TaskId) -> Option<&mut TaskSlot<'rt>> {
        if id == ROOT_TASK {
            self.root.as_mut()
        } else {
            self.children.get_mut(&id)
        }
    }

    fn take_task(&mut self, id: TaskId) -> TaskSlot<'rt> {
        if id == ROOT_TASK {
            self.root.take()
        } else {
            self.children.remove(&id)
        }
        .expect("ready task")
    }

    fn put_task(&mut self, id: TaskId, entry: TaskSlot<'rt>) {
        if id == ROOT_TASK {
            self.root = Some(entry);
        } else {
            self.children.insert(id, entry);
        }
    }

    fn fail_deadlocked_wait(&mut self) -> Result<(), EvalError> {
        const MESSAGE: &str = "async task scheduler deadlocked";
        let can_signal = |entry: &TaskSlot<'_>| match &entry.state {
            // The owner must stay suspended until its children have unwound.
            TaskState::Waiting { wait, .. } => !matches!(wait, Wait::Group { .. } | Wait::Host(_)),
            TaskState::Ready(_) => false,
        };
        let id = if self.root.as_ref().is_some_and(can_signal) {
            ROOT_TASK
        } else {
            self.children
                .iter()
                .filter_map(|(&id, entry)| can_signal(entry).then_some(id))
                .min()
                .ok_or_else(|| EvalError::new(0, MESSAGE))?
        };
        let entry = self.task_mut(id).expect("deadlock victim is live");
        let TaskState::Waiting { resume, at, .. } = entry.state else {
            unreachable!("deadlock victim is waiting")
        };
        entry.state = TaskState::Ready(Some(PendingContinuation(Pending::TaskCall(
            TaskCall::new(Operation::Error(MESSAGE), resume, at),
        ))));
        self.ready.push_back(id);
        Ok(())
    }

    fn wake_task(&mut self, id: TaskId, wake: Wake<'_>) {
        let Some(entry) = self.task_mut(id) else {
            return;
        };
        if !matches!(entry.state, TaskState::Waiting { .. }) {
            return;
        }
        let can_deliver_cancel = entry.exec.control.can_deliver_task_cancel();
        let TaskState::Waiting { wait, resume, at } =
            std::mem::replace(&mut entry.state, TaskState::Ready(None))
        else {
            unreachable!("only a suspended task can be woken")
        };
        debug_assert!(
            !matches!(wake, Wake::ClockFault(_))
                || matches!(wait, Wait::Sleep(_) | Wait::Select { .. })
        );
        let host_wait = matches!(&wait, Wait::Host(_));
        let pending = match wait {
            Wait::Sleep(timer) => {
                drop(timer);
                if let Wake::ClockFault(message) = wake {
                    Pending::TimerFault {
                        at,
                        message: Rc::clone(message),
                    }
                } else {
                    Pending::TaskCall(TaskCall::new(Operation::ReturnNil, resume, at))
                }
            }
            Wait::Await { await_task, .. } => {
                Pending::TaskCall(TaskCall::new(Operation::Await(await_task), resume, at))
            }
            Wait::AwaitAny {
                registrations,
                await_any,
                ..
            } => {
                drop(registrations);
                let operation = if let Wake::TaskFinished(finished) = wake {
                    Operation::AwaitAnyFinished(finished.word.clone())
                } else {
                    Operation::AwaitAny(await_any)
                };
                Pending::TaskCall(TaskCall::new(operation, resume, at))
            }
            Wait::AwaitPromise {
                registration,
                await_promise,
            } => {
                drop(registration);
                Pending::TaskCall(TaskCall::new(
                    Operation::AwaitPromise(await_promise),
                    resume,
                    at,
                ))
            }
            Wait::Select {
                claim,
                registrations,
                select,
                ..
            } => {
                let timeout_branch = registrations.timeout.as_ref().map(|(branch, _)| *branch);
                drop(registrations);
                match wake {
                    Wake::ClockFault(message) => Pending::TimerFault {
                        at,
                        message: Rc::clone(message),
                    },
                    Wake::TaskFinished(finished) => {
                        if claim.winner().is_none()
                            && let Some(branch) = select.branches.iter().position(|branch| {
                                matches!(branch, SelectBranch::Task(task) if task.id == finished.task.id)
                            })
                        {
                            claim.claim(branch);
                        }
                        Pending::TaskCall(TaskCall::new(
                            Operation::SelectFinished(SelectFinished { select, claim }),
                            resume,
                            at,
                        ))
                    }
                    Wake::Ready => {
                        if claim.winner().is_none()
                            && let Some(branch) = timeout_branch
                            && !can_deliver_cancel
                        {
                            claim.claim(branch);
                        }
                        let operation = if claim.winner().is_some() {
                            Operation::SelectFinished(SelectFinished { select, claim })
                        } else {
                            Operation::Select(select)
                        };
                        Pending::TaskCall(TaskCall::new(operation, resume, at))
                    }
                }
            }
            Wait::Group { group, unwinding } => Pending::TaskCall(TaskCall::new(
                Operation::FinishGroup { group, unwinding },
                resume,
                at,
            )),
            Wait::Event {
                registration,
                mut wait_event,
            } => {
                wait_event.woken = registration.is_woken();
                drop(registration);
                Pending::TaskCall(TaskCall::new(Operation::WaitEvent(wait_event), resume, at))
            }
            Wait::Semaphore {
                registration,
                mut acquire,
            } => {
                acquire.acquired = registration.is_woken();
                drop(registration);
                Pending::TaskCall(TaskCall::new(
                    Operation::AcquireSemaphore(acquire),
                    resume,
                    at,
                ))
            }
            Wait::ChannelSend {
                registration,
                mut send,
            } => {
                send.delivered = registration.is_delivered();
                drop(registration);
                Pending::TaskCall(TaskCall::new(Operation::ChannelSend(send), resume, at))
            }
            Wait::ChannelRecv {
                registration,
                mut recv,
            } => {
                recv.received = registration.take_payload();
                drop(registration);
                Pending::TaskCall(TaskCall::new(Operation::ChannelRecv(recv), resume, at))
            }
            Wait::Host(operation) => {
                Pending::TaskCall(TaskCall::new(Operation::ResumeHost(operation), resume, at))
            }
        };
        entry.state = TaskState::Ready(Some(PendingContinuation(pending)));
        self.host_waits -= usize::from(host_wait);
        self.ready.push_back(id);
    }

    fn wake_for_cancellation(&mut self, id: TaskId) {
        let Some(entry) = self.task(id) else { return };
        let TaskState::Waiting { wait, .. } = &entry.state else {
            return;
        };
        if !entry.exec.control.can_deliver_task_cancel() {
            return;
        }
        let cancellation = match wait {
            Wait::Group {
                unwinding: true, ..
            } => return,
            Wait::Host(operation) => operation.request_cancel(),
            _ => HostCancellation::Acknowledged,
        };
        match cancellation {
            HostCancellation::Acknowledged => self.wake_task(id, Wake::Ready),
            HostCancellation::Waiting(Some(waker)) => self.host_cancellations.push_back(waker),
            HostCancellation::Waiting(None) => {}
        }
    }

    fn request_cancellation(&mut self, id: TaskId) -> bool {
        if let Some(active) = &self.active
            && id == active.id
        {
            return active.cancellation.request();
        }
        let changed = self
            .task(id)
            .is_some_and(|entry| entry.exec.control.cancellation_handle().request());
        if changed {
            self.wake_for_cancellation(id);
        }
        changed
    }

    pub(super) fn cancel_task(&mut self, task: &Task) -> bool {
        // Terminal handles may outlive the scheduler and reuse a numeric ID.
        task.is_running() && self.request_cancellation(task.id)
    }

    pub(super) fn close_group(&mut self, group: &TaskGroup, cancel_all: bool) -> bool {
        group.borrow_mut().begin_close(cancel_all, |id| {
            self.request_cancellation(id);
        })
    }

    pub(super) fn cancel_group(&mut self, group: &TaskGroup) -> bool {
        group.borrow_mut().cancel(|id| {
            self.request_cancellation(id);
        })
    }

    pub(super) fn new_cancel_scope(&self) -> Rc<CancelScopeState> {
        let active = self
            .active
            .as_ref()
            .expect("cancel scope has an active task");
        Rc::new(CancelScopeState {
            owner: active.id,
            signal: BoundarySignal::for_scope(Rc::clone(&active.cancellation)),
        })
    }

    pub(super) fn cancel_scope(&mut self, scope: &CancelScopeState) -> bool {
        if !scope.signal.trigger() {
            return false;
        }
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.id != scope.owner)
        {
            self.wake_for_cancellation(scope.owner);
        }
        true
    }

    pub(super) fn await_would_cycle(&mut self, target: TaskId, shields_cancel: bool) -> bool {
        self.waits_would_cycle(std::iter::once(Dependency::result(target)), shields_cancel)
    }

    pub(super) fn await_any_would_cycle(
        &mut self,
        targets: &[Rc<Task>],
        shields_cancel: bool,
    ) -> bool {
        !targets.is_empty()
            && self.waits_would_cycle(
                targets.iter().map(|task| Dependency::result(task.id)),
                shields_cancel,
            )
    }

    pub(super) fn select_would_cycle(
        &mut self,
        branches: &[SelectBranch],
        shields_cancel: bool,
    ) -> bool {
        !branches.is_empty()
            && branches
                .iter()
                .all(|branch| matches!(branch, SelectBranch::Task(_)))
            && self.waits_would_cycle(
                branches.iter().filter_map(|branch| match branch {
                    SelectBranch::Task(task) => Some(Dependency::result(task.id)),
                    _ => None,
                }),
                shields_cancel,
            )
    }

    fn waits_would_cycle(
        &mut self,
        targets: impl Iterator<Item = Dependency> + Clone,
        shields_cancel: bool,
    ) -> bool {
        let active_id = self.active.as_ref().expect("await has an active task").id;
        let view = WaitDependencies {
            root: self.root.as_ref(),
            children: &self.children,
            groups: &self.groups,
            active_id,
            shields_cancel,
        };
        if targets.clone().any(|target| view.ready_leaf(target.task)) {
            return false;
        }
        !self
            .wait_graph
            .any_can_finish(targets, |dep, add| view.describe(dep, add))
    }

    fn suspend(
        &mut self,
        runtime: &Runtime,
        id: TaskId,
        mut entry: TaskSlot<'rt>,
        suspension: Suspension,
    ) {
        let Suspension { reason, resume, at } = suspension;
        let mut ready = false;
        let wait = match reason {
            SuspendReason::Yield => {
                self.ready.push_back(id);
                entry.state = TaskState::Ready(Some(PendingContinuation(Pending::TaskCall(
                    TaskCall::new(Operation::ReturnNil, resume, at),
                ))));
                self.put_task(id, entry);
                return;
            }
            SuspendReason::Sleep(deadline) => {
                Wait::Sleep(self.timers.register(deadline, TimerEvent::Wake(id)))
            }
            SuspendReason::Await {
                await_task,
                has_deadline,
            } => Wait::Await {
                registration: await_task
                    .task
                    .register_await(id)
                    .expect("one queued await per task"),
                await_task,
                has_deadline,
            },
            SuspendReason::AwaitAny {
                await_any,
                has_deadline,
            } => {
                let registrations = await_any
                    .tasks
                    .iter()
                    .filter_map(|task| task.register_await(id))
                    .collect();
                Wait::AwaitAny {
                    registrations,
                    await_any,
                    has_deadline,
                }
            }
            SuspendReason::AwaitPromise(await_promise) => Wait::AwaitPromise {
                registration: await_promise.promise.register_wait(id),
                await_promise,
            },
            SuspendReason::Select {
                select,
                timeout,
                has_deadline,
            } => {
                let claim = SelectClaim::new();
                let mut queues = Vec::new();
                let mut senders = Vec::new();
                let mut tasks = Vec::new();
                for (index, branch) in select.branches.iter().enumerate() {
                    match branch {
                        SelectBranch::Recv(channel) => {
                            queues.push(channel.register_select_recv(id, Rc::clone(&claim), index))
                        }
                        SelectBranch::Send(channel) => senders.push(channel.register_select_send(
                            runtime,
                            id,
                            select.branch_second(index),
                            Rc::clone(&claim),
                            index,
                        )),
                        SelectBranch::Event(event) => {
                            queues.push(event.register_select_wait(id, Rc::clone(&claim), index))
                        }
                        SelectBranch::Promise(promise) => {
                            queues.push(promise.register_select_wait(id, Rc::clone(&claim), index))
                        }
                        SelectBranch::Task(task) => {
                            if let Some(reg) = task.register_await(id) {
                                tasks.push(reg);
                            }
                        }
                        SelectBranch::Timeout(_) => {}
                    }
                }
                let timeout = timeout.map(|(branch, deadline)| {
                    (branch, self.timers.register(deadline, TimerEvent::Wake(id)))
                });
                Wait::Select {
                    claim,
                    registrations: SelectRegistrations {
                        _queues: queues,
                        _senders: senders,
                        _tasks: tasks,
                        timeout,
                    },
                    select,
                    has_deadline,
                }
            }
            SuspendReason::WaitGroup { group, unwinding } => Wait::Group { group, unwinding },
            SuspendReason::WaitEvent(wait_event) => Wait::Event {
                registration: wait_event.event.register_wait(id),
                wait_event,
            },
            SuspendReason::AcquireSemaphore(acquire) => Wait::Semaphore {
                registration: acquire.semaphore.register_acquire(id),
                acquire,
            },
            SuspendReason::ChannelSend(send) => Wait::ChannelSend {
                registration: send.channel.register_send(runtime, id, send.value()),
                send,
            },
            SuspendReason::ChannelRecv(recv) => Wait::ChannelRecv {
                registration: recv.channel.register_recv(id),
                recv,
            },
            SuspendReason::Host(operation) => {
                let events = self
                    .host_events
                    .get_or_insert_with(|| HostEvents::new(runtime.execution_wake()));
                ready = !operation.register(id, events);
                self.host_waits += 1;
                Wait::Host(operation)
            }
        };
        let cancelled_before_park = !ready && entry.exec.control.can_deliver_task_cancel();
        entry.state = TaskState::Waiting { wait, resume, at };
        self.put_task(id, entry);
        if ready {
            self.wake_task(id, Wake::Ready);
        } else if cancelled_before_park {
            // A synchronous provider or clock callback can request cancellation
            // after the last checkpoint and before this wait was registered.
            self.wake_for_cancellation(id);
        }
    }

    fn poll_host_events(&mut self) {
        if let Some(events) = self
            .host_events
            .as_ref()
            .filter(|events| events.is_ready())
            .cloned()
        {
            events.drain(|notification| {
                if self.task(notification.task).is_some_and(|entry| {
                    matches!(&entry.state, TaskState::Waiting { wait: Wait::Host(operation), .. }
                        if operation.matches(&notification))
                }) {
                    self.wake_task(notification.task, Wake::Ready);
                }
            });
        }
    }

    fn finish_child(
        &mut self,
        handle: LiveHandle,
        member: Membership,
        failure: Option<TaskFailure>,
    ) {
        let id = handle.task.id;
        handle.task.wake_waiters(|waiter| {
            self.wake_task(waiter, Wake::TaskFinished(&handle));
        });
        self.groups.retire_owner(id);
        let failure = if member.allow_failure { None } else { failure };
        let (owner, empty, first_failure) = self.groups.finish_child(&member.group, id, failure);
        if first_failure {
            self.close_group(&member.group, true);
            self.wake_for_cancellation(owner);
        }
        if empty
            && self.task(owner).is_some_and(|entry| {
                matches!(&entry.state, TaskState::Waiting { wait: Wait::Group { group, .. }, .. }
                if Rc::ptr_eq(group, &member.group))
            })
        {
            self.wake_task(owner, Wake::Ready);
        }
    }

    fn observe_host_cancel(&mut self) {
        if !self.host_cancelled {
            self.host_cancelled = true;
            self.request_cancellation(ROOT_TASK);
        }
    }

    fn check_host_stop(&mut self, request: u8) -> Result<(), EvalError> {
        match request {
            HOST_ABORT => Err(EvalError::execution_stopped(0, ExecutionStop::Aborted)),
            HOST_CANCEL => {
                self.observe_host_cancel();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn fail_timers(&mut self, error: &EvalError) {
        let message: Rc<str> = Rc::from(error.message());
        while let Some(event) = self.timers.pop_due(u64::MAX) {
            match event {
                TimerEvent::Wake(id) => self.wake_task(id, Wake::ClockFault(&message)),
                TimerEvent::Timeout(id, signal) => {
                    signal.fail_timer(Rc::clone(&message));
                    self.wake_for_cancellation(id);
                }
            }
        }
    }

    fn poll_timers(&mut self, clock: Result<u64, EvalError>) -> Option<Duration> {
        let now = match clock {
            Ok(now) => now,
            Err(error) => {
                self.fail_timers(&error);
                return None;
            }
        };
        while let Some(event) = self.timers.pop_due(now) {
            match event {
                TimerEvent::Wake(task_id) => self.wake_task(task_id, Wake::Ready),
                TimerEvent::Timeout(task_id, signal) => {
                    signal.trigger();
                    self.wake_for_cancellation(task_id);
                }
            }
        }
        self.timers
            .next_deadline()
            .map(|next| Duration::from_nanos(next - now))
    }
}

struct WaitDependencies<'a, 'rt> {
    root: Option<&'a TaskSlot<'rt>>,
    children: &'a FxHashMap<TaskId, TaskSlot<'rt>>,
    groups: &'a ActiveGroups,
    active_id: TaskId,
    shields_cancel: bool,
}

impl<'a, 'rt> WaitDependencies<'a, 'rt> {
    fn task(&self, id: TaskId) -> Option<&'a TaskSlot<'rt>> {
        if id == ROOT_TASK {
            self.root
        } else {
            self.children.get(&id)
        }
    }

    fn ready_leaf(&self, id: TaskId) -> bool {
        id != self.active_id
            && self
                .task(id)
                .is_some_and(|entry| matches!(entry.state, TaskState::Ready(_)))
            && self.groups.of(id).is_empty()
    }

    fn describe(&self, dep: Dependency, add: &mut dyn FnMut(Dependency, Requirement)) -> bool {
        let id = dep.task;
        let (cancelled, wait) = if id == self.active_id {
            let cancelled = dep.cancel_before_join && !self.shields_cancel;
            if !cancelled {
                return false;
            }
            (cancelled, None)
        } else {
            let entry = self.task(id).expect("live wait dependency");
            let cancelled =
                dep.cancel_before_join && !entry.exec.control.defers_host_cancellation();
            let wait = if !cancelled {
                match &entry.state {
                    TaskState::Waiting { wait, .. } => Some(wait),
                    _ => None,
                }
            } else {
                None
            };
            (cancelled, wait)
        };
        match wait {
            Some(Wait::Await {
                registration,
                has_deadline: false,
                ..
            }) => add(
                Dependency::result(registration.task.id),
                Requirement::Required,
            ),
            Some(Wait::AwaitAny {
                registrations,
                has_deadline: false,
                ..
            }) => {
                for registration in registrations {
                    add(
                        Dependency::result(registration.task.id),
                        Requirement::Alternative,
                    );
                }
            }
            Some(Wait::Select {
                select,
                has_deadline: false,
                ..
            }) if select
                .branches
                .iter()
                .all(|branch| matches!(branch, SelectBranch::Task(_))) =>
            {
                for branch in &select.branches {
                    if let SelectBranch::Task(task) = branch {
                        add(Dependency::result(task.id), Requirement::Alternative);
                    }
                }
            }
            _ => {}
        }
        // An unshielded cancellation can release the current await. The task
        // must still cancel and join its own children before its cleanup ends.
        for group in self.groups.of(id) {
            for (task, cancel_before_join) in group.borrow().joins(cancelled) {
                add(
                    Dependency {
                        task,
                        cancel_before_join,
                    },
                    Requirement::Required,
                );
            }
        }
        true
    }
}

fn scheduler<'rt>(runtime: &'rt Runtime) -> Option<&'rt RefCell<Scheduler<'rt>>> {
    let ptr = runtime.active_scheduler.get()?;
    // SAFETY: drive borrows the RefCell at a stable address and restores the
    // runtime pointer before that borrow ends. Callers only use it during drive.
    Some(unsafe { ptr.cast::<RefCell<Scheduler<'rt>>>().as_ref() })
}

pub(super) fn with_scheduler<'rt, R>(
    runtime: &'rt Runtime,
    f: impl FnOnce(&mut Scheduler<'rt>) -> R,
) -> Option<R> {
    Some(f(&mut scheduler(runtime)?.borrow_mut()))
}

pub(super) fn wake_waiting_task(runtime: &Runtime, id: TaskId) {
    with_scheduler(runtime, |scheduler| scheduler.wake_task(id, Wake::Ready));
}

pub(super) fn current_task(runtime: &Runtime, at: usize) -> Result<Word, Fault> {
    let active = with_scheduler(runtime, |s| {
        let active = s.active.as_ref()?;
        Some(match &active.handle {
            Some(handle) => Err(handle.word.get()),
            None => Ok((active.id, Rc::clone(&active.cancellation))),
        })
    })
    .flatten();
    let (id, cancellation) = match active.ok_or_else(|| {
        super::control_error(at, "CURRENT-TASK requires an active VM execution context")
    })? {
        Ok(active) => active,
        Err(word) => return Ok(word),
    };
    let allocated = Task::allocate_handle(runtime, id, Some("main".to_owned()), cancellation);
    with_scheduler(runtime, |s| {
        let active = s.active.as_mut().expect("active task");
        // A foreign destructor may have installed this task's handle already.
        // Only an installed handle has the obligation to mark the task terminal.
        if let Some(handle) = &active.handle {
            return Ok(handle.word.get());
        }
        let (word, task) = allocated.map_err(|error| EvalError::out_of_memory(at, error))?;
        let value = word.get();
        active.handle = Some(LiveHandle { word, task });
        Ok(value)
    })
    .expect("active task")
}

pub(super) fn spawn<'rt>(
    runtime: &'rt Runtime,
    parent: &ExecState<'rt>,
    spawn: Spawn,
) -> Result<Option<Word>, OutOfMemory> {
    let Spawn {
        group,
        target,
        name,
        no_wait,
        allow_failure,
    } = spawn;
    let id = with_scheduler(runtime, |s| {
        let id = s.next_id;
        s.next_id += 1;
        id
    })
    .expect("active task");
    let mut exec = ExecState::new(Values::new(runtime));
    let (word, task) =
        Task::allocate_handle(runtime, id, name, exec.control.cancellation_handle())?;
    let value = word.get();
    let mut specials = parent.control.snapshot_active_specials(&runtime.heap);
    // A new task starts outside its parent's logical blocks while inheriting print controls.
    if let Some(depth) = crate::vm::format::pprint::logical_block_depth_symbol(runtime) {
        specials.retain(|&(symbol, _)| symbol != depth);
    }
    exec.control
        .install_suspended_specials(&runtime.heap, specials);
    Ok(with_scheduler(runtime, |s| {
        if !s.groups.try_insert_child(&group, id, no_wait) {
            return None;
        }
        let entry = TaskSlot {
            exec,
            printer: PrinterTaskState::default(),
            class_updates: TaskClassUpdates::default(),
            handle: Some(LiveHandle { word, task }),
            membership: Some(Membership {
                group,
                allow_failure,
            }),
            state: TaskState::Ready(Some(PendingContinuation(Pending::StartCall {
                args: target,
                resume: Resume::Call(None),
                at: 0,
            }))),
        };
        s.children.insert(id, entry);
        s.ready.push_back(id);
        Some(value)
    })
    .expect("active task"))
}

/// Latch host cancellation for the root and report whether it is active.
pub(crate) fn observe_host_cancel(runtime: &Runtime) -> Option<bool> {
    with_scheduler(runtime, |scheduler| {
        scheduler.observe_host_cancel();
        !scheduler.is_child_task()
    })
}

pub(crate) fn host_cancel_is_shielded(runtime: &Runtime) -> bool {
    with_scheduler(runtime, |s| s.host_cancel_shield).unwrap_or(false)
}

pub(crate) fn refill_native_slice(runtime: &Runtime, exec: &ExecState<'_>) -> bool {
    crate::metrics::record_native_checkpoint();
    if runtime.evaluation_active.get()
        || runtime.host_cancel_request() != crate::execution::HOST_RUNNING
        || exec.control.can_deliver_task_cancel()
    {
        return false;
    }
    // A host clock may reenter Lisp, mutate bindings and collect. The VM
    // handles clocks after all native activations have returned their continuations.
    let idle = with_scheduler(runtime, |state| {
        !state.has_ready_tasks()
            && state.timers.next_deadline().is_none()
            && state.host_cancellations.is_empty()
            && state
                .host_events
                .as_ref()
                .is_none_or(|events| !events.is_ready())
    })
    .expect("a native execution slice belongs to the scheduler");
    if idle {
        runtime
            .native_slice_steps
            .set(dispatch::CHECKPOINT_QUANTUM - 1);
    }
    idle
}

pub(in crate::vm) fn set_host_cancel_shield(runtime: &Runtime, shielded: bool) -> bool {
    with_scheduler(runtime, |s| {
        std::mem::replace(&mut s.host_cancel_shield, shielded)
    })
    .unwrap_or(false)
}

pub(in crate::vm) fn poll_ready(runtime: &Runtime) -> bool {
    let Some(scheduler) = scheduler(runtime) else {
        return false;
    };
    let state = scheduler.borrow();
    let has_timers = state.timers.next_deadline().is_some();
    if !has_timers
        && state.host_cancellations.is_empty()
        && state
            .host_events
            .as_ref()
            .is_none_or(|events| !events.is_ready())
    {
        return state.has_ready_tasks();
    }
    drop(state);
    if has_timers {
        // Host clock implementations can call back into the runtime.
        let clock = now_nanos(runtime, 0);
        scheduler.borrow_mut().poll_timers(clock);
    }
    notify_host_cancellations(runtime, scheduler);
    let mut state = scheduler.borrow_mut();
    state.poll_host_events();
    state.has_ready_tasks()
}

fn notify_host_cancellations(runtime: &Runtime, scheduler: &RefCell<Scheduler<'_>>) -> bool {
    let first = scheduler.borrow_mut().host_cancellations.pop_front();
    let Some(mut waker) = first else { return false };
    let _pin = runtime.pin_rust_frame();
    loop {
        // A provider can acknowledge the request or reenter Lisp here.
        waker.wake();
        let next = scheduler.borrow_mut().host_cancellations.pop_front();
        let Some(next) = next else { return true };
        waker = next;
    }
}

pub(super) fn now_nanos(runtime: &Runtime, at: usize) -> Result<u64, EvalError> {
    let _pin = runtime.pin_rust_frame();
    runtime
        .host_services
        .monotonic_nanos()
        .map_err(|error| EvalError::io_error(at, error))
}

pub(in crate::vm) fn execute<'rt>(
    runtime: &'rt Runtime,
    values: Values,
    initial: Invocation<'rt>,
) -> Result<Values, EvalError> {
    let mut scheduled = TaskExecutor::new(values, initial);
    loop {
        match scheduled.drive(runtime, false)? {
            SchedulerStep::Complete(values) => return Ok(values),
            SchedulerStep::Runnable => unreachable!("blocking driver drains ready tasks"),
            SchedulerStep::Paused(_) => unreachable!("host debugger events are opt-in"),
            SchedulerStep::Parked(wait) => {
                if let Some(generation) = wait.host_generation {
                    runtime.execution_wake().wait(generation, wait.delay)?;
                } else if let Err(error) = park(runtime, wait.delay.unwrap(), wait.host_cancelled) {
                    scheduled.state.get_mut().fail_timers(&error);
                }
            }
        }
    }
}

pub(crate) struct TaskExecutor<'rt> {
    state: RefCell<Scheduler<'rt>>,
    initial: Option<Invocation<'rt>>,
    paused: RefCell<Option<(TaskId, PausedEvent)>>,
}

pub(crate) enum SchedulerStep<'rt> {
    Complete(Values),
    Runnable,
    Parked(ParkTarget),
    Paused(DebugEvent<'rt>),
}

pub(crate) struct ParkTarget {
    pub(crate) delay: Option<Duration>,
    host_cancelled: bool,
    host_generation: Option<u64>,
}

impl<'rt> TaskExecutor<'rt> {
    pub(in crate::vm) fn new(values: Values, initial: Invocation<'rt>) -> Self {
        Self {
            state: RefCell::new(Scheduler::new(values)),
            initial: Some(initial),
            paused: RefCell::new(None),
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.borrow().is_some()
    }

    pub(crate) fn with_debugger<T>(
        &mut self,
        runtime: &'rt Runtime,
        operation: impl FnOnce(&'rt Runtime) -> T,
    ) -> Option<T> {
        let id = self.paused.get_mut().as_ref().expect("paused evaluation").0;
        let _reset = enter_scheduler(runtime, &self.state);
        let mut entry = {
            let mut state = self.state.borrow_mut();
            let mut entry = state.take_task(id);
            state.active = Some(ActiveTask {
                id,
                cancellation: entry.exec.control.cancellation_handle(),
                handle: entry.handle.take(),
            });
            entry
        };
        let outcome = {
            let previous = runtime.child_task_running.replace(id != ROOT_TASK);
            let _restore = DropGuard::new(previous, |p| runtime.child_task_running.set(p));
            let _printer = entry.printer.activate();
            let _class_updates = entry.class_updates.activate(runtime);
            entry.exec.control.resume_task(runtime);
            let outcome =
                callback::with_debug_calls(runtime, &mut entry.exec, || operation(runtime));
            entry.exec.control.suspend_task(runtime);
            outcome
        };
        let (resume, pending, value) = match outcome {
            Outcome::Completed(value) => {
                let resume = entry
                    .exec
                    .control
                    .debug_pause()
                    .is_some_and(|pause| pause.resume_requested);
                let pending = resume.then(|| Pending::Signal(entry.exec.control.resume_signal()));
                (resume, pending, Some(value))
            }
            Outcome::Unwind(target) => {
                debug_assert!(matches!(target, super::super::UnwindTarget::Bytecode(_)));
                let pending = entry
                    .exec
                    .control
                    .take_resumed_restart()
                    .map(|continuation| Pending::Continuation {
                        continuation,
                        returned: false,
                    });
                (true, pending, None)
            }
            Outcome::Unhandled(error) => (true, Some(Pending::DebuggerFailure(error)), None),
        };
        let mut state = self.state.borrow_mut();
        entry.handle = state.active.take().unwrap().handle;
        entry.state = TaskState::Ready(pending.map(PendingContinuation));
        state.put_task(id, entry);
        if resume {
            state.ready.push_front(id);
            *self.paused.get_mut() = None;
        }
        value
    }

    pub(crate) fn drive(
        &mut self,
        runtime: &'rt Runtime,
        single_slice: bool,
    ) -> Result<SchedulerStep<'rt>, EvalError> {
        if let Some((id, event)) = self.paused.get_mut().as_ref() {
            if !runtime.cancellation.is_cancelled() {
                return Ok(SchedulerStep::Paused(event.event(runtime)));
            }
            let mut state = self.state.borrow_mut();
            let entry = state.task_mut(*id).expect("paused task");
            let search = entry.exec.control.resume_signal();
            entry.state = TaskState::Ready(Some(PendingContinuation(Pending::Signal(search))));
            state.ready.push_front(*id);
            *self.paused.get_mut() = None;
        }
        let root_profile_task = runtime.profile_current_task().unwrap_or((0, 0));
        let _profile_idle = runtime.profile_scheduler_scope();
        let _reset = enter_scheduler(runtime, &self.state);
        let _notify = DropGuard::new(&self.state, |state| {
            // A hard stop can arrive after a soft request queued a provider
            // notification. Deliver it before removing the scheduler context.
            if !std::thread::panicking() {
                notify_host_cancellations(runtime, state);
            }
        });
        let mut ran_slice = false;
        loop {
            notify_host_cancellations(runtime, &self.state);
            let initial = self.initial.take();
            let id = if initial.is_some() {
                ROOT_TASK
            } else {
                if single_slice && ran_slice && self.state.borrow().has_ready_tasks() {
                    return Ok(SchedulerStep::Runnable);
                }
                let wait = poll_waits(runtime, &self.state)?;
                if notify_host_cancellations(runtime, &self.state) {
                    continue;
                }
                match wait {
                    Some(wait) => return Ok(SchedulerStep::Parked(wait)),
                    None if single_slice && ran_slice => return Ok(SchedulerStep::Runnable),
                    None => self
                        .state
                        .borrow_mut()
                        .ready
                        .pop_front()
                        .expect("runnable task"),
                }
            };
            if let Some(outcome) = self.run_task(runtime, id, initial, root_profile_task)? {
                return Ok(outcome);
            }
            ran_slice = true;
        }
    }

    fn run_task(
        &self,
        runtime: &'rt Runtime,
        id: TaskId,
        initial: Option<Invocation<'rt>>,
        root_profile_task: (usize, u64),
    ) -> Result<Option<SchedulerStep<'rt>>, EvalError> {
        let task = if id == ROOT_TASK {
            root_profile_task
        } else {
            (std::ptr::from_ref(&self.state) as usize, id)
        };
        let _profile_task = runtime.profile_task_scope(Some(task));
        let mut entry = {
            let mut state = self.state.borrow_mut();
            let mut entry = state.take_task(id);
            state.active = Some(ActiveTask {
                id,
                cancellation: entry.exec.control.cancellation_handle(),
                handle: entry.handle.take(),
            });
            entry
        };
        let outcome = {
            let previous = runtime.child_task_running.replace(id != ROOT_TASK);
            let _restore = DropGuard::new(previous, |previous| {
                runtime.child_task_running.set(previous)
            });
            entry.exec.control.resume_task(runtime);
            entry.exec.host_debugging = runtime.evaluation_debugging.get();
            runtime.native_slice_steps.set(dispatch::CHECKPOINT_QUANTUM);
            step(runtime, &mut entry, initial)
        };
        let mut s = self.state.borrow_mut();
        // After a task leaves the dispatcher, cancellation must go through
        // its waiting entry so that it can claim the wakeup registration.
        let active = s.active.take().expect("task was dispatched");
        match outcome {
            dispatch::TaskStepOutcome::Completed | dispatch::TaskStepOutcome::Failed(_) => {
                let result = match outcome {
                    dispatch::TaskStepOutcome::Failed(error) => Err(error),
                    _ => Ok(()),
                };
                if result.as_ref().is_err_and(|error| {
                    matches!(
                        error.execution_stop(),
                        Some(ExecutionStop::Aborted | ExecutionStop::BudgetExhausted)
                    )
                }) {
                    return Err(result.unwrap_err());
                }
                let mut failure = None;
                let handle = active.handle;
                if let Some(handle) = &handle {
                    match result {
                        Ok(()) => handle
                            .complete((0..entry.exec.values.len).map(|i| entry.exec.values.get(i))),
                        Err(ref error) => {
                            failure =
                                handle.fail(entry.exec.control.take_unhandled_condition(), error);
                        }
                    }
                }
                if id == ROOT_TASK {
                    debug_assert!(s.children.is_empty());
                    return result.map(|()| Some(SchedulerStep::Complete(entry.exec.values)));
                }
                let member = entry.membership.take().expect("child membership");
                drop(entry);
                s.finish_child(handle.expect("child handle"), member, failure);
            }
            dispatch::TaskStepOutcome::QuantumYield(pending) => {
                entry.exec.control.suspend_task(runtime);
                entry.state = TaskState::Ready(pending);
                entry.handle = active.handle;
                s.put_task(id, entry);
                s.ready.push_back(id);
            }
            dispatch::TaskStepOutcome::Suspend(suspension) => {
                entry.exec.control.suspend_task(runtime);
                entry.handle = active.handle;
                s.suspend(runtime, id, entry, suspension);
            }
            dispatch::TaskStepOutcome::Paused(paused) => {
                entry.exec.control.suspend_task(runtime);
                entry.state = TaskState::Ready(None);
                entry.handle = active.handle;
                s.put_task(id, entry);
                let event = paused.event(runtime);
                self.paused.replace(Some((id, paused)));
                return Ok(Some(SchedulerStep::Paused(event)));
            }
        }
        Ok(None)
    }
}

fn enter_scheduler<'a>(
    runtime: &'a Runtime,
    state: &'a RefCell<Scheduler<'_>>,
) -> impl Drop + use<'a> {
    let previous = (
        runtime
            .active_scheduler
            .replace(Some(NonNull::from(state).cast())),
        runtime.cleanup_depth.replace(0),
    );
    DropGuard::new(previous, |(scheduler, cleanup_depth)| {
        runtime.active_scheduler.set(scheduler);
        runtime.cleanup_depth.set(cleanup_depth);
    })
}

fn step<'rt>(
    runtime: &'rt Runtime,
    entry: &mut TaskSlot<'rt>,
    initial: Option<Invocation<'rt>>,
) -> dispatch::TaskStepOutcome<'rt> {
    let _printer_scopes = entry.printer.activate();
    let _class_updates = entry.class_updates.activate(runtime);
    let TaskState::Ready(pending) = &mut entry.state else {
        unreachable!("scheduler only runs ready tasks");
    };
    let pending = pending.take();
    let exec = &mut entry.exec;
    let pending = if let Some(initial) = initial {
        match accept(runtime, exec, initial) {
            Ok(StepControl::Pending(next)) => next,
            Ok(StepControl::Unwind(_)) => None,
            Ok(StepControl::QuantumYield | StepControl::Suspend { .. }) => {
                unreachable!("invocation setup cannot suspend")
            }
            Err(error) => return dispatch::TaskStepOutcome::Failed(error),
        }
    } else {
        pending
    };
    dispatch::execute_task_step(runtime, exec, pending)
}

fn poll_waits(
    runtime: &Runtime,
    scheduler: &RefCell<Scheduler<'_>>,
) -> Result<Option<ParkTarget>, EvalError> {
    let mut state = scheduler.borrow_mut();
    let host_generation = (state.host_waits != 0).then(|| runtime.execution_wake().generation());
    state.check_host_stop(runtime.host_cancel_request())?;
    let delay = if state.timers.next_deadline().is_some() {
        drop(state);
        let clock = now_nanos(runtime, 0);
        state = scheduler.borrow_mut();
        state.check_host_stop(runtime.host_cancel_request())?;
        state.poll_timers(clock)
    } else {
        None
    };
    state.poll_host_events();
    if state.has_ready_tasks() {
        return Ok(None);
    }
    if delay.is_none() && host_generation.is_none() {
        state.fail_deadlocked_wait()?;
        return Ok(None);
    }
    Ok(Some(ParkTarget {
        delay,
        host_cancelled: state.host_cancelled,
        host_generation,
    }))
}

fn park(runtime: &Runtime, duration: Duration, host_cancelled: bool) -> Result<(), EvalError> {
    if !runtime.installing_builtins.get()
        && runtime.cancellation.park_timeout(duration, host_cancelled)
    {
        return Ok(());
    }
    #[cfg(all(target_family = "wasm", any(target_os = "unknown", target_os = "none")))]
    return runtime
        .host_services
        .park(duration)
        .map_err(|error| EvalError::io_error(0, error));
    #[cfg(not(all(target_family = "wasm", any(target_os = "unknown", target_os = "none"))))]
    crate::platform::sleep_host(&*runtime.host_services, duration, 0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::mem::DropGuard;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::{Rc, Weak};

    use crate::{HostServices, Runtime};

    fn evaluate_on_foreign_destruction(runtime: &Rc<Runtime>, form: &'static str) {
        struct EvaluateOnDrop(ThreadCheckedWeakRuntime, &'static str);

        struct ThreadCheckedWeakRuntime {
            runtime: std::rc::Weak<Runtime>,
            thread: std::thread::ThreadId,
        }

        // SAFETY: accessed only after asserting thread identity matches the owning test thread.
        unsafe impl Send for ThreadCheckedWeakRuntime {}
        // SAFETY: accessed only after asserting thread identity matches the owning test thread.
        unsafe impl Sync for ThreadCheckedWeakRuntime {}

        impl Clone for ThreadCheckedWeakRuntime {
            fn clone(&self) -> Self {
                assert_eq!(std::thread::current().id(), self.thread);
                Self {
                    runtime: self.runtime.clone(),
                    thread: self.thread,
                }
            }
        }

        impl Drop for EvaluateOnDrop {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.0.thread);
                if let Some(rt) = self.0.runtime.upgrade() {
                    rt.eval(self.1).unwrap();
                }
            }
        }

        let weak = ThreadCheckedWeakRuntime {
            runtime: Rc::downgrade(runtime),
            thread: std::thread::current().id(),
        };
        runtime
            .register_function("retire-foreign-observer", move |rt, _| {
                drop(rt.foreign(EvaluateOnDrop(weak.clone(), form))?);
                // Leave the Rust destructor for the next foreign allocation.
                rt.heap.collect();
                Ok(Vec::new())
            })
            .unwrap();
    }

    #[test]
    fn polling_timers_releases_the_scheduler_before_reading_the_host_clock() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        struct Clock {
            runtime: std::sync::Mutex<(std::rc::Rc<RefCell<std::rc::Weak<Runtime>>>, std::thread::ThreadId)>,
            now: AtomicU64,
            reading: AtomicBool,
        }

        // SAFETY: `runtime` inside `Clock` asserts thread identity before borrowing on test thread.
        unsafe impl Send for Clock {}
        // SAFETY: `runtime` inside `Clock` asserts thread identity before borrowing on test thread.
        unsafe impl Sync for Clock {}

        impl HostServices for Clock {
            fn wall_time_secs(&self) -> std::io::Result<i64> {
                Ok(0)
            }

            fn monotonic_nanos(&self) -> std::io::Result<u64> {
                assert!(
                    !self.reading.swap(true, Ordering::Relaxed),
                    "scheduler reentered the host clock"
                );
                let _reading = DropGuard::new(&self.reading, |reading| {
                    reading.store(false, Ordering::Relaxed);
                });
                let rt = {
                    let guard = self.runtime.lock().unwrap();
                    assert_eq!(std::thread::current().id(), guard.1);
                    guard.0.borrow().upgrade()
                };
                if let Some(rt) = rt {
                    let values = rt
                        .eval("(progn (zk-async:yield) (zk-async:task-cancelled-p))")
                        .unwrap();
                    assert!(values[0].is_nil());
                }
                let next = self.now.fetch_add(1_000_000_000, Ordering::Relaxed) + 1_000_000_000;
                Ok(next)
            }

            fn fill_entropy(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                bytes.fill(0);
                Ok(())
            }
        }

        let mut runtime = Runtime::start().unwrap();
        let weak = Rc::new(RefCell::new(Weak::new()));
        runtime.set_host_services(Clock {
            runtime: std::sync::Mutex::new((Rc::clone(&weak), std::thread::current().id())),
            now: AtomicU64::new(0),
            reading: AtomicBool::new(false),
        });
        let runtime = Rc::new(runtime);
        *weak.borrow_mut() = Rc::downgrade(&runtime);
        let result = runtime
            .eval("(zk-async:with-timeout (1 :on-timeout t) (loop (zk-async:yield)))")
            .unwrap();
        assert!(result[0].is_t());
    }

    #[test]
    fn foreign_destruction_preserves_the_current_task_identity() {
        let runtime = Rc::new(Runtime::start().unwrap());
        runtime.load_str("(defvar *task-from-drop* nil)").unwrap();
        evaluate_on_foreign_destruction(
            &runtime,
            "(setq *task-from-drop* (zk-async:current-task))",
        );
        let values = runtime
            .eval(
                "(progn
                   (retire-foreign-observer)
                   (let ((task (zk-async:current-task)))
                     (and (eq task *task-from-drop*)
                          (not (zk-async:task-cancelled-p task)))))",
            )
            .unwrap();
        assert!(values[0].is_t());
    }

    #[test]
    fn a_reentrantly_closed_group_does_not_admit_a_child() {
        let runtime = Rc::new(Runtime::start().unwrap());
        runtime.load_str("(defvar *group-to-close* nil)").unwrap();
        evaluate_on_foreign_destruction(&runtime, "(zk::%finish-task-group *group-to-close* nil)");
        let values = runtime
            .eval(
                "(let ((ran nil))
                   (and (eq (zk-async:with-task-group (group)
                              (let ((*group-to-close* group))
                                (retire-foreign-observer)
                                (handler-case
                                    (zk-async:spawn-task (group) (setq ran t))
                                  (zk-async:async-error () :closed))))
                            :closed)
                        (not ran)))",
            )
            .unwrap();
        assert!(values[0].is_t());
    }

    #[test]
    fn overlapping_task_frames_keep_the_main_root_stack_bounded() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runtime = Runtime::start().unwrap();
        let peak = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&peak);
        runtime
            .register_function("observe-root-top", move |rt, _| {
                observed.fetch_max(rt.root_stack.top(), Ordering::Relaxed);
                Ok(Vec::new())
            })
            .unwrap();
        runtime
            .load_str(
                "(defun yield-with-argument (&optional (payload (list :payload)))
                   (observe-root-top)
                   (zk-async:yield)
                   (assert (equal payload '(:payload))))",
            )
            .unwrap();
        let example = "(zk-async:with-task-group (group)
                         (dotimes (j 2)
                           (zk-async:spawn-task (group)
                             (dotimes (i 2500) (yield-with-argument (list :payload))))))";
        runtime.eval(example).unwrap();
        let peak = peak.load(Ordering::Relaxed);
        assert!(
            peak > 0 && peak < 4096,
            "root stack peak {peak}"
        );
        assert_eq!(runtime.root_stack.top(), 0);
        assert!(!runtime.child_task_running.get());
    }

    #[test]
    fn unwinding_a_rust_callback_restores_the_scheduler_context() {
        let runtime = Runtime::start().unwrap();
        runtime
            .load_str("(defparameter *task-binding* :global)")
            .unwrap();
        runtime
            .register_function("panic-in-cleanup", |rt, _| {
                assert!(rt.child_task_running.get());
                assert_ne!(rt.cleanup_depth.get(), 0);
                panic!("panic from the active child task")
            })
            .unwrap();

        let result = catch_unwind(AssertUnwindSafe(|| {
            runtime.eval(
                "(let ((*task-binding* :parent))
                   (zk-async:with-task-group (group)
                     (zk-async:spawn-task (group)
                       (let ((*task-binding* :child))
                         (unwind-protect nil (panic-in-cleanup))))))",
            )
        }));
        let panic = result.err().expect("the child callback panicked");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"panic from the active child task")
        );
        assert!(runtime.active_scheduler.get().is_none());
        assert!(!runtime.child_task_running.get());
        assert_eq!(runtime.cleanup_depth.get(), 0);
        assert_eq!(runtime.root_stack.top(), 0);
        let values = runtime
            .eval("(and (eq *task-binding* :global) (zk-async:taskp (zk-async:current-task)))")
            .unwrap();
        assert!(values[0].is_t());
    }
}
