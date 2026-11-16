use std::sync::atomic::AtomicBool;

use super::{
    Address, AtomicUsize, Header, Heap, HeapObject, ObjectKind, Ordering, OutOfMemory, SlotVisitor,
    ValueSlot, Word, WordSlot, offset_of,
};
use crate::encoding::ExternalFormat;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum StreamKind {
    StringInput,
    StringOutput,
    StringOutputTo,
    FileInput,
    FileOutput,
    FileIo,
    FileProbe,
    StandardInput,
    StandardOutput,
    ErrorOutput,
    Terminal,
    Broadcast,
    Concatenated,
    Echo,
    Synonym,
    TwoWay,
}

impl StreamKind {
    pub(crate) fn input(self) -> bool {
        matches!(
            self,
            Self::StringInput
                | Self::FileInput
                | Self::FileIo
                | Self::StandardInput
                | Self::Terminal
                | Self::Concatenated
                | Self::Echo
                | Self::Synonym
                | Self::TwoWay
        )
    }

    pub(crate) fn output(self) -> bool {
        matches!(
            self,
            Self::StringOutput
                | Self::StringOutputTo
                | Self::FileOutput
                | Self::FileIo
                | Self::StandardOutput
                | Self::ErrorOutput
                | Self::Terminal
                | Self::Broadcast
                | Self::Echo
                | Self::Synonym
                | Self::TwoWay
        )
    }

    pub(crate) fn string(self) -> bool {
        matches!(
            self,
            Self::StringInput | Self::StringOutput | Self::StringOutputTo
        )
    }

    pub(crate) fn file(self) -> bool {
        matches!(
            self,
            Self::FileInput | Self::FileOutput | Self::FileIo | Self::FileProbe
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum StreamElement {
    /// No element is representable, e.g. (MAKE-STRING-OUTPUT-STREAM
    /// :ELEMENT-TYPE NIL). Characters can still be written; the accumulated
    /// output is exposed as an array whose element type is NIL.
    Nil,
    Character,
    BaseChar,
    StandardChar,
    UnsignedByte(u16),
    SignedByte(u16),
}

#[repr(C)]
struct LispStream {
    header: Header,
    // String input owns its source. String output owns a reversed list of
    // chunks or holds the destination string when writing to a fill pointer.
    contents: WordSlot,
    cursor: AtomicUsize,
    start: usize,
    end: usize,
    column: AtomicUsize,
    protocol_components: bool,
    closed: AtomicBool,
    unread_allowed: AtomicBool,
    // File streams index the host file table; no other kind uses the handle.
    handle: AtomicUsize,
    last_char: AtomicUsize,
    kind: StreamKind,
    element: StreamElement,
    format: ExternalFormat,
}

// SAFETY: `LispStream` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Stream`.
unsafe impl HeapObject for LispStream {
    const KIND: ObjectKind = ObjectKind::Stream;
}

pub(crate) struct StreamState {
    pub(crate) kind: StreamKind,
    pub(crate) element: StreamElement,
    pub(crate) format: ExternalFormat,
    pub(crate) contents: Word,
    pub(crate) cursor: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) column: usize,
    pub(crate) closed: bool,
    pub(crate) protocol_components: bool,
    pub(crate) unread_allowed: bool,
    pub(crate) handle: usize,
    pub(crate) last_char: Option<char>,
}

impl Heap {
    pub(crate) fn stream(
        &self,
        kind: StreamKind,
        element: StreamElement,
        start: usize,
        end: usize,
        contents: impl FnOnce() -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.stream_with_format(kind, element, ExternalFormat::Default, start, end, contents)
    }

    pub(crate) fn stream_with_format(
        &self,
        kind: StreamKind,
        element: StreamElement,
        format: ExternalFormat,
        start: usize,
        end: usize,
        contents: impl FnOnce() -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            // Reload contents from its root after the allocating safepoint.
            let contents = contents();
            // A synonym may acquire a Lisp stream through a later dynamic binding.
            // Native-only component trees retain the buffered native I/O path.
            let may_call = |child| {
                self.stream_info(child).map_or_else(
                    || self.instance_class(child).is_some(),
                    |s| s.protocol_components,
                )
            };
            let protocol_components = match kind {
                StreamKind::Synonym => true,
                StreamKind::Echo | StreamKind::TwoWay => self
                    .cons_fields(contents)
                    .is_some_and(|(input, output)| may_call(input) || may_call(output)),
                StreamKind::Concatenated | StreamKind::Broadcast => {
                    let mut children = contents;
                    let mut found = false;
                    while let Some((child, tail)) = self.cons_fields(children) {
                        if may_call(child) {
                            found = true;
                            break;
                        }
                        children = tail;
                    }
                    found
                }
                _ => false,
            };
            LispStream {
                header,
                contents: WordSlot::new(contents),
                cursor: AtomicUsize::new(start),
                start,
                end,
                column: AtomicUsize::new(0),
                protocol_components,
                closed: AtomicBool::new(false),
                unread_allowed: AtomicBool::new(false),
                handle: AtomicUsize::new(0),
                last_char: AtomicUsize::new(0),
                kind,
                element,
                format,
            }
        })
    }

