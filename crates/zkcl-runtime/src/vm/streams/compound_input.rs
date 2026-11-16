use std::hash::{Hash, Hasher};

use rustc_hash::FxHashSet;

use crate::condition::Fault;
use crate::heap::{Heap, RootSlot, StreamKind, StreamState, Word};
use crate::read::{Form, StreamInput};
use crate::{EvalError, Runtime};

use super::file_input::FileInput;
use super::{
    CharacterRead, compound_pair, consume_input_prefix, input_stream_info, io_error,
    read_character, read_fault, stream_error,
};

// A pending reader can cross a moving collection between any two refills.
struct StreamIdentity(RootSlot);

impl PartialEq for StreamIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Eq for StreamIdentity {}

impl Hash for StreamIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Heap::word_identity_hash(self.0.get()).hash(state);
    }
}

enum Pending {
    Stream(RootSlot),
    Members(RootSlot),
    Leave(RootSlot),
}

enum Leaf {
    String {
        source: RootSlot,
        cursor: usize,
        end: usize,
    },
    File(FileInput),
    Host {
        stream: RootSlot,
        observed: usize,
    },
}

impl Leaf {
    fn string(runtime: &Runtime, info: &StreamState) -> Self {
        Self::String {
            source: runtime.root_slot(info.contents),
            cursor: info.cursor,
            end: info.end,
        }
    }
}

/// Look ahead through the input components of a Lisp stream. A string or
/// host stream contributes one leaf. Echo and concatenated streams commit
/// through the original stream to preserve output and component state.
pub(super) struct CompoundInput {
    stream: RootSlot,
    kind: StreamKind,
    pending: Vec<Pending>,
    seen: FxHashSet<StreamIdentity>,
    host_seen: bool,
    leaf: Option<Leaf>,
    text: String,
    prefix: usize,
    base: usize,
    committed: usize,
    fault: Option<Fault>,
    at: usize,
    submission: bool,
}

impl CompoundInput {
    pub(super) fn new(runtime: &Runtime, stream: Word, info: &StreamState, at: usize) -> Self {
        let direct_string = info.kind == StreamKind::StringInput;
        let (pending, leaf, base) = if direct_string {
            (Vec::new(), Some(Leaf::string(runtime, info)), info.cursor)
        } else {
            (vec![Pending::Stream(runtime.root_slot(stream))], None, 0)
        };
        Self {
            stream: runtime.root_slot(stream),
            kind: info.kind,
            pending,
            seen: FxHashSet::default(),
            host_seen: false,
            leaf,
            text: String::new(),
            prefix: 0,
            base,
            committed: base,
            fault: None,
            at,
            submission: false,
        }
    }

    pub(super) fn for_submission(runtime: &Runtime, stream: Word, info: &StreamState) -> Self {
        let mut input = Self::new(runtime, stream, info, 0);
        let offset = runtime.host_io.input.borrow().position();
        input.base = offset;
        input.committed = offset;
        input.submission = true;
        input
    }

    pub(super) fn finish_submission(
        mut self,
        runtime: &Runtime,
        result: Result<Option<Form>, EvalError>,
        consumed: usize,
    ) -> Result<Option<Form>, EvalError> {
        if super::read_needs_more_input(&result)
            && let Some(fault) = self.fault.take()
        {
            return Err(fault.host);
        }
        self.commit(runtime, consumed, 0)
            .map_err(|fault| fault.host)?;
        result
    }

