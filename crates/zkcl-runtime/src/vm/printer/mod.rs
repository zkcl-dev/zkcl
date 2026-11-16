use std::ops::Range;

use num_bigint::Sign;
use num_traits::ToPrimitive;

use crate::EvalError;
use crate::Runtime;
use crate::condition::{Class, Fault, named_word};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::printer::{self, PrintCase, PrintControls};
use crate::symbol::Name;
mod invocation;
pub(crate) use invocation::{PrintContext, PrintContinuation};

use super::{format::pprint, keyword_arguments, streams, symbol_value};

#[derive(Clone, Copy)]
pub(crate) enum PrinterPrimitive {
    DefaultMethod,
    Write,
    Prin1,
    Princ,
}

impl PrinterPrimitive {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::DefaultMethod => count == 4,
            Self::Write => count > 0,
            Self::Prin1 | Self::Princ => count == 1,
        }
    }
}

const PRINT_CONTROL_KEYWORDS: [(&str, &str); 15] = [
    ("BASE", "*PRINT-BASE*"),
    ("CASE", "*PRINT-CASE*"),
    ("CIRCLE", "*PRINT-CIRCLE*"),
    ("ESCAPE", "*PRINT-ESCAPE*"),
    ("GENSYM", "*PRINT-GENSYM*"),
    ("LENGTH", "*PRINT-LENGTH*"),
    ("LEVEL", "*PRINT-LEVEL*"),
    ("MISER-WIDTH", "*PRINT-MISER-WIDTH*"),
    ("PRETTY", "*PRINT-PRETTY*"),
    ("RADIX", "*PRINT-RADIX*"),
    ("READABLY", "*PRINT-READABLY*"),
    ("RIGHT-MARGIN", "*PRINT-RIGHT-MARGIN*"),
    ("ARRAY", "*PRINT-ARRAY*"),
    ("LINES", "*PRINT-LINES*"),
    ("PPRINT-DISPATCH", "*PRINT-PPRINT-DISPATCH*"),
];

fn print_control_symbols(runtime: &Runtime) -> &RootFrame {
    runtime.printer_controls.get_or_init(|| {
        let symbols = runtime.root_frame(PRINT_CONTROL_KEYWORDS.len());
        for (index, &(_, name)) in PRINT_CONTROL_KEYWORDS.iter().enumerate() {
            symbols.set(
                index,
                runtime
                    .find_named_symbol(&Name::common_lisp(name))
                    .expect("printer variable"),
            );
        }
        symbols
    })
}

fn print_keyword_slots(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    with_stream: bool,
    at: usize,
) -> Result<Vec<Option<usize>>, Fault> {
    let count = PRINT_CONTROL_KEYWORDS.len();
    if runtime.printer_keywords.get().is_none() {
        let slots = runtime.root_frame(count + 2);
        for (index, name) in PRINT_CONTROL_KEYWORDS
            .iter()
            .map(|(name, _)| *name)
            .chain(["STREAM", "ALLOW-OTHER-KEYS"])
            .enumerate()
        {
            slots.set(index, runtime.intern(&Name::keyword(name), at)?);
        }
        let _ = runtime.printer_keywords.set(slots);
    }
    let slots = runtime.printer_keywords.get().unwrap();
    let words =
        std::array::from_fn::<_, { PRINT_CONTROL_KEYWORDS.len() + 1 }, _>(|index| slots.get(index));
    keyword_arguments(
        runtime,
        &words[..count + usize::from(with_stream)],
        args,
        |i| frame.get(i),
        false,
        slots.get(count + 1),
    )
    .map_err(|error| error.program_error(at))
}

