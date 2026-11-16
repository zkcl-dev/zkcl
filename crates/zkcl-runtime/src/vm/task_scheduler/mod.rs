//! Lisp operations for structured concurrency and cooperative cancellation.

mod cancellation;
mod dependencies;
mod groups;
mod handles;
mod operations;
mod scheduler;
mod scopes;
mod sync;
mod timers;

use std::rc::Rc;
use std::time::Duration;

use smallvec::SmallVec;

pub(in crate::vm) use self::cancellation::{BoundarySignal, TaskCancellation};
use self::groups::TaskGroup;
use self::handles::{Task, TaskHandle, TaskStatus};
pub(crate) use self::operations::Suspension;
pub(in crate::vm) use self::operations::TaskCall;
use self::operations::{
    AcquireSemaphore, Await, AwaitAny, AwaitPromise, BranchProbe, ChannelRecv, ChannelSend,
    Operation, Select, SelectBranch, Spawn, WaitEvent,
};
use self::scheduler::with_scheduler;
pub(crate) use self::scheduler::{
    SchedulerStep, TaskExecutor, host_cancel_is_shielded, observe_host_cancel, refill_native_slice,
};
pub(in crate::vm) use self::scheduler::{
    execute as execute_scheduled, poll_ready, set_host_cancel_shield,
};
use self::scopes::{
    CancelScope, CancelScopeHandle, CancelScopeState, GroupScope, ProtectCancel, WithTimeout,
};
use self::sync::{
    Channel, ChannelHandle, Event, EventHandle, Promise, PromiseHandle, Semaphore, SemaphoreHandle,
};

use super::{
    ContinuationStep, ExecState, Invocation, Primitive, Register, Resume, Values, designator,
    environment,
};
use crate::condition::{Class, Fault};
use crate::execution::HOST_CANCEL;
use crate::heap::{RootFrame, RootSlots, Word};
use crate::package::Package;
use crate::{EvalError, Runtime};

pub(in crate::vm) type TaskId = u64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskPrimitive {
    CallWithTaskGroup,
    FinishTaskGroup,
    CancelTaskGroup,
    TaskGroupP,
    TaskGroupCancelledP,
    TaskGroupClosedP,
    Spawn,
    Await,
    AwaitAny,
    Select,
    SelectPacked,
    CancelTask,
    TaskCancelledP,
    TaskP,
    TaskName,
    TaskDoneP,
    TaskRunningP,
    TaskCompletedP,
    TaskFailedP,
    CurrentTask,
    Yield,
    InSchedulerP,
    ChildTaskP,
    ProtectFromCancel,
    WithCancelScope,
    CancelScopeP,
    CancelScope,
    CancelScopeCancelledP,
    WithTimeout,
    MakePromise,
    PromiseP,
    PromiseDoneP,
    FulfillPromise,
    FailPromise,
    AwaitPromise,
    MakeEvent,
    EventP,
    EventSetP,
    SetEvent,
    ResetEvent,
    WaitEvent,
    MakeSemaphore,
    SemaphoreP,
    SemaphoreCount,
    AcquireSemaphore,
    TryAcquireSemaphore,
    ReleaseSemaphore,
    MakeChannel,
    ChannelP,
    ChannelClosedP,
    CloseChannel,
    ChannelSend,
    ChannelRecv,
    ChannelTrySend,
    ChannelTryRecv,
    ReadFileOctets,
}

impl TaskPrimitive {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::CurrentTask
            | Self::Yield
            | Self::InSchedulerP
            | Self::ChildTaskP
            | Self::MakePromise => count == 0,
            Self::CallWithTaskGroup
            | Self::CancelTaskGroup
            | Self::TaskGroupP
            | Self::TaskGroupCancelledP
            | Self::TaskGroupClosedP
            | Self::Await
            | Self::AwaitAny
            | Self::CancelTask
            | Self::TaskP
            | Self::TaskName
            | Self::TaskDoneP
            | Self::TaskRunningP
            | Self::TaskCompletedP
            | Self::TaskFailedP
            | Self::ProtectFromCancel
            | Self::CancelScopeP
            | Self::CancelScope
            | Self::CancelScopeCancelledP
            | Self::PromiseP
            | Self::PromiseDoneP
            | Self::AwaitPromise
            | Self::MakeEvent
            | Self::EventP
            | Self::EventSetP
            | Self::SetEvent
            | Self::ResetEvent
            | Self::WaitEvent
            | Self::MakeSemaphore
            | Self::SemaphoreP
            | Self::SemaphoreCount
            | Self::ReleaseSemaphore
            | Self::MakeChannel
            | Self::ChannelP
            | Self::ChannelClosedP
            | Self::CloseChannel
            | Self::ChannelRecv
            | Self::ChannelTryRecv => count == 1,
            Self::TaskCancelledP => count <= 1,
            Self::FulfillPromise => count >= 1,
            Self::SelectPacked => count >= 3,
            Self::AcquireSemaphore | Self::TryAcquireSemaphore => matches!(count, 1 | 2),
            Self::FinishTaskGroup
            | Self::WithCancelScope
            | Self::FailPromise
            | Self::ChannelSend
            | Self::ChannelTrySend => count == 2,
            Self::Select | Self::WithTimeout | Self::ReadFileOctets => count == 3,
            Self::Spawn => count == 5,
        }
    }
}