    pub(crate) fn stream_info(&self, word: Word) -> Option<StreamState> {
        let stream = self.object_ref::<LispStream>(word)?;
        Some(StreamState {
            kind: stream.kind,
            element: stream.element,
            format: stream.format,
            contents: stream.contents.get(),
            cursor: stream.cursor.load(Ordering::Relaxed),
            start: stream.start,
            end: stream.end,
            column: stream.column.load(Ordering::Relaxed),
            protocol_components: stream.protocol_components,
            closed: stream.closed.load(Ordering::Relaxed),
            unread_allowed: stream.unread_allowed.load(Ordering::Relaxed),
            handle: stream.handle.load(Ordering::Relaxed),
            last_char: char::from_u32(
                u32::try_from(stream.last_char.load(Ordering::Relaxed)).ok()?,
            ),
        })
    }

    pub(crate) fn stream_set_cursor(&self, word: Word, cursor: usize, unread_allowed: bool) {
        let stream = self.expect_object::<LispStream>(word, "stream");
        stream.cursor.store(cursor, Ordering::Relaxed);
        stream
            .unread_allowed
            .store(unread_allowed, Ordering::Relaxed);
    }

    pub(crate) fn stream_set_handle(&self, word: Word, handle: usize) {
        self.expect_object::<LispStream>(word, "stream")
            .handle
            .store(handle, Ordering::Relaxed);
    }

