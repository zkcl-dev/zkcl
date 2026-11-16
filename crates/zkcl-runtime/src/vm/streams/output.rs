use std::io::Write;

use super::{
    compound_pair, for_each_broadcast, info, io_error, is_byte_element, last_stream, output_stream,
    resolve_synonym, stream_error, stream_file,
};
use crate::condition::Fault;
use crate::heap::{ArrayElement, StreamElement, StreamKind, StreamState, Word};
use crate::{EvalError, OutOfMemory, Runtime};

pub(in crate::vm) fn make_output_to_string(
    runtime: &Runtime,
    target: impl Fn() -> Word,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let word = target();
    let array = heap
        .array_info(word)
        .filter(|info| info.element == ArrayElement::Character && info.rank == 1);
    let array = array.ok_or_else(|| {
        Fault::type_error(runtime, word, "STRING", EvalError::new(at, "expected a string"))
    })?;
    if array.fill_pointer.is_none() {
        return Err(Fault::vector_satisfying(runtime, 
            word,
            "ARRAY-HAS-FILL-POINTER-P",
            EvalError::new(at, "output requires a string with a fill pointer"),
        ));
    }
    let column = heap
        .with_string_slice(word, |text| {
            column_after(Some(0), text).expect("initial column is known")
        })
        .ok_or_else(|| crate::vm::arrays::problem(at, "invalid string displacement"))?;
    let stream = heap
        .stream(
            StreamKind::StringOutputTo,
            StreamElement::Character,
            0,
            0,
            &target,
        )
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    heap.stream_set_output(stream, target(), column);
    Ok(stream)
}

pub(crate) fn output_column(
    runtime: &Runtime,
    stream_word: Word,
    at: usize,
) -> Result<Option<usize>, Fault> {
    let (stream, info) = output_stream(runtime, stream_word, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (_, output) = compound_pair(runtime, &info, stream, at)?;
            output_column(runtime, output, at)
        }
        StreamKind::Broadcast => match last_stream(runtime, &info) {
            Some(last) => output_column(runtime, last, at),
            None => Ok(Some(0)),
        },
        StreamKind::StringOutput
        | StreamKind::StringOutputTo
        | StreamKind::FileOutput
        | StreamKind::FileIo => Ok(Some(info.column)),
        StreamKind::StandardOutput | StreamKind::Terminal => {
            Ok(runtime.host_io.stdout_column.get())
        }
        StreamKind::ErrorOutput => Ok(runtime.host_io.stderr_column.get()),
        _ => unreachable!(),
    }
}

pub(super) fn column_after(previous: Option<usize>, text: &str) -> Option<usize> {
    match text.rsplit_once('\n') {
        Some((_, tail)) => Some(tail.chars().count()),
        None => previous.map(|column| column + text.chars().count()),
    }
}

pub(crate) fn write_text(
    runtime: &Runtime,
    stream_word: Word,
    text: &str,
    at: usize,
) -> Result<(), Fault> {
    let (stream, info) = output_stream(runtime, stream_word, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (_, output) = compound_pair(runtime, &info, stream, at)?;
            write_text(runtime, output, text, at)
        }
        StreamKind::Broadcast => for_each_broadcast(runtime, info.contents, |component| {
            write_text(runtime, component, text, at)
        }),
        _ => write_text_leaf(runtime, stream, &info, text, at),
    }
}

