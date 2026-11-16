//! Canonical source safepoints for native code compiled with retained debug frames.
//!
//! The compiler publishes tagged canonical registers before each instruction and
//! reloads them afterward. The snapshot owns GC roots; neither a machine address
//! nor a pointer into a code generator's temporary argument buffer escapes.

use std::cell::Cell;

use zkcl_bytecode::{Instruction, RegisterSet};

use super::context::{raw_slice, with_native_ctx, write_abi_result};
use super::{JitCallContext, JitReturn, Word};
use crate::heap::RootFrame;
use crate::vm::{self, ExecFrame, ExecState, NativeFrameAnchor};

pub(crate) struct NativeDebugState {
    pub(crate) pc: usize,
    registers: RootFrame,
    dirty: Vec<Cell<bool>>,
    unwind_slots: Vec<Option<usize>>,
}

impl NativeFrameAnchor {
    pub(crate) fn debug_pc(&self) -> Option<usize> {
        self.debug.as_ref().map(|state| state.pc)
    }

    pub(crate) fn debug_register(&self, index: usize) -> Option<Word> {
        let registers = &self.debug.as_ref()?.registers;
        (index < registers.len()).then(|| registers.get(index))
    }

    pub(crate) fn set_debug_register(&self, index: usize, value: Word) -> bool {
        let Some(state) = &self.debug else {
            return false;
        };
        if index >= state.registers.len() {
            return false;
        }
        state.registers.set(index, value);
        state.dirty[index].set(true);
        if let Some(slot) = state.unwind_slots.get(index).copied().flatten() {
            self.slots().set(slot, value);
        }
        true
    }
}

fn active_native_frame_index(exec: &ExecState<'_>) -> usize {
    exec.jit_frames
        .last()
        .expect("native debug frame")
        .frame_index
}

fn active_native_frame<'a>(exec: &'a ExecState<'_>) -> &'a NativeFrameAnchor {
    let frame_index = active_native_frame_index(exec);
    let ExecFrame::Native(frame) = &exec.frames[frame_index] else {
        unreachable!("native debug state belongs to a native activation")
    };
    frame
}

fn active_native_frame_mut<'a>(exec: &'a mut ExecState<'_>) -> (usize, &'a mut NativeFrameAnchor) {
    let frame_index = active_native_frame_index(exec);
    let ExecFrame::Native(frame) = &mut exec.frames[frame_index] else {
        unreachable!("native debug state belongs to a native activation")
    };
    (frame_index, frame)
}

