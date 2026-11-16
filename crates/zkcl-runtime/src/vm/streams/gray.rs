//! Bridge native stream operations to Lisp methods without leaving the active VM.
use std::ops::Range;

use super::{Op, resolve};
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlots, Word};
use crate::vm::printer::PrintContext;
use crate::vm::{
    Primitive, classes, format::OutputPrimitive, function_cell, generic::BuiltinGeneric, instances,
};
use crate::{EvalError, Root, Runtime};

pub(in crate::vm) fn initialize_generics(runtime: &Runtime) -> Result<(), EvalError> {
    for name in [
        "CLOSE",
        "STREAM-ELEMENT-TYPE",
        "INPUT-STREAM-P",
        "OUTPUT-STREAM-P",
        "OPEN-STREAM-P",
        "INTERACTIVE-STREAM-P",
    ] {
        let (signature, source) = if name == "CLOSE" {
            (
                "(stream &key abort)",
                "(lambda (next has stream &key abort) (declare (ignore next has)) (funcall %native stream :abort abort))",
            )
        } else {
            (
                "(stream)",
                "(lambda (next has stream) (declare (ignore next has)) (funcall %native stream))",
            )
        };
        let generic = BuiltinGeneric::standard(runtime, name, signature)?;
        let default = instances::compile_builtin_lambda(
            runtime,
            source,
            &[("%NATIVE", Primitive::named(name).expect("stream primitive"))],
        )?;
        generic.add(&["T"], default)?;
    }
    install_compound_operations(runtime)?;
    crate::vm::install_host_function(
        runtime,
        "ZK::%READER-WHITESPACE-P",
        std::sync::Arc::new(reader_whitespace),
    )
}

pub(crate) fn streamp(runtime: &Runtime, value: Word) -> bool {
    runtime.heap.stream_info(value).is_some() || is_stream_instance(runtime, value)
}

pub(in crate::vm) fn is_stream_instance(runtime: &Runtime, value: Word) -> bool {
    runtime.heap.instance_class(value).is_some()
        && classes::named_includes(runtime, value, "STREAM") == Some(true)
}

pub(super) fn synonym_leaf(
    runtime: &Runtime,
    mut stream: Word,
    require_open: bool,
    at: usize,
) -> Result<Word, Fault> {
    for _ in 0..super::SYNONYM_DEPTH {
        let Some(info) = runtime.heap.stream_info(stream) else {
            return Ok(stream);
        };
        if info.kind != crate::heap::StreamKind::Synonym {
            return Ok(stream);
        }
        if require_open && info.closed {
            return Err(super::stream_error(runtime, stream, at, "synonym stream is closed"));
        }
        stream = crate::vm::symbol_value(runtime, info.contents, at)?;
    }
    Err(super::stream_error(runtime, stream, at, "stream designator cycle"))
}

pub(crate) fn uses_protocol(runtime: &Runtime, stream: Word, at: usize) -> Result<bool, Fault> {
    if is_stream_instance(runtime, stream) {
        return Ok(true);
    }
    let Some(info) = runtime.heap.stream_info(stream) else {
        return Ok(false);
    };
    if !info.protocol_components || runtime.packages.borrow().named("ZK-GRAY").is_none() {
        return Ok(false);
    }
    if info.kind == crate::heap::StreamKind::Synonym {
        let leaf = synonym_leaf(runtime, stream, false, at)?;
        return Ok(runtime.heap.stream_info(leaf).map_or_else(
            || is_stream_instance(runtime, leaf),
            |s| s.protocol_components,
        ));
    }
    Ok(true)
}

