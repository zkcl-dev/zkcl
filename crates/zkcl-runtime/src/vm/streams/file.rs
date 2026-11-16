use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use num_bigint::Sign;

use super::super::condition::named_word;
use super::super::strings::index;
use super::{
    character_stream_element, fixnum, io_error, keys, last_stream, resolve_synonym, stream_error,
    stream_file, unsupported_stream_element,
};
use crate::condition::{Class, Fault};
use crate::encoding::ExternalFormat;
use crate::heap::{RootSlots, StreamElement, StreamKind, StreamState, Word};
use crate::package::Package;
use crate::pathname::{self, Pathname, Version};
use crate::symbol::Name;
use crate::{BigInt, EvalError, Root, Runtime};

const MAX_BYTE_ELEMENT_BITS: u16 = 1024;
const MAX_BYTE_ELEMENT_BYTES: usize = MAX_BYTE_ELEMENT_BITS.div_ceil(8) as usize;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExistingFile {
    Error,
    Skip,
    Keep,
    Append,
    Truncate,
}

#[derive(Clone, Copy)]
enum MissingFile {
    Error,
    Skip,
    Create,
}

pub(super) fn open(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let [
        direction,
        element_type,
        if_exists,
        if_missing,
        external_format,
    ] = keys(
        runtime,
        frame,
        args.start + 1..args.end,
        [
            "DIRECTION",
            "ELEMENT-TYPE",
            "IF-EXISTS",
            "IF-DOES-NOT-EXIST",
            "EXTERNAL-FORMAT",
        ],
        at,
    )?;
    let element = file_element(runtime, element_type.map(|index| frame.get(index)), at)?;
    let format = if is_byte_element(element) {
        ExternalFormat::Default
    } else {
        ExternalFormat::parse(
            runtime,
            external_format.map(|index| frame.get(index)),
            "OPEN",
            at,
        )?
    };
    let option = |index| option_keyword(runtime, frame.get(index), at);
    let direction = direction.map(option).transpose()?;
    let direction = match direction.as_deref() {
        None | Some("INPUT") => StreamKind::FileInput,
        Some("OUTPUT") => StreamKind::FileOutput,
        Some("IO") => StreamKind::FileIo,
        Some("PROBE") => StreamKind::FileProbe,
        _ => return Err(invalid_option(at, "direction")),
    };
    let if_exists = if_exists.map(option).transpose()?;
    let if_missing = if_missing.map(option).transpose()?;

    let (pathname_word, pathname) = open_pathname(runtime, frame.get(args.start), at)?;
    let version = pathname.version;
    let path = pathname::translate(runtime, pathname, at)?
        .file_path()
        .map_err(|error| {
            Fault::file_error(runtime, pathname_word.slot.get(), EvalError::from_error(at, error))
        })?;
    let file_error = |error: io::Error| {
        Fault::file_error(runtime, 
            pathname_word.slot.get(),
            EvalError::file_io(at, &path, error),
        )
    };

    let writes = direction.output();
    let existing = if writes {
        match if_exists.as_deref() {
            None if version == Version::Newest => ExistingFile::Truncate,
            None | Some("ERROR") => ExistingFile::Error,
            Some("NIL") => ExistingFile::Skip,
            Some("OVERWRITE") => ExistingFile::Keep,
            Some("APPEND") => ExistingFile::Append,
            Some("NEW-VERSION" | "RENAME" | "RENAME-AND-DELETE" | "SUPERSEDE") => {
                ExistingFile::Truncate
            }
            _ => return Err(invalid_option(at, "if-exists")),
        }
    } else {
        ExistingFile::Keep
    };
    let missing = match if_missing.as_deref() {
        None if direction == StreamKind::FileProbe => MissingFile::Skip,
        None if matches!(existing, ExistingFile::Keep | ExistingFile::Append) => MissingFile::Error,
        None | Some("CREATE") => MissingFile::Create,
        Some("ERROR") => MissingFile::Error,
        Some("NIL") => MissingFile::Skip,
        _ => return Err(invalid_option(at, "if-does-not-exist")),
    };

    // For a conditional create, let create_new decide atomically whether
    // the path already exists instead of checking before the open.
    let exclusive = existing == ExistingFile::Skip && matches!(missing, MissingFile::Create);
    let exists = !exclusive && path.try_exists().map_err(file_error)?;
    if exists {
        match existing {
            ExistingFile::Error => {
                return Err(file_error(io::Error::from(io::ErrorKind::AlreadyExists)));
            }
            ExistingFile::Skip => return Ok(Word::NIL),
            _ => {}
        }
    } else {
        match missing {
            MissingFile::Error => {
                return Err(file_error(io::Error::from(io::ErrorKind::NotFound)));
            }
            MissingFile::Skip => return Ok(Word::NIL),
            MissingFile::Create => {}
        }
    }

    let file = if direction == StreamKind::FileProbe {
        if !exists {
            File::create_new(&path).map_err(file_error)?;
        }
        None
    } else {
        let append = existing == ExistingFile::Append;
        let opened = OpenOptions::new()
            .read(direction != StreamKind::FileOutput)
            .write(writes || !exists)
            .append(append)
            .truncate(exists && existing == ExistingFile::Truncate)
            .create_new(!exists)
            .open(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if exclusive && error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(Word::NIL);
            }
            Err(error) => return Err(file_error(error)),
        };
        if append {
            file.seek(SeekFrom::End(0)).map_err(file_error)?;
        }
        Some(file)
    };

    let stream = runtime
        .heap
        .stream_with_format(direction, element, format, 0, 0, || {
            pathname_word.slot.get()
        })
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    if let Some(file) = file {
        let handle = runtime.host_io.next_file.get();
        runtime.host_io.next_file.set(handle + 1);
        runtime.host_io.files.borrow_mut().insert(handle, file);
        runtime.heap.stream_set_handle(stream, handle);
    } else {
        runtime.heap.stream_close(stream);
    }
    Ok(stream)
}

