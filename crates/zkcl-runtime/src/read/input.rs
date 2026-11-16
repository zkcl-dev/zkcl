use super::{BuiltinMacro, Form, Reader};
use crate::condition::Fault;
use crate::heap::Word;
use crate::{EvalError, Runtime};
use std::borrow::Cow;
use std::ops::Range;

/// A stream reader inspects decoded bytes without consuming them. Only
/// committed input is visible to Lisp and to other stream operations.
pub(crate) trait StreamInput {
    fn prepend_macro_character(&mut self, ch: char);
    fn finish(
        self,
        runtime: &Runtime,
        result: Result<Option<Form>, EvalError>,
        consumed: usize,
        at: usize,
    ) -> Result<Option<Form>, Fault>
    where
        Self: Sized;
    fn text(&self) -> &str;
    /// Whether obtaining more input can invoke host or Lisp code that changes
    /// the active reader bindings. Reader macro callbacks are handled separately.
    fn refill_invokes_callbacks(&self) -> bool {
        true
    }
    fn refill(&mut self, runtime: &Runtime, token_start: usize) -> bool;
    fn refill_available(&mut self, runtime: &Runtime, token_start: usize) -> bool {
        self.refill(runtime, token_start)
    }
    fn ends_at_eof(&self) -> bool;
    fn location(&self, index: usize) -> usize;
    fn stream(&self) -> Word;
    fn commit(&mut self, runtime: &Runtime, cursor: usize, at: usize) -> Result<(), Fault>;
    fn resume(&mut self, runtime: &Runtime, at: usize) -> Result<(), Fault>;
}

pub(super) enum ReaderInput<'a> {
    Text(&'a str),
    Stream(&'a mut dyn StreamInput),
}

impl<'a> ReaderInput<'a> {
    pub(super) fn text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Stream(stream) => stream.text(),
        }
    }

    pub(super) fn slice(&self, bytes: Range<usize>) -> Cow<'a, str> {
        match self {
            Self::Text(text) => Cow::Borrowed(&text[bytes]),
            Self::Stream(stream) => Cow::Owned(stream.text()[bytes].to_owned()),
        }
    }

    pub(super) fn refill(&mut self, runtime: &Runtime, token_start: usize) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Stream(stream) => stream.refill(runtime, token_start),
        }
    }

    pub(super) fn ends_at_eof(&self) -> bool {
        match self {
            Self::Text(_) => true,
            Self::Stream(stream) => stream.ends_at_eof(),
        }
    }

    pub(super) fn location(&self, index: usize) -> usize {
        match self {
            Self::Text(_) => index,
            Self::Stream(stream) => stream.location(index),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StreamRead {
    Form { recursive: bool, preserve: bool },
    DelimitedList { delimiter: char, recursive: bool },
    Macro { kind: BuiltinMacro, character: char },
}

impl StreamRead {
    pub(crate) fn introducer(self) -> Option<char> {
        match self {
            Self::Macro { character, .. } => Some(character),
            _ => None,
        }
    }

    pub(crate) fn parse(self, reader: &mut Reader<'_>) -> (Result<Option<Form>, EvalError>, usize) {
        let result = match self {
            Self::Form {
                recursive,
                preserve,
            } => reader.next_form_for_read(recursive, preserve),
            Self::DelimitedList {
                delimiter,
                recursive,
            } => reader.delimited_list(delimiter, recursive).map(Some),
            Self::Macro { kind, character } => reader.invoked_macro_form(kind, character),
        };
        let preserve = matches!(self, Self::DelimitedList { .. }) || !matches!(result, Ok(Some(_)));
        let consumed = reader.read_position(preserve);
        (result, consumed)
    }
}
