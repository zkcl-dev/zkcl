use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::ops::Range;

use super::sequence::{Kind, Sequence};
use super::strings::{self, index};
use super::{Values, named_keyword_arguments as keys, symbol_value};
use crate::character::utf8_byte_index as byte_index;
use crate::condition::{Class, Fault};
use crate::form_value;
use crate::heap::{ArrayElement, RootSlot, RootSlots, StreamElement, StreamKind, StreamState, Word};
use crate::read::{Form, Reader, StreamInput, StreamRead};
use crate::symbol::Name;
use crate::{BigInt, EvalError, EvalErrorKind, Root, Runtime};

mod character_input;
mod compound_input;
mod file;
mod file_input;
pub(crate) mod gray;
pub(in crate::vm) mod gray_input;
mod output;
mod submission;
use character_input::{
    CharacterRead, NoHang, clear_input, input_availability, read_character, read_character_no_hang,
    read_line, unread_character,
};
use compound_input::CompoundInput;
use file::{
    byte_element, file_length, file_stream_position, is_byte_element, open, read_file_byte,
    write_file_byte, write_file_bytes,
};
use file_input::FileInput;
pub(super) use output::make_output_to_string;
use output::{flush, get_output};
pub(crate) use output::{output_column, write_text};
pub(crate) use submission::Submission;

pub(crate) const INPUT_STRING_INDEX: &str = "%INPUT-STRING-INDEX";
pub(crate) const OUTPUT_TO_STRING: &str = "%OUTPUT-TO-STRING";

pub(crate) struct HostIo {
    input: RefCell<HostInput>,
    input_source: RefCell<Option<Box<dyn BufRead>>>,
    interactive_input: bool,
    stdout: RefCell<Option<Box<dyn Write>>>,
    stderr: RefCell<Option<Box<dyn Write>>>,
    stdout_column: Cell<Option<usize>>,
    stderr_column: Cell<Option<usize>>,
    files: RefCell<HashMap<usize, File>>,
    next_file: Cell<usize>,
    dribble: RefCell<Option<File>>,
}

impl Default for HostIo {
    fn default() -> Self {
        Self {
            input: RefCell::default(),
            input_source: RefCell::default(),
            interactive_input: false,
            stdout: RefCell::default(),
            stderr: RefCell::default(),
            stdout_column: Cell::default(),
            stderr_column: Cell::default(),
            files: RefCell::default(),
            next_file: Cell::new(1),
            dribble: RefCell::default(),
        }
    }
}

impl HostIo {
    pub(crate) fn set_input(&mut self, input: impl BufRead + 'static, interactive: bool) {
        *self.input_source.get_mut() = Some(Box::new(input));
        self.interactive_input = interactive;
        *self.input.get_mut() = HostInput::default();
    }

    pub(crate) fn set_output(&mut self, output: impl Write + 'static) {
        *self.stdout.get_mut() = Some(Box::new(output));
        self.stdout_column.set(None);
    }

    pub(crate) fn set_error(&mut self, output: impl Write + 'static) {
        *self.stderr.get_mut() = Some(Box::new(output));
        self.stderr_column.set(None);
    }

    fn file(&self, handle: usize) -> Option<RefMut<'_, File>> {
        RefMut::filter_map(self.files.borrow_mut(), |files| files.get_mut(&handle)).ok()
    }

    fn refill(&self, input: &mut HostInput) -> io::Result<()> {
        let previous_end = input.discarded + input.text.len();
        match self.input_source.borrow_mut().as_deref_mut() {
            Some(source) => input.refill(source)?,
            None => input.refill(&mut io::stdin().lock())?,
        }
        // Record newly received bytes once. Reader lookahead and UNREAD-CHAR
        // revisit the buffer and must never duplicate the transcript.
        self.record_dribble(&input.text[previous_end - input.discarded..])
    }

    pub(crate) fn start_dribble(&self, file: File) -> io::Result<()> {
        let mut current = self.dribble.borrow_mut();
        if current.is_some() {
            return Err(io::Error::other(
                "DRIBBLE is already recording; call (DRIBBLE) first",
            ));
        }
        *current = Some(file);
        Ok(())
    }

    pub(crate) fn stop_dribble(&self) -> io::Result<()> {
        if let Some(mut file) = self.dribble.borrow_mut().take() {
            file.flush()?;
        }
        Ok(())
    }

    pub(crate) fn dribbling(&self) -> bool {
        self.dribble.borrow().is_some()
    }

    pub(crate) fn record_dribble(&self, text: &str) -> io::Result<()> {
        if let Some(file) = self.dribble.borrow_mut().as_mut() {
            file.write_all(text.as_bytes())?;
            file.flush()?;
        }
        Ok(())
    }

    fn with_writer(
        &self,
        error_output: bool,
        operation: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        let output = if error_output {
            &self.stderr
        } else {
            &self.stdout
        };
        let mut output = output
            .try_borrow_mut()
            .map_err(|_| io::Error::other("host output is already in use"))?;
        match output.as_deref_mut() {
            Some(writer) => operation(writer),
            None if error_output => operation(&mut io::stderr().lock()),
            None => operation(&mut io::stdout().lock()),
        }
    }

    fn interactive(&self, kind: StreamKind) -> bool {
        use std::io::IsTerminal;
        let input = || {
            self.interactive_input
                || (self.input_source.borrow().is_none() && io::stdin().is_terminal())
        };
        let output = || self.stdout.borrow().is_none() && io::stdout().is_terminal();
        match kind {
            StreamKind::StandardInput => input(),
            StreamKind::StandardOutput => output(),
            StreamKind::ErrorOutput => self.stderr.borrow().is_none() && io::stderr().is_terminal(),
            StreamKind::Terminal => input() && output(),
            _ => false,
        }
    }
}

#[derive(Default)]
struct HostInput {
    text: String,
    cursor: usize,
    eof: bool,
    unread_allowed: bool,
    discarded: usize,
}

enum InputAvailability {
    Ready,
    EndOfFile,
    Pending,
}

impl HostInput {
    fn position(&self) -> usize {
        self.discarded + self.cursor
    }

    fn availability(&self) -> InputAvailability {
        if self.cursor < self.text.len() {
            InputAvailability::Ready
        } else if self.eof {
            InputAvailability::EndOfFile
        } else {
            InputAvailability::Pending
        }
    }

    fn refill(&mut self, input: &mut dyn BufRead) -> io::Result<()> {
        // Retain the last consumed character for UNREAD-CHAR.
        let keep = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(offset, _)| offset);
        self.text.drain(..keep);
        self.cursor -= keep;
        self.discarded += keep;
        self.eof = input.read_line(&mut self.text)? == 0;
        Ok(())
    }
}

pub(crate) struct StandardStreams<'rt> {
    input: Root<'rt>,
    output: Root<'rt>,
    error: Root<'rt>,
    terminal: Root<'rt>,
}