pub(super) fn run(
    runtime: &PrintContext<'_>,
    frame: &RootSlots,
    operation: PrinterPrimitive,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if matches!(operation, PrinterPrimitive::DefaultMethod) {
        write_default(
            runtime,
            frame.get(args.start + 2),
            frame.get(args.start + 3),
            at,
        )?;
        return Ok(frame.get(args.start + 2));
    }
    let overrides = if matches!(operation, PrinterPrimitive::Write) && args.len() > 1 {
        print_keyword_slots(runtime, frame, args.start + 1..args.end, false, at)?
            .try_into()
            .expect("printer controls")
    } else {
        [None; PRINT_CONTROL_KEYWORDS.len()]
    };
    let overrides = overrides.map(|location| location.map(|slot| frame.get(slot)));
    let _scope = printer::output_scope(Word::NIL);
    let _bindings = bind_controls(runtime, operation, overrides, None);
    let options = read_print_controls(runtime, 0, at)?;
    let result = render(runtime, &options, frame.get(args.start), None, at)?;
    runtime
        .heap
        .string(&result)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(super) fn print_text(
    runtime: &PrintContext<'_>,
    operation: PrinterPrimitive,
    value: Word,
    base: Option<u32>,
    at: usize,
) -> Result<String, Fault> {
    print_at(runtime, operation, value, base, 0, at)
}

/// Render a value to text for a destination that starts at COLUMN.
pub(super) fn print_at(
    runtime: &PrintContext<'_>,
    operation: PrinterPrimitive,
    value: Word,
    base: Option<u32>,
    column: usize,
    at: usize,
) -> Result<String, Fault> {
    let _bindings = bind_controls(
        runtime,
        operation,
        [None; PRINT_CONTROL_KEYWORDS.len()],
        base,
    );
    let options = read_print_controls(runtime, column, at)?;
    render(runtime, &options, value, None, at)
}

fn render(
    runtime: &PrintContext<'_>,
    options: &PrintControls,
    value: Word,
    stream: Option<&RootSlot>,
    at: usize,
) -> Result<String, Fault> {
    let object = runtime.root_slot(value);
    let package = printer::current_package(runtime, at)?;
    let render_pass = |stream: Option<&RootSlot>| {
        if options.pretty {
            let text = if stream.is_some_and(|s| pprint::in_block_buffer(s.get())) {
                pprint::render_pretty_markers(runtime, package, object.get(), options, None, at)?
            } else {
                pprint::render_pretty_text(runtime, package, object.get(), options, None, at)?
            };
            if let Some(text) = text {
                return Ok(text);
            }
        }
        printer::render(
            runtime,
            package,
            object.get(),
            options,
            stream,
            printer::PrintDispatchMode::current_depth(runtime),
            at,
        )
    };
    let _circle = printer::discover_shared_objects(runtime, options, || render_pass(None).map(drop))?;
    render_pass(stream)
}

pub(super) fn print_to(
    runtime: &PrintContext<'_>,
    op: PrinterPrimitive,
    value: Word,
    stream: Word,
    at: usize,
) -> Result<(), Fault> {
    let object = runtime.root_slot(value);
    write_to_stream(
        runtime,
        op,
        || object.get(),
        stream,
        [None; PRINT_CONTROL_KEYWORDS.len()],
        at,
    )
}

fn write_to_stream(
    runtime: &PrintContext<'_>,
    op: PrinterPrimitive,
    object: impl FnOnce() -> Word,
    stream: Word,
    overrides: [Option<Word>; PRINT_CONTROL_KEYWORDS.len()],
    at: usize,
) -> Result<(), Fault> {
    let stream = runtime.root_slot(pprint::output_stream(runtime, stream, at)?);
    let _scope = printer::output_scope(stream.get());
    let _depth = printer::write_depth(runtime, stream.get());
    let _bindings = bind_controls(runtime, op, overrides, None);
    let column = pprint::stream_column(runtime, stream.get(), at)?;
    let options = read_print_controls(runtime, column, at)?;
    let suffix = render(runtime, &options, object(), Some(&stream), at)?;
    if !suffix.is_empty() {
        streams::gray::print_text(runtime, stream.get(), &suffix, at)?;
    }
    Ok(())
}

/// The system structure method uses the iterative #S renderer. Keep it on
/// that traversal when no user method can specialize the object, so long
/// chains and cyclic slots share the same work stack and active path.
pub(crate) fn structure_has_print_method(runtime: &Runtime, object: Word) -> bool {
    let Some(generic) = runtime
        .find_named_symbol(&Name::common_lisp("PRINT-OBJECT"))
        .and_then(|name| runtime.heap.symbol_cell(name, true))
        .and_then(|function| runtime.heap.generic_info(function))
    else {
        return false;
    };
    let mut methods = generic.methods;
    while let Some((method, tail)) = runtime.heap.cons_fields(methods) {
        let method = runtime.heap.method_info(method).expect("generic method");
        if method.function != runtime.default_print_method.get() {
            let (specializer, _) = runtime
                .heap
                .cons_fields(method.specializers)
                .expect("object specializer");
            if runtime
                .heap
                .class_info(specializer)
                .is_some_and(|class| super::classes::typep(runtime, object, class))
                || runtime.heap.eql_specializer_object(specializer) == Some(object)
            {
                return true;
            }
        }
        methods = tail;
    }
    false
}

pub(crate) fn call_print_object(
    runtime: &PrintContext<'_>,
    object: Word,
    stream: Option<&RootSlot>,
    depth: usize,
    column: usize,
    text: &mut String,
    at: usize,
) -> Result<(), Fault> {
    let object = runtime.root(object);
    let symbol = runtime
        .find_named_symbol(&Name::common_lisp("PRINT-OBJECT"))
        .expect("standard generic");
    let function = runtime.root(super::function_cell(runtime, symbol, at)?);
    let call = |stream| {
        let destination = runtime.root(stream);
        printer::with_method(runtime, stream, depth, || {
            runtime.invoke(&function, &[&object, &destination], at)
        })?;
        Ok(())
    };
    pprint::write_callback(runtime, stream, column, text, at, call)
}

pub(crate) fn write_default(
    runtime: &PrintContext<'_>,
    object: Word,
    stream: Word,
    at: usize,
) -> Result<(), Fault> {
    let stream = runtime.root_slot(stream);
    let object = runtime.root_slot(object);
    let options = read_print_controls(runtime, 0, at)?;
    if !options.escape()
        && let Some(text) = crate::condition::report_to(runtime, object.get(), Some(&stream), at)?
    {
        return streams::gray::print_text(runtime, stream.get(), &text, at);
    }
    let package = printer::current_package(runtime, at)?;
    let text = printer::render(
        runtime,
        package,
        object.get(),
        &options,
        Some(&stream),
        printer::PrintDispatchMode::DefaultMethod,
        at,
    )?;
    if !text.is_empty() {
        streams::gray::print_text(runtime, stream.get(), &text, at)?;
    }
    Ok(())
}

/// WRITE dynamically binds precisely the variables corresponding to supplied
/// keywords. PRIN1 and PRINC also establish their specified escape settings.
fn bind_controls<'rt>(
    runtime: &'rt PrintContext<'_>,
    op: PrinterPrimitive,
    overrides: [Option<Word>; PRINT_CONTROL_KEYWORDS.len()],
    base: Option<u32>,
) -> impl Drop + use<'rt> {
    let mut bindings = Vec::new();
    let controls = print_control_symbols(runtime);
    for (index, ((_, name), value)) in PRINT_CONTROL_KEYWORDS.iter().zip(overrides).enumerate() {
        let value = value.or(match (op, *name, base) {
            (_, "*PRINT-BASE*", Some(base)) => Word::fixnum(base as isize),
            (_, "*PRINT-RADIX*", Some(_)) => Some(Word::NIL),
            (PrinterPrimitive::Prin1, "*PRINT-ESCAPE*", _) => Some(Word::T),
            (PrinterPrimitive::Princ, "*PRINT-ESCAPE*" | "*PRINT-READABLY*", _) => Some(Word::NIL),
            _ => None,
        });
        if let Some(value) = value {
            bindings.push((controls.get(index), value));
        }
    }
    runtime.bind_specials(bindings)
}

