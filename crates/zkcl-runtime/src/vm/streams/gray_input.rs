//! Read forms directly from a user stream with at most one character of lookahead.
use std::ops::Range;

use super::{Op, eof, gray, read_fault, resolve, stream_error};
use crate::condition::Fault;
use crate::heap::{RootSlot, RootSlots, Word};
use crate::read::{Form, Reader, StreamInput, StreamRead};
use crate::symbol::Name;
use crate::vm::{
    Continuation, ContinuationStep, ExecState, Primitive, Resume, Values, callback, readtables,
};
use crate::{EvalError, Root, Runtime, form_value};

pub(in crate::vm) fn reader_stream_argument(op: Primitive) -> Option<usize> {
    match op {
        Primitive::Stream(Op::Read | Op::ReadPreservingWhitespace) => Some(0),
        Primitive::Readtable(readtables::Op::ReadDelimitedList) => Some(1),
        Primitive::Readtable(op) if readtables::builtin_macro(op).is_some() => Some(0),
        _ => None,
    }
}

pub(in crate::vm) struct GrayRead {
    stream: RootSlot,
    operation: StreamRead,
    eof_error: bool,
    eof_value: RootSlot,
    at: usize,
    resume: Resume,
}

impl GrayRead {
    pub(in crate::vm) fn for_call(
        runtime: &Runtime,
        op: Primitive,
        frame: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Option<Self>, Fault> {
        let Some(index) = reader_stream_argument(op) else {
            return Ok(None);
        };
        let arg = |index| (index < args.len()).then(|| frame.get(args.start + index));
        let stream = resolve(
            runtime,
            arg(index).unwrap_or(Word::NIL),
            "*STANDARD-INPUT*",
            at,
        )?;
        if !gray::uses_protocol(runtime, stream, at)? {
            return Ok(None);
        }
        let operation = match op {
            Primitive::Stream(op) => StreamRead::Form {
                recursive: arg(3).is_some_and(|word| word != Word::NIL),
                preserve: matches!(op, Op::ReadPreservingWhitespace),
            },
            Primitive::Readtable(readtables::Op::ReadDelimitedList) => StreamRead::DelimitedList {
                delimiter: readtables::character_arg(runtime, arg(0).unwrap(), at)?,
                recursive: arg(2).is_some_and(|word| word != Word::NIL),
            },
            Primitive::Readtable(op) => StreamRead::Macro {
                kind: readtables::builtin_macro(op).unwrap(),
                character: readtables::character_arg(runtime, arg(1).unwrap(), at)?,
            },
            _ => unreachable!(),
        };
        Ok(Some(Self {
            stream: runtime.root_slot(stream),
            operation,
            eof_error: arg(1) != Some(Word::NIL),
            eof_value: runtime.root_slot(arg(2).unwrap_or(Word::NIL)),
            at,
            resume,
        }))
    }

    fn read(
        &self,
        runtime: &Runtime,
        call: Option<&callback::CallValues<'_>>,
    ) -> Result<ContinuationStep, Fault> {
        match read_form(runtime, &self.stream, self.operation, call, self.at)? {
            Some(form) => Ok(ContinuationStep::Done(
                form_value::to_value(runtime, &form)?.slot.get(),
            )),
            None if matches!(self.operation, StreamRead::Macro { .. }) => {
                Ok(ContinuationStep::DoneValues(vec![]))
            }
            None => Ok(ContinuationStep::Done(eof(
                runtime,
                Some(if self.eof_error { Word::T } else { Word::NIL }),
                Some(self.eof_value.get()),
                self.stream.get(),
                self.at,
            )?)),
        }
    }
}

pub(in crate::vm) fn read_form(
    runtime: &Runtime,
    stream: &RootSlot,
    operation: StreamRead,
    call: Option<&callback::CallValues<'_>>,
    at: usize,
) -> Result<Option<Form>, Fault> {
    // Dynamic bindings needed by a suppressed reader macro or #. belong
    // on the same Lisp control stack as handlers and nonlocal exits.
    let invoke =
        |function: &Root<'_>, arguments: &[&Root<'_>], binding: Option<(Word, Word)>, at| {
            let call = call.expect("active reader callback");
            if let Some((symbol, value)) = binding {
                let symbol = runtime.root(symbol);
                let value = runtime.root(value);
                let wrapper = runtime.root(
                    gray::entry(runtime, "%CALL-WITH-BINDING", at).map_err(|fault| fault.host)?,
                );
                let mut args = vec![&symbol, &value, function];
                args.extend_from_slice(arguments);
                call(&wrapper, &args, at)
            } else {
                call(function, arguments, at)
            }
        };
    let mut input = GrayInput::new(runtime, stream.get(), call, at)?;
    if let Some(ch) = operation.introducer() {
        input.prepend_macro_character(ch);
    }
    let mut reader = Reader::stream(runtime, &mut input)
        .with_calls(call.map(|_| &invoke as &crate::read::ReadCall<'_>));
    let (result, consumed) = operation.parse(&mut reader);
    input.finish(runtime, result, consumed, at)
}

impl Continuation for GrayRead {
    fn resume_to(&self) -> Resume {
        self.resume
    }

    fn step(&mut self, runtime: &Runtime, _: &Values, _: bool) -> Result<ContinuationStep, Fault> {
        self.read(runtime, None)
    }

    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        _: bool,
    ) -> callback::Outcome<Result<ContinuationStep, Fault>> {
        callback::run(runtime, exec, |_, call| self.read(runtime, call))
    }
}

struct GrayInput<'a> {
    stream: RootSlot,
    read: RootSlot,
    read_no_hang: RootSlot,
    peek: Option<RootSlot>,
    pending: Option<char>,
    unread: RootSlot,
    eof_value: RootSlot,
    call: Option<&'a callback::CallValues<'a>>,
    text: String,
    eof: bool,
    check_open_before_read: bool,
    fault: Option<Fault>,
    base: usize,
    prefix: usize,
    at: usize,
}

