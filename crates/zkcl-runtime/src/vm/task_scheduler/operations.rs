use std::rc::Rc;
use std::time::Duration;

use super::super::{
    ExecState, Invocation, Primitive, Resume, StepControl, accept, dispatch_error, invoke,
};
use super::groups::TaskGroup;
use super::handles::{Task, TaskFailure, TaskStatus};
use super::sync::{Channel, Event, Promise, PromiseStatus, SelectClaim, Semaphore};
use super::{
    async_error_call, can_suspend, deadline, has_concurrent_work, has_other_ready_tasks,
    poll_ready, require_suspendable, resolve_async_signal_cell, scheduler, with_scheduler,
};
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, Word};
use crate::host::{HostAsyncFunction, HostOperation, HostReply};
use crate::{EvalError, Runtime};

pub(in crate::vm) struct TaskCall {
    operation: Operation,
    resume: Resume,
    at: usize,
}

pub(super) enum Operation {
    Spawn(Spawn),
    Await(Await),
    AwaitAny(AwaitAny),
    AwaitAnyFinished(RootSlot),
    AwaitPromise(AwaitPromise),
    Select(Select),
    SelectFinished(SelectFinished),
    FinishGroup {
        group: TaskGroup,
        unwinding: bool,
    },
    Sleep(Duration),
    Yield,
    WaitEvent(WaitEvent),
    AcquireSemaphore(AcquireSemaphore),
    ChannelSend(ChannelSend),
    ChannelRecv(ChannelRecv),
    ReadFileOctets(RootFrame),
    StartHost {
        start: std::sync::Arc<HostAsyncFunction>,
        args: RootFrame,
    },
    ResumeHost(HostOperation),
    CurrentTask,
    ReturnNil,
    Error(&'static str),
}

pub(super) struct Spawn {
    pub(super) group: TaskGroup,
    pub(super) target: RootFrame,
    pub(super) no_wait: bool,
    pub(super) allow_failure: bool,
    pub(super) name: Option<String>,
}

pub(super) struct Await {
    pub(super) task: Rc<Task>,
    pub(super) task_word: RootFrame,
}

pub(super) struct AwaitAny {
    pub(super) tasks: Vec<Rc<Task>>,
    pub(super) task_words: RootFrame,
}

pub(super) struct AwaitPromise {
    pub(super) promise: Rc<Promise>,
    pub(super) _owner: RootFrame,
}

pub(super) struct WaitEvent {
    pub(super) event: Rc<Event>,
    pub(super) woken: bool,
}

pub(super) struct AcquireSemaphore {
    pub(super) semaphore: Rc<Semaphore>,
    pub(super) ownership: Option<RootSlot>,
    pub(super) acquired: bool,
}

pub(super) struct ChannelSend {
    pub(super) channel: Rc<Channel>,
    pub(super) arguments: RootFrame,
    pub(super) delivered: bool,
}

pub(super) struct ChannelRecv {
    pub(super) channel: Rc<Channel>,
    pub(super) _owner: RootFrame,
    pub(super) received: Option<RootFrame>,
}

pub(super) enum SelectBranch {
    Recv(Rc<Channel>),
    Send(Rc<Channel>),
    Event(Rc<Event>),
    Task(Rc<Task>),
    Promise(Rc<Promise>),
    Timeout(Duration),
}

pub(super) struct Select {
    pub(super) branches: Vec<SelectBranch>,
    pub(super) slots: RootFrame,
    pub(super) has_default: bool,
    pub(super) biased: bool,
}

impl Select {
    pub(super) fn branch_first(&self, index: usize) -> Word {
        self.slots.get(index * 2)
    }

