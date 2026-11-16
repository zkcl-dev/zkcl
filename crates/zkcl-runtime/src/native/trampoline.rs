use std::ops::Range;

use zkcl_bytecode::{Class, Handler};

use super::context::{JIT_FRAME_DEPTH, enter_jit_frame, jit_stack_exhausted};
use super::{
    Fault, JitCEntry, JitCallContext, JitReturn, NativeContext, NativeOutcome, NativeReturn,
    NativeRootFrame, NativeTailCall, Word,
};
use crate::heap::RootSlots;
use crate::vm::control_stack::{ExitPoint, NonLocalTransfer, TransferOutcome};
use crate::vm::{self, ExecState};
use crate::{EvalError, Runtime};

/// Invoke any Lisp callable (AOT/JIT function, bytecode function, closure, primitive,
/// `NativeFunction`, generic function, or symbol function designator) from native code.
pub fn native_invoke<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
    at: usize,
) -> Result<NativeReturn, Fault> {
    native_invoke_at_scope(ctx, target, args, range, at, None)
}

pub(crate) fn native_invoke_at_scope<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
    at: usize,
    min_scopes: Option<usize>,
) -> Result<NativeReturn, Fault> {
    let runtime = ctx.runtime;
    let exec = ctx.exec_mut();
    if let Some(frame) = exec.frames.last_mut() {
        frame.record_native_offset(at);
    }
    let base_frame_len = exec.frames.len();
    let control = match runtime.call_host_function(target, args, range.clone(), at) {
        Some(Ok(value)) => {
            exec.values.resize(0);
            return Ok(value.map_or(NativeReturn::Values, NativeReturn::One));
        }
        Some(Err(fault)) => vm::dispatch_error(runtime, exec, fault),
        None => vm::invoke_and_accept(runtime, exec, target, args, range, at),
    };
    match control.map_err(Fault::dispatched)? {
        vm::StepControl::Unwind(target) => Ok(target.into()),
        vm::StepControl::Pending(None) => {
            if exec.frames.len() <= base_frame_len {
                Ok(exec.values.take_native_return())
            } else {
                vm::execute_until_at_scope(runtime, exec, base_frame_len, None, min_scopes)
                    .map_err(Fault::dispatched)
            }
        }
        vm::StepControl::Pending(Some(pending)) => {
            vm::execute_until_at_scope(runtime, exec, base_frame_len, Some(pending), min_scopes)
                .map_err(Fault::dispatched)
        }
        vm::StepControl::QuantumYield | vm::StepControl::Suspend { .. } => {
            unreachable!("synchronous native call cannot suspend")
        }
    }
}

/// Establish an `UNWIND-PROTECT` scope on `frame`, jumping to `cleanup_state` on unwind.
pub fn native_protect<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    frame: &NativeRootFrame<'rt>,
    cleanup_state: usize,
    clear: usize,
) {
    let runtime = ctx.runtime;
    ctx.exec_mut()
        .control
        .protect(runtime, frame.frame_index(), cleanup_state, clear);
}

/// Begin the cleanup phase on normal fallthrough out of an `UNWIND-PROTECT` body.
///
/// The protected body's return values must already be stored in `ctx` (e.g. via
/// `ctx.store_return(ret)`) before calling `native_begin_cleanup`.
pub fn native_begin_cleanup<'rt, 'exec>(ctx: &mut NativeContext<'rt, 'exec>) {
    let exec = ctx.exec_mut();
    exec.control
        .begin_cleanup(&mut exec.frames, &mut exec.values);
}

/// Complete an `UNWIND-PROTECT` cleanup block, restoring saved values and resuming any pending exit.
pub fn native_end_cleanup<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
) -> Result<NativeReturn, Fault> {
    let exec = ctx.exec_mut();
    match exec
        .control
        .end_cleanup(&mut exec.frames, &mut exec.values)
        .map_err(Fault::from)?
    {
        Some(target) => Ok(target.into()),
        None => Ok(exec.values.take_native_return()),
    }
}

/// Enter a `CATCH` scope keyed by the `Word` in `frame.slots()[tag_slot]`.
pub fn native_enter_catch<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    frame: &NativeRootFrame<'rt>,
    tag_slot: usize,
    resume_state: usize,
    clear: usize,
) {
    ctx.exec_mut().control.enter_exit_point(
        frame.frame_index(),
        ExitPoint::Catch(tag_slot),
        resume_state,
        clear,
    );
}

/// Allocate a fresh lexical exit token (`BLOCK` / `TAGBODY`).
pub fn native_exit_token(rt: &Runtime, at: usize) -> Result<Word, Fault> {
    let id = rt.next_exit.get();
    let token =
        Word::exit_token(id).ok_or_else(|| EvalError::new(at, "too many control targets"))?;
    rt.next_exit.set(id + 1);
    Ok(token)
}

/// Enter a lexical `BLOCK` scope for `token`.
pub fn native_enter_block<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    frame: &NativeRootFrame<'rt>,
    token: Word,
    resume_state: usize,
    clear: usize,
) {
    ctx.exec_mut().control.enter_exit_point(
        frame.frame_index(),
        ExitPoint::Block(token),
        resume_state,
        clear,
    );
}

