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
    let observed = store(runtime, value, array, index);
    let format = match site.observed {
        SiteFeedback::ArrayWriter { format } if format != observed => None,
        _ => observed,
    };
    site.record(SiteFeedback::ArrayWriter { format });
    if observed.is_some() {
        crate::metrics::record_primitive_call();
        crate::metrics::record_primitive_call();
        values.one(value);
        true
    } else {
        false
    }
}

fn store(runtime: &Runtime, value: Word, array: Word, index: Word) -> Option<JitFloatFormat> {
    let index = usize::try_from(index.as_fixnum()?).ok()?;
    let (element, format) = if let Some(value) = runtime.heap.double_float(value) {
        if !value.is_finite() {
            return None;
        }
        (ArrayValue::DoubleFloat(value), JitFloatFormat::F64)
    } else {
        let value = runtime.heap.single_float(value)?;
        if !value.is_finite() {
            return None;
        }
        (ArrayValue::SingleFloat(value), JitFloatFormat::F32)
    };
    runtime
        .heap
        .write_simple_float_vector(array, index, element)
        .then_some(format)
}
