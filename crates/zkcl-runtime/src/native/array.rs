use crate::heap::{ArrayValue, Word};
use crate::vm::array_access::is_aref;

use super::{JitCallContext, native_function_constant};

/// Reads a finite float from a simple vector after validating the AREF binding.
/// Returns `u64::MAX` if the VM must handle the call.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `function` is a live function or
/// closure, `symbol` is its constant index, and `array` and `index` are live Lisp words.
#[unsafe(no_mangle)]
pub(super) unsafe extern "C" fn zkcl_rt_read_float_vector(
    ctx: *mut JitCallContext,
    function: usize,
    symbol: usize,
    array: usize,
    index: usize,
    double: bool,
) -> u64 {
    let index = Word::from_raw(index)
        .as_fixnum()
        .and_then(|index| usize::try_from(index).ok());
    let Some(index) = index else {
        return u64::MAX;
    };
    // SAFETY: The active native entry keeps the runtime live across the call.
    let runtime = unsafe { &*(*ctx).runtime };
    let heap = &runtime.heap;
    let symbol = native_function_constant(runtime, Word::from_raw(function), symbol);
    if !heap.symbol_cell(symbol, true).is_some_and(is_aref) {
        return u64::MAX;
    }
    match heap.simple_float_vector_value(Word::from_raw(array), index) {
        Some(ArrayValue::SingleFloat(value)) if !double && value.is_finite() => {
            u64::from(value.to_bits())
        }
        Some(ArrayValue::DoubleFloat(value)) if double && value.is_finite() => value.to_bits(),
        _ => u64::MAX,
    }
}
