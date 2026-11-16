use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootSlots, Word};
use crate::number::{Number, Real};
use crate::{EvalError, EvalErrorKind, Runtime};

use super::numbers::Precision;
use super::sequence_types::ResultSequence;
use super::{
    arrays, characters, function_cell, function_definitions, named_keyword_arguments, numbers,
    types,
};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    MakeSequence,
    Coerce,
}

pub(super) const PRIMITIVES: &[(&str, Op)] =
    &[("MAKE-SEQUENCE", Op::MakeSequence), ("COERCE", Op::Coerce)];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::MakeSequence => count >= 2,
            Self::Coerce => count == 2,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::MakeSequence => make_sequence(runtime, frame, args, at),
        Op::Coerce => coerce(runtime, frame, args, at),
    }
}

fn make_sequence(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let [initial] = named_keyword_arguments(
        runtime,
        frame,
        args.start + 2..args.end,
        ["INITIAL-ELEMENT"],
        at,
    )?;
    let result_type = ResultSequence::parse(runtime, frame.get(args.start), at)?;
    result_type.repeated(
        runtime,
        frame.get(args.start + 1),
        initial.map(|reg| frame.get(reg)),
        at,
    )
}

fn coerce(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let object = || frame.get(args.start);
    let target = || frame.get(args.start + 1);
    if types::typep(runtime, object(), target(), at).unwrap_or(false) {
        return Ok(object());
    }
    let impossible = || {
        Fault::type_error_specifier(runtime, 
            object(),
            target(),
            EvalError::new(at, "cannot coerce the object to the requested type"),
        )
    };
    let heap = &runtime.heap;
    let named = |word| types::specifier_name(runtime, word);
    let (name, type_args) = match heap.cons_fields(target()) {
        Some((head, rest)) => (named(head), Some(rest)),
        None => {
            let target = target();
            (
                named(heap.class_info(target).map_or(target, |info| info.name)),
                None,
            )
        }
    };
    let convert = |real, format| {
        numbers::coerce(real, format).map_err(|class| {
            numbers::named_arithmetic_fault(runtime, frame, "COERCE", args.clone(), at, class)
        })
    };
    let allocate = |number| {
        heap.allocate_number(&number)
            .map_err(|error| EvalError::out_of_memory(at, error).into())
    };
    match (name.as_deref(), type_args) {
        (Some("CHARACTER"), None) => {
            return characters::run(
                runtime,
                frame,
                characters::Op::Character,
                args.start..args.start + 1,
                at,
            );
        }
        (Some("FUNCTION"), None) => return coerce_function(runtime, object(), at, impossible),
        (Some("COMPLEX"), rest) => {
            let format = match rest {
                None | Some(Word::NIL) => None,
                Some(rest) => {
                    let Some(part) = types::single_argument(heap, rest) else {
                        return Err(impossible());
                    };
                    if named(part).as_deref() == Some("*") {
                        None
                    } else {
                        let part = types::upgraded_complex_part_type(runtime, part, at)
                            .map_err(|_| impossible())?;
                        match named(part).as_deref() {
                            Some("RATIONAL") => Some(Precision::Exact),
                            Some("FLOAT" | "SINGLE-FLOAT") => Some(Precision::Single),
                            Some("DOUBLE-FLOAT") => Some(Precision::Double),
                            Some("REAL") => None,
                            _ => return Err(impossible()),
                        }
                    }
                }
            };
            let (real, imaginary) = match heap.number(object()) {
                Some(Number::Real(real)) => (real, Real::Integer(0.into())),
                Some(Number::Complex(real, imaginary)) => (real, imaginary),
                None => return Err(impossible()),
            };
            let original_format = numbers::precision(&real);
            let format = format.unwrap_or(original_format);
            if format == Precision::Exact {
                // An exact real with zero imaginary part remains a real.
                return (original_format == Precision::Exact)
                    .then(object)
                    .ok_or_else(impossible);
            }
            return allocate(Number::Complex(
                convert(real, format)?,
                convert(imaginary, format)?,
            ));
        }
        (
            Some(name @ ("FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "LONG-FLOAT")),
            None,
        ) => {
            let Some(Number::Real(real)) = heap.number(object()) else {
                return Err(impossible());
            };
            let format = if matches!(name, "DOUBLE-FLOAT" | "LONG-FLOAT") {
                Precision::Double
            } else {
                Precision::Single
            };
            return allocate(Number::Real(convert(real, format)?));
        }
        _ => {}
    }
    let result_type = ResultSequence::parse(runtime, target(), at).map_err(|_| impossible())?;
    let elements = arrays::sequence_elements(runtime, object(), at).map_err(|fault| {
        if fault.eval_error().kind() == EvalErrorKind::OutOfMemory {
            fault
        } else {
            impossible()
        }
    })?;
    let count = elements.len();
    if !result_type.matches_length(count) {
        return Err(impossible());
    }
    result_type.build(runtime, &elements, 0..count, at)
}

fn coerce_function(
    runtime: &Runtime,
    object: Word,
    at: usize,
    impossible: impl FnOnce() -> Fault,
) -> Result<Word, Fault> {
    if runtime.heap.is_symbol(object) {
        return function_cell(runtime, object, at);
    }
    match runtime.heap.cons_fields(object).map(|(head, _)| head) {
        Some(head)
            if runtime.find_named_symbol(&crate::symbol::Name::common_lisp("LAMBDA"))
                == Some(head) =>
        {
            function_definitions::compile_definition(runtime, object, at)
                .map(|(function, _)| function)
        }
        Some(head)
            if runtime.find_named_symbol(&crate::symbol::Name::common_lisp("SETF"))
                == Some(head) =>
        {
            function_definitions::inspect(
                runtime,
                function_definitions::DefinitionOp::Fdefinition,
                object,
                at,
            )
        }
        _ => Err(impossible()),
    }
}