impl<'rt> StandardStreams<'rt> {
    pub(crate) fn new(runtime: &'rt Runtime) -> Result<Self, EvalError> {
        let stream = |kind| {
            runtime
                .heap
                .stream(kind, StreamElement::Character, 0, 0, || Word::NIL)
                .map(|word| runtime.root(word))
                .map_err(|error| EvalError::out_of_memory(0, error))
        };
        Ok(Self {
            input: stream(StreamKind::StandardInput)?,
            output: stream(StreamKind::StandardOutput)?,
            error: stream(StreamKind::ErrorOutput)?,
            terminal: stream(StreamKind::Terminal)?,
        })
    }

    pub(crate) fn initial(&self, name: &str) -> Option<Word> {
        let stream = match name {
            "*STANDARD-INPUT*" => &self.input,
            "*STANDARD-OUTPUT*" | "*TRACE-OUTPUT*" => &self.output,
            "*ERROR-OUTPUT*" => &self.error,
            "*TERMINAL-IO*" | "*QUERY-IO*" | "*DEBUG-IO*" => &self.terminal,
            _ => return None,
        };
        Some(stream.slot.get())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    MakeInput,
    MakeOutput,
    MakeOutputTo,
    GetOutput,
    Open,
    Read,
    ReadPreservingWhitespace,
    ReadChar,
    UnreadChar,
    PeekChar,
    ReadLine,
    WriteChar,
    WriteString,
    WriteLine,
    Terpri,
    FreshLine,
    FinishOutput,
    ForceOutput,
    Listen,
    ClearInput,
    ClearOutput,
    ReadByte,
    WriteByte,
    ReadCharNoHang,
    StreamP,
    InputStreamP,
    OutputStreamP,
    OpenStreamP,
    InteractiveStreamP,
    ElementType,
    ExternalFormat,
    Close,
    FileLength,
    FileStringLength,
    FilePosition,
    InputStringIndex,
    Prin1,
    Princ,
    Print,
    MakeBroadcast,
    BroadcastStreams,
    MakeConcatenated,
    ConcatenatedStreams,
    MakeEcho,
    EchoInputStream,
    EchoOutputStream,
    MakeTwoWay,
    TwoWayInputStream,
    TwoWayOutputStream,
    MakeSynonym,
    SynonymStreamSymbol,
    ReadSequence,
    WriteSequence,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAKE-STRING-INPUT-STREAM", Op::MakeInput),
    ("MAKE-STRING-OUTPUT-STREAM", Op::MakeOutput),
    (OUTPUT_TO_STRING, Op::MakeOutputTo),
    ("GET-OUTPUT-STREAM-STRING", Op::GetOutput),
    ("OPEN", Op::Open),
    ("READ", Op::Read),
    ("READ-PRESERVING-WHITESPACE", Op::ReadPreservingWhitespace),
    ("READ-CHAR", Op::ReadChar),
    ("UNREAD-CHAR", Op::UnreadChar),
    ("PEEK-CHAR", Op::PeekChar),
    ("READ-LINE", Op::ReadLine),
    ("WRITE-CHAR", Op::WriteChar),
    ("WRITE-STRING", Op::WriteString),
    ("WRITE-LINE", Op::WriteLine),
    ("TERPRI", Op::Terpri),
    ("FRESH-LINE", Op::FreshLine),
    ("FINISH-OUTPUT", Op::FinishOutput),
    ("FORCE-OUTPUT", Op::ForceOutput),
    ("READ-BYTE", Op::ReadByte),
    ("WRITE-BYTE", Op::WriteByte),
    ("READ-CHAR-NO-HANG", Op::ReadCharNoHang),
    ("LISTEN", Op::Listen),
    ("CLEAR-INPUT", Op::ClearInput),
    ("CLEAR-OUTPUT", Op::ClearOutput),
    ("STREAMP", Op::StreamP),
    ("INPUT-STREAM-P", Op::InputStreamP),
    ("OUTPUT-STREAM-P", Op::OutputStreamP),
    ("OPEN-STREAM-P", Op::OpenStreamP),
    ("INTERACTIVE-STREAM-P", Op::InteractiveStreamP),
    ("STREAM-ELEMENT-TYPE", Op::ElementType),
    ("STREAM-EXTERNAL-FORMAT", Op::ExternalFormat),
    ("CLOSE", Op::Close),
    ("FILE-LENGTH", Op::FileLength),
    ("FILE-STRING-LENGTH", Op::FileStringLength),
    ("FILE-POSITION", Op::FilePosition),
    (INPUT_STRING_INDEX, Op::InputStringIndex),
    ("PRIN1", Op::Prin1),
    ("PRINC", Op::Princ),
    ("PRINT", Op::Print),
    ("MAKE-BROADCAST-STREAM", Op::MakeBroadcast),
    ("BROADCAST-STREAM-STREAMS", Op::BroadcastStreams),
    ("MAKE-CONCATENATED-STREAM", Op::MakeConcatenated),
    ("CONCATENATED-STREAM-STREAMS", Op::ConcatenatedStreams),
    ("MAKE-ECHO-STREAM", Op::MakeEcho),
    ("ECHO-STREAM-INPUT-STREAM", Op::EchoInputStream),
    ("ECHO-STREAM-OUTPUT-STREAM", Op::EchoOutputStream),
    ("MAKE-TWO-WAY-STREAM", Op::MakeTwoWay),
    ("TWO-WAY-STREAM-INPUT-STREAM", Op::TwoWayInputStream),
    ("TWO-WAY-STREAM-OUTPUT-STREAM", Op::TwoWayOutputStream),
    ("MAKE-SYNONYM-STREAM", Op::MakeSynonym),
    ("SYNONYM-STREAM-SYMBOL", Op::SynonymStreamSymbol),
    ("READ-SEQUENCE", Op::ReadSequence),
    ("WRITE-SEQUENCE", Op::WriteSequence),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Open => count >= 1,
            Self::MakeInput => (1..=3).contains(&count),
            Self::MakeOutput => true,
            Self::Read | Self::ReadPreservingWhitespace | Self::ReadChar | Self::ReadLine => {
                count <= 4
            }
            Self::ReadCharNoHang => count <= 4,
            Self::ReadByte => (1..=3).contains(&count),
            Self::PeekChar => count <= 5,
            Self::UnreadChar
            | Self::WriteChar
            | Self::MakeOutputTo
            | Self::FilePosition
            | Self::Prin1
            | Self::Princ
            | Self::Print => (1..=2).contains(&count),
            Self::WriteByte | Self::FileStringLength => count == 2,
            Self::WriteString | Self::WriteLine | Self::Close => count >= 1,
            Self::MakeBroadcast | Self::MakeConcatenated => true,
            Self::MakeEcho | Self::MakeTwoWay => count == 2,
            Self::ReadSequence | Self::WriteSequence => count >= 2,
            Self::Terpri
            | Self::FreshLine
            | Self::ForceOutput
            | Self::FinishOutput
            | Self::Listen
            | Self::ClearInput
            | Self::ClearOutput => count <= 1,
            _ => count == 1,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let arg = |index| (index < args.len()).then(|| frame.get(args.start + index));
    let result = match op {
        Op::MakeInput => {
            let source = arg(0).unwrap();
            let range = runtime
                .heap
                .with_string_slice(source, |text| {
                    string_bounds(runtime, text, arg(1), arg(2), at)
                })
                .ok_or_else(|| {
                    Fault::type_error(runtime, source, "STRING", EvalError::new(at, "expected a string"))
                })??;
            // The stream keeps byte positions into its own snapshot.
            let source = runtime.root_slot(
                runtime
                    .heap
                    .copy_string(source)
                    .map_err(|error| EvalError::out_of_memory(at, error))?,
            );
            runtime
                .heap
                .stream(
                    StreamKind::StringInput,
                    StreamElement::Character,
                    range.start,
                    range.end,
                    || source.get(),
                )
                .map_err(|error| EvalError::out_of_memory(at, error))?
        }
        Op::MakeOutput => {
            let [element] = keys(runtime, frame, args, ["ELEMENT-TYPE"], at)?;
            let element = element.map_or(Ok(StreamElement::Character), |index| {
                let spec = super::type_check::expand_derived_type(runtime, frame.get(index), at)?;
                character_stream_element(runtime, spec)
                    .ok_or_else(|| unsupported_stream_element(at))
            })?;
            runtime
                .heap
                .stream(StreamKind::StringOutput, element, 0, 0, || Word::NIL)
                .map_err(|error| EvalError::out_of_memory(at, error))?
        }
        Op::MakeOutputTo => make_output_to_string(runtime, || arg(0).unwrap(), at)?,
        Op::GetOutput => get_output(runtime, arg(0).unwrap(), at)?,
        Op::Open => open(runtime, frame, args, at)?,
        Op::StreamP => truth(gray::streamp(runtime, arg(0).unwrap())),
        Op::InputStreamP
        | Op::OutputStreamP
        | Op::InteractiveStreamP
        | Op::OpenStreamP
        | Op::ElementType
        | Op::ExternalFormat => {
            let stream = arg(0).unwrap();
            let info = info(runtime, stream, at)?;
            match op {
                Op::InputStreamP => truth(stream_input_p(runtime, stream, at)?),
                Op::OutputStreamP => truth(stream_output_p(runtime, stream, at)?),
                Op::OpenStreamP => truth(!info.closed),
                Op::InteractiveStreamP => truth(runtime.host_io.interactive(info.kind)),
                Op::ElementType => {
                    element_type_word(runtime, effective_element(runtime, stream, at)?, at)?
                }
                Op::ExternalFormat => runtime.intern(&Name::keyword(info.format.keyword()), at)?,
                _ => unreachable!(),
            }
        }
        Op::Close => {
            let [_abort] = keys(runtime, frame, args.start + 1..args.end, ["ABORT"], at)?;
            let stream = arg(0).unwrap();
            let info = info(runtime, stream, at)?;
            if !info.closed && !info.protocol_components && stream_output_p(runtime, stream, at)? {
                flush(runtime, stream, at)?;
            }
            let closed = runtime.heap.stream_close(arg(0).unwrap());
            if closed && info.kind.file() {
                runtime.host_io.files.borrow_mut().remove(&info.handle);
            }
            truth(closed)
        }
        Op::Read | Op::ReadPreservingWhitespace => {
            let stream = input_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            let stream = runtime.root(stream);
            let recursive = arg(3).unwrap_or(Word::NIL) != Word::NIL;
            let preserve = matches!(op, Op::ReadPreservingWhitespace);
            let operation = StreamRead::Form {
                recursive,
                preserve,
            };
            match read_form(runtime, stream.slot.get(), operation, at)? {
                Some(form) => form_value::to_value(runtime, &form)?.slot.get(),
                None => eof(runtime, arg(1), arg(2), stream.slot.get(), at)?,
            }
        }
        Op::ReadChar | Op::PeekChar => {
            let peek = matches!(op, Op::PeekChar);
            let stream = input_stream(runtime, arg(usize::from(peek)).unwrap_or(Word::NIL), at)?;
            let operation = if peek {
                CharacterRead::peek(runtime, arg(0).unwrap_or(Word::NIL), at)?
            } else {
                CharacterRead::Read
            };
            let stream = runtime.root(stream);
            match read_character(runtime, stream.slot.get(), operation, at)? {
                Some(character) => Word::character(character),
                None => eof(
                    runtime,
                    arg(1 + usize::from(peek)),
                    arg(2 + usize::from(peek)),
                    stream.slot.get(),
                    at,
                )?,
            }
        }
        Op::ReadCharNoHang => {
            let stream = input_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            match read_character_no_hang(runtime, stream, at)? {
                NoHang::Character(character) => Word::character(character),
                NoHang::EndOfFile => eof(runtime, arg(1), arg(2), stream, at)?,
                NoHang::Pending => Word::NIL,
            }
        }
        Op::UnreadChar => {
            let character = arg(0).unwrap();
            let character = character.as_character().ok_or_else(|| {
                Fault::type_error(runtime, 
                    character,
                    "CHARACTER",
                    EvalError::new(at, "expected a character"),
                )
            })?;
            let stream = input_stream(runtime, arg(1).unwrap_or(Word::NIL), at)?;
            unread_character(runtime, stream, character, at)?;
            Word::NIL
        }
        Op::ReadLine => {
            let stream = input_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            let (line, missing_newline) = match read_line(runtime, stream, at)? {
                Some((text, missing_newline)) => (
                    runtime
                        .heap
                        .string(&text)
                        .map_err(|error| EvalError::out_of_memory(at, error))?,
                    missing_newline,
                ),
                None => (eof(runtime, arg(1), arg(2), stream, at)?, true),
            };
            values.resize(2);
            values.slots.set(0, line);
            values.slots.set(1, truth(missing_newline));
            return Ok(());
        }
        Op::FilePosition => file_position(runtime, arg(0).unwrap(), arg(1), false, at)?,
        Op::FileLength => file_length(runtime, arg(0).unwrap(), at)?,
        Op::FileStringLength => file_string_length(runtime, arg(0).unwrap(), arg(1).unwrap(), at)?,
        Op::InputStringIndex => file_position(runtime, arg(0).unwrap(), None, true, at)?,
        Op::ReadByte => {
            let stream = arg(0).unwrap();
            match read_byte(runtime, stream, at)? {
                Some(value) => value,
                None => eof(runtime, arg(1), arg(2), arg(0).unwrap(), at)?,
            }
        }
        Op::WriteByte => {
            let byte = arg(0).unwrap();
            let value = runtime.heap.integer(byte).ok_or_else(|| {
                Fault::type_error(runtime, byte, "INTEGER", EvalError::new(at, "expected an integer"))
            })?;
            let stream = arg(1).unwrap();
            write_byte(runtime, stream, &value, byte, at)?;
            byte
        }
        Op::Listen => {
            let stream = input_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            truth(matches!(
                input_availability(runtime, stream, at)?,
                InputAvailability::Ready
            ))
        }
        Op::ClearInput => {
            let stream = input_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            clear_input(runtime, stream, at)?;
            Word::NIL
        }
        Op::ClearOutput => {
            output_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            Word::NIL
        }
        Op::WriteChar => {
            let value = arg(0).unwrap();
            let character = value.as_character().ok_or_else(|| {
                Fault::type_error(runtime, 
                    value,
                    "CHARACTER",
                    EvalError::new(at, "expected a character"),
                )
            })?;
            write_text(
                runtime,
                arg(1).unwrap_or(Word::NIL),
                character.encode_utf8(&mut [0; 4]),
                at,
            )?;
            value
        }
        Op::WriteString | Op::WriteLine => {
            let keys_start = (args.start + 2).min(args.end);
            let [start, end] = keys(runtime, frame, keys_start..args.end, ["START", "END"], at)?;
            let string = runtime.root(arg(0).unwrap());
            let mut text = runtime
                .heap
                .with_string_slice(string.slot.get(), |text| {
                    let range = string_bounds(
                        runtime,
                        text,
                        start.map(|r| frame.get(r)),
                        end.map(|r| frame.get(r)),
                        at,
                    )?;
                    Ok::<_, Fault>(text[range].to_owned())
                })
                .ok_or_else(|| {
                    Fault::type_error(runtime, 
                        string.slot.get(),
                        "STRING",
                        EvalError::new(at, "expected a string"),
                    )
                })??;
            if matches!(op, Op::WriteLine) {
                text.push('\n');
            }
            write_text(runtime, arg(1).unwrap_or(Word::NIL), &text, at)?;
            string.slot.get()
        }
        Op::Terpri | Op::FreshLine => {
            let stream = arg(0).unwrap_or(Word::NIL);
            if matches!(op, Op::FreshLine) && output_column(runtime, stream, at)? == Some(0) {
                Word::NIL
            } else {
                write_text(runtime, stream, "\n", at)?;
                truth(matches!(op, Op::FreshLine))
            }
        }
        Op::FinishOutput | Op::ForceOutput => {
            let (stream, _) = output_stream(runtime, arg(0).unwrap_or(Word::NIL), at)?;
            flush(runtime, stream, at)?;
            Word::NIL
        }
        Op::Prin1 | Op::Princ | Op::Print => unreachable!("printer continuation"),
        Op::MakeBroadcast => make_broadcast(runtime, frame, args, at)?,
        Op::MakeConcatenated => make_concatenated(runtime, frame, args, at)?,
        Op::MakeEcho => make_echo(runtime, frame, args, at)?,
        Op::MakeTwoWay => make_two_way(runtime, frame, args, at)?,
        Op::MakeSynonym => make_synonym(runtime, frame.get(args.start), at)?,
        Op::BroadcastStreams | Op::ConcatenatedStreams => {
            let stream = arg(0).unwrap();
            let info = info(runtime, stream, at)?;
            let expected = if matches!(op, Op::BroadcastStreams) {
                StreamKind::Broadcast
            } else {
                StreamKind::Concatenated
            };
            if info.kind != expected {
                return Err(Fault::type_error(runtime, 
                    stream,
                    if matches!(op, Op::BroadcastStreams) {
                        "BROADCAST-STREAM"
                    } else {
                        "CONCATENATED-STREAM"
                    },
                    EvalError::new(at, "expected a compound stream of that kind"),
                ));
            }
            info.contents
        }
        Op::EchoInputStream
        | Op::TwoWayInputStream
        | Op::EchoOutputStream
        | Op::TwoWayOutputStream => {
            let stream = arg(0).unwrap();
            let info = info(runtime, stream, at)?;
            let expected = if matches!(op, Op::EchoInputStream | Op::EchoOutputStream) {
                StreamKind::Echo
            } else {
                StreamKind::TwoWay
            };
            if info.kind != expected {
                return Err(Fault::type_error(runtime, 
                    stream,
                    if expected == StreamKind::Echo {
                        "ECHO-STREAM"
                    } else {
                        "TWO-WAY-STREAM"
                    },
                    EvalError::new(at, "expected a compound stream of that kind"),
                ));
            }
            let (input, output) = compound_pair(runtime, &info, stream, at)?;
            if matches!(op, Op::EchoInputStream | Op::TwoWayInputStream) {
                input
            } else {
                output
            }
        }
        Op::SynonymStreamSymbol => {
            let stream = arg(0).unwrap();
            let info = info(runtime, stream, at)?;
            if info.kind != StreamKind::Synonym {
                return Err(Fault::type_error(runtime, 
                    stream,
                    "SYNONYM-STREAM",
                    EvalError::new(at, "expected a synonym stream"),
                ));
            }
            info.contents
        }
        Op::ReadSequence => read_sequence(runtime, frame, args, at)?,
        Op::WriteSequence => write_sequence(runtime, frame, args, at)?,
    };
    values.one(result);
    Ok(())
}

fn make_broadcast(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    make_compound(runtime, StreamKind::Broadcast, frame, args, at, false)
}

fn make_concatenated(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    make_compound(runtime, StreamKind::Concatenated, frame, args, at, true)
}

fn make_compound(
    runtime: &Runtime,
    kind: StreamKind,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
    input: bool,
) -> Result<Word, Fault> {
    let mut streams = runtime.root_slot(Word::NIL);
    for index in (args.start..args.end).rev() {
        let stream = frame.get(index);
        let direction_ok = if input {
            stream_input_p(runtime, stream, at)?
        } else {
            stream_output_p(runtime, stream, at)?
        };
        if !direction_ok {
            return Err(direction_type_error(runtime, stream, input, at));
        }
        streams = runtime.root_slot(
            runtime
                .heap
                .cons(|| (frame.get(index), streams.get()))
                .map_err(|error| EvalError::out_of_memory(at, error))?,
        );
    }
    let stream = runtime
        .heap
        .stream(kind, StreamElement::Character, 0, 0, || streams.get())
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(stream)
}

fn make_echo(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    make_directed(runtime, StreamKind::Echo, frame, args, at)
}

fn make_two_way(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    make_directed(runtime, StreamKind::TwoWay, frame, args, at)
}

fn make_directed(
    runtime: &Runtime,
    kind: StreamKind,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let input = frame.get(args.start);
    let output = frame.get(args.start + 1);
    if !stream_input_p(runtime, input, at)? {
        return Err(direction_type_error(runtime, input, true, at));
    }
    if !stream_output_p(runtime, output, at)? {
        return Err(direction_type_error(runtime, output, false, at));
    }
    let pair = runtime.root_slot(
        runtime
            .heap
            .cons(|| (frame.get(args.start), frame.get(args.start + 1)))
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    runtime
        .heap
        .stream(kind, StreamElement::Character, 0, 0, || pair.get())
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn direction_type_error(runtime: &Runtime, stream: Word, input: bool, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        stream,
        if input {
            "INPUT-STREAM"
        } else {
            "OUTPUT-STREAM"
        },
        EvalError::new(
            at,
            if input {
                "expected an input stream"
            } else {
                "expected an output stream"
            },
        ),
    )
}

/// The predicate behind the INPUT-STREAM type specifier. A non-stream object
/// does not belong to the type, so it answers false instead of signaling.
pub(super) fn stream_is_input(runtime: &Runtime, stream: Word) -> bool {
    if runtime.heap.stream_info(stream).is_some() {
        stream_input_p(runtime, stream, 0).unwrap_or(false)
    } else {
        gray::has_direction(runtime, stream, true)
    }
}

/// The predicate behind the OUTPUT-STREAM type specifier.
pub(super) fn stream_is_output(runtime: &Runtime, stream: Word) -> bool {
    if runtime.heap.stream_info(stream).is_some() {
        stream_output_p(runtime, stream, 0).unwrap_or(false)
    } else {
        gray::has_direction(runtime, stream, false)
    }
}

fn make_synonym(runtime: &Runtime, symbol: Word, at: usize) -> Result<Word, Fault> {
    if !runtime.heap.is_symbol(symbol) {
        return Err(Fault::type_error(runtime, 
            symbol,
            "SYMBOL",
            EvalError::new(at, "expected a symbol"),
        ));
    }
    let symbol = runtime.root_slot(symbol);
    let stream = runtime
        .heap
        .stream(StreamKind::Synonym, StreamElement::Character, 0, 0, || {
            symbol.get()
        })
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(stream)
}

fn read_sequence(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let sequence = Sequence::new(runtime, frame, args.start, at)?;
    let range = sequence_bounds(runtime, frame, args.start + 2..args.end, sequence.len, at)?;
    let stream = frame.get(args.start + 1);
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    if info.closed || !info.kind.input() {
        return Err(stream_error(runtime, stream, at, "expected an open input stream"));
    }
    let target = frame.get(args.start);
    if info.kind.file()
        && info.element == StreamElement::UnsignedByte(8)
        && matches!(sequence.kind, Kind::Vector(ArrayElement::General))
        && runtime
            .heap
            .array_info(target)
            .is_some_and(|array| array.simple())
    {
        let mut file = stream_file(runtime, info.handle, stream, at)?;
        let mut buffer = vec![0; range.len().min(8192)];
        let mut pos = range.start;
        while pos < range.end {
            let limit = buffer.len().min(range.end - pos);
            let n = file
                .read(&mut buffer[..limit])
                .map_err(|error| io_error(runtime, stream, at, error))?;
            if n == 0 {
                break;
            }
            for &octet in &buffer[..n] {
                let value = Word::fixnum(isize::from(octet)).expect("octet fits a fixnum");
                super::arrays::store(runtime, target, pos, value, at)?;
                pos += 1;
            }
        }
        return fixnum(pos, at);
    }
    let bytes = is_byte_element(effective_element(runtime, stream, at)?);
    let stream = runtime.root_slot(stream);
    if matches!(
        sequence.kind,
        Kind::Treelist(crate::heap::TreelistKind::Persistent)
    ) {
        super::treelist::expect_mutable_treelist(runtime, frame.get(args.start), at)?;
    }
    let cursor = runtime.root_frame(usize::from(matches!(sequence.kind, Kind::List)));
    if let Kind::List = sequence.kind {
        cursor.set(0, sequence.list_tail(runtime, range.start));
    }
    let mut position = range.start;
    for index in range {
        let value = if bytes {
            read_byte(runtime, stream.get(), at)?
        } else {
            read_character(runtime, stream.get(), CharacterRead::Read, at)?.map(Word::character)
        };
        let Some(value) = value else { break };
        match sequence.kind {
            Kind::List => {
                let tail = cursor.get(0);
                runtime.heap.set_cons_car(tail, value);
                cursor.set(
                    0,
                    runtime
                        .heap
                        .cons_fields(tail)
                        .expect("validated list index")
                        .1,
                );
            }
            Kind::Vector(_) => {
                super::arrays::store(runtime, frame.get(args.start), index, value, at)?;
            }
            Kind::Treelist(_) => {
                let seq = runtime.root_slot(frame.get(args.start));
                let val = runtime.root_slot(value);
                runtime
                    .heap
                    .mutable_treelist_set(&seq, index, &val)
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
            }
        }
        position = index + 1;
    }
    fixnum(position, at)
}

fn write_sequence(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let sequence = Sequence::new(runtime, frame, args.start, at)?;
    let range = sequence_bounds(runtime, frame, args.start + 2..args.end, sequence.len, at)?;
    let stream = frame.get(args.start + 1);
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    if info.closed || !info.kind.output() {
        return Err(stream_error(runtime, stream, at, "expected an open output stream"));
    }
    if !is_byte_element(effective_element(runtime, stream, at)?) {
        let text = sequence.character_text(runtime, range, at)?;
        write_text(runtime, stream, &text, at)?;
        return Ok(frame.get(args.start));
    }
    let elements = sequence.snapshot(runtime, range, at)?;
    if info.kind.file()
        && info.element == StreamElement::UnsignedByte(8)
        && let Some(octets) = (0..elements.len())
            .map(|i| u8::try_from(elements.get(i).as_fixnum()?).ok())
            .collect::<Option<Vec<_>>>()
    {
        write_file_bytes(runtime, &info, &octets, stream, at)?;
        return Ok(frame.get(args.start));
    }
    for index in 0..elements.len() {
        let word = elements.get(index);
        let value = runtime.heap.integer(word).ok_or_else(|| {
            Fault::type_error(runtime, word, "INTEGER", EvalError::new(at, "expected an integer"))
        })?;
        write_byte(runtime, stream, &value, word, at)?;
    }
    Ok(frame.get(args.start))
}

fn sequence_bounds(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    len: usize,
    at: usize,
) -> Result<Range<usize>, Fault> {
    let [start, end] = keys(runtime, frame, args, ["START", "END"], at)?;
    let start = start
        .map(|reg| index(runtime, frame.get(reg), at))
        .transpose()?
        .unwrap_or(0);
    let end = end
        .map(|reg| frame.get(reg))
        .filter(|&word| word != Word::NIL)
        .map(|word| index(runtime, word, at))
        .transpose()?
        .unwrap_or(len);
    if start <= end && end <= len {
        Ok(start..end)
    } else {
        Err(Fault::plain(
            Class::TypeError,
            EvalError::new(at, "sequence bounds are outside the sequence"),
        ))
    }
}

fn fixnum(value: usize, at: usize) -> Result<Word, Fault> {
    isize::try_from(value)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| EvalError::new(at, "value exceeds the supported fixnum range").into())
}

fn info(runtime: &Runtime, stream: Word, at: usize) -> Result<StreamState, Fault> {
    runtime
        .heap
        .stream_info(stream)
        .ok_or_else(|| Fault::type_error(runtime, stream, "STREAM", EvalError::new(at, "expected a stream")))
}

fn resolve(runtime: &Runtime, designator: Word, standard: &str, at: usize) -> Result<Word, Fault> {
    let special = match designator {
        Word::NIL => standard,
        Word::T => "*TERMINAL-IO*",
        stream => return Ok(stream),
    };
    let symbol = runtime.intern(&Name::common_lisp(special), at)?;
    symbol_value(runtime, symbol, at)
}

pub(crate) fn input_stream(runtime: &Runtime, designator: Word, at: usize) -> Result<Word, Fault> {
    input_stream_info(runtime, designator, at).map(|(stream, _)| stream)
}

fn input_stream_info(
    runtime: &Runtime,
    designator: Word,
    at: usize,
) -> Result<(Word, StreamState), Fault> {
    let original = resolve(runtime, designator, "*STANDARD-INPUT*", at)?;
    require_open_stream(runtime, original, true, at)
}

pub(super) fn require_input_stream(
    runtime: &Runtime,
    original: Word,
    at: usize,
) -> Result<Word, Fault> {
    require_open_stream(runtime, original, true, at).map(|(stream, _)| stream)
}

fn output_stream(
    runtime: &Runtime,
    designator: Word,
    at: usize,
) -> Result<(Word, StreamState), Fault> {
    let original = resolve(runtime, designator, "*STANDARD-OUTPUT*", at)?;
    require_open_stream(runtime, original, false, at)
}

fn require_open_stream(
    runtime: &Runtime,
    original: Word,
    input: bool,
    at: usize,
) -> Result<(Word, StreamState), Fault> {
    let failure = |stream| {
        let message = if input {
            "expected an open input stream"
        } else {
            "expected an open output stream"
        };
        stream_error(runtime, stream, at, message)
    };
    let original_info = info(runtime, original, at)?;
    if original_info.closed {
        return Err(failure(original));
    }
    let (stream, info) = follow_synonyms(runtime, original, original_info, at, |_| original)?;
    if info.closed || !supports_direction(info.kind, input) {
        return Err(failure(stream));
    }
    Ok((stream, info))
}

const SYNONYM_DEPTH: usize = 16;

fn synonym_target(
    runtime: &Runtime,
    info: &StreamState,
    at: usize,
) -> Result<(Word, StreamState), Fault> {
    let target = symbol_value(runtime, info.contents, at)?;
    let info = runtime.heap.stream_info(target).ok_or_else(|| {
        Fault::type_error(runtime, 
            target,
            "STREAM",
            EvalError::new(at, "the synonym stream's symbol value is not a stream"),
        )
    })?;
    Ok((target, info))
}

fn resolve_synonym(
    runtime: &Runtime,
    stream: Word,
    at: usize,
) -> Result<(Word, StreamState), Fault> {
    follow_synonyms(runtime, stream, info(runtime, stream, at)?, at, |_| stream)
}

fn follow_synonyms(
    runtime: &Runtime,
    mut stream: Word,
    mut info: StreamState,
    at: usize,
    cycle_stream: impl FnOnce(Word) -> Word,
) -> Result<(Word, StreamState), Fault> {
    for _ in 0..SYNONYM_DEPTH {
        if info.kind != StreamKind::Synonym {
            return Ok((stream, info));
        }
        (stream, info) = synonym_target(runtime, &info, at)?;
    }
    Err(stream_error(
        runtime,
        cycle_stream(stream),
        at,
        "stream designator cycle",
    ))
}

fn stream_input_p(runtime: &Runtime, stream: Word, at: usize) -> Result<bool, Fault> {
    stream_direction(runtime, stream, at, true)
}

fn stream_output_p(runtime: &Runtime, stream: Word, at: usize) -> Result<bool, Fault> {
    stream_direction(runtime, stream, at, false)
}

fn stream_direction(
    runtime: &Runtime,
    stream: Word,
    at: usize,
    input: bool,
) -> Result<bool, Fault> {
    let stream = gray::synonym_leaf(runtime, stream, false, at)?;
    if gray::is_stream_instance(runtime, stream) {
        return Ok(gray::has_direction(runtime, stream, input));
    }
    Ok(supports_direction(info(runtime, stream, at)?.kind, input))
}

fn supports_direction(kind: StreamKind, input: bool) -> bool {
    if input { kind.input() } else { kind.output() }
}

fn compound_pair(
    runtime: &Runtime,
    info: &StreamState,
    stream: Word,
    at: usize,
) -> Result<(Word, Word), Fault> {
    runtime
        .heap
        .cons_fields(info.contents)
        .ok_or_else(|| stream_error(runtime, stream, at, "invalid compound stream contents"))
}

fn effective_element(runtime: &Runtime, stream: Word, at: usize) -> Result<StreamElement, Fault> {
    effective_element_depth(runtime, stream, at, 0)
}

fn effective_element_depth(
    runtime: &Runtime,
    stream: Word,
    at: usize,
    depth: usize,
) -> Result<StreamElement, Fault> {
    if depth == SYNONYM_DEPTH {
        return Err(stream_error(runtime, stream, at, "stream designator cycle"));
    }
    let info = info(runtime, stream, at)?;
    Ok(match info.kind {
        StreamKind::Synonym => {
            let (target, _) = synonym_target(runtime, &info, at)?;
            effective_element_depth(runtime, target, at, depth + 1)?
        }
        StreamKind::TwoWay | StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            effective_element_depth(runtime, input, at, depth + 1)?
        }
        StreamKind::Concatenated => match remaining_stream(runtime, &info) {
            Some(first) => effective_element_depth(runtime, first, at, depth + 1)?,
            None => StreamElement::Character,
        },
        StreamKind::Broadcast => match last_stream(runtime, &info) {
            Some(last) => effective_element_depth(runtime, last, at, depth + 1)?,
            None => StreamElement::Character,
        },
        _ => info.element,
    })
}