fn open_pathname(
    runtime: &Runtime,
    filespec: Word,
    at: usize,
) -> Result<(Root<'_>, Pathname), Fault> {
    match runtime.heap.stream_info(filespec) {
        Some(info) if info.kind.file() => {
            let pathname = pathname::from_word(runtime, info.contents).ok_or_else(|| {
                Fault::type_error(runtime, 
                    filespec,
                    "FILE-STREAM",
                    EvalError::new(at, "file stream has no associated pathname"),
                )
            })?;
            Ok((runtime.root(info.contents), pathname))
        }
        Some(_) => Err(Fault::type_error(runtime, 
            filespec,
            "FILE-STREAM",
            EvalError::new(at, "OPEN requires a pathname designator"),
        )),
        None => {
            let pathname = pathname::merged_designator(runtime, filespec, at)?;
            let word = pathname.to_lisp_value(runtime, at)?;
            Ok((word, pathname))
        }
    }
}

fn option_keyword(runtime: &Runtime, word: Word, at: usize) -> Result<String, Fault> {
    if word == Word::NIL {
        return Ok("NIL".to_owned());
    }
    named_word(runtime, word, Package::KEYWORD).ok_or_else(|| invalid_option(at, "keyword option"))
}

fn invalid_option(at: usize, name: &str) -> Fault {
    Fault::plain(
        Class::TypeError,
        EvalError::new(at, format!("invalid {name}")),
    )
}

fn file_element(
    runtime: &Runtime,
    element: Option<Word>,
    at: usize,
) -> Result<StreamElement, Fault> {
    let Some(element) = element else {
        return Ok(StreamElement::Character);
    };
    if named_word(runtime, element, Package::KEYWORD).as_deref() == Some("DEFAULT") {
        return Ok(StreamElement::Character);
    }
    let element = crate::vm::type_check::expand_derived_type(runtime, element, at)?;
    if let Some(character) = character_stream_element(runtime, element) {
        return Ok(character);
    }
    integer_stream_element(runtime, element, 0, at)?.ok_or_else(|| unsupported_stream_element(at))
}

// Compound element types may contain independently defined aliases. Root all
// members before calling an expander because it can run arbitrary Lisp code.
fn integer_stream_element(
    runtime: &Runtime,
    spec: Word,
    depth: usize,
    at: usize,
) -> Result<Option<StreamElement>, Fault> {
    if depth >= crate::vm::type_check::TYPE_NESTING_LIMIT {
        return Err(unsupported_stream_element(at));
    }
    if let Some((head, rest)) = runtime.heap.cons_fields(spec)
        && named_word(runtime, head, Package::COMMON_LISP).as_deref() == Some("OR")
    {
        let Some(items) = crate::vm::types::arguments(&runtime.heap, rest, usize::MAX) else {
            return Ok(None);
        };
        let items: Vec<_> = items.into_iter().map(|item| runtime.root_slot(item)).collect();
        let mut signed_width = 0;
        let mut unsigned_width = 0;
        for item in items {
            let spec = crate::vm::type_check::expand_derived_type(runtime, item.get(), at)?;
            let Some(element) = integer_stream_element(runtime, spec, depth + 1, at)? else {
                return Ok(None);
            };
            match element {
                StreamElement::UnsignedByte(width) => unsigned_width = unsigned_width.max(width),
                StreamElement::SignedByte(width) => signed_width = signed_width.max(width),
                _ => unreachable!("integer element"),
            }
        }
        return Ok(match (signed_width, unsigned_width) {
            (0, 0) => None,
            (0, width) => Some(StreamElement::UnsignedByte(width)),
            (width, 0) => Some(StreamElement::SignedByte(width)),
            (signed, unsigned) => {
                integer_bits(u32::from(signed.max(unsigned + 1))).map(StreamElement::SignedByte)
            }
        });
    }
    Ok(integer_element(runtime, spec))
}