pub(super) fn bind_format_write_controls<'rt>(
    runtime: &'rt PrintContext<'_>,
    pretty: bool,
    unlimited: bool,
) -> impl Drop + use<'rt> {
    let overrides = PRINT_CONTROL_KEYWORDS.map(|(key, _)| match key {
        "PRETTY" if pretty => Some(Word::T),
        "LEVEL" | "LENGTH" if unlimited => Some(Word::NIL),
        _ => None,
    });
    bind_controls(runtime, PrinterPrimitive::Write, overrides, None)
}

/// Render VALUE with the current printer control variables at COLUMN.
pub(super) fn render_current(
    runtime: &PrintContext<'_>,
    value: Word,
    column: usize,
    at: usize,
) -> Result<String, Fault> {
    let options = read_print_controls(runtime, column, at)?;
    render(runtime, &options, value, None, at)
}

/// The WRITE function: render OBJECT with keyword overrides and send it to a
/// stream.  Returns OBJECT.
pub(super) fn write(
    runtime: &PrintContext<'_>,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let indices = print_keyword_slots(runtime, frame, args.start + 1..args.end, true, at)?;
    let overrides = std::array::from_fn(|i| indices[i].map(|slot| frame.get(slot)));
    let stream = indices[PRINT_CONTROL_KEYWORDS.len()].map_or(Word::NIL, |slot| frame.get(slot));
    write_to_stream(
        runtime,
        PrinterPrimitive::Write,
        || frame.get(args.start),
        stream,
        overrides,
        at,
    )?;
    Ok(frame.get(args.start))
}

