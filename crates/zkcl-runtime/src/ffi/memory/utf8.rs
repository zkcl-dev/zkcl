//! UTF-8 byte counts and bulk writes for the foreign-pointer string codec.
use super::{CallResult, Pointer, arity, integer};
use crate::character::utf8_byte_range;
use crate::condition::Fault;
use crate::{EvalError, Root, Runtime};

fn with_source<R>(
    rt: &Runtime,
    string: &Root<'_>,
    start: &Root<'_>,
    end: &Root<'_>,
    at: usize,
    read: impl FnOnce(&str, usize, Option<usize>) -> Result<R, Fault>,
) -> Result<R, Fault> {
    let start = integer(rt, start, at)?;
    let end = if end.is_nil() {
        None
    } else {
        Some(integer(rt, end, at)?)
    };
    let word = string.slot.get();
    rt.heap
        .with_string_slice(word, |text| {
            let bytes = utf8_byte_range(text, start, end).ok_or_else(|| {
                EvalError::new(at, "source character range is outside the string")
            })?;
            read(&text[bytes], start, end)
        })
        .ok_or_else(|| {
            Fault::type_error(rt, 
                word,
                "STRING",
                EvalError::new(at, "expected a UTF-8 source string"),
            )
        })?
}

pub(super) fn prefix<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [string, start, end, limit] = args else {
        return Err(arity(
            at,
            "UTF8-PREFIX expects a string, start, end and byte limit (or NIL)",
        ));
    };
    let limit = if limit.is_nil() {
        usize::MAX
    } else {
        integer(rt, limit, at)?
    };
    let (length, next) = with_source(rt, string, start, end, at, |text, first, end| {
        let length = text.floor_char_boundary(limit.min(text.len()));
        let next = match end {
            Some(end) if length == text.len() => end,
            _ => first + text[..length].chars().count(),
        };
        Ok((length, next))
    })?;
    let make = |n| {
        rt.integer_from(n)
            .map_err(|e| EvalError::out_of_memory(at, e))
    };
    Ok(vec![make(length)?, make(next)?])
}

pub(super) fn write<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let (dst, string, start, end, offset) = match args {
        [dst, string, start, end] => (dst, string, start, end, 0),
        [dst, string, start, end, offset] => (dst, string, start, end, integer(rt, offset, at)?),
        _ => {
            return Err(arity(
                at,
                "WRITE-UTF8 expects a pointer, string, start, end and optional byte offset",
            ));
        }
    };
    let destination = Pointer::get(rt, dst, at)?.offset(offset, at)?;
    let size = with_source(rt, string, start, end, at, |bytes, _, _| {
        let address = destination.access(bytes.len(), at)?;
        // SAFETY: `destination.access` checked external writable bounds separate from Lisp storage.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), address, bytes.len()) };
        Ok(bytes.len())
    })?;
    Ok(vec![
        rt.integer_from(size)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}
