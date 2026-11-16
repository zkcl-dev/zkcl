use std::ops::Range;

use crate::code_store::SiteFeedback;
use crate::condition::Fault;
use crate::heap::{ArrayValue, RootSlots, Word};
use crate::native::JitFloatFormat;
use crate::{EvalError, Runtime};

use super::call_site::CallSite;
use super::{Frame, Primitive, Register, Values, arrays};

pub(crate) fn is_aref(word: Word) -> bool {
    matches!(
        Primitive::decode(word),
        Some(Primitive::Array(arrays::Op::Aref))
    )
}

/// The caller resolved a two-argument call to the current AREF primitive.
pub(super) fn exec_named_aref(
    runtime: &Runtime,
    frame: &Frame<'_>,
    site: CallSite<'_>,
    first: Register,
    values: &mut Values,
    at: impl FnOnce() -> usize,
) -> Result<(), Fault> {
    crate::metrics::record_primitive_call();
    let (word, observed) = read_aref(runtime, &frame.slots, first..first + 2, at)?;
    values.one(word);
    let format = match site.observed {
        SiteFeedback::ArrayReader { format } => {
            if format.is_none() || format == observed {
                return Ok(());
            }
            None
        }
        _ => observed,
    };
    site.record(SiteFeedback::ArrayReader { format });
    Ok(())
}

#[inline]
pub(super) fn read_aref(
    runtime: &Runtime,
    slots: &RootSlots,
    args: Range<Register>,
    at: impl FnOnce() -> usize,
) -> Result<(Word, Option<JitFloatFormat>), Fault> {
    let array = slots.get(args.start);
    if args.len() == 2
        && let Some(raw) = try_simple_read(runtime, array, slots.get(args.start + 1))
    {
        let format = match raw {
            ArrayValue::SingleFloat(_) => Some(JitFloatFormat::F32),
            ArrayValue::DoubleFloat(_) => Some(JitFloatFormat::F64),
            ArrayValue::Word(_) | ArrayValue::Octet(_) => None,
        };
        let word = raw
            .boxed(&runtime.heap)
            .map_err(|error| EvalError::out_of_memory(at(), error))?;
        return Ok((word, format));
    }
    let subscripts = (args.start + 1..args.end).map(|reg| slots.get(reg));
    Ok((arrays::read_aref_slow(runtime, array, subscripts, at())?, None))
}

#[inline]
pub(super) fn try_simple_read(runtime: &Runtime, array: Word, index: Word) -> Option<ArrayValue> {
    let index = usize::try_from(index.as_fixnum()?).ok()?;
    runtime.heap.simple_array_vector_value(array, index)
}