fn remaining_stream(runtime: &Runtime, info: &StreamState) -> Option<Word> {
    runtime
        .heap
        .cons_fields(info.contents)
        .map(|(head, _)| head)
}

fn last_stream(runtime: &Runtime, info: &StreamState) -> Option<Word> {
    let mut last = None;
    let mut contents = info.contents;
    while let Some((stream, tail)) = runtime.heap.cons_fields(contents) {
        last = Some(stream);
        contents = tail;
    }
    last
}

fn read_concatenated<T>(
    runtime: &Runtime,
    stream: Word,
    contents: Word,
    mut read: impl FnMut(Word) -> Result<Option<T>, Fault>,
) -> Result<Option<T>, Fault> {
    let roots = runtime.root_frame(2);
    roots.set(0, stream);
    roots.set(1, contents);
    while let Some((component, _)) = runtime.heap.cons_fields(roots.get(1)) {
        if let Some(value) = read(component)? {
            return Ok(Some(value));
        }
        let (_, remaining) = runtime
            .heap
            .cons_fields(roots.get(1))
            .expect("current component list is rooted");
        runtime.heap.stream_set_contents(roots.get(0), remaining);
        roots.set(1, remaining);
    }
    Ok(None)
}

fn for_each_broadcast(
    runtime: &Runtime,
    contents: Word,
    mut operation: impl FnMut(Word) -> Result<(), Fault>,
) -> Result<(), Fault> {
    let cursor = runtime.root_frame(1);
    cursor.set(0, contents);
    while let Some((component, _)) = runtime.heap.cons_fields(cursor.get(0)) {
        operation(component)?;
        let (_, tail) = runtime
            .heap
            .cons_fields(cursor.get(0))
            .expect("current component list is rooted");
        cursor.set(0, tail);
    }
    Ok(())
}

