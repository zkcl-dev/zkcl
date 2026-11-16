use crate::Runtime;
use crate::code_store::SiteFeedback;
use crate::heap::{ArrayValue, Word};
use crate::native::JitFloatFormat;

use super::call_site::CallSite;
use super::{Frame, Primitive, Register, Values, arrays};

pub(crate) fn is_setf_aref(word: Word) -> bool {
    matches!(
        Primitive::decode(word),
        Some(Primitive::Array(arrays::Op::SetAref))
    )
}

/// The named call has resolved to FUNCALL and already evaluated all four operands.
pub(super) fn try_setf_aref(
    runtime: &Runtime,
    frame: &Frame<'_>,
    site: CallSite<'_>,
    start: Register,
    values: &mut Values,
) -> bool {
    let slots = &frame.slots;
    if !is_setf_aref(slots.get(start)) {
        if matches!(site.observed, SiteFeedback::ArrayWriter { .. }) {
            site.record(SiteFeedback::ArrayWriter { format: None });
        }
        return false;
    }
    let value = slots.get(start + 1);
    let array = slots.get(start + 2);
    let index = slots.get(start + 3);
    let stored = try_simple_write(runtime, array, index, value);
    let observed = match stored {
        Some(ArrayValue::DoubleFloat(value)) if value.is_finite() => Some(JitFloatFormat::F64),
        Some(ArrayValue::SingleFloat(value)) if value.is_finite() => Some(JitFloatFormat::F32),
        _ => None,
    };
    let format = match site.observed {
        SiteFeedback::ArrayWriter { format } if format != observed => None,
        _ => observed,
    };
    site.record(SiteFeedback::ArrayWriter { format });
    if stored.is_some() {
        crate::metrics::record_primitive_call();
        crate::metrics::record_primitive_call();
        values.one(value);
        true
    } else {
        false
    }
}

#[inline]
pub(super) fn try_simple_write(
    runtime: &Runtime,
    array: Word,
    index: Word,
    value: Word,
) -> Option<ArrayValue> {
    let index = usize::try_from(index.as_fixnum()?).ok()?;
    runtime.heap.write_simple_array_vector(array, index, value)
}