    pub(super) fn branch_second(&self, index: usize) -> Word {
        self.slots.get(index * 2 + 1)
    }
}

pub(super) struct SelectFinished {
    pub(super) select: Select,
    pub(super) claim: Rc<SelectClaim>,
}

pub(crate) struct Suspension {
    pub(super) reason: SuspendReason,
    pub(super) resume: Resume,
    pub(super) at: usize,
}

pub(super) enum SuspendReason {
    Yield,
    Sleep(u64),
    Await {
        await_task: Await,
        has_deadline: bool,
    },
    AwaitAny {
        await_any: AwaitAny,
        has_deadline: bool,
    },
    AwaitPromise(AwaitPromise),
    Select {
        select: Select,
        timeout: Option<(usize, u64)>,
        has_deadline: bool,
    },
    WaitGroup {
        group: TaskGroup,
        unwinding: bool,
    },
    WaitEvent(WaitEvent),
    AcquireSemaphore(AcquireSemaphore),
    ChannelSend(ChannelSend),
    ChannelRecv(ChannelRecv),
    Host(HostOperation),
}

enum TaskStep {
    Value(Word),
    Values2(Word, Word),
    Values3(Word, Word, Word),
    KeepValues,
    Call { target: Word, args: RootFrame },
    Suspend(SuspendReason),
}

impl TaskCall {
    pub(super) fn new(operation: Operation, resume: Resume, at: usize) -> Self {
        Self {
            operation,
            resume,
            at,
        }
    }

    pub(in crate::vm) fn ignores_task_cancellation(&self) -> bool {
        match &self.operation {
            Operation::FinishGroup {
                unwinding: true, ..
            }
            | Operation::ChannelSend(ChannelSend {
                delivered: true, ..
            })
            | Operation::ChannelRecv(ChannelRecv {
                received: Some(_), ..
            })
            | Operation::AcquireSemaphore(AcquireSemaphore { acquired: true, .. }) => true,
            Operation::SelectFinished(done) => done.claim.has_committed_transfer(),
            _ => false,
        }
    }

    pub(in crate::vm) fn dispatch<'rt>(
        self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
    ) -> Result<StepControl<'rt>, EvalError> {
        // The VM polls before committing a task operation. Work that waits
        // carries its Lisp return location to the scheduler as data.
        let Self {
            operation,
            resume,
            at,
        } = self;
        let invocation = match operation.step(runtime, exec, at) {
            Ok(TaskStep::Value(word)) => {
                exec.values.one(word);
                Ok(Invocation::Done(resume))
            }
            Ok(TaskStep::Values2(first, second)) => {
                exec.values.resize(2);
                exec.values.slots.set(0, first);
                exec.values.slots.set(1, second);
                Ok(Invocation::Done(resume))
            }
            Ok(TaskStep::Values3(first, second, third)) => {
                exec.values.resize(3);
                exec.values.slots.set(0, first);
                exec.values.slots.set(1, second);
                exec.values.slots.set(2, third);
                Ok(Invocation::Done(resume))
            }
            Ok(TaskStep::KeepValues) => Ok(Invocation::Done(resume)),
            Ok(TaskStep::Call { target, args }) => invoke(
                runtime,
                target,
                &args,
                0..args.len(),
                &mut exec.values,
                resume,
                at,
            ),
            Ok(TaskStep::Suspend(reason)) => {
                return Ok(StepControl::Suspend {
                    suspension: Suspension { reason, resume, at },
                });
            }
            Err(fault) => Err(fault),
        };
        match invocation {
            Ok(invocation) => accept(runtime, exec, invocation),
            Err(fault) => dispatch_error(runtime, exec, fault),
        }
    }
}

impl TaskStep {
    fn error(runtime: &Runtime, message: &str, at: usize) -> Result<Self, Fault> {
        let (target, args) = async_error_call(runtime, message, at)?;
        Ok(Self::Call { target, args })
    }

    fn failure(runtime: &Runtime, failure: TaskFailure, at: usize) -> Result<Self, Fault> {
        match failure {
            TaskFailure::Condition(condition) => {
                let target = Primitive::named("ERROR").expect("ERROR primitive");
                let args = runtime.root_frame(1);
                args.set(0, condition.get());
                Ok(Self::Call { target, args })
            }
            TaskFailure::Message(message) => Self::error(runtime, &message, at),
        }
    }
}

fn ready_for_yield(runtime: &Runtime, suspendable: bool) -> bool {
    // A host clock may call Lisp YIELD while it is being polled.
    // Pinned callbacks inspect readiness without driving the scheduler.
    if suspendable {
        poll_ready(runtime)
    } else {
        has_other_ready_tasks(runtime)
    }
}