fn stream_error(runtime: &Runtime, stream: Word, at: usize, message: &str) -> Fault {
    Fault::stream_error(
        runtime,
        Class::StreamError,
        stream,
        EvalError::new(at, message),
    )
}

fn io_error(runtime: &Runtime, stream: Word, at: usize, error: io::Error) -> Fault {
    Fault::stream_error(
        runtime,
        Class::StreamError,
        stream,
        EvalError::io_error(at, error),
    )
}

fn stream_file(
    runtime: &Runtime,
    handle: usize,
    stream: Word,
    at: usize,
) -> Result<RefMut<'_, File>, Fault> {
    runtime
        .host_io
        .file(handle)
        .ok_or_else(|| stream_error(runtime, stream, at, "stream has no open file"))
}

/// LOAD takes source forms from a character stream and a compiled file from an octet stream.
pub(crate) fn read_fasl_octets(
    runtime: &Runtime,
    stream: &RootSlot,
    at: usize,
) -> Result<Option<Vec<u8>>, Fault> {
    let (input, info) = resolve_synonym(runtime, stream.get(), at)?;
    match byte_element(effective_element(runtime, input, at)?) {
        None => return Ok(None),
        Some((8, false)) => {}
        Some(_) => {
            return Err(stream_error(
                runtime,
                input,
                at,
                "compiled input requires an (UNSIGNED-BYTE 8) stream",
            ));
        }
    }
    let mut bytes = Vec::new();
    if info.kind.file() {
        let mut file = stream_file(runtime, info.handle, input, at)?;
        file.read_to_end(&mut bytes)
            .map_err(|error| io_error(runtime, input, at, error))?;
    } else {
        while let Some(value) = read_byte(runtime, stream.get(), at)? {
            let octet = value
                .as_fixnum()
                .and_then(|value| u8::try_from(value).ok())
                .ok_or_else(|| {
                    stream_error(runtime, stream.get(), at, "compiled input contains a non-octet")
                })?;
            bytes.push(octet);
        }
    }
    Ok(Some(bytes))
}

