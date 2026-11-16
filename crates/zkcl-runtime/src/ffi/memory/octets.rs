use std::ptr;

use super::{CallResult, Pointer, arity, integer};
use crate::heap::ArrayElement;
use crate::{EvalError, Root, Runtime};

pub(super) fn read<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    copy(rt, args, at, Direction::FromForeign)
}

pub(super) fn write<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    copy(rt, args, at, Direction::ToForeign)
}

enum Direction {
    FromForeign,
    ToForeign,
}

fn copy<'rt>(
    rt: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
    direction: Direction,
) -> CallResult<'rt> {
    let [pointer, vector, start, end, offset] = args else {
        return Err(arity(
            at,
            "expected a foreign pointer, octet vector, start, end and offset",
        ));
    };
    let pointer = Pointer::get(rt, pointer, at)?;
    let offset = integer(rt, offset, at)?;
    let word = vector.slot.get();
    let info = rt
        .heap
        .array_info(word)
        .filter(|info| info.rank == 1 && info.element == ArrayElement::Octet)
        .ok_or_else(|| EvalError::new(at, "expected a vector of (unsigned-byte 8)"))?;
    let start = integer(rt, start, at)?;
    let end = if end.is_nil() {
        info.fill_pointer.unwrap_or(info.size)
    } else {
        integer(rt, end, at)?
    };
    let bounds = || EvalError::new(at, "octet vector range is out of bounds");
    let count = end.checked_sub(start).ok_or_else(bounds)?;
    let vector = rt
        .heap
        .octet_array_address(word, start, count)
        .ok_or_else(bounds)?;

    if pointer.raw.is_null() {
        if count != 0 || offset != 0 {
            return Err(EvalError::new(at, "cannot dereference a null pointer").into());
        }
    } else {
        let foreign = pointer.offset(offset, at)?.access(count, at)?;
        let (source, destination) = match direction {
            Direction::FromForeign => (foreign.cast_const(), vector),
            Direction::ToForeign => (vector.cast_const(), foreign),
        };
        // SAFETY: both complete ranges were checked. The opted-in caller
        // supplies a valid range for an external pointer. No call can collect
        // or displace the vector between resolving its address and copying.
        unsafe { ptr::copy(source, destination, count) };
    }
    Ok(vec![
        rt.integer_from(count)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}