pub(crate) const PRIMITIVES: &[(&str, TaskPrimitive)] = &[
    (
        "ZK::%CALL-WITH-TASK-GROUP",
        TaskPrimitive::CallWithTaskGroup,
    ),
    ("ZK::%FINISH-TASK-GROUP", TaskPrimitive::FinishTaskGroup),
    ("ZK::%CANCEL-TASK-GROUP", TaskPrimitive::CancelTaskGroup),
    ("ZK::%TASK-GROUP-P", TaskPrimitive::TaskGroupP),
    (
        "ZK::%TASK-GROUP-CANCELLED-P",
        TaskPrimitive::TaskGroupCancelledP,
    ),
    ("ZK::%TASK-GROUP-CLOSED-P", TaskPrimitive::TaskGroupClosedP),
    ("ZK::%SPAWN", TaskPrimitive::Spawn),
    ("ZK::%AWAIT", TaskPrimitive::Await),
    ("ZK::%AWAIT-ANY", TaskPrimitive::AwaitAny),
    ("ZK::%SELECT", TaskPrimitive::Select),
    ("ZK::%SELECT-PACKED", TaskPrimitive::SelectPacked),
    ("ZK::%CANCEL-TASK", TaskPrimitive::CancelTask),
    ("ZK::%TASK-CANCELLED-P", TaskPrimitive::TaskCancelledP),
    ("ZK::%TASKP", TaskPrimitive::TaskP),
    ("ZK::%TASK-NAME", TaskPrimitive::TaskName),
    ("ZK::%TASK-DONE-P", TaskPrimitive::TaskDoneP),
    ("ZK::%TASK-RUNNING-P", TaskPrimitive::TaskRunningP),
    ("ZK::%TASK-COMPLETED-P", TaskPrimitive::TaskCompletedP),
    ("ZK::%TASK-FAILED-P", TaskPrimitive::TaskFailedP),
    ("ZK::%CURRENT-TASK", TaskPrimitive::CurrentTask),
    ("ZK::%YIELD", TaskPrimitive::Yield),
    ("ZK::%IN-SCHEDULER-P", TaskPrimitive::InSchedulerP),
    ("ZK::%CHILD-TASK-P", TaskPrimitive::ChildTaskP),
    ("ZK::%PROTECT-FROM-CANCEL", TaskPrimitive::ProtectFromCancel),
    ("ZK::%WITH-CANCEL-SCOPE", TaskPrimitive::WithCancelScope),
    ("ZK::%CANCEL-SCOPE-P", TaskPrimitive::CancelScopeP),
    ("ZK::%CANCEL-SCOPE", TaskPrimitive::CancelScope),
    (
        "ZK::%CANCEL-SCOPE-CANCELLED-P",
        TaskPrimitive::CancelScopeCancelledP,
    ),
    ("ZK::%WITH-TIMEOUT", TaskPrimitive::WithTimeout),
    ("ZK::%MAKE-PROMISE", TaskPrimitive::MakePromise),
    ("ZK::%PROMISEP", TaskPrimitive::PromiseP),
    ("ZK::%PROMISE-DONE-P", TaskPrimitive::PromiseDoneP),
    ("ZK::%FULFILL-PROMISE", TaskPrimitive::FulfillPromise),
    ("ZK::%FAIL-PROMISE", TaskPrimitive::FailPromise),
    ("ZK::%AWAIT-PROMISE", TaskPrimitive::AwaitPromise),
    ("ZK::%MAKE-EVENT", TaskPrimitive::MakeEvent),
    ("ZK::%EVENTP", TaskPrimitive::EventP),
    ("ZK::%EVENT-SET-P", TaskPrimitive::EventSetP),
    ("ZK::%SET-EVENT", TaskPrimitive::SetEvent),
    ("ZK::%RESET-EVENT", TaskPrimitive::ResetEvent),
    ("ZK::%WAIT-EVENT", TaskPrimitive::WaitEvent),
    ("ZK::%MAKE-SEMAPHORE", TaskPrimitive::MakeSemaphore),
    ("ZK::%SEMAPHOREP", TaskPrimitive::SemaphoreP),
    ("ZK::%SEMAPHORE-COUNT", TaskPrimitive::SemaphoreCount),
    ("ZK::%ACQUIRE-SEMAPHORE", TaskPrimitive::AcquireSemaphore),
    (
        "ZK::%TRY-ACQUIRE-SEMAPHORE",
        TaskPrimitive::TryAcquireSemaphore,
    ),
    ("ZK::%RELEASE-SEMAPHORE", TaskPrimitive::ReleaseSemaphore),
    ("ZK::%MAKE-CHANNEL", TaskPrimitive::MakeChannel),
    ("ZK::%CHANNELP", TaskPrimitive::ChannelP),
    ("ZK::%CHANNEL-CLOSED-P", TaskPrimitive::ChannelClosedP),
    ("ZK::%CLOSE-CHANNEL", TaskPrimitive::CloseChannel),
    ("ZK::%CHANNEL-SEND", TaskPrimitive::ChannelSend),
    ("ZK::%CHANNEL-RECV", TaskPrimitive::ChannelRecv),
    ("ZK::%CHANNEL-TRY-SEND", TaskPrimitive::ChannelTrySend),
    ("ZK::%CHANNEL-TRY-RECV", TaskPrimitive::ChannelTryRecv),
    ("ZK::%READ-FILE-OCTETS", TaskPrimitive::ReadFileOctets),
];

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for &(name, _) in PRIMITIVES {
        let local_name = name.strip_prefix("ZK::").unwrap_or(name);
        let symbol = runtime.intern_package(Package::ZK, local_name, 0)?.0;
        let function = Primitive::named(name).expect("registered async primitive");
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    runtime.load_builtin_source(include_str!("../../../lisp/runtime/async/async.lisp"))
}

pub(super) struct GroupHandle(pub(super) TaskGroup);

#[inline]
pub(crate) fn in_scheduler(runtime: &Runtime) -> bool {
    runtime.active_scheduler.get().is_some()
}