fn element_type_word(runtime: &Runtime, element: StreamElement, at: usize) -> Result<Word, Fault> {
    let name = match element {
        StreamElement::Nil => return Ok(Word::NIL),
        StreamElement::Character => "CHARACTER",
        StreamElement::BaseChar => "BASE-CHAR",
        StreamElement::StandardChar => "STANDARD-CHAR",
        StreamElement::UnsignedByte(bits) => {
            return byte_type_word(runtime, "UNSIGNED-BYTE", bits, at);
        }
        StreamElement::SignedByte(bits) => {
            return byte_type_word(runtime, "SIGNED-BYTE", bits, at);
        }
    };
    runtime
        .intern(&Name::common_lisp(name), at)
        .map_err(Fault::from)
}

fn byte_type_word(runtime: &Runtime, name: &str, bits: u16, at: usize) -> Result<Word, Fault> {
    let head = runtime.root(runtime.intern(&Name::common_lisp(name), at)?);
    let size = runtime
        .fixnum(isize::try_from(bits).expect("bit width fits a fixnum"))
        .expect("bit width is a fixnum");
    let nil = runtime.root(Word::NIL);
    let tail = runtime
        .cons(&size, &nil)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(runtime
        .cons(&head, &tail)
        .map_err(|error| EvalError::out_of_memory(at, error))?
        .slot
        .get())
}

