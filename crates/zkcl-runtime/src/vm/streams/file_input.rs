use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom};

use crate::condition::Fault;
use crate::encoding::ExternalFormat;
use crate::heap::{RootSlot, StreamState, Word};
use crate::read::{Form, StreamInput};
use crate::{EvalError, Runtime};

use super::file::seek_file;
use super::{io_error, is_byte_element, read_fault, read_window_bytes, stream_error, stream_file};

enum InputEnd {
    More,
    Eof,
    Error(io::Error),
}

/// Decoded input for one reader operation. Lookahead leaves the file at its
/// visible position until the reader commits the bytes it consumed.
pub(super) struct FileInput {
    stream: RootSlot,
    handle: usize,
    format: ExternalFormat,
    start: u64,
    next_byte: u64,
    prefix: usize,
    /// Adjacent token ranges reuse their byte position when decoding Latin-1.
    latin1_position: Cell<(usize, u64)>,
    text: String,
    end: InputEnd,
}

impl FileInput {
    pub(super) fn open(
        runtime: &Runtime,
        stream: Word,
        info: &StreamState,
        at: usize,
    ) -> Result<Self, Fault> {
        if is_byte_element(info.element) {
            return Err(stream_error(runtime, stream, at, "READ requires a character stream"));
        }
        let mut file = stream_file(runtime, info.handle, stream, at)?;
        let start = file
            .stream_position()
            .map_err(|error| io_error(runtime, stream, at, error))?;
        Ok(Self {
            stream: runtime.root_slot(stream),
            handle: info.handle,
            format: info.format,
            start,
            next_byte: start,
            prefix: 0,
            latin1_position: Cell::new((0, start)),
            text: String::new(),
            end: InputEnd::More,
        })
    }

    fn position(&self, index: usize) -> u64 {
        let index = index.max(self.prefix);
        if self.format != ExternalFormat::Latin1 {
            return self.start + (index - self.prefix) as u64;
        }
        let (previous, byte) = self.latin1_position.get();
        let position = if previous <= index {
            byte + self.text[previous..index].chars().count() as u64
        } else {
            byte - self.text[index..previous].chars().count() as u64
        };
        self.latin1_position.set((index, position));
        position
    }

    fn read_at(&self, runtime: &Runtime, limit: usize) -> io::Result<Vec<u8>> {
        let mut file = runtime
            .host_io
            .file(self.handle)
            .ok_or_else(|| io::Error::other("stream has no open file"))?;
        let position = file.stream_position()?;
        if self.next_byte != position {
            file.seek(SeekFrom::Start(self.next_byte))?;
        }
        let mut bytes = Vec::with_capacity(limit);
        let read = (&mut *file).take(limit as u64).read_to_end(&mut bytes);
        file.seek(SeekFrom::Start(position))?;
        read?;
        Ok(bytes)
    }

    pub(super) fn take_error(&mut self) -> Option<io::Error> {
        match std::mem::replace(&mut self.end, InputEnd::Eof) {
            InputEnd::Error(error) => Some(error),
            end => {
                self.end = end;
                None
            }
        }
    }
}

impl StreamInput for FileInput {
    /// A saved standard reader macro receives the stream after its introducer.
    fn prepend_macro_character(&mut self, ch: char) {
        self.text.push(ch);
        self.prefix = ch.len_utf8();
        self.latin1_position.set((self.prefix, self.start));
    }

