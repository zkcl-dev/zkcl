use zkcl_bytecode::{LaneKind, PackShape, SimdPack};

use super::context::{with_native_ctx, write_native_result};
use super::runtime_calls::deopt_unboxed;
use super::{Fault, JitCallContext, JitReturn, NativeReturn, Word};
use crate::{EvalError, Runtime};

const I32X4: PackShape = PackShape::new(LaneKind::I32, 4).unwrap();

/// Copy the native-endian lanes of an exact i32x4 without allocating or collecting.
///
/// # Safety
///
/// `ctx` must be a current JIT call context, `value` a live Lisp word, and `out` a writable
/// 16-byte output buffer. The buffer is only initialized on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_unbox_i32x4(
    ctx: *mut JitCallContext,
    value: usize,
    out: *mut [u32; 4],
) -> bool {
    // SAFETY: Caller upholds the `JitCallContext` and `out` pointer contracts.
    unsafe {
        if JitCallContext::try_runtime(ctx).is_none() {
            return false;
        }
        with_native_ctx(ctx, |native_ctx| {
            let Some(lanes) = native_ctx
                .runtime
                .heap
                .simd_i32x4_lanes(Word::from_raw(value))
            else {
                return false;
            };
            out.write_unaligned(lanes);
            true
        })
    }
}

pub(super) fn allocate_i32x4(runtime: &Runtime, lanes: [u32; 4], at: usize) -> Result<Word, Fault> {
    let pack = SimdPack::from_lanes(I32X4, &lanes.map(u64::from)).expect("i32x4 lane layout");
    runtime
        .heap
        .allocate_simd_pack(pack)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

/// Allocate a fresh Lisp pack from a staged native-endian vector.
///
/// # Safety
///
/// Follow the JIT call contract. `bits` points to a fully initialized 16-byte vector spill.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_box_i32x4(
    ctx: *mut JitCallContext,
    bits: *const [u32; 4],
    at: usize,
    ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds `bits`, `ctx`, and `ret` contracts.
    let lanes = unsafe { bits.read_unaligned() };
    unsafe {
        write_native_result(ctx, ret, |native| {
            allocate_i32x4(native.runtime, lanes, at).map(NativeReturn::One)
        })
    }
}

/// Materialize a deoptimization state with optional i32x4 identities alongside floats.
///
/// # Safety
///
/// Follow the JIT call contract. The register, scalar and format arrays each contain
/// `count` initialized elements. The pack buffer has `count` slots with initialized payloads
/// at the first register for each distinct pack identity.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_deopt_simd_to_vm(
    ctx: *mut JitCallContext,
    target: usize,
    pc: usize,
    registers: *const u32,
    scalars: *const u64,
    formats: *const u8,
    packs: *const [u32; 4],
    count: usize,
    ret: *mut JitReturn,
) {
    unsafe {
        deopt_unboxed(
            ctx,
            target,
            pc,
            registers,
            scalars,
            formats,
            Some(packs),
            count,
            ret,
        )
    }
}