impl Operation {
    fn step(
        self,
        runtime: &Runtime,
        exec: &mut ExecState<'_>,
        at: usize,
    ) -> Result<TaskStep, Fault> {
        match self {
            Self::Spawn(spawn) => spawn.step(runtime, exec, at),
            Self::Await(await_task) => await_task.step(runtime, exec, at),
            Self::AwaitAny(await_any) => await_any.step(runtime, exec, at),
            Self::AwaitAnyFinished(result) => Ok(TaskStep::Value(result.get())),
            Self::AwaitPromise(await_promise) => await_promise.step(runtime, exec, at),
            Self::Select(select) => select.step(runtime, exec, at),
            Self::SelectFinished(done) => done.step(runtime, exec, at),
            Self::FinishGroup { group, unwinding } => {
                let drained = with_scheduler(runtime, |s| s.close_group(&group, unwinding))
                    .expect("group owner is scheduled");
                if drained {
                    if unwinding
                        && exec
                            .control
                            .unwinding_bypasses_group_scope(&group.borrow().boundary())
                        && let Some(failure) = group.borrow_mut().take_failure()
                    {
                        return TaskStep::failure(runtime, failure, at);
                    }
                    Ok(TaskStep::Value(Word::NIL))
                } else {
                    require_suspendable(runtime, exec, at, "wait for task group")?;
                    Ok(TaskStep::Suspend(SuspendReason::WaitGroup {
                        group,
                        unwinding,
                    }))
                }
            }
            Self::Sleep(duration) => {
                if duration.is_zero() {
                    let suspendable = can_suspend(runtime, exec);
                    if !ready_for_yield(runtime, suspendable) {
                        return Ok(TaskStep::Value(Word::NIL));
                    }
                    require_suspendable(runtime, exec, at, "suspend Lisp SLEEP")?;
                    return Ok(TaskStep::Suspend(SuspendReason::Yield));
                }
                // A bare Wasm host can wake the scheduler early. Keep even a
                // lone suspendable task on that path so an interrupted timer
                // reaches the cancellation checkpoint instead of sleeping again.
                let park_on_host = cfg!(all(
                    target_family = "wasm",
                    any(target_os = "unknown", target_os = "none")
                )) && can_suspend(runtime, exec);
                if !park_on_host
                    && !runtime.evaluation_active.get()
                    && !has_concurrent_work(runtime)
                {
                    runtime.sleep_for(duration, at)?;
                    return Ok(TaskStep::Value(Word::NIL));
                }
                require_suspendable(runtime, exec, at, "suspend Lisp SLEEP")?;
                Ok(TaskStep::Suspend(SuspendReason::Sleep(deadline(
                    runtime, duration, at,
                )?)))
            }
            Self::Yield => {
                let suspendable = can_suspend(runtime, exec);
                if !ready_for_yield(runtime, suspendable) {
                    Ok(TaskStep::Value(Word::NIL))
                } else {
                    require_suspendable(runtime, exec, at, "yield")?;
                    Ok(TaskStep::Suspend(SuspendReason::Yield))
                }
            }
            Self::WaitEvent(wait_event) => wait_event.step(runtime, exec, at),
            Self::AcquireSemaphore(acquire) => acquire.step(runtime, exec, at),
            Self::ChannelSend(send) => send.step(runtime, exec, at),
            Self::ChannelRecv(recv) => recv.step(runtime, exec, at),
            Self::ReadFileOctets(arguments) => {
                require_suspendable(runtime, exec, at, "read file asynchronously")?;
                let operation = super::super::filesystem::read_octets(runtime, &arguments, at)?;
                host_reply(runtime, exec, operation, at)
            }
            Self::StartHost { start, args } => {
                require_suspendable(runtime, exec, at, "start asynchronous host work")?;
                let (mut operation, completion) = HostOperation::new();
                {
                    let _pinned = runtime.pin_rust_frame();
                    let arguments: smallvec::SmallVec<[_; 4]> = (0..args.len())
                        .map(|index| runtime.root(args.get(index)))
                        .collect();
                    if let Err(error) = start(runtime, &arguments, completion)
                        && let Some(waker) =
                            operation.fail_start(EvalError::host_callback(at, error))
                    {
                        waker.wake();
                    }
                }
                host_reply(runtime, exec, operation, at)
            }
            Self::ResumeHost(operation) => host_reply(runtime, exec, operation, at),
            Self::CurrentTask => scheduler::current_task(runtime, at).map(TaskStep::Value),
            Self::ReturnNil => Ok(TaskStep::Value(Word::NIL)),
            Self::Error(message) => TaskStep::error(runtime, message, at),
        }
    }
}