    fn finish(
        mut self,
        runtime: &Runtime,
        result: Result<Option<Form>, EvalError>,
        consumed: usize,
        at: usize,
    ) -> Result<Option<Form>, Fault> {
        let stream = self.stream.get();
        if super::read_needs_more_input(&result)
            && let Some(error) = self.take_error()
        {
            return Err(io_error(runtime, stream, at, error));
        }
        let committed = self.commit(runtime, consumed, at);
        let form = result.map_err(|error| read_fault(runtime, error, at, 0, stream))?;
        committed?;
        Ok(form)
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn refill_invokes_callbacks(&self) -> bool {
        false
    }

    fn location(&self, index: usize) -> usize {
        self.position(index) as usize
    }

    fn stream(&self) -> Word {
        self.stream.get()
    }

    /// Lisp code must see the consumed position through the actual input stream.
    fn commit(&mut self, runtime: &Runtime, cursor: usize, at: usize) -> Result<(), Fault> {
        let position = self.position(cursor);
        let stream = self.stream();
        let mut file = stream_file(runtime, self.handle, stream, at)?;
        seek_file(runtime, stream, &mut file, position, at)
    }

    /// A reader callback can read, seek, or rewrite an :IO stream.
    fn resume(&mut self, runtime: &Runtime, at: usize) -> Result<(), Fault> {
        let stream = self.stream();
        let mut file = stream_file(runtime, self.handle, stream, at)?;
        self.start = file
            .stream_position()
            .map_err(|error| io_error(runtime, stream, at, error))?;
        self.next_byte = self.start;
        self.prefix = 0;
        self.latin1_position.set((0, self.start));
        self.text.clear();
        self.end = InputEnd::More;
        Ok(())
    }

    fn ends_at_eof(&self) -> bool {
        matches!(self.end, InputEnd::Eof)
    }

    fn refill(&mut self, runtime: &Runtime, token_start: usize) -> bool {
        if !matches!(self.end, InputEnd::More) {
            return false;
        }
        let limit = read_window_bytes(self.text.len(), token_start);
        let bytes = match self.read_at(runtime, limit) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.end = InputEnd::Error(error);
                return false;
            }
        };
        let eof = bytes.len() < limit;
        if eof {
            self.end = InputEnd::Eof;
        }
        let used = match self.format.decode(&bytes) {
            Ok(text) => {
                self.text.push_str(&text);
                bytes.len()
            }
            Err(error) => {
                let valid = error.valid_up_to();
                self.text
                    .push_str(std::str::from_utf8(&bytes[..valid]).expect("valid UTF-8 prefix"));
                if eof || error.error_len().is_some() {
                    self.end = InputEnd::Error(io::Error::new(io::ErrorKind::InvalidData, error));
                }
                // A partial scalar will be read again with its continuation bytes.
                valid
            }
        };
        self.next_byte += used as u64;
        used != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{FormKind, Reader, StreamRead};
    use std::io::Write;
    use std::path::PathBuf;

    struct TemporaryFile(PathBuf);

    impl Drop for TemporaryFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn reading_one_form_does_not_fetch_the_file_tail() {
        let path = std::env::temp_dir().join(format!("zkcl-reader-window-{}", std::process::id()));
        let mut output = std::fs::File::create_new(&path).unwrap();
        let file = TemporaryFile(path);
        output.write_all(b"42 ").unwrap();
        output.set_len(1 << 20).unwrap();
        drop(output);

        let runtime = Runtime::start().unwrap();
        let name = runtime.make_string(file.0.to_str().unwrap()).unwrap();
        let open = runtime.eval("#'open").unwrap().pop().unwrap();
        let stream = runtime.call(&open, &[&name]).unwrap().pop().unwrap();
        let info = runtime.heap.stream_info(stream.slot.get()).unwrap();
        let mut input = FileInput::open(&runtime, stream.slot.get(), &info, 0)
            .unwrap_or_else(|fault| panic!("{}", fault.host));
        let mut reader = Reader::stream(&runtime, &mut input);
        let operation = StreamRead::Form {
            recursive: false,
            preserve: false,
        };
        let (form, used) = operation.parse(&mut reader);
        assert!(matches!(form.unwrap().unwrap().kind, FormKind::Integer(42)));
        assert_eq!(used, 3);
        assert_eq!(input.next_byte, read_window_bytes(0, 0) as u64);
    }
}