impl<'a> GrayInput<'a> {
    fn new(
        runtime: &Runtime,
        stream: Word,
        call: Option<&'a callback::CallValues<'a>>,
        at: usize,
    ) -> Result<Self, Fault> {
        Ok(Self {
            stream: runtime.root_slot(stream),
            read: runtime.root_slot(gray::entry(runtime, "STREAM-READ-CHAR", at)?),
            peek: if runtime.heap.stream_info(stream).is_some() {
                Some(runtime.root_slot(gray::entry(runtime, "STREAM-PEEK-CHAR", at)?))
            } else {
                None
            },
            pending: None,
            read_no_hang: runtime.root_slot(gray::entry(runtime, "STREAM-READ-CHAR-NO-HANG", at)?),
            unread: runtime.root_slot(gray::entry(runtime, "STREAM-UNREAD-CHAR", at)?),
            eof_value: runtime.root_slot(runtime.intern(&Name::keyword("EOF"), at)?),
            call,
            text: String::new(),
            eof: false,
            check_open_before_read: true,
            fault: None,
            base: 0,
            prefix: 0,
            at,
        })
    }

    fn invoke(
        &self,
        runtime: &Runtime,
        target: Word,
        character: Option<char>,
    ) -> Result<Word, Fault> {
        let stream = runtime.root(self.stream.get());
        let target = runtime.root(target);
        let character = character.map(|character| runtime.character(character));
        let stream_arg = [&stream];
        let stream_and_character;
        let args: &[_] = if let Some(character) = &character {
            stream_and_character = [&stream, character];
            &stream_and_character
        } else {
            &stream_arg
        };
        match self.call {
            Some(call) => Ok(call(&target, args, self.at)?
                .first()
                .map_or(Word::NIL, |slot| slot.get())),
            None => Ok(runtime
                .call(&target, args)?
                .first()
                .map_or(Word::NIL, |value| value.slot.get())),
        }
    }