    fn next_leaf(&mut self, runtime: &Runtime, strings_only: bool) -> Result<Option<Leaf>, Fault> {
        while let Some(part) = self.pending.pop() {
            let root = match part {
                Pending::Members(list) => {
                    if let Some((head, tail)) = runtime.heap.cons_fields(list.get()) {
                        if tail != Word::NIL {
                            self.pending.push(Pending::Members(runtime.root_slot(tail)));
                        }
                        self.pending.push(Pending::Stream(runtime.root_slot(head)));
                    }
                    continue;
                }
                Pending::Leave(_) => continue,
                Pending::Stream(stream) => stream,
            };
            let (stream, info) = input_stream_info(runtime, root.get(), self.at)?;
            if strings_only
                && matches!(
                    info.kind,
                    StreamKind::FileInput
                        | StreamKind::FileIo
                        | StreamKind::StandardInput
                        | StreamKind::Terminal
                )
            {
                // Let the lexer inspect the new characters before touching the
                // next file or asking the host for input.
                self.pending.push(Pending::Stream(root));
                return Ok(None);
            }
            if !self.seen.insert(StreamIdentity(runtime.root_slot(stream))) {
                if self
                    .pending
                    .iter()
                    .any(|p| matches!(p, Pending::Leave(parent) if parent.get() == stream))
                {
                    return Err(stream_error(runtime, stream, self.at, "compound input stream cycle"));
                }
                // Consuming the first occurrence would already have exhausted this component.
                continue;
            }
            match info.kind {
                StreamKind::StringInput => {
                    return Ok(Some(Leaf::string(runtime, &info)));
                }
                StreamKind::FileInput | StreamKind::FileIo => {
                    return FileInput::open(runtime, stream, &info, self.at)
                        .map(|file| Some(Leaf::File(file)));
                }
                StreamKind::TwoWay | StreamKind::Echo => {
                    let (input, _) = compound_pair(runtime, &info, stream, self.at)?;
                    self.pending.push(Pending::Leave(runtime.root_slot(stream)));
                    self.pending.push(Pending::Stream(runtime.root_slot(input)));
                }
                StreamKind::Concatenated => {
                    self.pending.push(Pending::Leave(runtime.root_slot(stream)));
                    self.pending
                        .push(Pending::Members(runtime.root_slot(info.contents)));
                }
                StreamKind::StandardInput | StreamKind::Terminal => {
                    // Both designators draw from the same host buffer.
                    if !self.host_seen {
                        self.host_seen = true;
                        return Ok(Some(Leaf::Host {
                            stream: runtime.root_slot(stream),
                            observed: 0,
                        }));
                    }
                }
                _ => {
                    return Err(stream_error(
                        runtime,
                        stream,
                        self.at,
                        "expected a character input stream",
                    ));
                }
            }
        }
        Ok(None)
    }

    fn extend(
        &mut self,
        runtime: &Runtime,
        token_start: usize,
        allow_block: bool,
    ) -> Result<bool, Fault> {
        let start = self.text.len();
        let limit = super::read_window_bytes(start, token_start);
        // The lexer restarts an unfinished token at token_start. Grow that
        // token geometrically even when each string contributes one byte.
        // Initial reads and optional whitespace peeks still stop at one leaf.
        let target = if allow_block {
            (start - token_start).max(1)
        } else {
            1
        };
        loop {
            let added = self.text.len() - start;
            if self.leaf.is_none() {
                self.leaf = self.next_leaf(runtime, added != 0)?;
            }
            match &mut self.leaf {
                None => return Ok(added != 0),
                Some(Leaf::String {
                    source,
                    cursor,
                    end,
                }) => {
                    // Leave room for one complete UTF-8 scalar at the boundary.
                    let remaining = if added == 0 { limit } else { target - added };
                    let used = runtime
                        .heap
                        .with_string_slice(source.get(), |source| {
                            append_window(&mut self.text, &source[*cursor..*end], remaining.max(4))
                        })
                        .expect("input source");
                    *cursor += used;
                    if added + used >= target {
                        return Ok(true);
                    }
                    if *cursor != *end {
                        continue;
                    }
                }
                Some(Leaf::File(file)) => {
                    let previous = file.text().len();
                    let leaf_start = self.text.len() - previous;
                    if file.refill(runtime, token_start.saturating_sub(leaf_start)) {
                        self.text.push_str(&file.text()[previous..]);
                        return Ok(true);
                    }
                    if let Some(error) = file.take_error() {
                        return Err(io_error(runtime, file.stream(), self.at, error));
                    }
                }
                Some(Leaf::Host { stream, observed }) => {
                    let mut host = runtime.host_io.input.borrow_mut();
                    if host.cursor + *observed == host.text.len() && !host.eof {
                        if !allow_block {
                            return Ok(false);
                        }
                        runtime
                            .host_io
                            .refill(&mut host)
                            .map_err(|e| io_error(runtime, stream.get(), self.at, e))?;
                    }
                    // Host refill can compact bytes preceding the real cursor.
                    let used =
                        append_window(&mut self.text, &host.text[host.cursor + *observed..], limit);
                    *observed += used;
                    if used != 0 {
                        return Ok(true);
                    }
                }
            }
            self.leaf = None;
        }
    }