fn host_reply(
    runtime: &Runtime,
    exec: &mut ExecState<'_>,
    mut operation: HostOperation,
    at: usize,
) -> Result<TaskStep, Fault> {
    match operation.take_reply() {
        Some(Ok(HostReply::Complete(materialize))) => {
            let _pinned = runtime.pin_rust_frame();
            let values = materialize(runtime, at)?;
            if values.iter().any(|value| !value.belongs_to(runtime)) {
                return Err(EvalError::host_callback(
                    at,
                    "callback returned a value from a different runtime".into(),
                )
                .into());
            }
            exec.values.select_roots(&values);
            Ok(TaskStep::KeepValues)
        }
        Some(Ok(HostReply::Abandoned)) => {
            Err(EvalError::host_callback(at, "host operation was abandoned".into()).into())
        }
        Some(Err(error)) => Err(error.into()),
        None => Ok(TaskStep::Suspend(SuspendReason::Host(operation))),
    }
}

impl Spawn {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        if self.group.borrow().is_open() {
            require_suspendable(runtime, exec, at, "spawn async task")?;
            if let Some(handle) = scheduler::spawn(runtime, exec, self)
                .map_err(|error| EvalError::out_of_memory(at, error))?
            {
                return Ok(TaskStep::Value(handle));
            }
        }
        TaskStep::error(runtime, "Cannot spawn into a closed task group", at)
    }
}

impl Await {
    fn step(
        self,
        runtime: &Runtime,
        exec: &mut ExecState<'_>,
        at: usize,
    ) -> Result<TaskStep, Fault> {
        match self.task.status() {
            TaskStatus::Running => {
                require_suspendable(runtime, exec, at, "await async task")?;
                let has_deadline = exec.control.await_has_deadline();
                if !has_deadline
                    && with_scheduler(runtime, |s| {
                        s.await_would_cycle(self.task.id, exec.control.defers_host_cancellation())
                    })
                    .expect("active task")
                {
                    return TaskStep::error(
                        runtime,
                        "Await would create a task dependency cycle",
                        at,
                    );
                }
                Ok(TaskStep::Suspend(SuspendReason::Await {
                    await_task: self,
                    has_deadline,
                }))
            }
            TaskStatus::Completed => {
                self.task
                    .with_result(|slots| exec.values.select(slots, 0..slots.len()));
                Ok(TaskStep::KeepValues)
            }
            TaskStatus::Cancelled => Ok(TaskStep::Call {
                target: resolve_async_signal_cell(runtime, "%SIGNAL-WAITED-TASK-CANCELLED", at)?,
                args: self.task_word,
            }),
            TaskStatus::Failed(Some(message)) => TaskStep::error(runtime, &message, at),
            TaskStatus::Failed(None) => {
                let target = Primitive::named("ERROR").expect("ERROR primitive");
                let args = self.task_word;
                self.task.with_result(|slots| args.set(0, slots.get(0)));
                Ok(TaskStep::Call { target, args })
            }
        }
    }
}

impl AwaitAny {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        if let Some((index, _)) = self
            .tasks
            .iter()
            .enumerate()
            .find(|(_, task)| !task.is_running())
        {
            return Ok(TaskStep::Value(self.task_words.get(index)));
        }
        require_suspendable(runtime, exec, at, "await async tasks")?;
        let has_deadline = exec.control.await_has_deadline();
        if !has_deadline
            && with_scheduler(runtime, |s| {
                s.await_any_would_cycle(&self.tasks, exec.control.defers_host_cancellation())
            })
            .expect("active task")
        {
            return TaskStep::error(runtime, "Await would create a task dependency cycle", at);
        }
        Ok(TaskStep::Suspend(SuspendReason::AwaitAny {
            await_any: self,
            has_deadline,
        }))
    }
}

impl WaitEvent {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        if self.woken || self.event.is_set() {
            return Ok(TaskStep::Value(Word::T));
        }
        require_suspendable(runtime, exec, at, "wait on async event")?;
        Ok(TaskStep::Suspend(SuspendReason::WaitEvent(self)))
    }
}