fn integer_bits(size: u32) -> Option<u16> {
    let bits = u16::try_from(size).ok()?;
    (1..=MAX_BYTE_ELEMENT_BITS).contains(&bits).then_some(bits)
}

// A byte stream stores the smallest whole number of bytes that holds the
// declared integer type. Signed types keep their sign in the element type.
fn integer_element(runtime: &Runtime, element: Word) -> Option<StreamElement> {
    if let Some((head, rest)) = runtime.heap.cons_fields(element) {
        let name = named_word(runtime, head, Package::COMMON_LISP)?;
        return match name.as_str() {
            "UNSIGNED-BYTE" | "SIGNED-BYTE" => {
                let (size, tail) = runtime.heap.cons_fields(rest)?;
                if tail != Word::NIL {
                    return None;
                }
                let bits = integer_bits(u32::try_from(size.as_fixnum()?).ok()?)?;
                Some(if name == "SIGNED-BYTE" {
                    StreamElement::SignedByte(bits)
                } else {
                    StreamElement::UnsignedByte(bits)
                })
            }
            "INTEGER" => {
                let (low, rest) = runtime.heap.cons_fields(rest)?;
                let (high, tail) = runtime.heap.cons_fields(rest)?;
                if tail != Word::NIL {
                    return None;
                }
                let low = low.as_fixnum()?;
                let high = high.as_fixnum()?;
                if low >= 0 {
                    let magnitude = usize::try_from(high).unwrap_or(0);
                    let bits = integer_bits(usize::BITS - magnitude.leading_zeros())?;
                    Some(StreamElement::UnsignedByte(bits))
                } else {
                    let positive = usize::try_from(high).unwrap_or(0);
                    let magnitude = (low.unsigned_abs() - 1).max(positive);
                    let bits = integer_bits(usize::BITS - magnitude.leading_zeros() + 1)?;
                    Some(StreamElement::SignedByte(bits))
                }
            }
            _ => None,
        };
    }
    match named_word(runtime, element, Package::COMMON_LISP)?.as_str() {
        "BIT" => Some(StreamElement::UnsignedByte(1)),
        "UNSIGNED-BYTE" => Some(StreamElement::UnsignedByte(8)),
        "SIGNED-BYTE" => Some(StreamElement::SignedByte(8)),
        _ => None,
    }
}

pub(super) fn byte_element(element: StreamElement) -> Option<(u16, bool)> {
    match element {
        StreamElement::UnsignedByte(bits) => Some((bits, false)),
        StreamElement::SignedByte(bits) => Some((bits, true)),
        _ => None,
    }
}

pub(super) fn is_byte_element(element: StreamElement) -> bool {
    byte_element(element).is_some()
}

fn element_bytes(element: StreamElement) -> u64 {
    match element {
        StreamElement::UnsignedByte(bits) | StreamElement::SignedByte(bits) => {
            u64::from(bits.div_ceil(8))
        }
        _ => 1,
    }
}

pub(super) fn file_length(runtime: &Runtime, stream: Word, at: usize) -> Result<Word, Fault> {
    let (stream, info) = resolve_synonym(runtime, stream, at)?;
    if info.kind == StreamKind::Broadcast {
        return match last_stream(runtime, &info) {
            Some(last) => file_length(runtime, last, at),
            None => fixnum(0, at),
        };
    }
    if !info.kind.file() {
        return Err(Fault::type_error(runtime, 
            stream,
            "FILE-STREAM",
            EvalError::new(at, "expected a file stream"),
        ));
    }
    let files = runtime.host_io.files.borrow();
    let file = files
        .get(&info.handle)
        .ok_or_else(|| stream_error(runtime, stream, at, "stream is closed"))?;
    let length = file
        .metadata()
        .map_err(|error| io_error(runtime, stream, at, error))?
        .len()
        / element_bytes(info.element);
    isize::try_from(length)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| EvalError::new(at, "file length exceeds the supported fixnum range").into())
}

