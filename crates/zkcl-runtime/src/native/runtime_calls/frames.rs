use std::sync::atomic::AtomicUsize;

use super::super::context::{raw_slice, with_native_ctx};
use super::super::{JitCallContext, Word};
use crate::vm::{ExecFrame, JitFrameEntry};

/// Allocate a native frame and expose its registered root slots.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Each non-null output must point to writable,
/// aligned storage for its declared type. Access only the returned `slots_len` slots and release
/// the frame when leaving the activation. Its slot pointer expires when the frame is removed,
/// including during unwinding.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_alloc_root_frame(
    ctx: *mut JitCallContext,
    slots_len: usize,
    out_slots_ptr: *mut *const AtomicUsize,
    out_frame_index: *mut usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` and output-pointer contracts.
    let jit_ctx = unsafe { &mut *ctx };
    let code = jit_ctx.code.clone();
    let (runtime, exec) = unsafe { jit_ctx.runtime_and_exec_mut() };
    let slots = runtime.root_stack.alloc(slots_len);
    // SAFETY: `'rt` of `ExecState` outlives this LIFO frame popped by `drop_root_frame`.
    let slots: crate::heap::RootStackFrame<'static> = unsafe { std::mem::transmute(slots) };
    let slots_ptr = slots.slots().as_ptr();
    let frame_index = exec.frames.len();
    let control_base = exec.control.len();
    exec.frames
        .push(ExecFrame::Native(crate::vm::NativeFrameAnchor {
            slots_ptr,
            slots_len,
            pending_state: None,
            backing: Some(slots),
            arguments: crate::vm::ArgumentTail::default(),
            profile: code
                .as_deref()
                .and_then(|code| runtime.profile_native_activation(code)),
            code,
            source_offset: None,
            debug: None,
            debug_arguments: None,
        }));
    exec.jit_frames.push(JitFrameEntry {
        frame_index,
        control_base,
    });
    if !out_slots_ptr.is_null() {
        // SAFETY: Caller guarantees non-null `out_slots_ptr` is writable.
        unsafe { *out_slots_ptr = slots_ptr };
    }
    if !out_frame_index.is_null() {
        // SAFETY: Caller guarantees non-null `out_frame_index` is writable.
        unsafe { *out_frame_index = frame_index };
    }
}

/// Release the indicated native frame and any younger native frames.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. Use the frame index obtained for the activation
/// being left. Every removed frame's slot pointer must be retired before its storage is released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_drop_root_frame(ctx: *mut JitCallContext, frame_index: usize) {
    // SAFETY: Caller upholds the `JitCallContext` contract and retires slot pointers first.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let exec = native_ctx.exec_mut();
            while exec
                .jit_frames
                .last()
                .is_some_and(|entry| entry.frame_index >= frame_index)
            {
                let entry = exec.jit_frames.pop().unwrap();
                if matches!(
                    exec.frames.get(entry.frame_index),
                    Some(ExecFrame::Native(_))
                ) {
                    exec.control
                        .truncate_frame_scopes(entry.frame_index, entry.control_base);
                    exec.frames.truncate(entry.frame_index);
                }
            }
        });
    }
}

/// Replace the values buffer with a single word.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_set_one_value(ctx: *mut JitCallContext, word: usize) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx.set_one_value(Word::from_raw(word));
        })
    }
}

/// Read the indexed value, returning NIL beyond the current values count.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_get_value(ctx: *mut JitCallContext, index: usize) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe { with_native_ctx(ctx, |native_ctx| native_ctx.value(index).raw()) }
}

/// Replace the values buffer from a native array, or clear it for null input.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. For a non-null `words_ptr` and positive `count`,
/// supply `count` Lisp words in storage separate from the values buffer being resized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_set_values(
    ctx: *mut JitCallContext,
    words_ptr: *const usize,
    count: usize,
) {
    // SAFETY: Caller guarantees `words_ptr` holds `count` words outside the values buffer.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            // Word is transparent over usize; raw_slice borrows only for set_values's copy.
            let words = raw_slice(words_ptr.cast::<Word>(), count);
            if words.is_empty() {
                native_ctx.clear_values();
            } else {
                native_ctx.set_values(words);
            }
        });
    }
}

/// Push the current values onto the control stack for this frame.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_save_values(ctx: *mut JitCallContext, frame_index: usize) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let exec = native_ctx.exec_mut();
            exec.control.save_values(frame_index, &mut exec.values);
        });
    }
}

/// Append the current values to the saved group.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The innermost control scope must be the
/// saved-values group to extend.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_append_values(ctx: *mut JitCallContext) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let exec = native_ctx.exec_mut();
            exec.control.append_values(&mut exec.values);
        });
    }
}

/// Restore the saved group as the current values.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The innermost control scope must be the
/// saved-values group to pop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_restore_values(ctx: *mut JitCallContext) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            let exec = native_ctx.exec_mut();
            exec.control.restore_values(&mut exec.values);
        });
    }
}
