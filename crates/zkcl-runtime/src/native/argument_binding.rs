use crate::vm::{ArgumentTail, ExecFrame};

use super::context::{raw_slice, with_native_ctx, write_native_result};
use super::{JitCallContext, JitReturn, NativeReturn, Word, native_function_constant};

fn argument_tail<'a>(
    frames: &'a mut [ExecFrame<'_>],
    index: usize,
) -> &'a mut ArgumentTail<'static> {
    let ExecFrame::Native(frame) = &mut frames[index] else {
        unreachable!("argument tail belongs to a native frame")
    };
    &mut frame.arguments
}

/// Copy the tail of the invocation's argument list into GC roots.
///
/// # Safety
/// Follow the [`JitCallContext`] contract. `frame` must be the live activation
/// and `args` must supply `count` words when `count` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_stage_arguments(
    ctx: *mut JitCallContext,
    frame: usize,
    args: *const usize,
    count: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `args` slice contracts.
    unsafe {
        let raw_args = raw_slice(args, count);
        with_native_ctx(ctx, |native| {
            let tail = ArgumentTail::new(native.runtime, count, |index| {
                Word::from_raw(raw_args[index])
            });
            *argument_tail(&mut native.exec_mut().frames, frame) =
                std::mem::transmute::<ArgumentTail<'_>, ArgumentTail<'static>>(tail);
        });
    }
}

/// Read a positional argument from the activation's rooted argument tail.
///
/// # Safety
/// Follow the [`JitCallContext`] contract and use the activation's live `frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_optional_argument(
    ctx: *mut JitCallContext,
    frame: usize,
    index: usize,
) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract and active frame index.
    unsafe {
        with_native_ctx(ctx, |native| {
            argument_tail(&mut native.exec_mut().frames, frame)
                .get(index)
                .raw()
        })
    }
}

/// Construct a Lisp list from the argument tail starting at `start`.
///
/// # Safety
/// Follow the [`JitCallContext`] contract and use the activation's live `frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rest_arguments(
    ctx: *mut JitCallContext,
    frame: usize,
    start: usize,
    at: usize,
    result: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `result` out-pointer contracts.
    unsafe {
        write_native_result(ctx, result, |native| {
            let runtime = native.runtime;
            let output = runtime.staging_stack.alloc(1);
            argument_tail(&mut native.exec_mut().frames, frame)
                .rest(runtime, start, &output, 0, at)?;
            Ok(NativeReturn::One(output.get(0)))
        });
    }
}

/// Match keyword pairs using the same binder as bytecode frames.
///
/// # Safety
/// Follow the [`JitCallContext`] contract. Use the live activation and a table
/// index from its underlying function's bytecode.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_check_keywords(
    ctx: *mut JitCallContext,
    frame: usize,
    target: usize,
    start: usize,
    table: usize,
    result: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `result` out-pointer contracts.
    unsafe {
        write_native_result(ctx, result, |native| {
            let runtime = native.runtime;
            let target = Word::from_raw(target);
            let function = runtime.heap.function_code(target).expect("callable target");
            let info = runtime.heap.function_info(function).expect("function info");
            let bytecode = runtime.heap.code_store.get(info.code_id);
            argument_tail(&mut native.exec_mut().frames, frame).check_keywords(
                runtime,
                &bytecode.keywords[table],
                |index| native_function_constant(runtime, target, index),
                start,
                0,
            )?;
            Ok(NativeReturn::One(Word::NIL))
        });
    }
}

/// Read the value selected by the last keyword check.
///
/// # Safety
/// Follow the [`JitCallContext`] contract and use the activation's live `frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_keyword_argument(
    ctx: *mut JitCallContext,
    frame: usize,
    index: usize,
    result: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `result` out-pointer contracts.
    unsafe {
        write_native_result(ctx, result, |native| {
            argument_tail(&mut native.exec_mut().frames, frame)
                .keyword(index, 0)
                .map(NativeReturn::One)
        });
    }
}

/// Release the argument tail when binding has finished.
///
/// # Safety
/// Follow the [`JitCallContext`] contract and use the activation's live `frame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_release_arguments(ctx: *mut JitCallContext, frame: usize) {
    unsafe {
        with_native_ctx(ctx, |ctx| {
            *argument_tail(&mut ctx.exec.as_mut().frames, frame) = ArgumentTail::default()
        });
    }
}