/// Publish a complete canonical register map and observe debugger controls.
///
/// # Safety
/// The context is live and `registers` supplies `count` tagged words. Generated
/// code roots every live word across this call and reloads its debug registers
/// before executing the corresponding canonical instruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_debug_point(
    ctx: *mut JitCallContext,
    pc: usize,
    registers: *const usize,
    count: usize,
    out: *mut JitReturn,
) {
    // SAFETY: Caller guarantees `registers` supplies `count` tagged words and `ctx`/`out` are valid.
    let register_words = unsafe { raw_slice(registers, count) };
    unsafe {
        write_abi_result(ctx, out, |scope| {
            let runtime = scope.native.runtime;
            let slots = runtime.root_frame(count);
            for (index, &bits) in register_words.iter().enumerate() {
                slots.set(index, Word::from_raw(bits));
            }
            let exec = scope.native.exec_mut();
            let (frame_index, frame) = active_native_frame_mut(exec);
            frame.source_offset = frame
                .code
                .as_ref()
                .and_then(|code| code.canonical.get(pc))
                .and_then(Instruction::source_offset);
            // Edits to a caller paused inside its callee have not yet reached
            // machine SSA values. Carry them into the caller's next source point.
            let unwind_slots = if let Some(previous) = frame.debug.take() {
                let writes = frame
                    .code
                    .as_ref()
                    .and_then(|code| code.canonical.get(previous.pc))
                    .map_or(RegisterSet::Empty, |op| op.register_operands().writes);
                for (index, dirty) in previous.dirty.iter().enumerate().take(count) {
                    if dirty.get() && !writes.contains(index) {
                        slots.set(index, previous.registers.get(index));
                    }
                }
                previous.unwind_slots
            } else {
                Vec::new()
            };
            frame.debug = Some(NativeDebugState {
                pc,
                registers: slots,
                dirty: (0..count).map(|_| Cell::new(false)).collect(),
                unwind_slots,
            });
            runtime.profile_sample();
            let exact_vm_boundary = frame.code.as_ref().is_some_and(|code| {
                code.vm_pc(pc).and_then(|vm_pc| code.canonical_pc(vm_pc)) == Some(pc)
            });
            let handoff = scope.allow_handoff && exact_vm_boundary && exec.host_debugging;
            let Some((breakpoint, at)) =
                vm::debugging::native_debug_stop(runtime, exec, pc, handoff)
            else {
                return Ok(JitReturn::zeroed());
            };
            if handoff {
                let ExecFrame::Native(frame) = &exec.frames[frame_index] else {
                    unreachable!("the source point owns this activation")
                };
                let state = frame.debug.as_ref().expect("published source registers");
                let registers: Vec<_> = (0..count).map(|index| index as u32).collect();
                let words: Vec<_> = (0..count)
                    .map(|index| state.registers.get(index).raw())
                    .collect();
                let target = frame.slots().get(0);
                vm::recover_native_frame(runtime, exec, target, pc, &registers, &words, false)?;
                exec.native_debug_stop = Some(breakpoint);
                return Ok(JitReturn::control(JitReturn::TAG_VM_RESUME));
            }
            vm::debugging::drive_native_pause(runtime, exec, at, breakpoint)
                .map(JitReturn::from_native)
        });
    }
}

/// Reload a canonical register after a native debugger safepoint.
///
/// # Safety
/// The context and innermost native debug snapshot are live; `index` is a valid
/// canonical register for the same snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_debug_register(ctx: *mut JitCallContext, index: usize) -> usize {
    // SAFETY: Caller guarantees `ctx` and its innermost debug snapshot are live.
    unsafe {
        with_native_ctx(ctx, |native| {
            let frame = active_native_frame(native.exec_mut());
            let state = frame.debug.as_ref().expect("mapped native debug frame");
            state.dirty[index].set(false);
            state.registers.get(index).raw()
        })
    }
}

/// Preserve the actual incoming arguments for restarting a retained native frame.
///
/// # Safety
/// The context is live, and `arguments` supplies `count` tagged words. This
/// non-collecting entry runs in the prologue before any Lisp code executes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_stage_debug_arguments(
    ctx: *mut JitCallContext,
    arguments: *const usize,
    count: usize,
) {
    // SAFETY: Caller guarantees `ctx` is live and `arguments` supplies `count` tagged words.
    let arg_words = unsafe { raw_slice(arguments, count) };
    unsafe {
        with_native_ctx(ctx, |native| {
            let saved = native.runtime.root_frame(count);
            for (index, &bits) in arg_words.iter().enumerate() {
                saved.set(index, Word::from_raw(bits));
            }
            let (_, frame) = active_native_frame_mut(native.exec_mut());
            frame.debug_arguments = Some(saved);
        });
    }
}

/// Associate a canonical register with a root read by non-local native landings.
///
/// # Safety
/// The compiler supplies a valid canonical register and a valid root slot of
/// the innermost native frame. This non-collecting entry runs in its prologue.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_map_debug_unwind_register(
    ctx: *mut JitCallContext,
    register: usize,
    root_slot: usize,
) {
    // SAFETY: Caller guarantees `ctx` is live and `register`/`root_slot` are in bounds.
    unsafe {
        with_native_ctx(ctx, |native| {
            let (_, frame) = active_native_frame_mut(native.exec_mut());
            let state = frame.debug.get_or_insert_with(|| NativeDebugState {
                pc: 0,
                registers: RootFrame::empty(),
                dirty: Vec::new(),
                unwind_slots: Vec::new(),
            });
            state
                .unwind_slots
                .resize(state.unwind_slots.len().max(register + 1), None);
            state.unwind_slots[register] = Some(root_slot);
        });
    }
}