fn can_suspend(runtime: &Runtime, exec: &ExecState<'_>) -> bool {
    in_scheduler(runtime) && !runtime.has_pinned_rust_frame() && exec.jit_frames.is_empty()
}

pub(super) fn require_suspendable(
    runtime: &Runtime,
    exec: &ExecState<'_>,
    at: usize,
    action: &str,
) -> Result<(), Fault> {
    if can_suspend(runtime, exec) {
        Ok(())
    } else {
        Err(control_error(
            at,
            &format!("cannot {action} across a native or Rust callback frame"),
        ))
    }
}

pub(super) fn retains_task_result(runtime: &Runtime) -> bool {
    with_scheduler(runtime, |scheduler| scheduler.retains_task_result()).unwrap_or(false)
}

pub(super) fn has_other_ready_tasks(runtime: &Runtime) -> bool {
    with_scheduler(runtime, |scheduler| scheduler.has_ready_tasks()).unwrap_or(false)
}

#[inline]
fn has_concurrent_work(runtime: &Runtime) -> bool {
    with_scheduler(runtime, |scheduler| scheduler.has_concurrent_work()).unwrap_or(false)
}

#[inline]
pub(crate) fn needs_suspendable_calls(runtime: &Runtime) -> bool {
    runtime.evaluation_active.get()
        || with_scheduler(runtime, |scheduler| scheduler.needs_suspendable_calls()).unwrap_or(false)
}

fn control_error(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ControlError, EvalError::new(at, message))
}

pub(super) fn timer_fault(at: usize, message: &str) -> Fault {
    EvalError::io_error(at, std::io::Error::other(message.to_owned())).into()
}

fn boolean(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

fn deadline(runtime: &Runtime, duration: Duration, at: usize) -> Result<u64, Fault> {
    let now = scheduler::now_nanos(runtime, at)?;
    u64::try_from(duration.as_nanos())
        .ok()
        .and_then(|nanos| now.checked_add(nanos))
        .ok_or_else(|| control_error(at, "async duration exceeds the host clock range"))
}

fn call_one(runtime: &Runtime, target: Word, argument: Word, at: usize) -> ContinuationStep {
    let args = runtime.root_frame(1);
    args.set(0, argument);
    ContinuationStep::Call { target, args, at }
}

fn resolve_async_signal_cell(runtime: &Runtime, name: &str, at: usize) -> Result<Word, Fault> {
    let symbol = {
        let packages = runtime.packages.borrow();
        packages
            .named("ZK-ASYNC")
            .and_then(|package| packages.find(package, name))
    };
    let Some((symbol, _)) = symbol else {
        return Err(control_error(at, "async signaling function is missing"));
    };
    super::function_cell(runtime, symbol, at)
}

fn async_error(runtime: &Runtime, message: &str, at: usize) -> Result<ContinuationStep, Fault> {
    let (target, args) = async_error_call(runtime, message, at)?;
    Ok(ContinuationStep::Call { target, args, at })
}

fn async_error_call(
    runtime: &Runtime,
    message: &str,
    at: usize,
) -> Result<(Word, RootFrame), Fault> {
    let message = runtime
        .heap
        .string(message)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let args = runtime.root_frame(1);
    args.set(0, message);
    Ok((
        resolve_async_signal_cell(runtime, "%SIGNAL-ASYNC-ERROR", at)?,
        args,
    ))
}

fn raise_condition(runtime: &Runtime, condition: Word, at: usize) -> ContinuationStep {
    call_one(
        runtime,
        Primitive::named("ERROR").expect("ERROR primitive"),
        condition,
        at,
    )
}

fn bind_foreign_rc<H: 'static, T>(
    runtime: &Runtime,
    word: Word,
    expected: &'static str,
    message: &'static str,
    at: usize,
    extract: impl FnOnce(&H) -> &Rc<T>,
) -> Result<Rc<T>, Fault> {
    runtime
        .with_rooted_foreign_ref::<H, _>(word, |handle| Rc::clone(extract(handle)))
        .map_err(|_| Fault::type_error(runtime, word, expected, EvalError::new(at, message)))
}

fn is_foreign<H: 'static>(runtime: &Runtime, word: Word) -> bool {
    runtime
        .with_rooted_foreign_ref::<H, _>(word, |_| ())
        .is_ok()
}

fn bind_task(runtime: &Runtime, word: Word, at: usize) -> Result<Rc<Task>, Fault> {
    bind_foreign_rc::<TaskHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::TASK",
        "expected an async task handle",
        at,
        |handle| &handle.0,
    )
}

fn bind_group(runtime: &Runtime, word: Word, at: usize) -> Result<TaskGroup, Fault> {
    bind_foreign_rc::<GroupHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::TASK-GROUP",
        "expected a task group",
        at,
        |handle| &handle.0,
    )
}

fn bind_event(runtime: &Runtime, word: Word, at: usize) -> Result<Rc<Event>, Fault> {
    bind_foreign_rc::<EventHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::EVENT",
        "expected an async event",
        at,
        |handle| &handle.0,
    )
}

fn bind_channel(runtime: &Runtime, word: Word, at: usize) -> Result<Rc<Channel>, Fault> {
    bind_foreign_rc::<ChannelHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::CHANNEL",
        "expected an async channel",
        at,
        |handle| &handle.0,
    )
}

fn bind_semaphore(runtime: &Runtime, word: Word, at: usize) -> Result<Rc<Semaphore>, Fault> {
    bind_foreign_rc::<SemaphoreHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::SEMAPHORE",
        "expected an async semaphore",
        at,
        |handle| &handle.0,
    )
}