fn eof(
    runtime: &Runtime,
    eof_error: Option<Word>,
    value: Option<Word>,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    if eof_error == Some(Word::NIL) {
        return Ok(value.unwrap_or(Word::NIL));
    }
    Err(Fault::stream_error(
        runtime,
        Class::EndOfFile,
        stream,
        EvalError::new(at, "end of file"),
    ))
}

/// An error in unread input is relevant only if this read reached it.
fn read_needs_more_input(result: &Result<Option<Form>, EvalError>) -> bool {
    match result {
        Ok(form) => form.is_none(),
        Err(error) => error.is_read_eof(),
    }
}

fn read_fault(runtime: &Runtime, error: EvalError, at: usize, start: usize, stream: Word) -> Fault {
    if error.kind() == EvalErrorKind::Read {
        let (class, host) = error.in_lisp_read(at, start);
        Fault::stream_error(runtime, class, stream, host)
    } else {
        error.into()
    }
}

pub(super) fn read_form(
    runtime: &Runtime,
    stream: Word,
    operation: StreamRead,
    at: usize,
) -> Result<Option<Form>, Fault> {
    let info = info(runtime, stream, at)?;
    match info.kind {
        StreamKind::FileInput | StreamKind::FileIo => read_buffered(
            runtime,
            FileInput::open(runtime, stream, &info, at)?,
            operation,
            at,
        ),
        _ => read_buffered(
            runtime,
            CompoundInput::new(runtime, stream, &info, at),
            operation,
            at,
        ),
    }
}