impl AcquireSemaphore {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        if self.acquired || self.semaphore.try_acquire() {
            if let Some(marker) = self.ownership {
                // The Lisp cleanup must own the permit before another
                // checkpoint can deliver cancellation to this task.
                runtime.heap.set_cons_car(marker.get(), Word::T);
            }
            return Ok(TaskStep::Value(Word::T));
        }
        require_suspendable(runtime, exec, at, "acquire async semaphore")?;
        Ok(TaskStep::Suspend(SuspendReason::AcquireSemaphore(self)))
    }
}

impl ChannelSend {
    pub(super) fn value(&self) -> Word {
        self.arguments.get(1)
    }

    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        let word = self.value();
        if self.delivered {
            return Ok(TaskStep::Value(word));
        }
        let sent = self
            .channel
            .try_send(runtime, word, |id| scheduler::wake_waiting_task(runtime, id));
        match sent {
            Ok(true) => Ok(TaskStep::Value(word)),
            Ok(false) => {
                require_suspendable(runtime, exec, at, "send to async channel")?;
                Ok(TaskStep::Suspend(SuspendReason::ChannelSend(self)))
            }
            Err(message) => TaskStep::error(runtime, message, at),
        }
    }
}

impl ChannelRecv {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        if let Some(slot) = self.received {
            return Ok(TaskStep::Values2(slot.get(0), Word::T));
        }
        let received = self
            .channel
            .try_recv(|id| scheduler::wake_waiting_task(runtime, id));
        if let Some(word) = received {
            return Ok(TaskStep::Values2(word, Word::T));
        }
        if self.channel.is_closed() {
            return Ok(TaskStep::Values2(Word::NIL, Word::NIL));
        }
        require_suspendable(runtime, exec, at, "receive from async channel")?;
        Ok(TaskStep::Suspend(SuspendReason::ChannelRecv(self)))
    }
}

fn select_branch_step(branch: usize, first: Word, second: Word) -> TaskStep {
    let index = Word::fixnum(branch as isize).expect("select branch fits in fixnum");
    TaskStep::Values3(index, first, second)
}

impl AwaitPromise {
    fn step(
        self,
        runtime: &Runtime,
        exec: &mut ExecState<'_>,
        at: usize,
    ) -> Result<TaskStep, Fault> {
        match self.promise.status() {
            PromiseStatus::Pending => {
                require_suspendable(runtime, exec, at, "await async promise")?;
                Ok(TaskStep::Suspend(SuspendReason::AwaitPromise(self)))
            }
            PromiseStatus::Fulfilled => {
                self.promise
                    .with_result(|slots| exec.values.select(slots, 0..slots.len()));
                Ok(TaskStep::KeepValues)
            }
            PromiseStatus::Failed(Some(message)) => TaskStep::error(runtime, &message, at),
            PromiseStatus::Failed(None) => {
                let target = Primitive::named("ERROR").expect("ERROR primitive");
                let args = runtime.root_frame(1);
                self.promise.with_result(|slots| args.set(0, slots.get(0)));
                Ok(TaskStep::Call { target, args })
            }
        }
    }
}