fn write_text_leaf(
    runtime: &Runtime,
    stream: Word,
    info: &StreamState,
    text: &str,
    at: usize,
) -> Result<(), Fault> {
    match info.kind {
        StreamKind::StringOutput => {
            if text.is_empty() {
                return Ok(());
            }
            if info.element == StreamElement::StandardChar
                && let Some(character) = text.chars().find(|&c| !crate::character::is_standard(c))
            {
                return Err(Fault::type_error(runtime, 
                    Word::character(character),
                    "STANDARD-CHAR",
                    EvalError::new(at, "character is not representable on this stream"),
                ));
            }
            let stream = runtime.root(stream);
            let previous = runtime.root(info.contents);
            let column =
                column_after(Some(info.column), text).expect("string streams track their column");
            let text = runtime.root(
                runtime
                    .heap
                    .string(text)
                    .map_err(|error| EvalError::out_of_memory(at, error))?,
            );
            let contents = runtime
                .cons(&text, &previous)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            runtime
                .heap
                .stream_set_output(stream.slot.get(), contents.slot.get(), column);
        }
        StreamKind::StringOutputTo => append_to_output_string(runtime, stream, info, text, at)?,
        StreamKind::FileOutput | StreamKind::FileIo => {
            if is_byte_element(info.element) {
                return Err(stream_error(
                    runtime,
                    stream,
                    at,
                    "WRITE-CHAR requires a character stream",
                ));
            }
            let mut file = stream_file(runtime, info.handle, stream, at)?;
            let bytes = info
                .format
                .encode(text)
                .map_err(|error| io_error(runtime, stream, at, error))?;
            if !bytes.is_empty() {
                runtime.heap.stream_set_unread(stream, None);
            }
            file.write_all(&bytes)
                .map_err(|error| io_error(runtime, stream, at, error))?;
            let column =
                column_after(Some(info.column), text).expect("file streams track their column");
            runtime.heap.stream_set_column(stream, column);
        }
        StreamKind::StandardOutput | StreamKind::Terminal | StreamKind::ErrorOutput => {
            let error_output = info.kind == StreamKind::ErrorOutput;
            runtime
                .host_io
                .with_writer(error_output, |writer| writer.write_all(text.as_bytes()))
                .map_err(|error| io_error(runtime, stream, at, error))?;
            runtime
                .host_io
                .record_dribble(text)
                .map_err(|error| io_error(runtime, stream, at, error))?;
            let column = if error_output {
                &runtime.host_io.stderr_column
            } else {
                &runtime.host_io.stdout_column
            };
            column.update(|col| column_after(col, text));
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn append_to_output_string(
    runtime: &Runtime,
    stream: Word,
    info: &StreamState,
    text: &str,
    at: usize,
) -> Result<(), Fault> {
    if text.is_empty() {
        return Ok(());
    }
    let heap = &runtime.heap;
    let stream = runtime.root_slot(stream);
    let target = runtime.root_slot(info.contents);
    let array = heap.array_info(target.get()).expect("string destination");
    let fill = array
        .fill_pointer
        .expect("string destination has a fill pointer");
    let (fits, extra) = text.split_at(
        text.char_indices()
            .nth(array.size - fill)
            .map_or(text.len(), |(byte, _)| byte),
    );
    let invalid_backing = || stream_error(runtime, stream.get(), at, "invalid string displacement");
    let mut column = info.column;
    if !fits.is_empty() {
        if !heap
            .set_array_string_range(target.get(), fill, fits)
            .map_err(|error| EvalError::out_of_memory(at, error))?
        {
            return Err(invalid_backing());
        }
        heap.set_array_fill_pointer(target.get(), fill + fits.chars().count())
            .expect("updated fill pointer is within capacity");
        column = column_after(Some(column), fits).expect("string streams track their column");
        heap.stream_set_output(stream.get(), target.get(), column);
    }
    if extra.is_empty() {
        return Ok(());
    }
    if !array.adjustable {
        return Err(Fault::vector_satisfying(runtime, 
            target.get(),
            "ADJUSTABLE-ARRAY-P",
            EvalError::new(at, "cannot extend a non-adjustable string"),
        ));
    }
    let limit = Word::FIXNUM_MAX as usize - 1;
    let fill = array
        .size
        .checked_add(extra.chars().count())
        .filter(|&size| size <= limit)
        .ok_or_else(|| {
            super::super::arrays::problem(at, "array dimension exceeds the implementation limit")
        })?;
    let capacity = array.size.saturating_mul(2).min(limit).max(fill);
    let mut characters = Vec::new();
    characters
        .try_reserve_exact(fill)
        .map_err(|_| EvalError::out_of_memory(at, OutOfMemory))?;
    heap.with_array_character_slice(target.get(), 0, array.size, |prefix| {
        characters.extend(prefix.chars());
    })
    .ok_or_else(invalid_backing)?;
    characters.extend(extra.chars());
    let replacement = heap
        .array(
            &[capacity],
            ArrayElement::Character,
            true,
            Some(fill),
            |index| Word::character(characters.get(index).copied().unwrap_or('\0')),
        )
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    heap.replace_adjustable_array(target.get(), replacement)
        .expect("adjustable string destination");
    let column = column_after(Some(column), extra).expect("string streams track their column");
    heap.stream_set_output(stream.get(), target.get(), column);
    Ok(())
}

pub(super) fn flush(runtime: &Runtime, stream: Word, at: usize) -> Result<(), Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    match info.kind {
        StreamKind::TwoWay | StreamKind::Echo => {
            let (_, output) = compound_pair(runtime, &info, stream, at)?;
            flush(runtime, output, at)
        }
        StreamKind::Broadcast => for_each_broadcast(runtime, info.contents, |component| {
            flush(runtime, component, at)
        }),
        StreamKind::StandardOutput | StreamKind::Terminal | StreamKind::ErrorOutput => runtime
            .host_io
            .with_writer(info.kind == StreamKind::ErrorOutput, |writer| {
                writer.flush()
            })
            .map_err(|error| io_error(runtime, stream, at, error)),
        StreamKind::FileOutput | StreamKind::FileIo => {
            let mut file = stream_file(runtime, info.handle, stream, at)?;
            file.flush().map_err(|error| io_error(runtime, stream, at, error))
        }
        _ => Ok(()),
    }
}

pub(super) fn get_output(runtime: &Runtime, stream: Word, at: usize) -> Result<Word, Fault> {
    let info = info(runtime, stream, at)?;
    if info.kind != StreamKind::StringOutput {
        return Err(Fault::type_error(runtime, 
            stream,
            "STRING-STREAM",
            EvalError::new(at, "expected a string output stream"),
        ));
    }
    let mut contents = info.contents;
    let mut chunks = Vec::new();
    let mut total_bytes = 0usize;
    while let Some((chunk, tail)) = runtime.heap.cons_fields(contents) {
        total_bytes += runtime
            .heap
            .with_string_slice(chunk, str::len)
            .expect("output chunk");
        chunks.push(chunk);
        contents = tail;
    }
    let mut text = String::with_capacity(total_bytes);
    for chunk in chunks.into_iter().rev() {
        runtime
            .heap
            .with_string_slice(chunk, |chunk| text.push_str(chunk))
            .expect("output chunk");
    }
    let stream = runtime.root(stream);
    let output = if info.element == StreamElement::Nil {
        let characters: Vec<char> = text.chars().collect();
        runtime
            .heap
            .nil_element_array(
                &[characters.len()],
                false,
                None,
                characters.len(),
                |index| characters[index],
            )
            .map_err(|error| EvalError::out_of_memory(at, error))?
    } else {
        runtime
            .heap
            .string(&text)
            .map_err(|error| EvalError::out_of_memory(at, error))?
    };
    runtime
        .heap
        .stream_set_output(stream.slot.get(), Word::NIL, 0);
    Ok(output)
}