/// Growing with the unfinished token keeps rescans of a long string or
/// escaped symbol linear. A short form needs only a small initial window.
fn read_window_bytes(buffered: usize, token_start: usize) -> usize {
    512.max(buffered - token_start)
}

fn read_buffered(
    runtime: &Runtime,
    mut input: impl StreamInput,
    operation: StreamRead,
    at: usize,
) -> Result<Option<Form>, Fault> {
    if let Some(ch) = operation.introducer() {
        input.prepend_macro_character(ch);
    }
    let mut reader = Reader::stream(runtime, &mut input);
    let (result, consumed) = operation.parse(&mut reader);
    input.finish(runtime, result, consumed, at)
}

fn consume_input_prefix(
    runtime: &Runtime,
    stream: &Root<'_>,
    bytes: usize,
    at: usize,
) -> Result<(), Fault> {
    let mut remaining = bytes;
    while remaining != 0 {
        let Some(character) = read_character(runtime, stream.slot.get(), CharacterRead::Read, at)?
        else {
            return Err(stream_error(
                runtime,
                stream.slot.get(),
                at,
                "compound stream ended while advancing the parsed form",
            ));
        };
        remaining = remaining.checked_sub(character.len_utf8()).ok_or_else(|| {
            stream_error(
                runtime,
                stream.slot.get(),
                at,
                "compound stream position crossed a character boundary",
            )
        })?;
    }
    Ok(())
}

fn read_byte(runtime: &Runtime, stream: Word, at: usize) -> Result<Option<Word>, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            read_byte(runtime, input, at)
        }
        StreamKind::Echo => {
            let (input, output) = compound_pair(runtime, &info, stream, at)?;
            let output = runtime.root_slot(output);
            let Some(datum) = read_byte(runtime, input, at)? else {
                return Ok(None);
            };
            let value = runtime
                .heap
                .integer(datum)
                .expect("binary input returns an integer");
            write_byte(runtime, output.get(), &value, datum, at)?;
            Ok(Some(datum))
        }
        StreamKind::Concatenated => {
            read_concatenated(runtime, stream, info.contents, |component| {
                read_byte(runtime, component, at)
            })
        }
        StreamKind::Broadcast => Err(stream_error(
            runtime,
            stream,
            at,
            "READ-BYTE requires an input stream",
        )),
        _ => {
            if info.closed || !info.kind.input() {
                return Err(stream_error(runtime, stream, at, "expected an open input stream"));
            }
            let Some((bits, signed)) = byte_element(info.element) else {
                return Err(stream_error(runtime, stream, at, "READ-BYTE requires a byte stream"));
            };
            read_file_byte(runtime, &info, bits, signed, stream, at)
        }
    }
}