fn bind_cancel_scope(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<Rc<CancelScopeState>, Fault> {
    bind_foreign_rc::<CancelScopeHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::CANCEL-SCOPE",
        "expected an async cancel scope",
        at,
        |handle| &handle.0,
    )
}

fn bind_promise(runtime: &Runtime, word: Word, at: usize) -> Result<Rc<Promise>, Fault> {
    bind_foreign_rc::<PromiseHandle, _>(
        runtime,
        word,
        "ZK-ASYNC::PROMISE",
        "expected an async promise",
        at,
        |handle| &handle.0,
    )
}

fn non_negative_usize(
    runtime: &Runtime,
    word: Word,
    expected_type: &'static str,
    label: &str,
    at: usize,
) -> Result<usize, Fault> {
    if let Some(n) = runtime.target_fixnum_value(word)
        && let Ok(value) = usize::try_from(n)
    {
        return Ok(value);
    }
    if let Some(integer) = runtime.heap.integer(word)
        && integer.sign() != num_bigint::Sign::Minus
    {
        return Err(control_error(
            at,
            &format!("{label} exceeds supported range"),
        ));
    }
    Err(Fault::type_error(runtime, 
        word,
        expected_type,
        EvalError::new(at, format!("{label} must be {expected_type}")),
    ))
}

fn channel_capacity(runtime: &Runtime, word: Word, at: usize) -> Result<Option<usize>, Fault> {
    if word == Word::NIL {
        return Ok(None);
    }
    non_negative_usize(
        runtime,
        word,
        "(OR NULL (INTEGER 0 *))",
        "channel :CAPACITY",
        at,
    )
    .map(Some)
}

fn usize_word(runtime: &Runtime, value: usize, at: usize) -> Result<Word, Fault> {
    if let Ok(signed) = isize::try_from(value)
        && let Some(fixnum) = Word::fixnum(signed)
    {
        return Ok(fixnum);
    }
    runtime
        .integer_from(value)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
        .map(|root| root.slot.get())
}

#[inline]
fn cancel_pending(runtime: &Runtime) -> bool {
    runtime.host_cancel_request() == HOST_CANCEL
        || with_scheduler(runtime, |s| s.active_cancel_pending()).unwrap_or(false)
}

#[inline]
fn finish_values2(
    values: &mut Values,
    first: Word,
    second: Word,
    resume: Resume,
) -> Invocation<'static> {
    values.resize(2);
    values.slots.set(0, first);
    values.slots.set(1, second);
    Invocation::Done(resume)
}

#[inline]
fn finish_values3(
    values: &mut Values,
    first: Word,
    second: Word,
    third: Word,
    resume: Resume,
) -> Invocation<'static> {
    values.resize(3);
    values.slots.set(0, first);
    values.slots.set(1, second);
    values.slots.set(2, third);
    Invocation::Done(resume)
}

const SELECT_OP_RECV: isize = 0;
const SELECT_OP_SEND: isize = 1;
const SELECT_OP_EVENT: isize = 2;
const SELECT_OP_TASK: isize = 3;
const SELECT_OP_PROMISE: isize = 4;
const SELECT_OP_TIMEOUT: isize = 5;