/// Inspect the element type and read binary input on the caller's active
/// control stack. Character input remains live while LOAD evaluates each form.
pub(in crate::vm) fn load_octets(
    runtime: &Runtime,
    stream: &crate::heap::RootSlot,
    call: Option<&crate::vm::callback::CallValues<'_>>,
    at: usize,
) -> Result<Option<Vec<u8>>, Fault> {
    let invoke = |function: &Root<'_>, args: &[&Root<'_>]| -> Result<Word, Fault> {
        Ok(match call {
            Some(call) => call(function, args, at)?
                .first()
                .map_or(Word::NIL, |slot| slot.get()),
            None => runtime
                .call(function, args)?
                .first()
                .map_or(Word::NIL, |value| value.slot.get()),
        })
    };
    let stream = runtime.root(stream.get());
    let binary = runtime.root(entry(runtime, "%LOAD-BINARY-P", at)?);
    if invoke(&binary, &[&stream])? == Word::NIL {
        return Ok(None);
    }
    const CHUNK: usize = 4096;
    let buffer = runtime.root(
        runtime
            .heap
            .array(
                &[CHUNK],
                crate::heap::ArrayElement::General,
                false,
                None,
                |_| Word::NIL,
            )
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    let read = runtime.root(Primitive::named("READ-SEQUENCE").expect("READ-SEQUENCE"));
    let mut bytes = Vec::new();
    loop {
        let count = invoke(&read, &[&buffer, &stream])?
            .as_fixnum()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= CHUNK)
            .ok_or_else(|| {
                super::stream_error(runtime, stream.slot.get(), at, "invalid stream READ-SEQUENCE result")
            })?;
        bytes
            .try_reserve(count)
            .map_err(|_| EvalError::out_of_memory(at, crate::OutOfMemory))?;
        for index in 0..count {
            let value = runtime
                .heap
                .array_slot(buffer.slot.get(), index)
                .map_err(|error| EvalError::out_of_memory(at, error))?
                .expect("index in octet buffer");
            bytes.push(
                value
                    .as_fixnum()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| {
                        super::stream_error(
                            runtime,
                            stream.slot.get(),
                            at,
                            "compiled input contains a non-octet",
                        )
                    })?,
            );
        }
        if count < CHUNK {
            return Ok(Some(bytes));
        }
    }
}

pub(super) fn has_direction(runtime: &Runtime, stream: Word, input: bool) -> bool {
    if runtime.heap.instance_class(stream).is_none() {
        return false;
    }
    let name = if input {
        "FUNDAMENTAL-INPUT-STREAM"
    } else {
        "FUNDAMENTAL-OUTPUT-STREAM"
    };
    let symbol = {
        let packages = runtime.packages.borrow();
        packages
            .named("ZK-GRAY")
            .and_then(|package| packages.present(package, name))
    };
    symbol
        .is_some_and(|(name, _)| classes::named_user_includes(runtime, stream, name) == Some(true))
}

pub(super) fn entry(runtime: &Runtime, name: &str, at: usize) -> Result<Word, Fault> {
    let packages = runtime.packages.borrow();
    let symbol = packages
        .named("ZK-GRAY")
        .and_then(|package| packages.present(package, name))
        .map(|(symbol, _)| symbol)
        .ok_or_else(|| super::stream_error(runtime, Word::NIL, at, "Gray stream protocol is not loaded"))?;
    function_cell(runtime, symbol, at)
}

#[derive(Clone, Copy)]
pub(in crate::vm) enum Designator {
    Input,
    Output,
    Stream,
}

impl Op {
    pub(in crate::vm) fn gray_entry(self) -> Option<(&'static str, usize, Designator)> {
        use Designator::*;
        Some(match self {
            Self::ReadChar => ("%READ-CHAR", 0, Input),
            Self::ReadCharNoHang => ("%READ-CHAR-NO-HANG", 0, Input),
            Self::UnreadChar => ("%UNREAD-CHAR", 1, Input),
            Self::PeekChar => ("%PEEK-CHAR", 1, Input),
            Self::ReadLine => ("%READ-LINE", 0, Input),
            Self::Listen => ("%LISTEN", 0, Input),
            Self::ClearInput => ("%CLEAR-INPUT", 0, Input),
            Self::ReadByte => ("%READ-BYTE", 0, Stream),
            Self::WriteByte => ("%WRITE-BYTE", 1, Stream),
            Self::ReadSequence => ("%READ-SEQUENCE", 1, Stream),
            Self::WriteSequence => ("%WRITE-SEQUENCE", 1, Stream),
            Self::ElementType => ("%COMPOUND-ELEMENT-TYPE", 0, Stream),
            Self::FilePosition => ("%FILE-POSITION", 0, Stream),
            Self::WriteChar => ("%WRITE-CHAR", 1, Output),
            Self::WriteString => ("%WRITE-STRING", 1, Output),
            Self::WriteLine => ("%WRITE-LINE", 1, Output),
            Self::Terpri => ("%TERPRI", 0, Output),
            Self::FreshLine => ("%FRESH-LINE", 0, Output),
            Self::FinishOutput => ("%FINISH-OUTPUT", 0, Output),
            Self::ForceOutput => ("%FORCE-OUTPUT", 0, Output),
            Self::ClearOutput => ("%CLEAR-OUTPUT", 0, Output),
            _ => return None,
        })
    }
}