fn write_byte(
    runtime: &Runtime,
    stream: Word,
    value: &BigInt,
    datum: Word,
    at: usize,
) -> Result<(), Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (_, output) = compound_pair(runtime, &info, stream, at)?;
            write_byte(runtime, output, value, datum, at)
        }
        StreamKind::Broadcast => for_each_broadcast(runtime, info.contents, |component| {
            write_byte(runtime, component, value, datum, at)
        }),
        StreamKind::Concatenated => Err(stream_error(
            runtime,
            stream,
            at,
            "WRITE-BYTE requires an output stream",
        )),
        _ => {
            if info.closed || !info.kind.output() {
                return Err(stream_error(runtime, stream, at, "expected an open output stream"));
            }
            let Some((bits, signed)) = byte_element(info.element) else {
                return Err(stream_error(
                    runtime,
                    stream,
                    at,
                    "WRITE-BYTE requires a byte stream",
                ));
            };
            write_file_byte(runtime, &info, bits, signed, value, datum, stream, at)
        }
    }
}

fn file_position(
    runtime: &Runtime,
    stream: Word,
    position: Option<Word>,
    source_index: bool,
    at: usize,
) -> Result<Word, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    if info.closed {
        return Err(stream_error(runtime, stream, at, "stream is closed"));
    }
    if info.kind == StreamKind::Broadcast {
        return match last_stream(runtime, &info) {
            Some(last) => file_position(runtime, last, position, false, at),
            None => fixnum(0, at),
        };
    }
    if info.kind == StreamKind::StringOutput {
        let mut chars = 0usize;
        let mut contents = info.contents;
        while let Some((chunk, tail)) = runtime.heap.cons_fields(contents) {
            chars += runtime
                .heap
                .with_string_slice(chunk, |text| text.chars().count())
                .expect("output chunk");
            contents = tail;
        }
        return fixnum(chars, at);
    }
    if info.kind == StreamKind::StringOutputTo {
        let fill = runtime
            .heap
            .array_info(info.contents)
            .and_then(|array| array.fill_pointer)
            .expect("string destination has a fill pointer");
        return fixnum(fill, at);
    }
    if info.kind.file() {
        return file_stream_position(runtime, &info, stream, position, at);
    }
    if info.kind != StreamKind::StringInput {
        return Ok(Word::NIL);
    }
    if let Some(position) = position {
        let start = runtime.find_named_symbol(&Name::keyword("START"));
        let end = runtime.find_named_symbol(&Name::keyword("END"));
        let offset = if Some(position) == start {
            Some(info.start)
        } else if Some(position) == end {
            Some(info.end)
        } else {
            let index = index(runtime, position, at)?;
            runtime
                .heap
                .with_string_slice(info.contents, |source| {
                    byte_index(&source[info.start..info.end], index)
                        .map(|offset| info.start + offset)
                })
                .expect("input source")
        };
        if let Some(offset) = offset {
            runtime.heap.stream_set_cursor(stream, offset, false);
            return Ok(Word::T);
        }
        return Err(stream_error(
            runtime,
            stream,
            at,
            "position is outside the input stream",
        ));
    }
    let start = if source_index { 0 } else { info.start };
    let count = runtime
        .heap
        .with_string_slice(info.contents, |source| {
            source[start..info.cursor].chars().count()
        })
        .expect("input source");
    isize::try_from(count)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            EvalError::new(at, "stream position exceeds the supported fixnum range").into()
        })
}

fn file_string_length(
    runtime: &Runtime,
    stream: Word,
    object: Word,
    at: usize,
) -> Result<Word, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    if info.closed {
        return Err(stream_error(runtime, stream, at, "stream is closed"));
    }
    if info.kind == StreamKind::Broadcast {
        return match last_stream(runtime, &info) {
            Some(last) => file_string_length(runtime, last, object, at),
            None => fixnum(1, at),
        };
    }
    let character = object.as_character();
    let text = runtime.heap.string_contents(object);
    if character.is_none() && text.is_none() {
        return Err(Fault::type_error(runtime, 
            object,
            "CHARACTER-OR-STRING",
            EvalError::new(at, "expected a character or string"),
        ));
    }
    let length = match info.kind {
        StreamKind::FileOutput | StreamKind::FileIo => {
            if is_byte_element(info.element) {
                return Err(stream_error(runtime, stream, at, "requires a character stream"));
            }
            let mut buffer = [0; 4];
            let text = match character {
                Some(ch) => ch.encode_utf8(&mut buffer),
                None => text.as_deref().expect("a string"),
            };
            info.format
                .encoded_byte_length(text)
                .map_err(|error| io_error(runtime, stream, at, error))?
        }
        StreamKind::StringOutput | StreamKind::StringOutputTo => {
            character.map_or_else(|| text.as_deref().expect("a string").chars().count(), |_| 1)
        }
        _ => return Ok(Word::NIL),
    };
    isize::try_from(length)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            EvalError::new(at, "string length exceeds the supported fixnum range").into()
        })
}

fn character_stream_element(runtime: &Runtime, value: Word) -> Option<StreamElement> {
    if value == Word::NIL {
        return Some(StreamElement::Nil);
    }
    let name = super::condition::named_word(runtime, value, crate::package::Package::COMMON_LISP);
    match name.as_deref() {
        Some("CHARACTER") => Some(StreamElement::Character),
        Some("BASE-CHAR") => Some(StreamElement::BaseChar),
        Some("STANDARD-CHAR") => Some(StreamElement::StandardChar),
        _ => None,
    }
}

fn unsupported_stream_element(at: usize) -> Fault {
    Fault::plain(
        Class::TypeError,
        EvalError::new(at, "unsupported stream element type"),
    )
}

fn string_bounds(
    runtime: &Runtime,
    text: &str,
    start: Option<Word>,
    end: Option<Word>,
    at: usize,
) -> Result<Range<usize>, Fault> {
    strings::bounds(
        runtime,
        text,
        start,
        end,
        "string bounds are outside the string",
        at,
    )
    .map(|(range, _)| range)
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

#[cfg(test)]
mod tests {
    use crate::{Runtime, heap::Heap};

    #[test]
    fn compound_character_io_preserves_streams_during_relocation() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(128 << 10);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let values = runtime
            .eval(
                r#"(let* ((payload (make-string 4096 :initial-element #\λ))
                          (text (concatenate 'string "\"" payload "\" " payload))
                          (first (make-string-output-stream))
                          (second (make-string-output-stream))
                          (out (make-broadcast-stream first second))
                          (input (make-echo-stream (make-string-input-stream text) out))
                          (stream (make-concatenated-stream
                                    input (make-string-input-stream "β")))
                          (buffer (make-array (length payload))))
                     (assert (string= (read stream) payload))
                     (assert (= (read-sequence buffer stream) (length payload)))
                     (assert (string= (coerce buffer 'string) payload))
                     (assert (char= (read-char stream) #\β))
                     (values (string= (get-output-stream-string first) text)
                             (string= (get-output-stream-string second) text)))"#,
            )
            .unwrap();
        assert_eq!(values.len(), 2);
        assert!(values.iter().all(|value| !value.is_nil()));
        assert!(runtime.heap_stats().collections > 1);
    }
}

impl HostIo {
    pub(crate) fn check_image_idle(&self) -> Result<(), EvalError> {
        if self.dribble.borrow().is_some() {
            return Err(EvalError::new(
                0,
                "close DRIBBLE before saving a runtime image; transcript files are host resources",
            ));
        }
        Ok(())
    }
}