fn invoke_select_packed<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: std::ops::Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let opcodes_word = frame.get(args.start);
    let has_default = frame.get(args.start + 1) != Word::NIL;
    let biased = frame.get(args.start + 2) != Word::NIL;
    let operand_base = args.start + 3;
    let operand_count = args.len() - 3;

    let Some(info) = runtime.heap.array_info(opcodes_word) else {
        return Err(Fault::type_error(runtime, 
            opcodes_word,
            "VECTOR",
            EvalError::new(at, "SELECT descriptor must be a vector"),
        ));
    };
    if info.rank != 1 {
        return Err(Fault::type_error(runtime, 
            opcodes_word,
            "VECTOR",
            EvalError::new(at, "SELECT descriptor must be a vector"),
        ));
    }
    let branch_count = info.fill_pointer.unwrap_or(info.size);
    if branch_count == 0 {
        if operand_count != 0 {
            return Err(control_error(at, "invalid SELECT operand count"));
        }
        if has_default {
            return Ok(finish_values3(
                values,
                Word::NIL,
                Word::NIL,
                Word::NIL,
                resume,
            ));
        }
        return Ok(Invocation::TaskCall(TaskCall::new(
            Operation::Error("SELECT requires at least one branch"),
            resume,
            at,
        )));
    }

    let mut branches: SmallVec<[SelectBranch; 8]> = SmallVec::with_capacity(branch_count);
    let mut arg_offsets: SmallVec<[usize; 8]> = SmallVec::with_capacity(branch_count);
    let mut cursor = 0usize;

    for i in 0..branch_count {
        let op_word = runtime
            .heap
            .array_slot(opcodes_word, i)
            .map_err(|error| EvalError::out_of_memory(at, error))?
            .ok_or_else(|| control_error(at, "invalid SELECT descriptor index"))?;
        let Some(opcode) = op_word.as_fixnum() else {
            return Err(control_error(at, "invalid SELECT opcode"));
        };
        let arity = if opcode == SELECT_OP_SEND { 2 } else { 1 };
        if cursor + arity > operand_count {
            return Err(control_error(at, "invalid SELECT operand count"));
        }
        arg_offsets.push(cursor);
        let first = frame.get(operand_base + cursor);
        let branch = match opcode {
            SELECT_OP_RECV => SelectBranch::Recv(bind_channel(runtime, first, at)?),
            SELECT_OP_SEND => SelectBranch::Send(bind_channel(runtime, first, at)?),
            SELECT_OP_EVENT => SelectBranch::Event(bind_event(runtime, first, at)?),
            SELECT_OP_TASK => SelectBranch::Task(bind_task(runtime, first, at)?),
            SELECT_OP_PROMISE => SelectBranch::Promise(bind_promise(runtime, first, at)?),
            SELECT_OP_TIMEOUT => {
                SelectBranch::Timeout(environment::sleep_duration(runtime, first, at)?)
            }
            _ => return Err(control_error(at, "invalid SELECT clause specification")),
        };
        branches.push(branch);
        cursor += arity;
    }
    if cursor != operand_count {
        return Err(control_error(at, "invalid SELECT operand count"));
    }

    if !cancel_pending(runtime) {
        let start_offset = if biased || branch_count <= 1 {
            0
        } else {
            with_scheduler(runtime, |s| s.peek_select_cursor()).unwrap_or(0)
        };
        for offset in 0..branch_count {
            let index = (start_offset + offset) % branch_count;
            let arg_reg = operand_base + arg_offsets[index];
            match branches[index].probe(runtime, || frame.get(arg_reg), || frame.get(arg_reg + 1)) {
                BranchProbe::Ready(first, second) => {
                    if !biased && branch_count > 1 {
                        with_scheduler(runtime, |s| s.next_select_cursor());
                    }
                    let idx_word =
                        Word::fixnum(index as isize).expect("select branch fits in fixnum");
                    return Ok(finish_values3(values, idx_word, first, second, resume));
                }
                BranchProbe::Pending => {}
                BranchProbe::Error(message) => {
                    return Ok(Invocation::TaskCall(TaskCall::new(
                        Operation::Error(message),
                        resume,
                        at,
                    )));
                }
            }
        }
        if let Some((index, _)) = branches
            .iter()
            .enumerate()
            .find(|(_, branch)| matches!(branch, SelectBranch::Timeout(d) if d.is_zero()))
        {
            let idx_word = Word::fixnum(index as isize).expect("select branch fits in fixnum");
            return Ok(finish_values3(
                values,
                idx_word,
                Word::NIL,
                Word::NIL,
                resume,
            ));
        }
        if has_default {
            return Ok(finish_values3(
                values,
                Word::NIL,
                Word::NIL,
                Word::NIL,
                resume,
            ));
        }
    }

    let slots = runtime.root_frame(branch_count * 2);
    for (index, branch) in branches.iter().enumerate() {
        let arg_reg = operand_base + arg_offsets[index];
        match branch {
            SelectBranch::Recv(_)
            | SelectBranch::Event(_)
            | SelectBranch::Task(_)
            | SelectBranch::Promise(_) => slots.set(index * 2, frame.get(arg_reg)),
            SelectBranch::Send(_) => {
                slots.set(index * 2, frame.get(arg_reg));
                slots.set(index * 2 + 1, frame.get(arg_reg + 1));
            }
            SelectBranch::Timeout(_) => {}
        }
    }
    Ok(Invocation::TaskCall(TaskCall::new(
        Operation::Select(Select {
            branches: branches.into_vec(),
            slots,
            has_default,
            biased,
        }),
        resume,
        at,
    )))
}

fn parse_select_branches(
    runtime: &Runtime,
    specs: &[Word],
    at: usize,
) -> Result<(Vec<SelectBranch>, RootFrame), Fault> {
    let mut branches = Vec::with_capacity(specs.len());
    let slots = runtime.root_frame(specs.len() * 2);
    for (index, &spec) in specs.iter().enumerate() {
        let items = super::proper_list(runtime, spec, at)
            .map_err(|error| Fault::type_error(runtime, spec, "LIST", error))?;
        let Some(&tag_word) = items.first() else {
            return Err(control_error(at, "SELECT clause must not be empty"));
        };
        let Some(tag) = runtime.heap.symbol_name(tag_word) else {
            return Err(control_error(at, "SELECT clause tag must be a symbol"));
        };
        let branch = match (tag.as_str(), items.as_slice()) {
            ("RECV", [_, channel]) => {
                slots.set(index * 2, *channel);
                SelectBranch::Recv(bind_channel(runtime, *channel, at)?)
            }
            ("SEND", [_, channel, value]) => {
                slots.set(index * 2, *channel);
                slots.set(index * 2 + 1, *value);
                SelectBranch::Send(bind_channel(runtime, *channel, at)?)
            }
            ("EVENT", [_, event]) => {
                slots.set(index * 2, *event);
                SelectBranch::Event(bind_event(runtime, *event, at)?)
            }
            ("TASK", [_, task]) => {
                slots.set(index * 2, *task);
                SelectBranch::Task(bind_task(runtime, *task, at)?)
            }
            ("PROMISE", [_, promise]) => {
                slots.set(index * 2, *promise);
                SelectBranch::Promise(bind_promise(runtime, *promise, at)?)
            }
            ("TIMEOUT", [_, seconds]) => {
                SelectBranch::Timeout(environment::sleep_duration(runtime, *seconds, at)?)
            }
            _ => return Err(control_error(at, "invalid SELECT clause specification")),
        };
        branches.push(branch);
    }
    Ok((branches, slots))
}

pub(super) fn invoke_sleep<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: std::ops::Range<Register>,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let duration = environment::sleep_duration(runtime, frame.get(args.start), at)?;
    Ok(Invocation::TaskCall(TaskCall::new(
        Operation::Sleep(duration),
        resume,
        at,
    )))
}

