use std::io::Seek;

use super::{
    InputAvailability, compound_pair, info, io_error, is_byte_element, read_concatenated,
    resolve_synonym, stream_error, stream_file, symbol_value, write_text,
};
use crate::condition::Fault;
use crate::heap::{RootSlot, StreamKind, StreamState, Word};
use crate::symbol::Name;
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
pub(super) enum CharacterRead {
    Read,
    Peek,
    SkipWhitespace,
    Find(char),
}

impl CharacterRead {
    pub(super) fn peek(runtime: &Runtime, selector: Word, at: usize) -> Result<Self, Fault> {
        match selector {
            Word::NIL => Ok(Self::Peek),
            Word::T => Ok(Self::SkipWhitespace),
            word => word.as_character().map(Self::Find).ok_or_else(|| {
                Fault::type_error(runtime, word, "CHARACTER", EvalError::new(at, "invalid peek type"))
            }),
        }
    }

    fn consumes(self) -> bool {
        matches!(self, Self::Read)
    }

    pub(super) fn skips(
        self,
        runtime: &Runtime,
        character: char,
        at: usize,
    ) -> Result<bool, Fault> {
        match self {
            Self::Read | Self::Peek => Ok(false),
            Self::Find(target) => Ok(character != target),
            Self::SkipWhitespace => {
                let symbol = runtime.intern(&Name::common_lisp("*READTABLE*"), at)?;
                let table = symbol_value(runtime, symbol, at)?;
                let syntax = crate::read::effective_syntax(runtime, table, character)?;
                Ok(syntax == crate::read::CharSyntax::Whitespace)
            }
        }
    }

    fn scan_text(
        self,
        runtime: &Runtime,
        text: &str,
        at: usize,
    ) -> Result<(usize, Option<char>), Fault> {
        for (offset, character) in text.char_indices() {
            if !self.skips(runtime, character, at)? {
                let advance = if self.consumes() {
                    character.len_utf8()
                } else {
                    0
                };
                return Ok((offset + advance, Some(character)));
            }
        }
        Ok((text.len(), None))
    }
}

pub(super) fn read_character(
    runtime: &Runtime,
    stream: Word,
    operation: CharacterRead,
    at: usize,
) -> Result<Option<char>, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            read_character(runtime, input, operation, at)
        }
        StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            read_echo_character(runtime, stream, input, operation, at)
        }
        StreamKind::Concatenated => {
            read_concatenated(runtime, stream, info.contents, |component| {
                read_character(runtime, component, operation, at)
            })
        }
        StreamKind::Broadcast => Err(stream_error(
            runtime,
            stream,
            at,
            "READ-CHAR requires an input stream",
        )),
        StreamKind::StringInput => {
            let (advance, character) = runtime
                .heap
                .with_string_slice(info.contents, |source| {
                    operation.scan_text(runtime, &source[info.cursor..info.end], at)
                })
                .expect("input source")?;
            runtime.heap.stream_set_cursor(
                stream,
                info.cursor + advance,
                character.is_some() && operation.consumes(),
            );
            Ok(character)
        }
        kind if kind.file() => read_file_character(runtime, stream, &info, operation, at),
        _ => read_host_character(runtime, stream, operation, at),
    }
}

fn read_echo_character(
    runtime: &Runtime,
    stream: Word,
    input: Word,
    operation: CharacterRead,
    at: usize,
) -> Result<Option<char>, Fault> {
    let echo = runtime.root_slot(stream);
    let input = runtime.root_slot(input);
    if operation.consumes() {
        let character = read_character(runtime, input.get(), operation, at)?;
        if let Some(character) = character {
            echo_character(runtime, &echo, character, at)?;
        }
        return Ok(character);
    }
    // Skipped characters are consumed and echoed. The selected character stays unread.
    while let Some(character) = read_character(runtime, input.get(), CharacterRead::Peek, at)? {
        if !operation.skips(runtime, character, at)? {
            return Ok(Some(character));
        }
        read_character(runtime, echo.get(), CharacterRead::Read, at)?;
    }
    Ok(None)
}