pub(super) enum BranchProbe {
    Ready(Word, Word),
    Pending,
    Error(&'static str),
}

impl SelectBranch {
    pub(super) fn probe(
        &self,
        runtime: &Runtime,
        first_arg: impl FnOnce() -> Word,
        second_arg: impl FnOnce() -> Word,
    ) -> BranchProbe {
        match self {
            Self::Recv(channel) => {
                if let Some(word) = channel.try_recv(|id| scheduler::wake_waiting_task(runtime, id))
                {
                    return BranchProbe::Ready(word, Word::T);
                }
                if channel.is_closed() {
                    return BranchProbe::Ready(Word::NIL, Word::NIL);
                }
                BranchProbe::Pending
            }
            Self::Send(channel) => {
                let word = second_arg();
                match channel.try_send(runtime, word, |id| scheduler::wake_waiting_task(runtime, id)) {
                    Ok(true) => BranchProbe::Ready(word, Word::T),
                    Ok(false) => BranchProbe::Pending,
                    Err(message) => BranchProbe::Error(message),
                }
            }
            Self::Event(event) => {
                if event.is_set() {
                    BranchProbe::Ready(Word::T, Word::T)
                } else {
                    BranchProbe::Pending
                }
            }
            Self::Task(task) => {
                if !task.is_running() {
                    BranchProbe::Ready(first_arg(), Word::T)
                } else {
                    BranchProbe::Pending
                }
            }
            Self::Promise(promise) => {
                if promise.is_done() {
                    BranchProbe::Ready(first_arg(), Word::T)
                } else {
                    BranchProbe::Pending
                }
            }
            Self::Timeout(_) => BranchProbe::Pending,
        }
    }
}

impl Select {
    fn poll_non_timeout(
        &self,
        runtime: &Runtime,
        at: usize,
        start_offset: usize,
    ) -> Result<Option<TaskStep>, Fault> {
        let len = self.branches.len();
        for offset in 0..len {
            let index = (start_offset + offset) % len;
            match self.branches[index].probe(
                runtime,
                || self.branch_first(index),
                || self.branch_second(index),
            ) {
                BranchProbe::Ready(first, second) => {
                    return Ok(Some(select_branch_step(index, first, second)));
                }
                BranchProbe::Pending => {}
                BranchProbe::Error(message) => {
                    return TaskStep::error(runtime, message, at).map(Some);
                }
            }
        }
        Ok(None)
    }

    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        let start_offset = if self.biased || self.branches.len() <= 1 {
            0
        } else {
            with_scheduler(runtime, |s| s.next_select_cursor()).unwrap_or(0)
        };
        if let Some(step) = self.poll_non_timeout(runtime, at, start_offset)? {
            return Ok(step);
        }
        let mut zero_timeout = None;
        let mut min_timeout: Option<(usize, Duration)> = None;
        for (index, branch) in self.branches.iter().enumerate() {
            if let SelectBranch::Timeout(duration) = branch {
                if duration.is_zero() {
                    zero_timeout.get_or_insert(index);
                } else if min_timeout.is_none_or(|(_, best)| *duration < best) {
                    min_timeout = Some((index, *duration));
                }
            }
        }
        if let Some(index) = zero_timeout {
            return Ok(select_branch_step(index, Word::NIL, Word::NIL));
        }
        if self.has_default {
            return Ok(TaskStep::Values3(Word::NIL, Word::NIL, Word::NIL));
        }
        require_suspendable(runtime, exec, at, "select async operations")?;
        let timeout = min_timeout
            .map(|(index, duration)| {
                deadline(runtime, duration, at).map(|deadline| (index, deadline))
            })
            .transpose()?;
        // Host clock callbacks can re-enter the runtime and signal an event,
        // transfer on a channel, or finish a task or promise before this wait registers.
        if timeout.is_some()
            && let Some(step) = self.poll_non_timeout(runtime, at, start_offset)?
        {
            return Ok(step);
        }
        let has_deadline = timeout.is_some() || exec.control.await_has_deadline();
        if !has_deadline
            && with_scheduler(runtime, |s| {
                s.select_would_cycle(&self.branches, exec.control.defers_host_cancellation())
            })
            .expect("active task")
        {
            return TaskStep::error(runtime, "Await would create a task dependency cycle", at);
        }
        Ok(TaskStep::Suspend(SuspendReason::Select {
            select: self,
            timeout,
            has_deadline,
        }))
    }
}

