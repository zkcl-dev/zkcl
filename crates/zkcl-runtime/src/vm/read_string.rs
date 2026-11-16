use std::ops::Range;

use super::{Values, named_keyword_arguments, strings};
use crate::condition::Fault;
use crate::form_value;
use crate::heap::{RootSlots, StreamElement, StreamKind, Word};
use crate::read::Reader;
use crate::{EvalError, EvalErrorKind, Runtime};

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let string = frame.get(args.start);
    let string = runtime.heap.string_contents(string).ok_or_else(|| {
        Fault::type_error(runtime, string, "STRING", EvalError::new(at, "expected a string"))
    })?;
    let eof_error = args.len() < 2 || frame.get(args.start + 1) != Word::NIL;
    let [start, end, preserve] = named_keyword_arguments(
        runtime,
        frame,
        (args.start + 3).min(args.end)..args.end,
        ["START", "END", "PRESERVE-WHITESPACE"],
        at,
    )?;
    let (range, start) = strings::bounds(
        runtime,
        &string,
        start.map(|reg| frame.get(reg)),
        end.map(|reg| frame.get(reg)),
        "READ-FROM-STRING bounds are outside the string",
        at,
    )?;
    let preserve = preserve.is_some_and(|reg| frame.get(reg) != Word::NIL);
    let start_byte = range.start;
    let input = &string[range];

    let mut reader = Reader::new(runtime, input);
    let form = reader
        .next_form_for_read(false, preserve)
        .map_err(|error| read_fault(runtime, error, at, start_byte, input))?;
    let position = start + input[..reader.read_position(preserve)].chars().count();
    let position = isize::try_from(position)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| EvalError::new(at, "read position exceeds the supported fixnum range"))?;
    let result = match form {
        Some(form) => form_value::to_value(runtime, &form)?,
        None if eof_error => {
            let error = EvalError::unexpected_read_eof(input.len()..input.len(), "end of input");
            return Err(read_fault(runtime, error, at, start_byte, input));
        }
        None if args.len() >= 3 => runtime.root(frame.get(args.start + 2)),
        None => runtime.nil(),
    };
    values.resize(2);
    values.slots.set(0, result.slot.get());
    values.slots.set(1, position);
    Ok(())
}

fn read_fault(
    runtime: &Runtime,
    error: EvalError,
    at: usize,
    start_byte: usize,
    input: &str,
) -> Fault {
    if error.kind() != EvalErrorKind::Read {
        return Fault::from(error);
    }
    let (class, host) = error.in_lisp_read(at, start_byte);
    // READ-FROM-STRING reads an internal string stream; reader-error and
    // end-of-file conditions name it through their STREAM slot.
    let contents = match runtime.heap.string(input) {
        Ok(word) => runtime.root_slot(word),
        Err(error) => return Fault::from(EvalError::out_of_memory(at, error)),
    };
    let stream = match runtime.heap.stream(
        StreamKind::StringInput,
        StreamElement::Character,
        0,
        input.len(),
        || contents.get(),
    ) {
        Ok(word) => word,
        Err(error) => return Fault::from(EvalError::out_of_memory(at, error)),
    };
    Fault::stream_error(runtime, class, stream, host)
}
