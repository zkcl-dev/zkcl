use zkcl_bytecode::{Handler, HandlerTarget};

use super::super::context::{raw_slice, with_native_ctx, write_abi_result, write_native_result};
use super::super::{
    JitCallContext, JitReturn, NativeReturn, Word, native_begin_cleanup, native_end_cleanup,
    native_error, native_exit_token, native_leave_target, native_return_from, native_throw,
    native_transfer, native_unbind,
};
use crate::vm::ExecFrame;
use crate::vm::control_stack::{ExitPoint, NonLocalTransfer};

/// Push a dynamic binding owned by the calling frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `symbol` must name the special variable being
/// bound.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_bind_special(
    ctx: *mut JitCallContext,
    frame_index: usize,
    symbol: usize,
    value: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let heap = &native_ctx.runtime.heap;
            native_ctx.exec_mut().control.bind(
                frame_index,
                heap,
                std::iter::once((Word::from_raw(symbol), Word::from_raw(value))),
            );
        });
    }
}

/// Validate and push a PROGV binding scope owned by the calling frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Use the calling frame's index.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_bind_progv(
    ctx: *mut JitCallContext,
    frame_index: usize,
    symbols: usize,
    values: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let runtime = native_ctx.runtime;
            let result = native_ctx.exec_mut().control.bind_progv(
                runtime,
                frame_index,
                Word::from_raw(symbols),
                Word::from_raw(values),
                at,
            );
            match result {
                Ok(()) => Ok(NativeReturn::Values),
                // The current frame can still own a handler or cleanup for this error.
                Err(error) => native_error(native_ctx, error.into()),
            }
        });
    }
}

/// Restore dynamic bindings in reverse order.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The top `count` control scopes must all be dynamic
/// bindings owned by this activation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_unbind(ctx: *mut JitCallContext, count: usize) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe { with_native_ctx(ctx, |native_ctx| native_unbind(native_ctx, count)) };
}

/// Install an unwind-protect cleanup on the calling frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_protect(
    ctx: *mut JitCallContext,
    frame_index: usize,
    cleanup_state: usize,
    clear: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let runtime = native_ctx.runtime;
            native_ctx
                .exec_mut()
                .control
                .protect(runtime, frame_index, cleanup_state, clear);
        });
    }
}

/// Enter an unwind-protect cleanup on normal fallthrough.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The innermost scope must be the matching
/// protection, with the protected result in the values buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_begin_cleanup(ctx: *mut JitCallContext) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_begin_cleanup(native_ctx);
        })
    }
}

/// Finish a cleanup and resume any pending transfer.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The innermost scope must be the cleanup being
/// completed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_end_cleanup(ctx: *mut JitCallContext, out_ret: *mut JitReturn) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, native_end_cleanup);
    }
}

/// Transfer the current values to the catch matching `tag`.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_throw(
    ctx: *mut JitCallContext,
    tag: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            native_throw(native_ctx, Word::from_raw(tag), at)
        });
    }
}

/// Return the current values to the block identified by `token`.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_return_from(
    ctx: *mut JitCallContext,
    token: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            native_return_from(native_ctx, Word::from_raw(token), at)
        });
    }
}

/// Install a catch target on the native frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `tag_slot` must index the frame slot holding the
/// catch tag for the duration of the scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_enter_catch(
    ctx: *mut JitCallContext,
    frame_index: usize,
    tag_slot: usize,
    resume_state: usize,
    clear: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.exec_mut().control.enter_exit_point(
                frame_index,
                ExitPoint::Catch(tag_slot),
                resume_state,
                clear,
            );
        });
    }
}

/// Install a lexical block exit on the native frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Use the exit token allocated for this block
/// activation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_enter_block(
    ctx: *mut JitCallContext,
    frame_index: usize,
    token: usize,
    resume_state: usize,
    clear: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.exec_mut().control.enter_exit_point(
                frame_index,
                ExitPoint::Block(Word::from_raw(token)),
                resume_state,
                clear,
            );
        });
    }
}

/// Remove the innermost catch, block, or tagbody exit point.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The exit point being left must be the innermost
/// control scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_leave_target(ctx: *mut JitCallContext) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe { with_native_ctx(ctx, native_leave_target) };
}

