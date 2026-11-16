use std::cell::RefCell;
use std::io::{self, BufRead, Cursor, Read};
use std::mem::DropGuard;
use std::rc::Rc;

use crate::heap::{StreamElement, StreamKind, Word};
use crate::read::{Form, Reader};
use crate::{EvalError, Root, Runtime, SourceDocument};

use super::{CompoundInput, HostInput};

pub(crate) struct Submission<'rt> {
    runtime: &'rt Runtime,
    stream: Root<'rt>,
    input: HostInput,
    lines: Option<Box<dyn BufRead>>,
    text: Rc<RefCell<String>>,
    document: SourceDocument,
}

impl<'rt> Submission<'rt> {
    pub(crate) fn new(
        runtime: &'rt Runtime,
        initial: &str,
        next: impl FnMut() -> io::Result<Option<String>> + 'static,
    ) -> Result<Self, EvalError> {
        let mut initial = initial.to_owned();
        if !initial.ends_with('\n') {
            initial.push('\n');
        }
        let stream = runtime
            .heap
            .stream(
                StreamKind::StandardInput,
                StreamElement::Character,
                0,
                0,
                || Word::NIL,
            )
            .map_err(|error| EvalError::out_of_memory(0, error))?;
        runtime
            .host_io
            .record_dribble(&initial)
            .map_err(|error| EvalError::io_error(0, error))?;
        let text = Rc::new(RefCell::new(initial.clone()));
        let document = SourceDocument::input(&initial).with_uri("<repl>");
        Ok(Self {
            runtime,
            stream: runtime.root(stream),
            input: HostInput {
                text: initial,
                ..HostInput::default()
            },
            lines: Some(Box::new(ContinuationLines {
                text: Rc::clone(&text),
                next,
                pending: Cursor::default(),
            })),
            text,
            document,
        })
    }

    pub(crate) fn next_form(&mut self) -> Result<Option<Form>, EvalError> {
        let io = &self.runtime.host_io;
        let stream = self.stream.slot.get();
        let info = super::info(self.runtime, stream, 0).map_err(|fault| fault.host)?;
        // Reader macros receive the live source stream. Restore ordinary Lisp
        // input before running the completed form, even on read errors.
        let swap = |input: &mut HostInput, lines: &mut Option<Box<dyn BufRead>>| {
            std::mem::swap(&mut *io.input.borrow_mut(), input);
            std::mem::swap(&mut *io.input_source.borrow_mut(), lines);
        };
        swap(&mut self.input, &mut self.lines);
        let guard = DropGuard::new((&mut self.input, &mut self.lines), |(input, lines)| {
            swap(input, lines)
        });
        let mut input = CompoundInput::for_submission(self.runtime, stream, &info);
        let mut reader = Reader::submission(self.runtime, &mut input);
        let form = reader.next_form();
        let consumed = reader.read_position(true);
        let form = input.finish_submission(self.runtime, form, consumed);
        drop(guard);
        self.runtime.check_execution_safepoint(0)?;
        let text = self.text.borrow();
        if self
            .document
            .text()
            .is_none_or(|old| old.len() != text.len())
        {
            self.document = SourceDocument::input(&text).with_uri("<repl>");
        }
        form.map_err(|error| error.in_document(&self.document))
    }

    pub(crate) fn document(&self) -> &SourceDocument {
        &self.document
    }

    pub(crate) fn offset(&self) -> usize {
        self.input.position()
    }
}

impl Drop for Submission<'_> {
    fn drop(&mut self) {
        self.runtime.heap.stream_close(self.stream.slot.get());
    }
}

struct ContinuationLines<F> {
    text: Rc<RefCell<String>>,
    next: F,
    pending: Cursor<Vec<u8>>,
}

impl<F: FnMut() -> io::Result<Option<String>>> Read for ContinuationLines<F> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = self.fill_buf()?.read(output)?;
        self.consume(count);
        Ok(count)
    }
}

impl<F: FnMut() -> io::Result<Option<String>>> BufRead for ContinuationLines<F> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pending.position() as usize == self.pending.get_ref().len()
            && let Some(mut line) = (self.next)()?
        {
            if !line.ends_with('\n') {
                line.push('\n');
            }
            self.text.borrow_mut().push_str(&line);
            self.pending = Cursor::new(line.into_bytes());
        }
        self.pending.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.pending.consume(count);
    }
}