/// Leave the innermost `CATCH` / `BLOCK` / `TAGBODY` scope on normal completion.
pub fn native_leave_target<'rt, 'exec>(ctx: &mut NativeContext<'rt, 'exec>) {
    ctx.exec_mut().control.leave_exit_point();
}

/// Enter a `HANDLER-CASE` / `HANDLER-BIND` handler cluster on `frame`.
pub fn native_enter_handlers<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    frame: &NativeRootFrame<'rt>,
    handlers: Vec<Handler>,
) {
    ctx.exec_mut()
        .control
        .enter_handlers(frame.frame_index(), handlers);
}

/// Leave the innermost handler cluster on normal completion.
pub fn native_leave_handlers<'rt, 'exec>(ctx: &mut NativeContext<'rt, 'exec>) {
    ctx.exec_mut().control.leave_handlers();
}

/// Perform a non-local `THROW` to an active `CATCH` matching `tag`, preserving values in `ctx`.
pub fn native_throw<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    tag: Word,
    at: usize,
) -> Result<NativeReturn, Fault> {
    native_transfer(ctx, NonLocalTransfer::Throw(tag), at)
}

/// Perform a non-local `RETURN-FROM` targeting the `BLOCK` identified by `token`.
pub fn native_return_from<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    token: Word,
    at: usize,
) -> Result<NativeReturn, Fault> {
    native_transfer(ctx, NonLocalTransfer::ReturnFrom(token), at)
}

pub(super) fn native_transfer(
    ctx: &mut NativeContext<'_, '_>,
    transfer: NonLocalTransfer,
    at: usize,
) -> Result<NativeReturn, Fault> {
    let exec = ctx.exec_mut();
    match exec
        .control
        .transfer_control(&mut exec.frames, &mut exec.values, transfer, at)?
    {
        TransferOutcome::ResumeFrame(target) => Ok(target.into()),
        TransferOutcome::InvalidExit(error) => {
            native_error(ctx, Fault::plain(Class::ControlError, error))
        }
    }
}

/// Signal a runtime `Fault` through the unified `ControlStack` (`HANDLER-BIND` / `HANDLER-CASE` / `UNWIND-PROTECT`).
pub fn native_error<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    fault: Fault,
) -> Result<NativeReturn, Fault> {
    let runtime = ctx.runtime;
    vm::dispatch_fault_from_native(runtime, ctx.exec_mut(), fault)
}

/// Dynamically bind a special variable `symbol` to `value` on `frame`.
pub fn native_bind_special<'rt, 'exec>(
    ctx: &mut NativeContext<'rt, 'exec>,
    frame: &NativeRootFrame<'rt>,
    symbol: Word,
    value: Word,
) {
    let heap = &ctx.runtime.heap;
    ctx.exec_mut()
        .control
        .bind(frame.frame_index(), heap, std::iter::once((symbol, value)));
}

/// Unbind `count` dynamic binding scopes in LIFO order.
pub fn native_unbind<'rt, 'exec>(ctx: &mut NativeContext<'rt, 'exec>, count: usize) {
    ctx.exec_mut().control.unbind(count);
}

/// Intern or look up a symbol in `package` (defaults to current `*PACKAGE*` if `None`,
/// `"COMMON-LISP"` / `"KEYWORD"` / any named package if `Some`).
/// Keep the returned symbol in a live root slot and reload it across GC safepoints.
pub fn native_intern(
    rt: &Runtime,
    package: Option<&str>,
    name: &str,
    at: usize,
) -> Result<Word, Fault> {
    rt.initialize_packages()?;
    let pkg = match package {
        Some("COMMON-LISP") | Some("CL") => crate::package::Package::COMMON_LISP,
        Some("KEYWORD") => crate::package::Package::KEYWORD,
        Some("COMMON-LISP-USER") | Some("CL-USER") => crate::package::Package::USER,
        Some(other) => rt.packages.borrow().named(other).ok_or_else(|| {
            Fault::plain(
                Class::PackageError,
                EvalError::new(at, format!("missing package {other}")),
            )
        })?,
        None => rt.current_package_at(at)?,
    };
    Ok(rt.intern_package(pkg, name, at)?.0)
}

pub(super) fn invoke_native_tail(
    ctx: &mut NativeContext<'_, '_>,
    call: NativeTailCall,
) -> Result<NativeReturn, Fault> {
    if let Err(error) = ctx.runtime.check_execution_safepoint(call.at) {
        return native_error(ctx, error.into());
    }
    native_invoke(
        ctx,
        call.slots.get(0),
        &call.slots,
        1..call.slots.len(),
        call.at,
    )
}