    pub(crate) fn stream_set_unread(&self, word: Word, last: Option<char>) {
        let stream = self.expect_object::<LispStream>(word, "stream");
        stream
            .unread_allowed
            .store(last.is_some(), Ordering::Relaxed);
        stream.last_char.store(
            last.map_or(0, |character| character as usize),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn stream_set_echo_character(
        &self,
        word: Word,
        character: char,
        awaiting_reread: bool,
    ) {
        let stream = self.expect_object::<LispStream>(word, "echo stream");
        debug_assert!(stream.kind == StreamKind::Echo);
        // Echo streams do not have an input cursor of their own.
        stream
            .cursor
            .store(usize::from(awaiting_reread), Ordering::Relaxed);
        stream
            .unread_allowed
            .store(!awaiting_reread, Ordering::Relaxed);
        stream
            .last_char
            .store(character as usize, Ordering::Relaxed);
    }

    pub(crate) fn stream_set_column(&self, word: Word, column: usize) {
        self.expect_object::<LispStream>(word, "stream")
            .column
            .store(column, Ordering::Relaxed);
    }

    pub(crate) fn stream_set_output(&self, word: Word, contents: Word, column: usize) {
        let stream = self.expect_object::<LispStream>(word, "stream");
        self.write_slot_object(stream, &stream.contents, contents);
        stream.column.store(column, Ordering::Relaxed);
    }

    pub(crate) fn stream_set_contents(&self, word: Word, contents: Word) {
        let stream = self.expect_object::<LispStream>(word, "stream");
        self.write_slot_object(stream, &stream.contents, contents);
    }

    pub(crate) fn stream_close(&self, word: Word) -> bool {
        !self
            .expect_object::<LispStream>(word, "stream")
            .closed
            .swap(true, Ordering::Relaxed)
    }
}

pub(super) fn trace_slots(base: Address, visitor: &mut impl SlotVisitor) {
    visitor.visit_slot(ValueSlot(base + offset_of!(LispStream, contents)));
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    let s = heap.object_ref::<LispStream>(word)?;
    // OS file handles belong to the host and require an explicit image hook.
    if s.kind.file() && !s.closed.load(Ordering::Relaxed) {
        return None;
    }
    let (element, bits) = match s.element {
        StreamElement::Nil => (0, 0),
        StreamElement::Character => (1, 0),
        StreamElement::BaseChar => (2, 0),
        StreamElement::StandardChar => (3, 0),
        StreamElement::UnsignedByte(b) => (4, b),
        StreamElement::SignedByte(b) => (5, b),
    };
    Some((
        "stream",
        vec![
            s.kind as u64,
            element,
            bits as u64,
            s.format as u64,
            s.cursor.load(Ordering::Relaxed) as u64,
            s.start as u64,
            s.end as u64,
            s.column.load(Ordering::Relaxed) as u64,
            s.protocol_components as u64,
            s.closed.load(Ordering::Relaxed) as u64,
            s.unread_allowed.load(Ordering::Relaxed) as u64,
            s.last_char.load(Ordering::Relaxed) as u64,
        ],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if kind != "stream" {
        return Ok(None);
    }
    let mut d = super::image::Data(data);
    let kinds = [
        StreamKind::StringInput,
        StreamKind::StringOutput,
        StreamKind::StringOutputTo,
        StreamKind::FileInput,
        StreamKind::FileOutput,
        StreamKind::FileIo,
        StreamKind::FileProbe,
        StreamKind::StandardInput,
        StreamKind::StandardOutput,
        StreamKind::ErrorOutput,
        StreamKind::Terminal,
        StreamKind::Broadcast,
        StreamKind::Concatenated,
        StreamKind::Echo,
        StreamKind::Synonym,
        StreamKind::TwoWay,
    ];
    let kind = *kinds.get(d.number()?).ok_or_else(super::image::invalid)?;
    let tag = d.number()?;
    let bits = u16::try_from(d.number()?).map_err(|_| super::image::invalid())?;
    let element = match tag {
        0 => StreamElement::Nil,
        1 => StreamElement::Character,
        2 => StreamElement::BaseChar,
        3 => StreamElement::StandardChar,
        4 => StreamElement::UnsignedByte(bits),
        5 => StreamElement::SignedByte(bits),
        _ => return Err(super::image::invalid()),
    };
    let format = match d.number()? {
        0 => ExternalFormat::Default,
        1 => ExternalFormat::Utf8,
        2 => ExternalFormat::Latin1,
        _ => return Err(super::image::invalid()),
    };
    let cursor = d.number()?;
    let start = d.number()?;
    let end = d.number()?;
    let column = d.number()?;
    let protocol_components = d.flag()?;
    let closed = d.flag()?;
    let unread_allowed = d.flag()?;
    let last_char = d.number()?;
    d.done()?;
    if kind.file() && !closed {
        return Err(super::image::invalid());
    }
    heap.alloc_object(|header| LispStream {
        header,
        contents: WordSlot::nil(),
        cursor: AtomicUsize::new(cursor),
        start,
        end,
        column: AtomicUsize::new(column),
        protocol_components,
        closed: AtomicBool::new(closed),
        unread_allowed: AtomicBool::new(unread_allowed),
        handle: AtomicUsize::new(0),
        last_char: AtomicUsize::new(last_char),
        kind,
        element,
        format,
    })
    .map(Some)
    .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
