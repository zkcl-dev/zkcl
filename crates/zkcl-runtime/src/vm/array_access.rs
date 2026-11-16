use crate::code_store::SiteFeedback;
use crate::condition::Fault;
use crate::heap::{ArrayValue, Word};
use crate::native::JitFloatFormat;
use crate::{EvalError, Runtime};

use super::call_site::CallSite;
use super::{Frame, Primitive, Register, Values, arrays, primitive};

const AREF: Primitive = Primitive::Array(arrays::Op::Aref);

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
    let raw = frame.slots.get(first + 1).as_fixnum().and_then(|index| {
        let index = usize::try_from(index).ok()?;
        runtime
            .heap
            .simple_float_vector_value(frame.slots.get(first), index)
    });
    let observed = if let Some(raw) = raw {
        let format = match raw {
            ArrayValue::SingleFloat(_) => JitFloatFormat::F32,
            ArrayValue::DoubleFloat(_) => JitFloatFormat::F64,
            ArrayValue::Word(_) | ArrayValue::Octet(_) => {
                unreachable!("simple float vector element")
            }
        };
        let word = raw
            .boxed(&runtime.heap)
            .map_err(|error| EvalError::out_of_memory(at(), error))?;
        values.one(word);
        Some(format)
    } else {
        primitive(runtime, &frame.slots, AREF, first..first + 2, values, at())?;
        None
    };
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