pub(super) fn read_file_byte(
    runtime: &Runtime,
    info: &StreamState,
    bits: u16,
    signed: bool,
    stream: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let count = usize::from(bits.div_ceil(8));
    let mut file = stream_file(runtime, info.handle, stream, at)?;
    let mut buffer = [0u8; MAX_BYTE_ELEMENT_BYTES];
    let buffer = &mut buffer[..count];
    let mut filled = 0;
    while filled < count {
        match file
            .read(&mut buffer[filled..])
            .map_err(|error| io_error(runtime, stream, at, error))?
        {
            0 if filled == 0 => return Ok(None),
            0 => {
                return Err(io_error(
                    runtime,
                    stream,
                    at,
                    io::Error::from(io::ErrorKind::UnexpectedEof),
                ));
            }
            read => filled += read,
        }
    }
    drop(file);
    if bits == 8 {
        let octet = buffer[0];
        let value = if signed {
            isize::from(octet as i8)
        } else {
            isize::from(octet)
        };
        return Ok(Some(Word::fixnum(value).expect("an octet fits a fixnum")));
    }
    let mut value = BigInt::from_bytes_be(Sign::Plus, buffer);
    if signed {
        let modulus = BigInt::from(1) << bits;
        let limit = &modulus >> 1;
        if value >= limit {
            value -= modulus;
        }
    }
    let value = runtime
        .heap
        .allocate_integer(&value)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(Some(value))
}

#[expect(
    clippy::too_many_arguments,
    reason = "binary stream write helper forwards element width, signedness, and source location"
)]
pub(super) fn write_file_byte(
    runtime: &Runtime,
    info: &StreamState,
    bits: u16,
    signed: bool,
    value: &BigInt,
    datum: Word,
    stream: Word,
    at: usize,
) -> Result<(), Fault> {
    let count = usize::from(bits.div_ceil(8));
    let modulus = BigInt::from(1) << bits;
    let in_range = if signed {
        let limit = &modulus >> 1;
        let minimum = -&limit;
        &minimum <= value && value < &limit
    } else {
        value.sign() != Sign::Minus && value < &modulus
    };
    if !in_range {
        return Err(Fault::type_error(runtime, 
            datum,
            if signed {
                "SIGNED-BYTE"
            } else {
                "UNSIGNED-BYTE"
            },
            EvalError::new(at, "integer does not fit the stream element type"),
        ));
    }
    let (_, digits) = if value.sign() == Sign::Minus {
        (value + modulus).to_bytes_be()
    } else {
        value.to_bytes_be()
    };
    let mut buffer = [0u8; MAX_BYTE_ELEMENT_BYTES];
    buffer[count - digits.len()..count].copy_from_slice(&digits);
    write_file_bytes(runtime, info, &buffer[..count], stream, at)
}

pub(super) fn write_file_bytes(
    runtime: &Runtime,
    info: &StreamState,
    buffer: &[u8],
    stream: Word,
    at: usize,
) -> Result<(), Fault> {
    let mut file = stream_file(runtime, info.handle, stream, at)?;
    file.write_all(buffer)
        .map_err(|error| io_error(runtime, stream, at, error))
}

pub(super) fn file_stream_position(
    runtime: &Runtime,
    info: &StreamState,
    stream: Word,
    position: Option<Word>,
    at: usize,
) -> Result<Word, Fault> {
    let start = runtime.find_named_symbol(&Name::keyword("START"));
    let end = runtime.find_named_symbol(&Name::keyword("END"));
    let width = element_bytes(info.element);
    let mut file = runtime
        .host_io
        .file(info.handle)
        .ok_or_else(|| stream_error(runtime, stream, at, "stream is closed"))?;
    if let Some(position) = position {
        let target = if Some(position) == start {
            0
        } else if Some(position) == end {
            file.metadata()
                .map_err(|error| io_error(runtime, stream, at, error))?
                .len()
        } else {
            (index(runtime, position, at)? as u64)
                .checked_mul(width)
                .ok_or_else(|| {
                    EvalError::new(at, "stream position exceeds the supported fixnum range")
                })?
        };
        seek_file(runtime, stream, &mut file, target, at)?;
        return Ok(Word::T);
    }
    let position = file
        .stream_position()
        .map_err(|error| io_error(runtime, stream, at, error))?
        / width;
    isize::try_from(position)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            EvalError::new(at, "stream position exceeds the supported fixnum range").into()
        })
}

pub(super) fn seek_file(
    runtime: &Runtime,
    stream: Word,
    file: &mut File,
    position: u64,
    at: usize,
) -> Result<(), Fault> {
    file.seek(SeekFrom::Start(position))
        .map_err(|error| io_error(runtime, stream, at, error))?;
    runtime.heap.stream_set_unread(stream, None);
    Ok(())
}