fn read_print_controls(
    runtime: &Runtime,
    column: usize,
    at: usize,
) -> Result<PrintControls, Fault> {
    let symbols = print_control_symbols(runtime);
    let mut controls = [Word::NIL; PRINT_CONTROL_KEYWORDS.len()];
    for (index, slot) in controls.iter_mut().enumerate() {
        *slot = symbol_value(runtime, symbols.get(index), at)?;
    }
    let [
        base,
        case,
        circle,
        escape,
        gensym,
        length,
        level,
        miser,
        pretty,
        radix,
        readably,
        margin,
        array,
        lines,
        _,
    ] = controls;
    let base = base
        .as_fixnum()
        .filter(|base| (2..=36).contains(base))
        .ok_or_else(|| {
            Fault::integer_range(runtime, 
                base,
                2,
                36,
                EvalError::new(at, "printer base must be an integer from 2 to 36"),
            )
        })? as u32;
    let case = match named_word(runtime, case, Package::KEYWORD).as_deref() {
        Some("UPCASE") => PrintCase::Upcase,
        Some("DOWNCASE") => PrintCase::Downcase,
        Some("CAPITALIZE") => PrintCase::Capitalize,
        _ => {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "printer case must be :UPCASE, :DOWNCASE or :CAPITALIZE"),
            ));
        }
    };
    let _ = limit(runtime, lines, "*PRINT-LINES*", at)?;
    Ok(PrintControls {
        base,
        case,
        escape: escape != Word::NIL,
        gensym: gensym != Word::NIL,
        length: limit(runtime, length, "*PRINT-LENGTH*", at)?,
        level: limit(runtime, level, "*PRINT-LEVEL*", at)?,
        miser_width: limit(runtime, miser, "*PRINT-MISER-WIDTH*", at)?,
        right_margin: limit(runtime, margin, "*PRINT-RIGHT-MARGIN*", at)?,
        radix: radix != Word::NIL,
        readably: readably != Word::NIL,
        array: array != Word::NIL,
        pretty: pretty != Word::NIL,
        circle: circle != Word::NIL,
        start_column: column,
    })
}

fn limit(
    runtime: &Runtime,
    value: Word,
    name: &'static str,
    at: usize,
) -> Result<Option<usize>, Fault> {
    if value == Word::NIL {
        return Ok(None);
    }
    runtime
        .heap
        .integer(value)
        .filter(|number| number.sign() != Sign::Minus)
        .and_then(|number| number.to_usize())
        .map(Some)
        .ok_or_else(|| {
            Fault::print_limit(runtime, 
                value,
                EvalError::new(at, format!("{name} must be NIL or a non-negative integer")),
            )
        })
}