impl SelectFinished {
    fn step(self, runtime: &Runtime, exec: &ExecState<'_>, at: usize) -> Result<TaskStep, Fault> {
        let Some(index) = self.claim.winner() else {
            return self.select.step(runtime, exec, at);
        };
        match &self.select.branches[index] {
            SelectBranch::Recv(channel) => {
                if let Some(payload) = self.claim.take_payload() {
                    return Ok(select_branch_step(index, payload.get(0), Word::T));
                }
                if let Some(word) = channel.try_recv(|id| scheduler::wake_waiting_task(runtime, id))
                {
                    return Ok(select_branch_step(index, word, Word::T));
                }
                Ok(select_branch_step(index, Word::NIL, Word::NIL))
            }
            SelectBranch::Send(channel) => {
                let word = self.select.branch_second(index);
                if self.claim.has_committed_transfer() {
                    return Ok(select_branch_step(index, word, Word::T));
                }
                match channel.try_send(runtime, word, |id| scheduler::wake_waiting_task(runtime, id)) {
                    Ok(true) => Ok(select_branch_step(index, word, Word::T)),
                    Ok(false) => self.select.step(runtime, exec, at),
                    Err(message) => TaskStep::error(runtime, message, at),
                }
            }
            SelectBranch::Event(_) => Ok(select_branch_step(index, Word::T, Word::T)),
            SelectBranch::Task(_) | SelectBranch::Promise(_) => Ok(select_branch_step(
                index,
                self.select.branch_first(index),
                Word::T,
            )),
            SelectBranch::Timeout(_) => Ok(select_branch_step(index, Word::NIL, Word::NIL)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::{Rc, Weak};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread::ThreadId;

    use crate::{HostServices, Runtime};

    // Temporary thread-checked backpointer for scheduler mock clocks until Wave 4 converts Runtime to Arc<Vm>.
    struct ThreadCheckedRuntime {
        runtime: Rc<RefCell<Weak<Runtime>>>,
        thread: ThreadId,
    }

    // SAFETY: `get` panics if called from any thread other than the constructing test thread.
    unsafe impl Send for ThreadCheckedRuntime {}
    // SAFETY: `get` panics if called from any thread other than the constructing test thread.
    unsafe impl Sync for ThreadCheckedRuntime {}

    impl ThreadCheckedRuntime {
        fn new(runtime: Rc<RefCell<Weak<Runtime>>>) -> Self {
            Self {
                runtime,
                thread: std::thread::current().id(),
            }
        }

        fn eval(&self, code: &str) {
            assert_eq!(std::thread::current().id(), self.thread);
            self.runtime.borrow().upgrade().unwrap().eval(code).unwrap();
        }
    }

    #[test]
    fn select_observes_readiness_created_by_the_deadline_clock_callback() {
        struct Clock {
            runtime: ThreadCheckedRuntime,
            callback: Mutex<Option<&'static str>>,
            now: AtomicU64,
        }

        impl HostServices for Clock {
            fn wall_time_secs(&self) -> std::io::Result<i64> {
                Ok(0)
            }

            fn monotonic_nanos(&self) -> std::io::Result<u64> {
                if let Some(callback) = self.callback.lock().unwrap().take() {
                    self.runtime.eval(callback);
                }
                let next = self.now.fetch_add(1_000_000_000, Ordering::Relaxed) + 1_000_000_000;
                Ok(next)
            }

            fn fill_entropy(&self, bytes: &mut [u8]) -> std::io::Result<()> {
                bytes.fill(0);
                Ok(())
            }
        }

        let cases = [
            (
                "(defparameter *clock-event* (zk-async:make-event))",
                "(zk-async:set-event *clock-event*)",
                "(zk-async:select ((:event *clock-event*) t) ((:timeout 1) nil))",
            ),
            (
                "(defparameter *clock-channel* (zk-async:make-channel :capacity 1))",
                "(zk-async:channel-try-send *clock-channel* :message)",
                "(zk-async:select
                   ((:recv *clock-channel* value) (eq value :message))
                   ((:timeout 1) nil))",
            ),
            (
                "(defparameter *clock-channel* (zk-async:make-channel :capacity 1))
                 (zk-async:channel-try-send *clock-channel* :old)",
                "(zk-async:channel-try-recv *clock-channel*)",
                "(zk-async:select
                   ((:send *clock-channel* :new value) (eq value :new))
                   ((:timeout 1) nil))",
            ),
        ];
        for (setup, callback, select) in cases {
            let mut runtime = Runtime::start().unwrap();
            runtime.load_str(setup).unwrap();
            let weak = Rc::new(RefCell::new(Weak::new()));
            runtime.set_host_services(Clock {
                runtime: ThreadCheckedRuntime::new(Rc::clone(&weak)),
                callback: Mutex::new(Some(callback)),
                now: AtomicU64::new(0),
            });
            let runtime = Rc::new(runtime);
            *weak.borrow_mut() = Rc::downgrade(&runtime);
            let values = runtime.eval(select).unwrap();
            assert!(values[0].is_t(), "readiness was lost for {select}");
        }
    }
}