    fn consume_peek(&self, runtime: &Runtime, expected: char) -> Result<(), Fault> {
        let value = self.invoke(runtime, self.read.get(), None)?;
        if value.as_character() == Some(expected) {
            Ok(())
        } else {
            Err(stream_error(
                runtime,
                self.stream(),
                self.at,
                "stream changed after PEEK-CHAR",
            ))
        }
    }

    fn check_open(&mut self, runtime: &Runtime) -> Result<(), Fault> {
        self.invoke(runtime, gray::entry(runtime, "CHECK-OPEN", self.at)?, None)?;
        Ok(())
    }

    fn extend(&mut self, runtime: &Runtime, wait: bool) -> bool {
        if self.eof || self.fault.is_some() {
            return false;
        }
        if self.check_open_before_read {
            if let Err(error) = self.check_open(runtime) {
                self.fault = Some(error);
                return false;
            }
            self.check_open_before_read = false;
        }
        let peek = self.peek.as_ref();
        let function = if let Some(peek) = peek {
            // A composite may echo each consumed character. Leave the lexer’s
            // delimiter on the source until it commits, including across macros.
            if !wait {
                return false;
            }
            if let Some(expected) = self.pending.take()
                && let Err(error) = self.consume_peek(runtime, expected)
            {
                self.fault = Some(error);
                return false;
            }
            peek.get()
        } else if wait {
            self.read.get()
        } else {
            self.read_no_hang.get()
        };
        match self.invoke(runtime, function, None) {
            Ok(value) if value == self.eof_value.get() => self.eof = true,
            Ok(value) if value == Word::NIL && !wait => {}
            Ok(value) => match value.as_character() {
                Some(ch) => {
                    if peek.is_some() {
                        self.pending = Some(ch);
                    }
                    self.text.push(ch);
                    return true;
                }
                None => {
                    self.fault = Some(Fault::type_error(runtime, 
                        value,
                        "CHARACTER",
                        EvalError::new(self.at, "Gray character read returned an invalid value"),
                    ))
                }
            },
            Err(error) => self.fault = Some(error),
        }
        false
    }
}

impl StreamInput for GrayInput<'_> {
    fn prepend_macro_character(&mut self, ch: char) {
        self.text.push(ch);
        self.prefix = ch.len_utf8();
    }
    fn text(&self) -> &str {
        &self.text
    }
    fn stream(&self) -> Word {
        self.stream.get()
    }
    fn location(&self, index: usize) -> usize {
        self.base + index.saturating_sub(self.prefix)
    }
    fn ends_at_eof(&self) -> bool {
        self.eof
    }
    fn refill(&mut self, runtime: &Runtime, _: usize) -> bool {
        self.extend(runtime, true)
    }
    fn refill_available(&mut self, runtime: &Runtime, _: usize) -> bool {
        self.extend(runtime, false)
    }

    fn commit(&mut self, runtime: &Runtime, cursor: usize, at: usize) -> Result<(), Fault> {
        let tail = &self.text[cursor..];
        let mut chars = tail.chars();
        if let Some(ch) = chars.next() {
            if chars.next().is_some() {
                return Err(stream_error(
                    runtime,
                    self.stream(),
                    at,
                    "Gray reader exceeded its one-character lookahead",
                ));
            }
            if self.peek.is_none() {
                self.invoke(runtime, self.unread.get(), Some(ch))?;
            }
        } else if let Some(ch) = self.pending {
            self.consume_peek(runtime, ch)?;
        }
        self.pending = None;
        self.base = self.location(cursor);
        Ok(())
    }

    fn resume(&mut self, _: &Runtime, _: usize) -> Result<(), Fault> {
        self.text.clear();
        self.prefix = 0;
        self.eof = false;
        self.check_open_before_read = true;
        Ok(())
    }

    fn finish(
        mut self,
        runtime: &Runtime,
        result: Result<Option<Form>, EvalError>,
        consumed: usize,
        at: usize,
    ) -> Result<Option<Form>, Fault> {
        if let Some(fault) = self.fault.take() {
            return Err(fault);
        }
        let form = result.map_err(|error| read_fault(runtime, error, at, 0, self.stream()))?;
        self.commit(runtime, consumed, at)?;
        Ok(form)
    }
}