pub(in crate::vm) fn redirect(
    runtime: &Runtime,
    op: Primitive,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Option<(Word, RootFrame)>, Fault> {
    let route = match op {
        Primitive::Stream(op) => op.gray_entry(),
        Primitive::Format(OutputPrimitive::Terpri) => Op::Terpri.gray_entry(),
        Primitive::Format(OutputPrimitive::FreshLine) => Op::FreshLine.gray_entry(),
        _ => None,
    };
    let Some((name, index, kind)) = route else {
        return Ok(None);
    };
    let stream = if index < args.len() {
        frame.get(args.start + index)
    } else {
        Word::NIL
    };
    let stream = match kind {
        Designator::Stream => stream,
        Designator::Input => resolve(runtime, stream, "*STANDARD-INPUT*", at)?,
        Designator::Output => resolve(runtime, stream, "*STANDARD-OUTPUT*", at)?,
    };
    if !uses_protocol(runtime, stream, at)?
        || (matches!(op, Primitive::Stream(Op::ElementType))
            && runtime.heap.stream_info(stream).is_none())
    {
        return Ok(None);
    }
    let arguments = runtime.root_frame(args.len().max(index + 1));
    for (i, reg) in args.enumerate() {
        arguments.set(i, frame.get(reg));
    }
    arguments.set(index, stream);
    Ok(Some((entry(runtime, name, at)?, arguments)))
}

pub(crate) fn print_column(
    runtime: &PrintContext<'_>,
    stream: Word,
    at: usize,
) -> Result<Option<usize>, Fault> {
    let stream = resolve(runtime, stream, "*STANDARD-OUTPUT*", at)?;
    if !uses_protocol(runtime, stream, at)? {
        return super::output_column(runtime, stream, at);
    }
    let function = runtime.root(entry(runtime, "STREAM-LINE-COLUMN", at)?);
    let values = runtime.invoke(&function, &[&runtime.root(stream)], at)?;
    Ok(values
        .first()
        .and_then(|value| value.as_fixnum())
        .and_then(|column| usize::try_from(column).ok()))
}

pub(crate) fn print_text(
    runtime: &PrintContext<'_>,
    stream: Word,
    text: &str,
    at: usize,
) -> Result<(), Fault> {
    let stream = resolve(runtime, stream, "*STANDARD-OUTPUT*", at)?;
    if !uses_protocol(runtime, stream, at)? {
        return super::write_text(runtime, stream, text, at);
    }
    let stream = runtime.root(stream);
    let function = runtime.root(entry(runtime, "%WRITE-STRING", at)?);
    let text = runtime
        .make_string(text)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    runtime.invoke(&function, &[&text, &stream], at)?;
    Ok(())
}

pub(in crate::vm) fn advance_to_column(
    runtime: &PrintContext<'_>,
    stream: Word,
    column: usize,
    at: usize,
) -> Result<bool, Fault> {
    let function = runtime.root(entry(runtime, "STREAM-ADVANCE-TO-COLUMN", at)?);
    let column = Word::fixnum(column as isize)
        .ok_or_else(|| EvalError::new(at, "column exceeds fixnum range"))?;
    Ok(runtime
        .invoke(
            &function,
            &[&runtime.root(stream), &runtime.root(column)],
            at,
        )?
        .first()
        .is_some_and(|value| !value.is_nil()))
}

fn reader_whitespace<'rt>(
    runtime: &'rt Runtime,
    args: &[crate::Root<'rt>],
    at: usize,
) -> Result<Vec<crate::Root<'rt>>, Fault> {
    let [value] = args else {
        return Err(crate::vm::modules::arity_error(at, "expected a character"));
    };
    let character = value.slot.get().as_character().ok_or_else(|| {
        Fault::type_error(runtime, 
            value.slot.get(),
            "CHARACTER",
            EvalError::new(at, "expected a character"),
        )
    })?;
    let whitespace = super::CharacterRead::SkipWhitespace.skips(runtime, character, at)?;
    Ok(vec![runtime.root(super::truth(whitespace))])
}