    fn refill_window(&mut self, runtime: &Runtime, token_start: usize, allow_block: bool) -> bool {
        if self.fault.is_some() {
            return false;
        }
        let before = self.text.len();
        match self.extend(runtime, token_start, allow_block) {
            Ok(grew) => grew,
            Err(fault) => {
                self.fault = Some(fault);
                self.text.len() > before
            }
        }
    }
}

fn append_window(text: &mut String, source: &str, limit: usize) -> usize {
    let end = source.floor_char_boundary(limit.min(source.len()));
    text.push_str(&source[..end]);
    end
}

impl StreamInput for CompoundInput {
    fn prepend_macro_character(&mut self, ch: char) {
        self.text.push(ch);
        self.prefix = ch.len_utf8();
    }

    fn finish(
        mut self,
        runtime: &Runtime,
        result: Result<Option<Form>, EvalError>,
        consumed: usize,
        at: usize,
    ) -> Result<Option<Form>, Fault> {
        if super::read_needs_more_input(&result)
            && let Some(fault) = self.fault.take()
        {
            return Err(fault);
        }
        let committed = self.commit(runtime, consumed, at);
        let form = result.map_err(|error| read_fault(runtime, error, at, 0, self.stream()))?;
        committed?;
        if matches!(
            self.kind,
            StreamKind::Concatenated | StreamKind::Echo | StreamKind::TwoWay
        ) && form.is_none()
            && self.ends_at_eof()
        {
            // Observing EOF on the actual stream discards its exhausted members.
            read_character(runtime, self.stream(), CharacterRead::Peek, at)?;
        }
        Ok(form)
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn refill_invokes_callbacks(&self) -> bool {
        self.kind != StreamKind::StringInput
    }

    fn refill(&mut self, runtime: &Runtime, token_start: usize) -> bool {
        self.refill_window(runtime, token_start, true)
    }

    fn refill_available(&mut self, runtime: &Runtime, token_start: usize) -> bool {
        self.refill_window(runtime, token_start, false)
    }

    fn ends_at_eof(&self) -> bool {
        self.leaf.is_none() && self.pending.is_empty() && self.fault.is_none()
    }

    fn location(&self, index: usize) -> usize {
        self.base + index.saturating_sub(self.prefix)
    }

    fn stream(&self) -> Word {
        self.stream.get()
    }

    fn commit(&mut self, runtime: &Runtime, cursor: usize, at: usize) -> Result<(), Fault> {
        let position = self.location(cursor);
        // If echoing fails mid-prefix, finishing the failed read must not
        // consume and echo that same prefix a second time.
        let previous = std::mem::replace(&mut self.committed, position);
        let consumed = position.saturating_sub(previous);
        match self.kind {
            StreamKind::StringInput => {
                runtime
                    .heap
                    .stream_set_cursor(self.stream(), position, false)
            }
            StreamKind::StandardInput | StreamKind::Terminal => {
                let mut input = runtime.host_io.input.borrow_mut();
                input.cursor += consumed;
                input.unread_allowed = false;
            }
            _ => consume_input_prefix(runtime, &runtime.root(self.stream()), consumed, at)?,
        }
        Ok(())
    }

    fn resume(&mut self, runtime: &Runtime, at: usize) -> Result<(), Fault> {
        self.text.clear();
        self.prefix = 0;
        self.base = self.committed;
        self.fault = None;
        self.at = at;
        if self.kind == StreamKind::StringInput {
            let info = runtime
                .heap
                .stream_info(self.stream())
                .expect("rooted string input");
            self.base = info.cursor;
            self.committed = info.cursor;
            if info.closed {
                return Err(stream_error(
                    runtime,
                    self.stream(),
                    at,
                    "expected an open input stream",
                ));
            }
            self.leaf = Some(Leaf::string(runtime, &info));
        } else {
            if self.submission {
                self.base = runtime.host_io.input.borrow().position();
                self.committed = self.base;
            }
            self.pending.clear();
            self.pending
                .push(Pending::Stream(runtime.root_slot(self.stream())));
            self.seen.clear();
            self.host_seen = false;
            self.leaf = None;
        }
        Ok(())
    }
}