pub(super) fn invoke_host_operation<'rt>(
    runtime: &'rt Runtime,
    start: std::sync::Arc<crate::host::HostAsyncFunction>,
    frame: &RootSlots,
    args: std::ops::Range<Register>,
    resume: Resume,
    at: usize,
) -> Invocation<'rt> {
    let slots = runtime.root_frame(args.len());
    for (index, reg) in args.enumerate() {
        slots.set(index, frame.get(reg));
    }
    Invocation::TaskCall(TaskCall::new(
        Operation::StartHost { start, args: slots },
        resume,
        at,
    ))
}

pub(super) fn invoke_async<'rt>(
    runtime: &'rt Runtime,
    op: TaskPrimitive,
    frame: &RootSlots,
    args: std::ops::Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let arg = |i: usize| frame.get(args.start + i);
    let call = |operation| Ok(Invocation::TaskCall(TaskCall::new(operation, resume, at)));
    let result = match op {
        TaskPrimitive::CallWithTaskGroup => {
            let body = runtime.root_slot(designator(runtime, arg(0), at)?);
            return Ok(Invocation::Continuation(Box::new(GroupScope {
                body,
                group: None,
                resume,
                at,
            })));
        }
        TaskPrimitive::FinishTaskGroup => {
            let group = bind_group(runtime, arg(0), at)?;
            if !with_scheduler(runtime, |s| s.owns_group(&group.borrow())).unwrap_or(false) {
                return Err(control_error(at, "only the owner can finish a task group"));
            }
            return call(Operation::FinishGroup {
                group,
                unwinding: arg(1) != Word::NIL,
            });
        }
        TaskPrimitive::CancelTaskGroup => {
            let group = bind_group(runtime, arg(0), at)?;
            let changed = with_scheduler(runtime, |s| s.cancel_group(&group)).unwrap_or(false);
            boolean(changed)
        }
        TaskPrimitive::TaskGroupP => boolean(is_foreign::<GroupHandle>(runtime, arg(0))),
        TaskPrimitive::TaskGroupCancelledP => {
            boolean(bind_group(runtime, arg(0), at)?.borrow().is_cancelled())
        }
        TaskPrimitive::TaskGroupClosedP => {
            boolean(!bind_group(runtime, arg(0), at)?.borrow().is_open())
        }
        TaskPrimitive::Spawn => {
            let group = bind_group(runtime, arg(0), at)?;
            let target = runtime.root_frame(1);
            target.set(0, designator(runtime, arg(1), at)?);
            let name = match arg(4) {
                Word::NIL => None,
                word => Some(
                    runtime
                        .heap
                        .string_contents(word)
                        .or_else(|| runtime.heap.symbol_name(word))
                        .ok_or_else(|| {
                            Fault::type_error(runtime, 
                                word,
                                "(OR STRING SYMBOL NULL)",
                                EvalError::new(at, "task :NAME must be a string, symbol, or NIL"),
                            )
                        })?,
                ),
            };
            return call(Operation::Spawn(Spawn {
                group,
                target,
                no_wait: arg(2) != Word::NIL,
                allow_failure: arg(3) != Word::NIL,
                name,
            }));
        }
        TaskPrimitive::Await => {
            let word = arg(0);
            let task = bind_task(runtime, word, at)?;
            if matches!(task.status(), TaskStatus::Completed) {
                task.with_result(|slots| values.select(slots, 0..slots.len()));
                return Ok(Invocation::Done(resume));
            }
            let task_word = runtime.root_frame(1);
            task_word.set(0, word);
            return call(Operation::Await(Await { task, task_word }));
        }
        TaskPrimitive::AwaitAny => {
            let list_word = arg(0);
            let words = super::proper_list(runtime, list_word, at)
                .map_err(|error| Fault::type_error(runtime, list_word, "LIST", error))?;
            if words.is_empty() {
                return call(Operation::Error("AWAIT-ANY requires at least one task"));
            }
            let mut tasks = Vec::with_capacity(words.len());
            for &word in &words {
                tasks.push(bind_task(runtime, word, at)?);
            }
            if let Some((index, _)) = tasks
                .iter()
                .enumerate()
                .find(|(_, task)| !task.is_running())
            {
                values.one(words[index]);
                return Ok(Invocation::Done(resume));
            }
            let task_words = runtime.root_frame(words.len());
            for (index, word) in words.into_iter().enumerate() {
                task_words.set(index, word);
            }
            return call(Operation::AwaitAny(AwaitAny { tasks, task_words }));
        }
        TaskPrimitive::Select => {
            let specs_word = arg(0);
            let specs = super::proper_list(runtime, specs_word, at)
                .map_err(|error| Fault::type_error(runtime, specs_word, "LIST", error))?;
            let has_default = arg(1) != Word::NIL;
            if specs.is_empty() {
                if has_default {
                    return Ok(finish_values3(
                        values,
                        Word::NIL,
                        Word::NIL,
                        Word::NIL,
                        resume,
                    ));
                }
                return call(Operation::Error("SELECT requires at least one branch"));
            }
            let (branches, slots) = parse_select_branches(runtime, &specs, at)?;
            return call(Operation::Select(Select {
                branches,
                slots,
                has_default,
                biased: arg(2) != Word::NIL,
            }));
        }
        TaskPrimitive::SelectPacked => {
            return invoke_select_packed(runtime, frame, args, values, resume, at);
        }
        TaskPrimitive::CancelTask => {
            let task = bind_task(runtime, arg(0), at)?;
            let changed = with_scheduler(runtime, |s| s.cancel_task(&task)).unwrap_or(false);
            boolean(changed)
        }
        TaskPrimitive::TaskCancelledP => {
            if runtime.host_cancel_request() == HOST_CANCEL {
                observe_host_cancel(runtime);
            }
            let pending = if args.is_empty() || arg(0) == Word::NIL {
                with_scheduler(runtime, |s| s.active_cancel_pending()).unwrap_or(false)
            } else {
                bind_task(runtime, arg(0), at)?.cancellation.is_pending()
            };
            boolean(pending)
        }
        TaskPrimitive::TaskP => boolean(is_foreign::<TaskHandle>(runtime, arg(0))),
        TaskPrimitive::TaskName => {
            let task = bind_task(runtime, arg(0), at)?;
            task.name
                .as_deref()
                .map(|name| runtime.heap.string(name))
                .transpose()
                .map_err(|error| EvalError::out_of_memory(at, error))?
                .unwrap_or(Word::NIL)
        }
        TaskPrimitive::TaskDoneP => {
            let task = bind_task(runtime, arg(0), at)?;
            boolean(!task.is_running())
        }
        TaskPrimitive::TaskRunningP => {
            let task = bind_task(runtime, arg(0), at)?;
            boolean(task.is_running())
        }
        TaskPrimitive::TaskCompletedP => {
            let task = bind_task(runtime, arg(0), at)?;
            boolean(matches!(task.status(), TaskStatus::Completed))
        }
        TaskPrimitive::TaskFailedP => {
            let task = bind_task(runtime, arg(0), at)?;
            boolean(matches!(task.status(), TaskStatus::Failed(_)))
        }
        TaskPrimitive::CurrentTask => return call(Operation::CurrentTask),
        TaskPrimitive::Yield => return call(Operation::Yield),
        TaskPrimitive::InSchedulerP => boolean(in_scheduler(runtime)),
        TaskPrimitive::ChildTaskP => boolean(runtime.child_task_running.get()),
        TaskPrimitive::ProtectFromCancel => {
            let thunk = runtime.root_slot(designator(runtime, arg(0), at)?);
            return Ok(Invocation::Continuation(Box::new(ProtectCancel {
                thunk,
                resume,
                at,
            })));
        }
        TaskPrimitive::WithCancelScope => {
            let body = runtime.root_slot(designator(runtime, arg(0), at)?);
            let fallback = (arg(1) != Word::NIL)
                .then(|| designator(runtime, arg(1), at).map(|target| runtime.root_slot(target)))
                .transpose()?;
            return Ok(Invocation::Continuation(Box::new(CancelScope {
                body,
                fallback,
                running: None,
                resume,
                at,
            })));
        }
        TaskPrimitive::CancelScopeP => boolean(is_foreign::<CancelScopeHandle>(runtime, arg(0))),
        TaskPrimitive::CancelScope => {
            let scope = bind_cancel_scope(runtime, arg(0), at)?;
            let changed = with_scheduler(runtime, |s| s.cancel_scope(&scope)).unwrap_or(false);
            boolean(changed)
        }
        TaskPrimitive::CancelScopeCancelledP => {
            boolean(bind_cancel_scope(runtime, arg(0), at)?.is_cancelled())
        }
        TaskPrimitive::WithTimeout => {
            let duration = environment::sleep_duration(runtime, arg(0), at)?;
            let body = runtime.root_slot(designator(runtime, arg(1), at)?);
            let fallback = runtime.root_slot(designator(runtime, arg(2), at)?);
            return Ok(Invocation::Continuation(Box::new(WithTimeout {
                duration,
                body,
                fallback,
                running: None,
                resume,
                at,
            })));
        }
        TaskPrimitive::MakePromise => {
            let promise = Promise::new(runtime);
            let edge = promise.edge();
            let handle = runtime
                .foreign_with_edge(PromiseHandle(promise), Some(edge))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            handle.slot.get()
        }
        TaskPrimitive::PromiseP => boolean(is_foreign::<PromiseHandle>(runtime, arg(0))),
        TaskPrimitive::PromiseDoneP => boolean(bind_promise(runtime, arg(0), at)?.is_done()),
        TaskPrimitive::FulfillPromise => {
            let promise = bind_promise(runtime, arg(0), at)?;
            boolean(
                promise.fulfill((args.start + 1..args.end).map(|reg| frame.get(reg)), |id| {
                    scheduler::wake_waiting_task(runtime, id)
                }),
            )
        }
        TaskPrimitive::FailPromise => {
            let promise = bind_promise(runtime, arg(0), at)?;
            let reason = arg(1);
            let changed =
                match runtime.heap.string_contents(reason) {
                    Some(message) => promise
                        .fail_message(&message, |id| scheduler::wake_waiting_task(runtime, id)),
                    None => promise
                        .fail_condition(reason, |id| scheduler::wake_waiting_task(runtime, id)),
                };
            boolean(changed)
        }
        TaskPrimitive::AwaitPromise => {
            let promise_word = arg(0);
            let promise = bind_promise(runtime, promise_word, at)?;
            if matches!(promise.status(), self::sync::PromiseStatus::Fulfilled) {
                promise.with_result(|slots| values.select(slots, 0..slots.len()));
                return Ok(Invocation::Done(resume));
            }
            let owner = runtime.root_frame(1);
            owner.set(0, promise_word);
            return call(Operation::AwaitPromise(AwaitPromise {
                promise,
                _owner: owner,
            }));
        }
        TaskPrimitive::MakeEvent => {
            let event = Event::new(arg(0) != Word::NIL);
            let handle = runtime
                .foreign_owned(EventHandle(event))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            handle.slot.get()
        }
        TaskPrimitive::EventP => boolean(is_foreign::<EventHandle>(runtime, arg(0))),
        TaskPrimitive::EventSetP => boolean(bind_event(runtime, arg(0), at)?.is_set()),
        TaskPrimitive::SetEvent => {
            let event = bind_event(runtime, arg(0), at)?;
            boolean(event.set(|id| scheduler::wake_waiting_task(runtime, id)))
        }
        TaskPrimitive::ResetEvent => boolean(bind_event(runtime, arg(0), at)?.reset()),
        TaskPrimitive::WaitEvent => {
            let event = bind_event(runtime, arg(0), at)?;
            if event.is_set() {
                Word::T
            } else {
                return call(Operation::WaitEvent(WaitEvent {
                    event,
                    woken: false,
                }));
            }
        }
        TaskPrimitive::MakeSemaphore => {
            let permits = non_negative_usize(
                runtime,
                arg(0),
                "(INTEGER 0 *)",
                "semaphore permit count",
                at,
            )?;
            let semaphore = Semaphore::new(permits);
            let handle = runtime
                .foreign_owned(SemaphoreHandle(semaphore))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            handle.slot.get()
        }
        TaskPrimitive::SemaphoreP => boolean(is_foreign::<SemaphoreHandle>(runtime, arg(0))),
        TaskPrimitive::SemaphoreCount => {
            let count = bind_semaphore(runtime, arg(0), at)?.count();
            usize_word(runtime, count, at)?
        }
        TaskPrimitive::AcquireSemaphore | TaskPrimitive::TryAcquireSemaphore => {
            let semaphore = bind_semaphore(runtime, arg(0), at)?;
            let ownership = if args.len() == 2 {
                let marker = arg(1);
                if runtime.heap.cons_fields(marker).is_none() {
                    return Err(Fault::type_error(runtime, 
                        marker,
                        "CONS",
                        EvalError::new(at, "semaphore ownership marker must be a cons"),
                    ));
                }
                Some(marker)
            } else {
                None
            };
            if semaphore.try_acquire() {
                if let Some(marker) = ownership {
                    runtime.heap.set_cons_car(marker, Word::T);
                }
                Word::T
            } else if op == TaskPrimitive::AcquireSemaphore {
                return call(Operation::AcquireSemaphore(AcquireSemaphore {
                    semaphore,
                    ownership: ownership.map(|marker| runtime.root_slot(marker)),
                    acquired: false,
                }));
            } else {
                Word::NIL
            }
        }
        TaskPrimitive::ReleaseSemaphore => {
            let semaphore = bind_semaphore(runtime, arg(0), at)?;
            match semaphore.release(|id| scheduler::wake_waiting_task(runtime, id)) {
                Ok(count) => usize_word(runtime, count, at)?,
                Err(message) => return call(Operation::Error(message)),
            }
        }
        TaskPrimitive::MakeChannel => {
            let capacity = channel_capacity(runtime, arg(0), at)?;
            let channel = Channel::new(runtime, capacity);
            let edge = channel.edge();
            let handle = runtime
                .foreign_with_edge(ChannelHandle(channel), Some(edge))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            handle.slot.get()
        }
        TaskPrimitive::ChannelP => boolean(is_foreign::<ChannelHandle>(runtime, arg(0))),
        TaskPrimitive::ChannelClosedP => boolean(bind_channel(runtime, arg(0), at)?.is_closed()),
        TaskPrimitive::CloseChannel => {
            let channel = bind_channel(runtime, arg(0), at)?;
            boolean(channel.close(|id| scheduler::wake_waiting_task(runtime, id)))
        }
        TaskPrimitive::ChannelSend => {
            let channel = bind_channel(runtime, arg(0), at)?;
            let value = arg(1);
            if !cancel_pending(runtime) {
                match channel.try_send(runtime, value, |id| scheduler::wake_waiting_task(runtime, id)) {
                    Ok(true) => {
                        values.one(value);
                        return Ok(Invocation::Done(resume));
                    }
                    Ok(false) => {}
                    Err(message) => return call(Operation::Error(message)),
                }
            }
            let arguments = runtime.root_frame(2);
            arguments.set(0, arg(0));
            arguments.set(1, value);
            return call(Operation::ChannelSend(ChannelSend {
                channel,
                arguments,
                delivered: false,
            }));
        }
        TaskPrimitive::ChannelRecv => {
            let channel = bind_channel(runtime, arg(0), at)?;
            if !cancel_pending(runtime) {
                if let Some(word) = channel.try_recv(|id| scheduler::wake_waiting_task(runtime, id))
                {
                    return Ok(finish_values2(values, word, Word::T, resume));
                }
                if channel.is_closed() {
                    return Ok(finish_values2(values, Word::NIL, Word::NIL, resume));
                }
            }
            let owner = runtime.root_frame(1);
            owner.set(0, arg(0));
            return call(Operation::ChannelRecv(ChannelRecv {
                channel,
                _owner: owner,
                received: None,
            }));
        }
        TaskPrimitive::ChannelTrySend => {
            let channel = bind_channel(runtime, arg(0), at)?;
            let value = arg(1);
            let sent = channel.try_send(runtime, value, |id| scheduler::wake_waiting_task(runtime, id));
            match sent {
                Ok(sent) => boolean(sent),
                Err(message) => return call(Operation::Error(message)),
            }
        }
        TaskPrimitive::ChannelTryRecv => {
            let channel = bind_channel(runtime, arg(0), at)?;
            let received = channel.try_recv(|id| scheduler::wake_waiting_task(runtime, id));
            match received {
                Some(word) => return Ok(finish_values2(values, word, Word::T, resume)),
                None => return Ok(finish_values2(values, Word::NIL, Word::NIL, resume)),
            }
        }
        TaskPrimitive::ReadFileOctets => {
            let arguments = runtime.root_frame(3);
            for index in 0..3 {
                arguments.set(index, arg(index));
            }
            return call(Operation::ReadFileOctets(arguments));
        }
    };
    values.one(result);
    Ok(Invocation::Done(resume))
}