fn read_file_character(
    runtime: &Runtime,
    stream: Word,
    info: &StreamState,
    operation: CharacterRead,
    at: usize,
) -> Result<Option<char>, Fault> {
    if is_byte_element(info.element) {
        return Err(stream_error(
            runtime,
            stream,
            at,
            "READ-CHAR requires a character stream",
        ));
    }
    let mut file = stream_file(runtime, info.handle, stream, at)?;
    runtime.heap.stream_set_unread(stream, None);
    while let Some(character) = info
        .format
        .read_char(&mut *file)
        .map_err(|error| io_error(runtime, stream, at, error))?
    {
        if operation.skips(runtime, character, at)? {
            continue;
        }
        if operation.consumes() {
            runtime.heap.stream_set_unread(stream, Some(character));
        } else {
            let length = info.format.decoded_char_length(character);
            file.seek_relative(-(length as i64))
                .map_err(|error| io_error(runtime, stream, at, error))?;
        }
        return Ok(Some(character));
    }
    Ok(None)
}

fn read_host_character(
    runtime: &Runtime,
    stream: Word,
    operation: CharacterRead,
    at: usize,
) -> Result<Option<char>, Fault> {
    let mut input = runtime.host_io.input.borrow_mut();
    loop {
        let (advance, character) = operation.scan_text(runtime, &input.text[input.cursor..], at)?;
        input.cursor += advance;
        input.unread_allowed = character.is_some() && operation.consumes();
        if character.is_some() || input.eof {
            return Ok(character);
        }
        runtime
            .host_io
            .refill(&mut input)
            .map_err(|error| io_error(runtime, stream, at, error))?;
    }
}

pub(super) fn unread_character(
    runtime: &Runtime,
    stream: Word,
    character: char,
    at: usize,
) -> Result<(), Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    let mismatch = || {
        stream_error(
            runtime,
            stream,
            at,
            "character is not the most recently read character",
        )
    };
    match info.kind {
        StreamKind::TwoWay => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            return unread_character(runtime, input, character, at);
        }
        StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            let echoed = info
                .last_char
                .filter(|&last| info.unread_allowed && last == character);
            let Some(_) = echoed else {
                return Err(mismatch());
            };
            unread_character(runtime, input, character, at)?;
            runtime
                .heap
                .stream_set_echo_character(stream, character, true);
            return Ok(());
        }
        StreamKind::Concatenated => {
            let Some((head, _)) = runtime.heap.cons_fields(info.contents) else {
                return Err(mismatch());
            };
            return unread_character(runtime, head, character, at);
        }
        StreamKind::Broadcast => {
            return Err(stream_error(
                runtime,
                stream,
                at,
                "UNREAD-CHAR requires an input stream",
            ));
        }
        _ => {}
    }
    if info.kind == StreamKind::StringInput {
        let previous = runtime
            .heap
            .with_string_slice(info.contents, |source| {
                source[info.start..info.cursor].char_indices().next_back()
            })
            .expect("input source");
        let Some((start, _)) = previous.filter(|&(_, c)| info.unread_allowed && c == character)
        else {
            return Err(mismatch());
        };
        runtime
            .heap
            .stream_set_cursor(stream, info.start + start, false);
    } else if info.kind.file() {
        if is_byte_element(info.element) {
            return Err(stream_error(
                runtime,
                stream,
                at,
                "UNREAD-CHAR requires a character stream",
            ));
        }
        let Some(last) = info
            .last_char
            .filter(|&last| info.unread_allowed && last == character)
        else {
            return Err(mismatch());
        };
        let mut file = stream_file(runtime, info.handle, stream, at)?;
        let length = info.format.decoded_char_length(last);
        file.seek_relative(-(length as i64))
            .map_err(|error| io_error(runtime, stream, at, error))?;
        runtime.heap.stream_set_unread(stream, None);
    } else {
        let mut input = runtime.host_io.input.borrow_mut();
        let previous = input.text[..input.cursor].char_indices().next_back();
        let Some((start, _)) = previous.filter(|&(_, c)| input.unread_allowed && c == character)
        else {
            return Err(mismatch());
        };
        input.cursor = start;
        input.unread_allowed = false;
    }
    Ok(())
}

