use crate::heap::{ArrayValue, Word};
use crate::vm::{array_write::is_setf_aref, is_funcall};

use super::{JitCallContext, native_function_constant};

/// Checks the FUNCALL binding and stores a raw float through the built-in setter.
/// A false result leaves the array unchanged.
/// # Safety
/// Follow the [`JitCallContext`] call contract. `function` is a live function or
/// closure, `symbol` is its constant index, and `setter`, `array` and `index` are
/// live Lisp words.
#[unsafe(no_mangle)]
pub(super) unsafe extern "C" fn zkcl_rt_write_float_vector(
    ctx: *mut JitCallContext,
    function: usize,
    symbol: usize,
    setter: usize,
    array: usize,
    index: usize,
    bits: u64,
    double: bool,
) -> bool {
    let Some(index) = Word::from_raw(index)
        .as_fixnum()
        .and_then(|index| usize::try_from(index).ok())
    else {
        return false;
    };
    if !is_setf_aref(Word::from_raw(setter)) {
        return false;
    }
    // SAFETY: The active native entry keeps the runtime live, and this store cannot trigger GC.
    let runtime = unsafe { &*(*ctx).runtime };
    let symbol = native_function_constant(runtime, Word::from_raw(function), symbol);
    if !runtime
        .heap
        .symbol_cell(symbol, true)
        .is_some_and(is_funcall)
    {
        return false;
    }
    let value = if double {
        ArrayValue::DoubleFloat(f64::from_bits(bits))
    } else {
        ArrayValue::SingleFloat(f32::from_bits(bits as u32))
    };
    runtime
        .heap
        .write_simple_float_vector(Word::from_raw(array), index, value)
}