fn install_compound_operations(runtime: &Runtime) -> Result<(), EvalError> {
    use crate::{NativeFunction, heap::StreamKind};
    use std::sync::Arc;
    let install = |name, function: Arc<NativeFunction>| {
        crate::vm::install_host_function(runtime, name, function)
    };
    install(
        "ZK::%SYNONYM-TARGET",
        Arc::new(|rt, args, at| {
            let [stream] = args else {
                return Err(crate::vm::modules::arity_error(
                    at,
                    "expected a synonym stream",
                ));
            };
            Ok(vec![rt.root(synonym_leaf(
                rt,
                stream.slot.get(),
                true,
                at,
            )?)])
        }),
    )?;
    install(
        "ZK::%DISCARD-CONCATENATED-HEAD",
        Arc::new(|rt, args, at| {
            let [stream, list] = args else {
                return Err(crate::vm::modules::arity_error(
                    at,
                    "expected stream and remaining components",
                ));
            };
            let stream = stream.slot.get();
            let info = super::info(rt, stream, at)?;
            if info.kind != StreamKind::Concatenated {
                return Err(super::stream_error(
                    rt,
                    stream,
                    at,
                    "expected concatenated stream",
                ));
            }
            // A component method can reenter the outer stream before reporting EOF.
            if info.contents == list.slot.get()
                && let Some((_, tail)) = rt.heap.cons_fields(info.contents)
            {
                rt.heap.stream_set_contents(stream, tail);
            }
            Ok(vec![rt.nil()])
        }),
    )?;
    install(
        "ZK::%ECHO-CHARACTER-STATE",
        Arc::new(|rt, args, at| {
            let [stream] = args else {
                return Err(crate::vm::modules::arity_error(at, "expected echo stream"));
            };
            let info = super::info(rt, stream.slot.get(), at)?;
            if info.kind != StreamKind::Echo {
                return Err(super::stream_error(
                    rt,
                    stream.slot.get(),
                    at,
                    "expected echo stream",
                ));
            }
            let last = info
                .last_char
                .filter(|_| info.unread_allowed || info.cursor == 1)
                .map_or(Word::NIL, Word::character);
            Ok(vec![rt.root(last), rt.root(super::truth(info.cursor == 1))])
        }),
    )?;
    install(
        "ZK::%RECORD-ECHO-CHARACTER",
        Arc::new(|rt, args, at| {
            let [stream, character, pending] = args else {
                return Err(crate::vm::modules::arity_error(
                    at,
                    "expected echo stream, character and unread state",
                ));
            };
            let stream = stream.slot.get();
            if super::info(rt, stream, at)?.kind != StreamKind::Echo {
                return Err(super::stream_error(rt, stream, at, "expected echo stream"));
            }
            let ch = crate::vm::readtables::character_arg(rt, character.slot.get(), at)?;
            rt.heap
                .stream_set_echo_character(stream, ch, !pending.is_nil());
            Ok(vec![rt.nil()])
        }),
    )?;
    install(
        "ZK::%NATIVE-INPUT-READINESS",
        Arc::new(|rt, args, at| {
            let [stream] = args else {
                return Err(crate::vm::modules::arity_error(at, "expected input stream"));
            };
            let s = stream.slot.get();
            let name = if uses_protocol(rt, s, at)? {
                "PROTOCOL"
            } else {
                match super::input_availability(rt, s, at)? {
                    super::InputAvailability::Ready => "READY",
                    super::InputAvailability::Pending => "PENDING",
                    super::InputAvailability::EndOfFile => "EOF",
                }
            };
            Ok(vec![
                rt.root(rt.intern(&crate::symbol::Name::keyword(name), at)?),
            ])
        }),
    )?;
    install(
        "ZK::%NATIVE-OUTPUT-COLUMN",
        Arc::new(|rt, args, at| {
            let [stream] = args else {
                return Err(crate::vm::modules::arity_error(
                    at,
                    "expected output stream",
                ));
            };
            let column = super::output_column(rt, stream.slot.get(), at)?;
            Ok(vec![rt.root(
                column.map_or(Ok(Word::NIL), |n| super::fixnum(n, at))?,
            )])
        }),
    )
}