pub(super) fn read_line(
    runtime: &Runtime,
    stream: Word,
    at: usize,
) -> Result<Option<(String, bool)>, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            return read_line(runtime, input, at);
        }
        StreamKind::Echo => {
            let stream = runtime.root_slot(stream);
            return collect_line(|| read_character(runtime, stream.get(), CharacterRead::Read, at));
        }
        StreamKind::Concatenated => {
            let mut line = String::new();
            let newline = read_concatenated(runtime, stream, info.contents, |component| {
                let Some((text, missing_newline)) = read_line(runtime, component, at)? else {
                    return Ok(None);
                };
                line.push_str(&text);
                Ok((!missing_newline).then_some(()))
            })?;
            return Ok((newline.is_some() || !line.is_empty()).then_some((line, newline.is_none())));
        }
        StreamKind::Broadcast => {
            return Err(stream_error(
                runtime,
                stream,
                at,
                "READ-LINE requires an input stream",
            ));
        }
        _ => {}
    }
    let take = |text: &str| {
        let newline = text.find('\n');
        if text.is_empty() {
            return (0, None);
        }
        let line_end = newline.unwrap_or(text.len());
        (
            line_end + usize::from(newline.is_some()),
            Some((text[..line_end].to_owned(), newline.is_none())),
        )
    };
    if info.kind == StreamKind::StringInput {
        let (advance, line) = runtime
            .heap
            .with_string_slice(info.contents, |source| take(&source[info.cursor..info.end]))
            .expect("input source");
        runtime
            .heap
            .stream_set_cursor(stream, info.cursor + advance, false);
        return Ok(line);
    }
    if info.kind.file() {
        if is_byte_element(info.element) {
            return Err(stream_error(
                runtime,
                stream,
                at,
                "READ-LINE requires a character stream",
            ));
        }
        let mut file = stream_file(runtime, info.handle, stream, at)?;
        runtime.heap.stream_set_unread(stream, None);
        return collect_line(|| {
            info.format
                .read_char(&mut *file)
                .map_err(|error| io_error(runtime, stream, at, error))
        });
    }
    let mut input = runtime.host_io.input.borrow_mut();
    if input.cursor == input.text.len() && !input.eof {
        runtime
            .host_io
            .refill(&mut input)
            .map_err(|error| io_error(runtime, stream, at, error))?;
    }
    let (advance, line) = take(&input.text[input.cursor..]);
    input.cursor += advance;
    input.unread_allowed = false;
    Ok(line)
}

fn collect_line(
    mut next: impl FnMut() -> Result<Option<char>, Fault>,
) -> Result<Option<(String, bool)>, Fault> {
    let mut line = String::new();
    while let Some(character) = next()? {
        if character == '\n' {
            return Ok(Some((line, false)));
        }
        line.push(character);
    }
    Ok((!line.is_empty()).then_some((line, true)))
}

// A cursor of 1 suppresses the echo of a character read before UNREAD-CHAR.
fn echo_character(
    runtime: &Runtime,
    stream: &RootSlot,
    character: char,
    at: usize,
) -> Result<(), Fault> {
    let info = info(runtime, stream.get(), at)?;
    let suppressed = info.cursor == 1 && info.last_char == Some(character);
    if !suppressed {
        let (_, output) = compound_pair(runtime, &info, stream.get(), at)?;
        write_text(runtime, output, character.encode_utf8(&mut [0; 4]), at)?;
    }
    runtime
        .heap
        .stream_set_echo_character(stream.get(), character, false);
    Ok(())
}

pub(super) enum NoHang {
    Character(char),
    EndOfFile,
    Pending,
}

