use super::super::context::{
    AbiScope, raw_slice, write_abi_result, write_fault_return, write_native_result,
};
use super::super::{Fault, JitCallContext, JitReturn, NativeReturn, Word};
use crate::{EvalError, Runtime, vm};

fn restore_vm(
    scope: &mut AbiScope<'_, '_>,
    target: Word,
    pc: usize,
    regs: &[u32],
    vals: &[usize],
) -> Result<JitReturn, Fault> {
    if !scope.resumable {
        return vm::deopt_into_vm(
            scope.native.runtime,
            scope.native.exec_mut(),
            target,
            pc,
            regs,
            vals,
        )
        .map(JitReturn::from_native);
    }
    let after_call = scope.take_resume_after_call();
    let runtime = scope.native.runtime;
    let index = vm::recover_native_frame(
        runtime,
        scope.native.exec_mut(),
        target,
        pc,
        regs,
        vals,
        after_call,
    )?;
    if scope.allow_handoff {
        Ok(JitReturn::control(JitReturn::TAG_VM_RESUME))
    } else {
        vm::execute_until(runtime, scope.native.exec_mut(), index, None)
            .map(JitReturn::from_native)
            .map_err(Fault::from)
    }
}

/// Resume bytecode execution with the registers materialized by a deoptimization site.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. A null `ctx` or a detached context writes a fault
/// to `out_ret`. For a live context, supply the deoptimization map for `target_word` at canonical
/// `pc`. Each non-null array must supply `reg_count` elements of its declared type. Register
/// numbers and Lisp values are paired in order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_deopt_to_vm(
    ctx: *mut JitCallContext,
    target_word: usize,
    pc: usize,
    regs_ptr: *const u32,
    vals_ptr: *const usize,
    reg_count: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Validated before dereferencing `runtime` or `exec`.
    if unsafe { JitCallContext::from_raw(ctx) }.is_none() {
        unsafe { write_fault_return(out_ret) };
        return;
    }
    // SAFETY: Caller guarantees `regs_ptr` and `vals_ptr` supply `reg_count` elements when non-null.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            let regs = raw_slice(regs_ptr, reg_count);
            let vals = raw_slice(vals_ptr, reg_count);
            restore_vm(scope, Word::from_raw(target_word), pc, regs, vals)
        });
    }
}

/// Return the raw bits of a finite single float, or `u64::MAX` on mismatch.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `value` must be a valid Lisp word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_unbox_f32(ctx: *mut JitCallContext, value: usize) -> u64 {
    unsafe {
        unbox_float(ctx, |runtime| {
            runtime
                .heap
                .single_float(Word::from_raw(value))
                .filter(|float| float.is_finite())
                .map(|float| u64::from(float.to_bits()))
        })
    }
}

/// Return the raw bits of a finite double float, or `u64::MAX` on mismatch.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `value` must be a valid Lisp word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_unbox_f64(ctx: *mut JitCallContext, value: usize) -> u64 {
    unsafe {
        unbox_float(ctx, |runtime| {
            runtime
                .heap
                .double_float(Word::from_raw(value))
                .filter(|float| float.is_finite())
                .map(f64::to_bits)
        })
    }
}

unsafe fn unbox_float(ctx: *mut JitCallContext, read: impl FnOnce(&Runtime) -> Option<u64>) -> u64 {
    // SAFETY: Caller guarantees `ctx` is live; null `runtime` is rejected cleanly.
    let Some(runtime) = (unsafe { JitCallContext::try_runtime(ctx) }) else {
        return u64::MAX;
    };
    read(runtime).unwrap_or(u64::MAX)
}

fn box_native_float(
    runtime: &Runtime,
    float: crate::number::Real,
    at: usize,
) -> Result<Word, Fault> {
    runtime
        .heap
        .allocate_number(&crate::number::Number::Real(float))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

/// Box a finite single float from its raw bits.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The low 32 bits must encode a finite float.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_box_f32(
    ctx: *mut JitCallContext,
    bits: u64,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let float = crate::number::Real::Single(f32::from_bits(bits as u32));
            box_native_float(native_ctx.runtime, float, at).map(NativeReturn::One)
        })
    }
}