/// Allocate a lexical exit token for a block or tagbody.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_exit_token(
    ctx: *mut JitCallContext,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            native_exit_token(native_ctx.runtime, at).map(NativeReturn::One)
        });
    }
}

/// Consume a frame's pending unwind state, encoded as state plus one or zero if absent.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Use the index of the activation resuming execution.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_pending_unwind_state(
    ctx: *mut JitCallContext,
    frame_index: usize,
) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            match native_ctx.exec_mut().frames.get_mut(frame_index) {
                Some(ExecFrame::Native(anchor)) => anchor.pending_state.take().map_or(0, |s| s + 1),
                _ => 0,
            }
        })
    }
}

/// Install the native states for a lexical tagbody.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Use its activation token. The `count` consecutive
/// states starting at `base_state` must correspond to its tags in label order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_enter_tagbody(
    ctx: *mut JitCallContext,
    frame_index: usize,
    token: usize,
    base_state: usize,
    count: usize,
    clear: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.exec_mut().control.enter_exit_point(
                frame_index,
                ExitPoint::Tagbody(Word::from_raw(token), count),
                base_state,
                clear,
            );
        });
    }
}

/// Transfer control to the label selected by a tagbody token.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_go(
    ctx: *mut JitCallContext,
    token: usize,
    label: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            if !scope.resumable
                && let Err(error) = scope.native.runtime.check_execution_safepoint(at)
            {
                return native_error(&mut scope.native, error.into()).map(JitReturn::from_native);
            }
            native_transfer(
                &mut scope.native,
                NonLocalTransfer::Go(Word::from_raw(token), label),
                at,
            )
            .map(JitReturn::from_native)
        });
    }
}

/// Rewrite bytecode handler targets/condition registers in `handlers` to native unwind slots.
fn rewrite_handler_targets(
    handlers: &mut [Handler],
    states: &[usize],
    condition_slots: &[usize],
    root_count: usize,
) {
    let mut case = 0;
    for handler in handlers {
        if let HandlerTarget::Case {
            pc,
            condition,
            clear,
        } = &mut handler.target
        {
            *pc = states[case];
            *condition = condition_slots[case];
            // SSA handles the bytecode register clear at the landing block.
            // Native root slots have a different layout and hold its live inputs.
            *clear = root_count;
            case += 1;
        }
    }
    debug_assert_eq!(case, states.len());
}

/// Install the handler cluster of table on the AOT frame, rewriting each
/// handler's bytecode target and condition register to AOT unwind state and
/// root slot indices.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `target` and `table` must identify the current
/// function and its handler table. For each case handler, in table order, `states` and
/// `condition_slots` must supply its native state and writable frame slot index. `count` must equal
/// the number of case handlers. Both pointers may be null when that count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_enter_handlers(
    ctx: *mut JitCallContext,
    frame_index: usize,
    target: usize,
    table: usize,
    states: *const usize,
    condition_slots: *const usize,
    count: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `states`/`condition_slots` slice contracts.
    let states = unsafe { raw_slice(states, count) };
    let condition_slots = unsafe { raw_slice(condition_slots, count) };
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let runtime = native_ctx.runtime;
            let function = Word::from_raw(target);
            let code = runtime
                .heap
                .function_code(function)
                .expect("HANDLERS target is a function");
            let info = runtime
                .heap
                .function_info(code)
                .expect("HANDLERS target has function info");
            let vm_code = runtime.heap.code_store.get(info.code_id);
            let mut handlers = vm_code.handlers[table].handlers.clone();
            let exec = native_ctx.exec_mut();
            let root_count = exec.frames[frame_index].slots().len();
            rewrite_handler_targets(&mut handlers, states, condition_slots, root_count);
            exec.control
                .enter_native_handlers(frame_index, handlers, Some(table));
        });
    }
}

/// Associate the scope just entered with its canonical bytecode instruction.
///
/// # Safety
/// The context and origin must name the active generated scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_mark_scope(ctx: *mut JitCallContext, pc: usize) {
    // SAFETY: Caller guarantees `ctx` points to the active native context.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.exec_mut().control.mark_native_scope(pc);
        });
    }
}

/// Remove the innermost handler cluster.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The cluster being left must be the innermost
/// control scope.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_leave_handlers(ctx: *mut JitCallContext) {
    // SAFETY: Caller guarantees `ctx` points to the active native context.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.exec_mut().control.leave_handlers();
        });
    }
}