pub(super) fn read_character_no_hang(
    runtime: &Runtime,
    stream: Word,
    at: usize,
) -> Result<NoHang, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    let availability = match info.kind {
        StreamKind::TwoWay => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            return read_character_no_hang(runtime, input, at);
        }
        StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            let stream = runtime.root_slot(stream);
            return match read_character_no_hang(runtime, input, at)? {
                NoHang::Character(character) => {
                    echo_character(runtime, &stream, character, at)?;
                    Ok(NoHang::Character(character))
                }
                other => Ok(other),
            };
        }
        StreamKind::Concatenated => {
            let result = read_concatenated(runtime, stream, info.contents, |component| {
                Ok(match read_character_no_hang(runtime, component, at)? {
                    NoHang::EndOfFile => None,
                    result => Some(result),
                })
            })?;
            return Ok(result.unwrap_or(NoHang::EndOfFile));
        }
        StreamKind::Broadcast => {
            return Err(stream_error(
                runtime,
                stream,
                at,
                "READ-CHAR-NO-HANG requires an input stream",
            ));
        }
        StreamKind::StringInput => InputAvailability::Ready,
        kind if kind.file() => InputAvailability::Ready,
        _ => runtime.host_io.input.borrow().availability(),
    };
    Ok(match availability {
        InputAvailability::Ready => match read_character(runtime, stream, CharacterRead::Read, at)?
        {
            Some(character) => NoHang::Character(character),
            None => NoHang::EndOfFile,
        },
        InputAvailability::EndOfFile => NoHang::EndOfFile,
        InputAvailability::Pending => NoHang::Pending,
    })
}

pub(super) fn input_availability(
    runtime: &Runtime,
    stream: Word,
    at: usize,
) -> Result<InputAvailability, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            input_availability(runtime, input, at)
        }
        StreamKind::Concatenated => {
            let mut contents = info.contents;
            // LISTEN inspects later components without discarding any.
            // UNREAD-CHAR can still refer to the current component.
            while let Some((component, tail)) = runtime.heap.cons_fields(contents) {
                match input_availability(runtime, component, at)? {
                    InputAvailability::EndOfFile => contents = tail,
                    availability => return Ok(availability),
                }
            }
            Ok(InputAvailability::EndOfFile)
        }
        StreamKind::Broadcast => Err(stream_error(runtime, stream, at, "LISTEN requires an input stream")),
        StreamKind::StringInput => Ok(if info.cursor < info.end {
            InputAvailability::Ready
        } else {
            InputAvailability::EndOfFile
        }),
        StreamKind::FileInput | StreamKind::FileIo => {
            let mut file = stream_file(runtime, info.handle, stream, at)?;
            let end = file
                .metadata()
                .map_err(|error| io_error(runtime, stream, at, error))?
                .len();
            let position = file
                .stream_position()
                .map_err(|error| io_error(runtime, stream, at, error))?;
            Ok(if position < end {
                InputAvailability::Ready
            } else {
                InputAvailability::EndOfFile
            })
        }
        _ => Ok(runtime.host_io.input.borrow().availability()),
    }
}

pub(super) fn clear_input(runtime: &Runtime, stream: Word, at: usize) -> Result<(), Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (input, _) = compound_pair(runtime, &info, stream, at)?;
            clear_input(runtime, input, at)
        }
        StreamKind::Concatenated => {
            if let Some((head, _)) = runtime.heap.cons_fields(info.contents) {
                clear_input(runtime, head, at)?;
            }
            Ok(())
        }
        StreamKind::Broadcast => Err(stream_error(
            runtime,
            stream,
            at,
            "CLEAR-INPUT requires an input stream",
        )),
        StreamKind::StringInput => {
            runtime.heap.stream_set_cursor(stream, info.end, false);
            Ok(())
        }
        StreamKind::FileInput | StreamKind::FileIo => Ok(()),
        _ => {
            let mut input = runtime.host_io.input.borrow_mut();
            input.text.clear();
            input.cursor = 0;
            input.eof = false;
            Ok(())
        }
    }
}