/// Box a finite double float from its raw bits.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `bits` must encode a finite float.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_box_f64(
    ctx: *mut JitCallContext,
    bits: u64,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let float = crate::number::Real::Double(f64::from_bits(bits));
            box_native_float(native_ctx.runtime, float, at).map(NativeReturn::One)
        })
    }
}

/// Materialize a deoptimization state containing unboxed, finite floats and resume bytecode.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The three input arrays must each have
/// `reg_count` valid elements and describe the state before the instruction at canonical `pc`.
/// Format byte 0 describes a tagged Lisp word, 32 describes binary32 bits in the low word,
/// and 64 describes binary64 bits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_deopt_float_to_vm(
    ctx: *mut JitCallContext,
    target_word: usize,
    pc: usize,
    regs_ptr: *const u32,
    vals_ptr: *const u64,
    formats_ptr: *const u8,
    reg_count: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the deoptimization ABI for float format tags.
    unsafe {
        deopt_unboxed(
            ctx,
            target_word,
            pc,
            regs_ptr,
            vals_ptr,
            formats_ptr,
            None,
            reg_count,
            out_ret,
        )
    }
}

/// Materialize a single deoptimization slot whose raw bits represent an unboxed float or SIMD pack.
unsafe fn materialize_deopt_slot(
    runtime: &Runtime,
    staged: &crate::heap::RootStackFrame<'_>,
    index: usize,
    bits: u64,
    format: u8,
    packs_ptr: Option<*const [u32; 4]>,
    pc: usize,
) -> Result<Option<Word>, Fault> {
    let word = match format {
        0 => return Ok(None),
        32 => box_native_float(
            runtime,
            crate::number::Real::Single(f32::from_bits(bits as u32)),
            pc,
        )?,
        64 => box_native_float(
            runtime,
            crate::number::Real::Double(f64::from_bits(bits)),
            pc,
        )?,
        zkcl_bytecode::native_abi::DEOPT_I32X4_FORMAT => {
            let first = bits as usize;
            if first == index {
                let packs = packs_ptr.expect("pack payloads for SIMD deoptimization");
                // SAFETY: Caller guarantees `packs` has an entry for every distinct SIMD pack.
                super::super::simd::allocate_i32x4(
                    runtime,
                    unsafe { packs.add(first).read_unaligned() },
                    pc,
                )?
            } else {
                debug_assert!(first < index);
                staged.get(first)
            }
        }
        _ => unreachable!("native deoptimization format"),
    };
    Ok(Some(word))
}

/// # Safety
///
/// The stack arrays and context must obey the deoptimization ABI for their format tags.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the two native deoptimization ABIs"
)]
pub(in crate::native) unsafe fn deopt_unboxed(
    ctx: *mut JitCallContext,
    target_word: usize,
    pc: usize,
    regs_ptr: *const u32,
    vals_ptr: *const u64,
    formats_ptr: *const u8,
    packs_ptr: Option<*const [u32; 4]>,
    reg_count: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller guarantees the input deoptimization arrays hold `reg_count` entries.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            let runtime = scope.native.runtime;
            let regs = raw_slice(regs_ptr, reg_count);
            let vals = raw_slice(vals_ptr, reg_count);
            let formats = raw_slice(formats_ptr, reg_count);
            let staged = runtime.staging_stack.alloc(reg_count + 1);
            staged.set(reg_count, Word::from_raw(target_word));
            for (index, (&bits, &format)) in vals.iter().zip(formats).enumerate() {
                if format == 0 {
                    staged.set(index, Word::from_raw(bits as usize));
                }
            }
            // Root every tagged input before allocating, and each materialized object as soon
            // as it exists. Alias records point to the first root slot for that pack identity.
            for (index, (&bits, &format)) in vals.iter().zip(formats).enumerate() {
                if let Some(value) =
                    materialize_deopt_slot(runtime, &staged, index, bits, format, packs_ptr, pc)?
                {
                    staged.set(index, value);
                }
            }
            let restored: Vec<usize> = (0..reg_count).map(|i| staged.get(i).raw()).collect();
            restore_vm(scope, staged.get(reg_count), pc, regs, &restored)
        });
    }
}