/// Trampoline bridging `NativeGenericFn` to a `JitCEntryFn` installed on `VmCode`.
pub fn jit_c_trampoline<'rt, 'exec>(
    rt: &'rt Runtime,
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
) -> Result<NativeReturn, Fault> {
    let code = rt.heap.function_code(target).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(0, "JIT trampoline called on non-function"),
        )
    })?;
    let info = rt.heap.function_info(code).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(0, "JIT trampoline missing FunctionFields"),
        )
    })?;
    let vm_code = rt.heap.code_store.get(info.code_id);
    let executable = vm_code.native_entry();
    let c_entry = executable
        .as_ref()
        .and_then(|code| code.c_entry)
        .ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(0, "JIT C-ABI entry is not installed"),
            )
        })?;
    match invoke_c_entry(ctx, c_entry, false, target, args, range)? {
        NativeOutcome::Returned(result) => Ok(result),
        NativeOutcome::TailCall(call) => invoke_native_tail(ctx, call),
        NativeOutcome::ResumeVm => unreachable!("synchronous native entry cannot hand off"),
    }
}

pub(crate) fn invoke_c_entry(
    ctx: &mut NativeContext<'_, '_>,
    c_entry: JitCEntry,
    resumable: bool,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
) -> Result<NativeOutcome, Fault> {
    invoke_c_entry_with_activation(ctx, c_entry, resumable, target, args, range, None)
}

pub(crate) fn invoke_osr_entry(
    ctx: &mut NativeContext<'_, '_>,
    c_entry: JitCEntry,
    target: Word,
    registers: &RootSlots,
    count: usize,
    activation: vm::OsrActivation,
) -> Result<NativeOutcome, Fault> {
    invoke_c_entry_with_activation(
        ctx,
        c_entry,
        true,
        target,
        registers,
        0..count,
        Some(activation),
    )
}

fn invoke_c_entry_with_activation(
    ctx: &mut NativeContext<'_, '_>,
    c_entry: JitCEntry,
    resumable: bool,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
    osr: Option<vm::OsrActivation>,
) -> Result<NativeOutcome, Fault> {
    let rt = ctx.runtime;
    let argc = range.len();
    let mut stack_args = [0usize; 8];
    let mut heap_args: Vec<usize> = Vec::new();
    let args_ptr = if argc == 0 {
        std::ptr::null()
    } else if argc <= stack_args.len() {
        for (i, reg) in range.enumerate() {
            stack_args[i] = args.get(reg).raw();
        }
        stack_args.as_ptr()
    } else {
        heap_args.reserve_exact(argc);
        for reg in range {
            heap_args.push(args.get(reg).raw());
        }
        heap_args.as_ptr()
    };
    JIT_FRAME_DEPTH.with(|depth| {
        let _guard = enter_jit_frame(depth).ok_or_else(jit_stack_exhausted)?;
        let allow_handoff =
            resumable && vm::task_scheduler::in_scheduler(rt) && !rt.has_pinned_rust_frame();
        let slice_limited = allow_handoff && vm::task_scheduler::needs_suspendable_calls(rt);
        let _pin = rt.pin_rust_frame();
        let mut jit_ctx = JitCallContext {
            cancel_flag: rt.cancellation_flag_ptr(),
            remaining_steps: rt.remaining_steps_ptr(),
            runtime: rt as *const Runtime,
            slice_steps: if slice_limited {
                &rt.native_slice_steps
            } else {
                std::ptr::null()
            },
            exec: ctx.exec.as_ptr().cast::<ExecState<'static>>(),
            // Self calls use the function's ordinary entry, even when this
            // activation starts from an already executing loop.
            current_entry: osr.is_none().then_some(c_entry),
            pending_fault: None,
            self_call: None,
            frame_depth: depth,
            pending_tail_call: None,
            resumable,
            allow_handoff,
            resume_after_call: false,
            code: ctx.code.clone(),
            osr,
        };
        if slice_limited {
            debug_assert!(rt.native_slice_steps.get() != 0);
            // SAFETY: `slice_steps` points to `rt.native_slice_steps` and `jit_ctx` is active.
            unsafe { jit_ctx.charge_slice() };
        }
        let mut jit_ret = JitReturn::zeroed();
        // SAFETY: `c_entry` adheres to `JitCEntryFn` and `jit_ctx` + `jit_ret` are valid for the call.
        unsafe { c_entry.invoke(&mut jit_ctx, target.raw(), args_ptr, argc, &mut jit_ret) };
        let result = match jit_ret.tag {
            JitReturn::TAG_ONE => NativeReturn::One(Word::from_raw(jit_ret.single_value)),
            JitReturn::TAG_VALUES => NativeReturn::Values,
            JitReturn::TAG_UNWIND => {
                if let Some(restart) = unsafe { jit_ctx.take_debug_restart(jit_ret.target_frame) } {
                    return restart.map(NativeOutcome::TailCall);
                }
                NativeReturn::Unwind {
                    target_frame: jit_ret.target_frame,
                    state: jit_ret.state,
                }
            }
            JitReturn::TAG_TAIL_CALL => {
                return jit_ctx.take_tail_call().map(NativeOutcome::TailCall);
            }
            JitReturn::TAG_VM_RESUME if allow_handoff => return Ok(NativeOutcome::ResumeVm),
            JitReturn::TAG_FAULT => return Err(jit_ctx.take_fault()),
            other => {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(0, format!("invalid JitReturn tag {other}")),
                ));
            }
        };
        Ok(NativeOutcome::Returned(result))
    })
}
