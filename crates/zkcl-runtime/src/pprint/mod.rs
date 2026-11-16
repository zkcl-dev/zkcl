//! Pretty printer core: out-of-band token layout, PPRINT family and pprint
//! dispatch tables.  Structured pretty-printing tokens (conditional newlines,
//! indentation, tabs, and logical block boundaries) are stored out-of-band as
//! `LayoutItem`s alongside plain text chunks and laid out directly without embedding
//! sentinel characters in internal pprint streams.  Legacy string-marker shims
//! are retained solely for `vm/format.rs` and `vm/printer.rs` compatibility.

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::DropGuard;

use num_traits::ToPrimitive;

use crate::condition::{Class, Fault};
use crate::heap::{Heap, RootSlot, StreamElement, StreamKind, Word};
use crate::package::Package;
use crate::printer::{self, PrintCase, PrintControls, SharingMarker};
use crate::symbol::Name;
use crate::vm::printer::PrintContext;
use crate::{EvalError, Root, Runtime};

use super::super::{streams, symbol_value};

mod layout;
mod markers;

use layout::render_layout;
pub(crate) use layout::tab_padding;
use markers::{FORMAT_MARK, encode_format_markers, parse_format_markers};
pub(crate) use markers::{
    block_end_marker, indent_marker_for, logical_block_marker, newline_marker_for, tab_marker_for,
};

pub(crate) struct LogicalBlockBuffer {
    stream: RootSlot,
    destination: RootSlot,
    /// One entry per logical block, present when `*PRINT-CIRCLE*` is active.
    list_origins: Vec<Option<LogicalBlockListOrigin>>,
    layout: LayoutBuffer,
}

struct LogicalBlockListOrigin {
    object: RootSlot,
    checked_by_pprint_pop: bool,
}

impl LogicalBlockBuffer {
    fn new(runtime: &Runtime, stream: Word, destination: Word, column: usize) -> Self {
        Self {
            stream: runtime.root_slot(stream),
            destination: runtime.root_slot(destination),
            list_origins: Vec::new(),
            layout: LayoutBuffer::new(column),
        }
    }

    fn push_text_or_markers(&mut self, text: &str) {
        if text.contains(FORMAT_MARK) {
            self.layout.flush_text();
            self.layout.push_items(parse_format_markers(text));
        } else {
            self.layout.push_text(text);
        }
    }
}

struct LayoutBuffer {
    column: usize,
    items: Vec<LayoutItem>,
    text_run: String,
    depth: usize,
}

impl LayoutBuffer {
    fn new(column: usize) -> Self {
        Self {
            column,
            items: Vec::new(),
            text_run: String::new(),
            depth: 0,
        }
    }

    fn flush_text(&mut self) {
        if !self.text_run.is_empty() {
            self.items.push(LayoutItem {
                depth: self.depth,
                token: LayoutToken::Text(std::mem::take(&mut self.text_run)),
            });
        }
    }

    fn push_text(&mut self, mut text: &str) {
        while let Some((line, rest)) = text.split_once('\n') {
            self.text_run.push_str(line);
            self.push_token(LayoutToken::Newline(NewlineKind::Literal));
            text = rest;
        }
        self.column += text.chars().count();
        self.text_run.push_str(text);
    }

    fn push_token(&mut self, token: LayoutToken) {
        if let LayoutToken::Text(text) = token {
            self.push_text(&text);
            return;
        }
        self.flush_text();
        match &token {
            LayoutToken::BlockStart(_) => self.depth += 1,
            LayoutToken::Newline(NewlineKind::Literal) => self.column = 0,
            _ => {}
        }
        let closes_block = matches!(token, LayoutToken::BlockEnd);
        self.items.push(LayoutItem {
            depth: self.depth,
            token,
        });
        if closes_block {
            self.depth = self.depth.saturating_sub(1);
        }
    }

    fn push_items(&mut self, items: Vec<LayoutItem>) {
        for item in items {
            self.push_token(item.token);
        }
    }

    fn append_capture(&mut self, mut items: Vec<LayoutItem>) {
        self.flush_text();
        let (reset, width) = layout_trailing_width(&items);
        self.column = if reset { width } else { self.column + width };
        for item in &mut items {
            item.depth += self.depth;
        }
        self.items.append(&mut items);
    }

    fn finish(mut self) -> Vec<LayoutItem> {
        self.flush_text();
        self.items
    }
}

thread_local! {
    /// Active logical block buffers, holding GC-rooted stream and list words
    /// together with out-of-band layout tokens.
    static LOGICAL_BLOCK_BUFFERS: RefCell<Vec<LogicalBlockBuffer>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn swap_logical_block_buffers(buffers: &mut Vec<LogicalBlockBuffer>) {
    LOGICAL_BLOCK_BUFFERS.with_borrow_mut(|active| std::mem::swap(active, buffers));
}

/// Whether STREAM is the buffer of a logical block that is still being
/// collected.
pub(crate) fn in_block_buffer(stream: Word) -> bool {
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        buffers
            .borrow()
            .iter()
            .any(|entry| entry.stream.get() == stream)
    })
}

/// A logical block forwards to the same printer destination as its enclosing
/// output operation. A private FORMAT field owns an independent stream.
pub(crate) fn canonical_destination(stream: Word) -> Word {
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        buffers
            .borrow()
            .iter()
            .rev()
            .find(|entry| entry.stream.get() == stream)
            .map_or(stream, |entry| entry.destination.get())
    })
}

fn drain_block_buffer(runtime: &PrintContext<'_>, stream: Word, at: usize) -> Result<(), Fault> {
    let text = stream_text(runtime, stream, at)?;
    if text.is_empty() {
        return Ok(());
    }
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        if let Some(entry) = buffers
            .iter_mut()
            .rev()
            .find(|entry| entry.stream.get() == stream)
        {
            entry.push_text_or_markers(&text);
        }
    });
    Ok(())
}

fn append_to_block_buffer(
    runtime: &PrintContext<'_>,
    stream: Word,
    at: usize,
    update: impl FnOnce(&mut LogicalBlockBuffer),
) -> Result<(), Fault> {
    let stream = runtime.root(stream);
    drain_block_buffer(runtime, stream.slot.get(), at)?;
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        if let Some(entry) = buffers
            .iter_mut()
            .rev()
            .find(|entry| entry.stream.get() == stream.slot.get())
        {
            update(entry);
        }
    });
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewlineKind {
    Linear,
    Fill,
    Miser,
    Mandatory,
    Literal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabKind {
    Line,
    Section,
    LineRelative,
    SectionRelative,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndentKind {
    Block,
    Current,
}

#[derive(Clone)]
enum LayoutToken {
    Text(String),
    Newline(NewlineKind),
    Indent(IndentKind, i64),
    Tab(TabKind, usize, usize),
    BlockStart(Option<String>),
    BlockEnd,
}

#[derive(Clone)]
struct LayoutItem {
    depth: usize,
    token: LayoutToken,
}

// ---------------------------------------------------------------------------
// Runtime accessors

fn special(runtime: &PrintContext<'_>, name: &str, at: usize) -> Result<Word, Fault> {
    let symbol = runtime
        .find_named_symbol(&Name::common_lisp(name))
        .ok_or_else(|| error(at, format!("missing printer variable {name}")))?;
    symbol_value(runtime, symbol, at)
}

fn special_optional(runtime: &PrintContext<'_>, name: &str) -> Option<Word> {
    let symbol = runtime.find_named_symbol(&Name::common_lisp(name))?;
    super::super::symbol_value(runtime, symbol, 0).ok()
}

fn boolean(value: Word) -> bool {
    value != Word::NIL
}

fn print_pretty(runtime: &PrintContext<'_>, at: usize) -> Result<bool, Fault> {
    Ok(boolean(special(runtime, "*PRINT-PRETTY*", at)?))
}

fn numeric_limit(
    runtime: &PrintContext<'_>,
    name: &str,
    at: usize,
) -> Result<Option<usize>, Fault> {
    let value = special(runtime, name, at)?;
    if value == Word::NIL {
        return Ok(None);
    }
    numeric_limit_of(runtime, value, name, at)
}

fn numeric_limit_of(
    runtime: &PrintContext<'_>,
    value: Word,
    name: &str,
    at: usize,
) -> Result<Option<usize>, Fault> {
    if value == Word::NIL {
        return Ok(None);
    }
    if let Some(number) = value.as_fixnum() {
        return usize::try_from(number)
            .map(Some)
            .map_err(|_| error(at, format!("{name} must be a non-negative integer")));
    }
    let number = runtime
        .heap
        .integer(value)
        .ok_or_else(|| error(at, format!("{name} must be NIL or a non-negative integer")))?;
    if number.sign() == num_bigint::Sign::Minus {
        return Err(error(at, format!("{name} must be a non-negative integer")));
    }
    Ok(number.to_usize())
}

fn error(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn type_error<T>(
    runtime: &Runtime,
    value: Word,
    expected: &'static str,
    at: usize,
    message: impl Into<String>,
) -> Result<T, Fault> {
    Err(Fault::type_error(
        runtime,
        value,
        expected,
        EvalError::new(at, message),
    ))
}

fn real_priority(runtime: &PrintContext<'_>, value: Word, at: usize) -> Result<f64, Fault> {
    match runtime.heap.number(value) {
        Some(crate::number::Number::Real(_)) => Ok(dispatch_priority_as_f64(runtime, value)),
        _ => type_error(
            runtime,
            value,
            "REAL",
            at,
            "pprint dispatch priority must be a real number",
        ),
    }
}

/// Resolve STREAM to an output stream.  T designates *TERMINAL-IO* and NIL
/// designates *STANDARD-OUTPUT*.
pub(crate) fn output_stream(
    runtime: &PrintContext<'_>,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    if stream == Word::NIL {
        return special(runtime, "*STANDARD-OUTPUT*", at);
    }
    if stream == Word::T {
        return special(runtime, "*TERMINAL-IO*", at);
    }
    if !streams::gray::streamp(runtime, stream) {
        return type_error(
            runtime,
            stream,
            "STREAM",
            at,
            "expected an output stream designator",
        );
    }
    Ok(stream)
}

fn write(runtime: &PrintContext<'_>, stream: Word, text: &str, at: usize) -> Result<(), Fault> {
    if text.is_empty() {
        return Ok(());
    }
    streams::gray::print_text(runtime, stream, text, at)
}

pub(crate) fn string_stream(runtime: &PrintContext<'_>, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .stream(
            StreamKind::StringOutput,
            StreamElement::Character,
            0,
            0,
            || Word::NIL,
        )
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

/// Run a formatter or printer that needs a Lisp stream while building a
/// buffered FORMAT field. A pretty field keeps layout operations for its
/// enclosing block, which will choose line breaks once the suffix is known.
pub(crate) fn capture_output<T>(
    runtime: &PrintContext<'_>,
    column: usize,
    markers: bool,
    at: usize,
    call: impl FnOnce(Word) -> Result<T, Fault>,
) -> Result<(T, String), Fault> {
    if markers {
        let (result, items) = capture_layout_items(runtime, column, at, call)?;
        return Ok((result, encode_format_markers(&items)));
    }
    let stream = runtime.root_slot(string_stream(runtime, at)?);
    runtime
        .heap
        .stream_set_output(stream.get(), Word::NIL, column);
    let result = call(stream.get())?;
    Ok((result, stream_text(runtime, stream.get(), at)?))
}

fn capture_layout_items<T>(
    runtime: &PrintContext<'_>,
    column: usize,
    at: usize,
    call: impl FnOnce(Word) -> Result<T, Fault>,
) -> Result<(T, Vec<LayoutItem>), Fault> {
    let stream = runtime.root_slot(string_stream(runtime, at)?);
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        buffers
            .borrow_mut()
            .push(LogicalBlockBuffer::new(runtime, stream.get(), stream.get(), column))
    });
    let capture = DropGuard::new(stream, |stream| {
        take_block_buffer(stream.get());
    });
    let result = call(capture.get())?;
    let stream = DropGuard::dismiss(capture);
    let mut block = take_block_buffer(stream.get()).expect("captured pretty stream");
    block.push_text_or_markers(&stream_text(runtime, stream.get(), at)?);
    Ok((result, block.layout.finish()))
}

fn take_block_buffer(stream: Word) -> Option<LogicalBlockBuffer> {
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let pos = buffers
            .iter()
            .rposition(|entry| entry.stream.get() == stream)?;
        Some(buffers.remove(pos))
    })
}

fn peek_stream_text(runtime: &PrintContext<'_>, stream: Word) -> Option<String> {
    let info = runtime.heap.stream_info(stream)?;
    if info.kind != StreamKind::StringOutput {
        return None;
    }
    let mut contents = info.contents;
    let mut chunks = Vec::new();
    while let Some((chunk, tail)) = runtime.heap.cons_fields(contents) {
        chunks.push(
            runtime
                .heap
                .with_string_slice(chunk, str::to_owned)
                .expect("output chunk"),
        );
        contents = tail;
    }
    let mut text = String::new();
    for chunk in chunks.into_iter().rev() {
        text.push_str(&chunk);
    }
    Some(text)
}

pub(crate) fn stream_text(
    runtime: &PrintContext<'_>,
    stream: Word,
    at: usize,
) -> Result<String, Fault> {
    let text = peek_stream_text(runtime, stream)
        .ok_or_else(|| error(at, "expected a string output stream"))?;
    let word = runtime.root(stream);
    runtime
        .heap
        .stream_set_output(word.slot.get(), Word::NIL, 0);
    Ok(text)
}

fn layout_trailing_width(items: &[LayoutItem]) -> (bool, usize) {
    let mut saw_newline = false;
    let mut width = 0usize;
    for item in items {
        match &item.token {
            LayoutToken::Text(text) => width += text.chars().count(),
            LayoutToken::Newline(NewlineKind::Literal) => {
                saw_newline = true;
                width = 0;
            }
            _ => {}
        }
    }
    (saw_newline, width)
}

/// Output column of STREAM, ignoring pretty-printer markers in string output
/// streams.
pub(crate) fn stream_column(
    runtime: &PrintContext<'_>,
    stream: Word,
    at: usize,
) -> Result<usize, Fault> {
    if !in_block_buffer(stream) {
        return Ok(streams::gray::print_column(runtime, stream, at)?.unwrap_or(0));
    }
    drain_block_buffer(runtime, stream, at)?;
    Ok(LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        buffers
            .borrow()
            .iter()
            .rev()
            .find(|entry| entry.stream.get() == stream)
            .expect("active pretty stream")
            .layout
            .column
    }))
}

fn call0<'rt>(
    runtime: &PrintContext<'rt>,
    function: Word,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let function = runtime.root(function);
    runtime.invoke(&function, &[], at)
}

fn call1<'rt>(
    runtime: &PrintContext<'rt>,
    function: Word,
    argument: Word,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let function = runtime.root(function);
    let argument = runtime.root(argument);
    runtime.invoke(&function, &[&argument], at)
}

fn call2<'rt>(
    runtime: &PrintContext<'rt>,
    function: Word,
    first: Word,
    second: Word,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let function = runtime.root(function);
    let first = runtime.root(first);
    let second = runtime.root(second);
    runtime.invoke(&function, &[&first, &second], at)
}

// ---------------------------------------------------------------------------
// Pprint dispatch tables.  A table is a cons (:ZK-PPRINT-DISPATCH . ENTRIES)
// whose entries are lists (TYPE-SPECIFIER FUNCTION PRIORITY).

fn table_tag(runtime: &PrintContext<'_>, at: usize) -> Result<Word, Fault> {
    runtime
        .intern(&Name::keyword("ZK-PPRINT-DISPATCH"), at)
        .map_err(Into::into)
}

fn initial_table(runtime: &PrintContext<'_>, at: usize) -> Result<Word, Fault> {
    let tag = table_tag(runtime, at)?;
    let tag = runtime.root(tag);
    let nil = runtime.root(Word::NIL);
    let table = runtime
        .cons(&tag, &nil)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(table.slot.get())
}

fn current_table(
    runtime: &PrintContext<'_>,
    table: Option<Word>,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let table = match table {
        Some(word) if word != Word::NIL => word,
        _ => special(runtime, "*PRINT-PPRINT-DISPATCH*", at)?,
    };
    if table == Word::NIL {
        return Ok(None);
    }
    if !is_table(runtime, table) {
        return type_error(
            runtime,
            table,
            "NULL",
            at,
            "expected a pprint dispatch table",
        );
    }
    Ok(Some(table))
}

fn dispatch_table(
    runtime: &PrintContext<'_>,
    table: Option<Word>,
    at: usize,
) -> Result<Word, Fault> {
    match current_table(runtime, table, at)? {
        Some(table) => Ok(table),
        None => initial_table(runtime, at),
    }
}

/// The initial dispatch table has no user callbacks. Skip per-node lookups
/// while rendering when the selected table is empty.
pub(crate) fn active_dispatch_table(
    runtime: &PrintContext<'_>,
    table: Option<Word>,
    at: usize,
) -> Result<Option<RootSlot>, Fault> {
    Ok(current_table(runtime, table, at)?
        .filter(|&table| {
            runtime
                .heap
                .cons_fields(table)
                .is_some_and(|(_, dispatch_entries)| dispatch_entries != Word::NIL)
        })
        .map(|table| runtime.root_slot(table)))
}

fn is_table(runtime: &PrintContext<'_>, table: Word) -> bool {
    let Some((tag, _)) = runtime.heap.cons_fields(table) else {
        return false;
    };
    runtime.find_named_symbol(&Name::keyword("ZK-PPRINT-DISPATCH")) == Some(tag)
}

fn dispatch_entries(
    table: Word,
    runtime: &PrintContext<'_>,
) -> Result<Vec<(RootSlot, RootSlot, f64)>, Fault> {
    let Some((_, mut spine)) = runtime.heap.cons_fields(table) else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    while let Some((entry, tail)) = runtime.heap.cons_fields(spine) {
        let Some((spec, rest)) = runtime.heap.cons_fields(entry) else {
            break;
        };
        let Some((function, rest)) = runtime.heap.cons_fields(rest) else {
            break;
        };
        let Some((priority, _)) = runtime.heap.cons_fields(rest) else {
            break;
        };
        result.push((
            runtime.root_slot(spec),
            runtime.root_slot(function),
            dispatch_priority_as_f64(runtime, priority),
        ));
        spine = tail;
    }
    Ok(result)
}

fn dispatch_priority_as_f64(runtime: &PrintContext<'_>, value: Word) -> f64 {
    if let Some(number) = value.as_fixnum() {
        return number as f64;
    }
    match runtime.heap.number(value) {
        Some(crate::number::Number::Real(real)) => match real {
            crate::number::Real::Integer(int) => {
                use num_traits::ToPrimitive;
                int.to_f64().unwrap_or(f64::INFINITY)
            }
            crate::number::Real::Ratio(ratio) => {
                use num_traits::ToPrimitive;
                ratio.to_f64().unwrap_or(f64::INFINITY)
            }
            crate::number::Real::Single(value) => f64::from(value),
            crate::number::Real::Double(value) => value,
        },
        _ => 0.0,
    }
}

fn find_dispatch_function(
    runtime: &PrintContext<'_>,
    object: Word,
    table: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let object = runtime.root(object);
    let mut best: Option<(f64, RootSlot)> = None;
    for (spec, function, priority) in dispatch_entries(table, runtime)? {
        if best.as_ref().is_some_and(|(best, _)| *best >= priority) {
            continue;
        }
        if super::super::types::typep(runtime, object.slot.get(), spec.get(), at)? {
            best = Some((priority, function));
        }
    }
    Ok(best.map(|(_, function)| function.get()))
}

/// Consult the selected table for a child reached by the ordinary printer
/// (e.g. an array element). Flush the enclosing prefix before calling Lisp.
#[expect(
    clippy::too_many_arguments,
    reason = "internal pprint dispatch forwards print controls and destination directly"
)]
pub(crate) fn dispatch_into(
    runtime: &PrintContext<'_>,
    value: Word,
    table: &RootSlot,
    options: &PrintControls,
    stream: Option<&RootSlot>,
    text: &mut String,
    depth: usize,
    at: usize,
) -> Result<bool, Fault> {
    let object = runtime.root_slot(value);
    let Some(function) = find_dispatch_function(runtime, object.get(), table.get(), at)? else {
        return Ok(false);
    };
    let function = runtime.root_slot(function);
    let call = |stream| {
        printer::with_method(runtime, stream, depth, || {
            call2(runtime, function.get(), stream, object.get(), at)
        })?;
        Ok(())
    };
    write_callback(runtime, stream, options.start_column, text, at, call)?;
    Ok(true)
}

pub(crate) fn write_callback(
    runtime: &PrintContext<'_>,
    stream: Option<&RootSlot>,
    column: usize,
    text: &mut String,
    at: usize,
    call: impl FnOnce(Word) -> Result<(), Fault>,
) -> Result<(), Fault> {
    if let Some(stream) = stream {
        write(runtime, stream.get(), text, at)?;
        text.clear();
        call(stream.get())?;
    } else {
        let column = text.rsplit_once('\n').map_or_else(
            || column + text.chars().count(),
            |(_, tail)| tail.chars().count(),
        );
        let (_, printed) = capture_output(runtime, column, false, at, call)?;
        text.push_str(&printed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pretty rendering of values.  Values are rendered into out-of-band token
// streams first and the enclosing block decides the layout, keeping nested
// lists aligned without in-band sentinels.

struct PrettyRenderBuffer {
    table: Option<RootSlot>,
    layout: LayoutBuffer,
    path: HashMap<usize, Vec<RootSlot>>,
}

impl PrettyRenderBuffer {
    fn new(column: usize, table: Option<RootSlot>) -> Self {
        Self {
            table,
            layout: LayoutBuffer::new(column),
            path: HashMap::new(),
        }
    }

    fn dispatch(
        &mut self,
        runtime: &PrintContext<'_>,
        object: &RootSlot,
        depth: usize,
        at: usize,
    ) -> Result<bool, Fault> {
        let Some(table) = &self.table else {
            return Ok(false);
        };
        let Some(function) = find_dispatch_function(runtime, object.get(), table.get(), at)? else {
            return Ok(false);
        };
        let function = runtime.root_slot(function);
        let (_, items) = capture_layout_items(runtime, self.layout.column, at, |stream| {
            printer::with_method(runtime, stream, depth, || {
                call2(runtime, function.get(), stream, object.get(), at)
            })
        })?;
        self.layout.append_capture(items);
        Ok(true)
    }

    fn insert_path(&mut self, runtime: &Runtime, word: Word) -> bool {
        let identity = Heap::word_identity_hash(word);
        let slots = self.path.entry(identity).or_default();
        if slots.iter().any(|slot| slot.get() == word) {
            return false;
        }
        slots.push(runtime.root_slot(word));
        true
    }

    fn remove_path(&mut self, word: Word) {
        let identity = Heap::word_identity_hash(word);
        if let Some(slots) = self.path.get_mut(&identity) {
            slots.retain(|slot| slot.get() != word);
            if slots.is_empty() {
                self.path.remove(&identity);
            }
        }
    }
}

fn append_pretty(
    runtime: &PrintContext<'_>,
    package: Package,
    value: Word,
    options: &PrintControls,
    depth: usize,
    buffer: &mut PrettyRenderBuffer,
    at: usize,
) -> Result<(), Fault> {
    let rooted = buffer.table.as_ref().map(|_| runtime.root_slot(value));
    if let Some(object) = &rooted
        && buffer.dispatch(runtime, object, depth, at)?
    {
        return Ok(());
    }
    let value = rooted.as_ref().map_or(value, RootSlot::get);
    if runtime.heap.cons_fields(value).is_some() {
        if options.level_reached(depth) {
            buffer.layout.push_text("#");
            return Ok(());
        }
        match printer::next_sharing_marker(runtime, options, value) {
            Some(SharingMarker::Definition(id)) => buffer.layout.push_text(&format!("#{id}=")),
            Some(SharingMarker::Reference(id)) => {
                buffer.layout.push_text(&format!("#{id}#"));
                return Ok(());
            }
            None => {}
        }
        if !buffer.insert_path(runtime, value) {
            return Err(error(at, "circular structure in pretty printing"));
        }
        let value = runtime.root(value);
        buffer.layout.push_text("(");
        buffer.layout.push_token(LayoutToken::BlockStart(None));
        let mut spine = value.slot.get();
        let mut count = 0usize;
        loop {
            if !options.readably
                && let Some(limit) = options.length
                && count >= limit
            {
                buffer.layout.push_text("...");
                break;
            }
            let Some((car, cdr)) = runtime.heap.cons_fields(spine) else {
                buffer.layout.push_text(" . ");
                append_pretty(runtime, package, spine, options, depth + 1, buffer, at)?;
                break;
            };
            let car = runtime.root(car);
            let cdr = runtime.root(cdr);
            append_pretty(
                runtime,
                package,
                car.slot.get(),
                options,
                depth + 1,
                buffer,
                at,
            )?;
            count += 1;
            let cdr_word = cdr.slot.get();
            if cdr_word == Word::NIL {
                break;
            }
            if runtime.heap.cons_fields(cdr_word).is_none() {
                buffer.layout.push_text(" . ");
                append_pretty(runtime, package, cdr_word, options, depth + 1, buffer, at)?;
                break;
            }
            if printer::list_tail_needs_label(runtime, options, cdr_word) {
                buffer.layout.push_text(" . ");
                append_pretty(runtime, package, cdr_word, options, depth + 1, buffer, at)?;
                break;
            }
            buffer.layout.push_text(" ");
            buffer
                .layout
                .push_token(LayoutToken::Newline(NewlineKind::Fill));
            spine = cdr_word;
        }
        buffer.layout.push_text(")");
        buffer.layout.push_token(LayoutToken::BlockEnd);
        buffer.remove_path(value.slot.get());
        return Ok(());
    }
    let compound_array = runtime.heap.array_info(value).is_some_and(|array| {
        matches!(
            array.element,
            crate::heap::ArrayElement::General
                | crate::heap::ArrayElement::SingleFloat
                | crate::heap::ArrayElement::DoubleFloat
        ) && (options.array || options.readably)
    });
    if printer::uses_print_object(runtime, value) || compound_array {
        let object = runtime.root_slot(value);
        let (_, items) = capture_layout_items(runtime, buffer.layout.column, at, |stream| {
            let stream = runtime.root_slot(stream);
            let suffix = printer::render(
                runtime,
                package,
                object.get(),
                options,
                Some(&stream),
                printer::PrintDispatchMode::Dispatch(depth),
                at,
            )?;
            write(runtime, stream.get(), &suffix, at)
        })?;
        buffer.layout.append_capture(items);
    } else {
        let text = printer::render(
            runtime,
            package,
            value,
            options,
            None,
            printer::PrintDispatchMode::Dispatch(depth),
            at,
        )?;
        buffer.layout.push_text(&text);
    }
    Ok(())
}

fn render_pretty_items(
    runtime: &PrintContext<'_>,
    package: Package,
    object: Word,
    options: &PrintControls,
    table: Option<Word>,
    at: usize,
) -> Result<Option<Vec<LayoutItem>>, Fault> {
    let object = runtime.root(object);
    let mut buffer = PrettyRenderBuffer::new(
        options.start_column,
        active_dispatch_table(runtime, table, at)?,
    );
    let depth = logical_block_depth(runtime);
    if runtime.heap.cons_fields(object.slot.get()).is_none() {
        return Ok(buffer
            .dispatch(runtime, &object.slot, depth, at)?
            .then(|| buffer.layout.finish()));
    }
    let _sharing = printer::sharing_scope(runtime, object.slot.get(), options);
    append_pretty(
        runtime,
        package,
        object.slot.get(),
        options,
        depth,
        &mut buffer,
        at,
    )?;
    Ok(Some(buffer.layout.finish()))
}

/// Render VALUE through the pprint dispatch table when *PRINT-PRETTY* is in
/// effect.  Returns None when ordinary printing should be used.
pub(crate) fn render_pretty_text(
    runtime: &PrintContext<'_>,
    package: Package,
    object: Word,
    options: &PrintControls,
    table: Option<Word>,
    at: usize,
) -> Result<Option<String>, Fault> {
    Ok(
        render_pretty_items(runtime, package, object, options, table, at)?.map(|items| {
            render_layout(
                &items,
                options.start_column,
                options.right_margin,
                options.miser_width,
            )
        }),
    )
}

/// Render OBJECT as pretty-printer marker text without laying it out.  The
/// enclosing logical block owns the layout when the destination is a block
/// buffer.
pub(crate) fn render_pretty_markers(
    runtime: &PrintContext<'_>,
    package: Package,
    object: Word,
    options: &PrintControls,
    table: Option<Word>,
    at: usize,
) -> Result<Option<String>, Fault> {
    Ok(
        render_pretty_items(runtime, package, object, options, table, at)?
            .map(|items| encode_format_markers(&items)),
    )
}

// ---------------------------------------------------------------------------
// Layout entry points used by the PPRINT functions.

fn layout_for_stream(
    runtime: &PrintContext<'_>,
    at: usize,
) -> Result<(Option<usize>, Option<usize>), Fault> {
    Ok((
        numeric_limit(runtime, "*PRINT-RIGHT-MARGIN*", at)?,
        numeric_limit(runtime, "*PRINT-MISER-WIDTH*", at)?,
    ))
}

fn read_pretty_controls(
    runtime: &PrintContext<'_>,
    column: usize,
    at: usize,
) -> Result<PrintControls, Fault> {
    let base = special(runtime, "*PRINT-BASE*", at)?
        .as_fixnum()
        .and_then(|base| u32::try_from(base).ok())
        .filter(|base| (2..=36).contains(base))
        .unwrap_or(10);
    let case = match special_optional(runtime, "*PRINT-CASE*")
        .and_then(|value| runtime.heap.symbol_name(value))
        .as_deref()
    {
        Some("DOWNCASE") => PrintCase::Downcase,
        Some("CAPITALIZE") => PrintCase::Capitalize,
        _ => PrintCase::Upcase,
    };
    let readably = boolean(special(runtime, "*PRINT-READABLY*", at)?);
    if !readably {
        let _ = numeric_limit(runtime, "*PRINT-LINES*", at)?;
    }
    Ok(PrintControls {
        base,
        case,
        escape: boolean(special(runtime, "*PRINT-ESCAPE*", at)?),
        gensym: boolean(special(runtime, "*PRINT-GENSYM*", at)?),
        length: if readably {
            None
        } else {
            numeric_limit(runtime, "*PRINT-LENGTH*", at)?
        },
        level: if readably {
            None
        } else {
            numeric_limit(runtime, "*PRINT-LEVEL*", at)?
        },
        miser_width: numeric_limit(runtime, "*PRINT-MISER-WIDTH*", at)?,
        right_margin: numeric_limit(runtime, "*PRINT-RIGHT-MARGIN*", at)?,
        radix: boolean(special(runtime, "*PRINT-RADIX*", at)?),
        readably,
        array: boolean(special(runtime, "*PRINT-ARRAY*", at)?),
        pretty: true,
        circle: boolean(special(runtime, "*PRINT-CIRCLE*", at)?),
        start_column: column,
    })
}

fn render_pretty_to_stream(
    runtime: &PrintContext<'_>,
    stream: Word,
    object: Word,
    at: usize,
) -> Result<(), Fault> {
    let stream = runtime.root(stream);
    let object = runtime.root(object);
    let column = stream_column(runtime, stream.slot.get(), at)?;
    let package = printer::current_package(runtime, at)?;
    let options = read_pretty_controls(runtime, column, at)?;
    if in_block_buffer(stream.slot.get()) {
        if let Some(items) =
            render_pretty_items(runtime, package, object.slot.get(), &options, None, at)?
        {
            return append_to_block_buffer(runtime, stream.slot.get(), at, |entry| {
                entry.layout.push_items(items);
            });
        }
    } else if let Some(text) =
        render_pretty_text(runtime, package, object.slot.get(), &options, None, at)?
    {
        return write(runtime, stream.slot.get(), &text, at);
    }
    let text = printer::render(
        runtime,
        package,
        object.slot.get(),
        &options,
        Some(&stream.slot),
        printer::PrintDispatchMode::current_depth(runtime),
        at,
    )?;
    write(runtime, stream.slot.get(), &text, at)
}

/// Printer options built from the current dynamic printer variables.
pub(crate) fn printer_options(
    runtime: &PrintContext<'_>,
    at: usize,
) -> Result<PrintControls, Fault> {
    read_pretty_controls(runtime, 0, at)
}

// ---------------------------------------------------------------------------
// Logical block primitives used by the PPRINT-LOGICAL-BLOCK macro.

#[expect(
    clippy::too_many_arguments,
    reason = "Lisp PPRINT-LOGICAL-BLOCK primitive receives its 7 macro operands directly"
)]
pub(crate) fn block_start(
    runtime: &PrintContext<'_>,
    stream: Word,
    object: Word,
    prefix: Word,
    per_line: Word,
    prefix_supplied: Word,
    per_line_supplied: Word,
    at: usize,
) -> Result<Word, Fault> {
    let object_root = runtime.root(object);
    let prefix_root = runtime.root(prefix);
    let per_line_root = runtime.root(per_line);
    let target = runtime.root(output_stream(runtime, stream, at)?);
    if !ensure_list(object_root.slot.get(), runtime) {
        super::super::printer::print_to(
            runtime,
            super::super::printer::PrinterPrimitive::Write,
            object_root.slot.get(),
            target.slot.get(),
            at,
        )?;
        return Ok(Word::NIL);
    }
    if print_level_reached(runtime, logical_block_depth(runtime), at)? {
        write(runtime, target.slot.get(), "#", at)?;
        return Ok(Word::NIL);
    }
    let prefix_text = string_designator(
        runtime,
        prefix_root.slot.get(),
        prefix_supplied != Word::NIL,
        "prefix",
        at,
    )?;
    let per_line_text = string_designator(
        runtime,
        per_line_root.slot.get(),
        per_line_supplied != Word::NIL,
        "per-line prefix",
        at,
    )?;
    let options = read_pretty_controls(runtime, 0, at)?;
    let pushed = printer::push_sharing_labels(runtime, object_root.slot.get(), &options);
    let mut sharing_guard = pushed.then(|| DropGuard::new((), |()| printer::pop_sharing_labels()));
    let sharing_label = if options.circle {
        printer::next_sharing_marker(runtime, &options, object_root.slot.get())
    } else {
        None
    };
    if let Some(SharingMarker::Reference(id)) = sharing_label {
        drop(sharing_guard);
        write(runtime, target.slot.get(), &format!("#{id}#"), at)?;
        return Ok(Word::NIL);
    }
    let nested = in_block_buffer(target.slot.get());
    let buffer_root = if nested {
        drain_block_buffer(runtime, target.slot.get(), at)?;
        target.clone()
    } else {
        runtime.root(string_stream(runtime, at)?)
    };
    let scope_flag = runtime.root(if pushed { Word::T } else { Word::NIL });
    let inner = runtime
        .cons(&target, &scope_flag)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let pair = runtime
        .cons(&buffer_root, &inner)
        .map_err(|error| EvalError::out_of_memory(at, error))?;

    if !nested {
        let column = stream_column(runtime, target.slot.get(), at)?;
        LOGICAL_BLOCK_BUFFERS.with(|buffers| {
            buffers.borrow_mut().push(LogicalBlockBuffer::new(
                runtime,
                buffer_root.slot.get(),
                target.slot.get(),
                column,
            ))
        });
    }
    LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let block = buffers
            .iter_mut()
            .rev()
            .find(|entry| entry.stream.get() == buffer_root.slot.get())
            .expect("active logical block");
        block
            .list_origins
            .push(options.circle.then(|| LogicalBlockListOrigin {
                object: runtime.root_slot(object_root.slot.get()),
                checked_by_pprint_pop: false,
            }));
        if let Some(SharingMarker::Definition(id)) = sharing_label {
            block.push_text_or_markers(&format!("#{id}="));
        }
        if let Some(prefix) = prefix_text.as_ref().or(per_line_text.as_ref()) {
            block.push_text_or_markers(prefix);
        }
        block
            .layout
            .push_token(LayoutToken::BlockStart(per_line_text));
    });
    if let Some(guard) = sharing_guard.take() {
        DropGuard::dismiss(guard);
    }
    Ok(pair.slot.get())
}

fn string_designator(
    runtime: &PrintContext<'_>,
    value: Word,
    supplied: bool,
    what: &str,
    at: usize,
) -> Result<Option<String>, Fault> {
    if !supplied {
        return Ok(None);
    }
    if let Some(text) = runtime.heap.string_contents(value) {
        let active = runtime
            .heap
            .array_info(value)
            .and_then(|info| info.fill_pointer);
        return Ok(Some(match active {
            Some(fill) if fill < text.chars().count() => text.chars().take(fill).collect(),
            _ => text,
        }));
    }
    Err(Fault::type_error(runtime, 
        value,
        "STRING",
        EvalError::new(at, format!("logical block {what} must be a string")),
    ))
}

pub(crate) fn block_end(
    runtime: &PrintContext<'_>,
    state: Word,
    suffix: Word,
    suffix_supplied: Word,
    at: usize,
) -> Result<Word, Fault> {
    let Some((buffer_word, rest)) = runtime.heap.cons_fields(state) else {
        return Ok(Word::NIL);
    };
    let (target_word, scope_flag) = runtime.heap.cons_fields(rest).unwrap_or((rest, Word::NIL));
    let _sharing_guard =
        (scope_flag != Word::NIL).then(|| DropGuard::new((), |()| printer::pop_sharing_labels()));
    let buffer = runtime.root(buffer_word);
    let target = runtime.root(target_word);
    let suffix = runtime.root(suffix);

    drain_block_buffer(runtime, buffer.slot.get(), at)?;
    // Remove the block on an invalid suffix as well, so later writes on the
    // enclosing stream resume in their original logical block.
    let suffix_text = string_designator(
        runtime,
        suffix.slot.get(),
        suffix_supplied != Word::NIL,
        "suffix",
        at,
    );
    let outermost = buffer.slot.get() != target.slot.get();
    let items = LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let pos = buffers
            .iter()
            .rposition(|entry| entry.stream.get() == buffer.slot.get())?;
        let block = &mut buffers[pos];
        if let Ok(Some(suffix)) = &suffix_text {
            block.push_text_or_markers(suffix);
        }
        block.layout.push_token(LayoutToken::BlockEnd);
        block.list_origins.pop();
        outermost.then(|| buffers.remove(pos).layout.finish())
    });
    suffix_text?;
    let Some(items) = items else {
        return Ok(Word::NIL);
    };
    let column = stream_column(runtime, target.slot.get(), at)?;
    let (margin, miser) = layout_for_stream(runtime, at)?;
    let laid_out = render_layout(&items, column, margin, miser);
    write(runtime, target.slot.get(), &laid_out, at)?;
    Ok(Word::NIL)
}

pub(crate) fn length_check(
    runtime: &PrintContext<'_>,
    list: Word,
    buffer: Word,
    counter: Word,
    at: usize,
) -> Result<Word, Fault> {
    let list = runtime.root(list);
    let buffer = runtime.root(buffer);
    let counter = runtime.root(counter);
    let length = if boolean(special(runtime, "*PRINT-READABLY*", at)?) {
        Word::NIL
    } else {
        special(runtime, "*PRINT-LENGTH*", at)?
    };
    let count = runtime
        .heap
        .cons_fields(counter.slot.get())
        .and_then(|(value, _)| value.as_fixnum())
        .unwrap_or(0);
    let initial = LOGICAL_BLOCK_BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let buffer_word = buffer.slot.get();
        let list_word = list.slot.get();
        match buffers
            .iter_mut()
            .rev()
            .find(|entry| entry.stream.get() == buffer_word)
            .and_then(|entry| entry.list_origins.last_mut()?.as_mut())
        {
            Some(origin) if !origin.checked_by_pprint_pop && origin.object.get() == list_word => {
                origin.checked_by_pprint_pop = true;
                true
            }
            _ => false,
        }
    });
    if !initial {
        let options = read_pretty_controls(runtime, 0, at)?;
        if printer::list_tail_needs_label(runtime, &options, list.slot.get()) {
            write(runtime, buffer.slot.get(), ". ", at)?;
            let package = printer::current_package(runtime, at)?;
            let mut options = options;
            options.pretty = false;
            let text = printer::render(
                runtime,
                package,
                list.slot.get(),
                &options,
                None,
                printer::PrintDispatchMode::current_depth(runtime),
                at,
            )?;
            write(runtime, buffer.slot.get(), &text, at)?;
            return Ok(Word::NIL);
        }
    }
    if runtime.heap.cons_fields(list.slot.get()).is_none() && list.slot.get() != Word::NIL {
        write(runtime, buffer.slot.get(), ". ", at)?;
        let package = printer::current_package(runtime, at)?;
        let column = stream_column(runtime, buffer.slot.get(), at)?;
        let mut options = read_pretty_controls(runtime, column, at)?;
        options.pretty = false;
        let text = printer::render(
            runtime,
            package,
            list.slot.get(),
            &options,
            None,
            printer::PrintDispatchMode::current_depth(runtime),
            at,
        )?;
        write(runtime, buffer.slot.get(), &text, at)?;
        return Ok(Word::NIL);
    }
    if length != Word::NIL && length.as_fixnum().is_some_and(|limit| count == limit) {
        write(runtime, buffer.slot.get(), "...", at)?;
        return Ok(Word::NIL);
    }
    runtime.heap.set_cons_car(
        counter.slot.get(),
        Word::fixnum(count + 1).unwrap_or(Word::NIL),
    );
    Ok(Word::T)
}

// ---------------------------------------------------------------------------
// PPRINT family

#[derive(Clone, Copy)]
pub(crate) enum ListLayout {
    Fill,
    Linear,
    Tabular(Option<usize>),
}

fn ensure_list(value: Word, runtime: &PrintContext<'_>) -> bool {
    value == Word::NIL || runtime.heap.cons_fields(value).is_some()
}

fn pprint_list(
    runtime: &PrintContext<'_>,
    stream: Word,
    object: Word,
    colon: bool,
    style: ListLayout,
    at: usize,
) -> Result<Word, Fault> {
    let target = runtime.root(output_stream(runtime, stream, at)?);
    let object = runtime.root(object);
    let column = stream_column(runtime, target.slot.get(), at)?;
    let pretty = print_pretty(runtime, at)?;
    if !pretty || !ensure_list(object.slot.get(), runtime) {
        let package = printer::current_package(runtime, at)?;
        let mut options = read_pretty_controls(runtime, column, at)?;
        if !pretty {
            options.pretty = false;
        }
        let text = if pretty {
            match render_pretty_text(runtime, package, object.slot.get(), &options, None, at)? {
                Some(text) => text,
                None => printer::render(
                    runtime,
                    package,
                    object.slot.get(),
                    &options,
                    Some(&target.slot),
                    printer::PrintDispatchMode::current_depth(runtime),
                    at,
                )?,
            }
        } else {
            printer::render(
                runtime,
                package,
                object.slot.get(),
                &options,
                Some(&target.slot),
                printer::PrintDispatchMode::current_depth(runtime),
                at,
            )?
        };
        return write(runtime, target.slot.get(), &text, at).map(|_| Word::NIL);
    }
    let element_options = read_pretty_controls(runtime, column, at)?;
    let base_depth = logical_block_depth(runtime);
    if element_options.level_reached(base_depth) {
        return write(runtime, target.slot.get(), "#", at).map(|_| Word::NIL);
    }
    let _sharing = printer::sharing_scope(runtime, object.slot.get(), &element_options);
    let top_label = match printer::next_sharing_marker(runtime, &element_options, object.slot.get()) {
        Some(SharingMarker::Reference(id)) => {
            return write(runtime, target.slot.get(), &format!("#{id}#"), at).map(|_| Word::NIL);
        }
        Some(SharingMarker::Definition(id)) => Some(id),
        None => None,
    };
    let mut buffer = PrettyRenderBuffer::new(column, active_dispatch_table(runtime, None, at)?);
    if let Some(id) = top_label {
        buffer.layout.push_text(&format!("#{id}="));
    }
    if colon {
        buffer.layout.push_text("(");
    }
    buffer.layout.push_token(LayoutToken::BlockStart(None));
    let package = printer::current_package(runtime, at)?;
    let mut spine = object.slot.get();
    let mut count = 0usize;
    loop {
        if !element_options.readably
            && let Some(limit) = element_options.length
            && count >= limit
        {
            buffer.layout.push_text("...");
            break;
        }
        if spine == Word::NIL {
            break;
        }
        let Some((car, cdr)) = runtime.heap.cons_fields(spine) else {
            buffer.layout.push_text(" . ");
            append_pretty(
                runtime,
                package,
                spine,
                &element_options,
                base_depth,
                &mut buffer,
                at,
            )?;
            break;
        };
        let car = runtime.root(car);
        let cdr = runtime.root(cdr);
        append_pretty(
            runtime,
            package,
            car.slot.get(),
            &element_options,
            base_depth + 1,
            &mut buffer,
            at,
        )?;
        count += 1;
        let cdr_word = cdr.slot.get();
        if cdr_word == Word::NIL {
            break;
        }
        if runtime.heap.cons_fields(cdr_word).is_none() {
            buffer.layout.push_text(" . ");
            append_pretty(
                runtime,
                package,
                cdr_word,
                &element_options,
                base_depth + 1,
                &mut buffer,
                at,
            )?;
            break;
        }
        if printer::list_tail_needs_label(runtime, &element_options, cdr_word) {
            buffer.layout.push_text(" . ");
            append_pretty(
                runtime,
                package,
                cdr_word,
                &element_options,
                base_depth + 1,
                &mut buffer,
                at,
            )?;
            break;
        }
        buffer.layout.push_text(" ");
        match style {
            ListLayout::Fill => buffer
                .layout
                .push_token(LayoutToken::Newline(NewlineKind::Fill)),
            ListLayout::Linear => buffer
                .layout
                .push_token(LayoutToken::Newline(NewlineKind::Linear)),
            ListLayout::Tabular(tabsize) => {
                buffer.layout.push_token(LayoutToken::Tab(
                    TabKind::SectionRelative,
                    0,
                    tabsize.unwrap_or(16),
                ));
                buffer
                    .layout
                    .push_token(LayoutToken::Newline(NewlineKind::Fill));
            }
        }
        spine = cdr_word;
    }
    if colon {
        buffer.layout.push_text(")");
    }
    buffer.layout.push_token(LayoutToken::BlockEnd);
    let items = buffer.layout.finish();
    let target_word = target.slot.get();
    if in_block_buffer(target_word) {
        append_to_block_buffer(runtime, target_word, at, |entry| {
            entry.layout.push_items(items);
        })?;
    } else {
        let (margin, miser) = layout_for_stream(runtime, at)?;
        let text = render_layout(&items, column, margin, miser);
        write(runtime, target_word, &text, at)?;
    }
    Ok(Word::NIL)
}

fn keyword_argument(
    runtime: &PrintContext<'_>,
    value: Word,
    at: usize,
) -> Result<Option<String>, Fault> {
    if value == Word::NIL {
        return Ok(None);
    }
    match crate::condition::named_word(runtime, value, Package::KEYWORD) {
        Some(name) => Ok(Some(name)),
        None => type_error(runtime, value, "SYMBOL", at, "expected a keyword"),
    }
}

pub(crate) fn pprint(
    runtime: &PrintContext<'_>,
    object: Word,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    let target = runtime.root(output_stream(runtime, stream, at)?);
    let object = runtime.root(object);
    write(runtime, target.slot.get(), "\n", at)?;
    render_pretty_to_stream(runtime, target.slot.get(), object.slot.get(), at)?;
    Ok(Word::NIL)
}

pub(crate) fn pprint_newline(
    runtime: &PrintContext<'_>,
    kind: Word,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    let target = runtime.root(output_stream(runtime, stream, at)?);
    let kind_name = keyword_argument(runtime, kind, at)?.ok_or_else(|| {
        Fault::type_error(runtime, 
            kind,
            "KEYWORD",
            EvalError::new(at, "PPRINT-NEWLINE requires a newline kind"),
        )
    })?;
    let kind = match kind_name.as_str() {
        "LINEAR" => NewlineKind::Linear,
        "FILL" => NewlineKind::Fill,
        "MISER" => NewlineKind::Miser,
        "MANDATORY" => NewlineKind::Mandatory,
        _ => return type_error(runtime, kind, "KEYWORD", at, "invalid pprint newline kind"),
    };
    if in_block_buffer(target.slot.get()) && print_pretty(runtime, at)? {
        append_to_block_buffer(runtime, target.slot.get(), at, |entry| {
            entry.layout.push_token(LayoutToken::Newline(kind));
        })?;
    }
    Ok(Word::NIL)
}

const LOGICAL_BLOCK_DEPTH_SPECIAL: &str = "*%PPRINT-DEPTH*";

pub(crate) fn initialize_logical_block_depth(runtime: &Runtime) -> Result<(), EvalError> {
    let (symbol, _) = runtime.intern_package(Package::ZK, LOGICAL_BLOCK_DEPTH_SPECIAL, 0)?;
    runtime.heap.proclaim_special(symbol);
    runtime
        .heap
        .set_symbol_cell(symbol, false, Word::fixnum(0).unwrap());
    Ok(())
}

pub(crate) fn logical_block_depth_symbol(runtime: &Runtime) -> Option<Word> {
    runtime
        .packages
        .borrow()
        .present(Package::ZK, LOGICAL_BLOCK_DEPTH_SPECIAL)
        .map(|(symbol, _)| symbol)
}

pub(crate) fn logical_block_depth(runtime: &Runtime) -> usize {
    logical_block_depth_symbol(runtime)
        .and_then(|symbol| runtime.heap.symbol_cell(symbol, false))
        .and_then(Word::as_fixnum)
        .unwrap_or(0)
        .max(0) as usize
}

pub(crate) fn bind_logical_block_depth<'a>(
    runtime: &'a PrintContext<'_>,
    depth: usize,
) -> impl Drop + use<'a> {
    let symbol = logical_block_depth_symbol(runtime).expect("initialized printer depth");
    let depth = Word::fixnum(depth as isize).expect("printer depth fits in a fixnum");
    runtime.bind_specials(vec![(symbol, depth)])
}

pub(super) fn print_level_reached(
    runtime: &PrintContext<'_>,
    depth: usize,
    at: usize,
) -> Result<bool, Fault> {
    Ok(!boolean(special(runtime, "*PRINT-READABLY*", at)?)
        && numeric_limit(runtime, "*PRINT-LEVEL*", at)?.is_some_and(|limit| depth >= limit))
}

pub(crate) fn pprint_indent(
    runtime: &PrintContext<'_>,
    relative_to: Word,
    amount: Word,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    let target = runtime.root(output_stream(runtime, stream, at)?);
    let kind = keyword_argument(runtime, relative_to, at)?
        .ok_or_else(|| error(at, "PPRINT-INDENT requires :BLOCK or :CURRENT"))?;
    let kind = match kind.as_str() {
        "BLOCK" => IndentKind::Block,
        "CURRENT" => IndentKind::Current,
        _ => {
            return type_error(
                runtime,
                relative_to,
                "KEYWORD",
                at,
                "PPRINT-INDENT requires :BLOCK or :CURRENT",
            );
        }
    };
    let amount = real_to_i64(runtime, amount, at)?;
    if in_block_buffer(target.slot.get()) && print_pretty(runtime, at)? {
        append_to_block_buffer(runtime, target.slot.get(), at, |entry| {
            entry.layout.push_token(LayoutToken::Indent(kind, amount));
        })?;
    }
    Ok(Word::NIL)
}

pub(crate) fn pprint_tab(
    runtime: &PrintContext<'_>,
    kind: Word,
    colnum: Word,
    colinc: Word,
    stream: Word,
    at: usize,
) -> Result<Word, Fault> {
    let target = runtime.root(output_stream(runtime, stream, at)?);
    let name = keyword_argument(runtime, kind, at)?
        .ok_or_else(|| error(at, "PPRINT-TAB requires a tab kind"))?;
    let kind = match name.as_str() {
        "LINE" => TabKind::Line,
        "SECTION" => TabKind::Section,
        "LINE-RELATIVE" => TabKind::LineRelative,
        "SECTION-RELATIVE" => TabKind::SectionRelative,
        _ => return type_error(runtime, kind, "KEYWORD", at, "invalid pprint tab kind"),
    };
    let colnum = non_negative(runtime, colnum, "colnum", at)?;
    let colinc = non_negative(runtime, colinc, "colinc", at)?;
    if in_block_buffer(target.slot.get()) && print_pretty(runtime, at)? {
        append_to_block_buffer(runtime, target.slot.get(), at, |entry| {
            entry
                .layout
                .push_token(LayoutToken::Tab(kind, colnum, colinc));
        })?;
    }
    Ok(Word::NIL)
}

fn real_to_i64(runtime: &PrintContext<'_>, value: Word, at: usize) -> Result<i64, Fault> {
    if let Some(number) = value.as_fixnum() {
        return Ok(number as i64);
    }
    if let Some(crate::number::Number::Real(real)) = runtime.heap.number(value) {
        return Ok(match real {
            crate::number::Real::Integer(integer) => {
                use num_traits::ToPrimitive;
                integer.to_i64().unwrap_or(i64::MAX)
            }
            crate::number::Real::Ratio(ratio) => {
                use num_traits::ToPrimitive;
                ratio.to_i64().unwrap_or(i64::MAX)
            }
            crate::number::Real::Single(number) => number.trunc() as i64,
            crate::number::Real::Double(number) => number.trunc() as i64,
        });
    }
    type_error(runtime, value, "REAL", at, "expected a real indentation amount")
}

fn non_negative(
    runtime: &PrintContext<'_>,
    value: Word,
    name: &str,
    at: usize,
) -> Result<usize, Fault> {
    let number = real_to_i64(runtime, value, at)?;
    usize::try_from(number).map_err(|_| error(at, format!("{name} must be a non-negative integer")))
}

pub(crate) fn copy_pprint_dispatch(
    runtime: &PrintContext<'_>,
    table: Word,
    at: usize,
) -> Result<Word, Fault> {
    let source = dispatch_table(runtime, (table != Word::NIL).then_some(table), at)?;
    let mut copied = Vec::new();
    let Some((_, mut spine)) = runtime.heap.cons_fields(source) else {
        return initial_table(runtime, at);
    };
    while let Some((entry, tail)) = runtime.heap.cons_fields(spine) {
        copied.push(runtime.root_slot(entry));
        spine = tail;
    }
    let tag = runtime.root(table_tag(runtime, at)?);
    let mut result = runtime.nil();
    for entry in copied.into_iter().rev() {
        let entry = runtime.root(entry.get());
        result = runtime
            .cons(&entry, &result)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    let table = runtime
        .cons(&tag, &result)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(table.slot.get())
}

pub(crate) fn set_pprint_dispatch(
    runtime: &PrintContext<'_>,
    spec: Word,
    function: Word,
    priority: Option<Word>,
    table: Option<Word>,
    at: usize,
) -> Result<Word, Fault> {
    let spec = runtime.root(spec);
    let priority_value = match priority {
        None => 0.0,
        Some(priority) => real_priority(runtime, priority, at)?,
    };
    let function = if function == Word::NIL {
        Word::NIL
    } else {
        super::super::designator(runtime, function, at)?
    };
    let function = runtime.root(function);
    let table = match table {
        None => {
            let current = special(runtime, "*PRINT-PPRINT-DISPATCH*", at)?;
            if current != Word::NIL && is_table(runtime, current) {
                current
            } else if current == Word::NIL {
                let table = initial_table(runtime, at)?;
                let symbol = runtime
                    .find_named_symbol(&Name::common_lisp("*PRINT-PPRINT-DISPATCH*"))
                    .ok_or_else(|| error(at, "missing *PRINT-PPRINT-DISPATCH*"))?;
                runtime.heap.set_symbol_cell(symbol, false, table);
                table
            } else {
                return type_error(runtime, current, "NULL", at, "expected a pprint dispatch table");
            }
        }
        Some(table) => {
            if table == Word::NIL || !is_table(runtime, table) {
                return type_error(runtime, table, "NULL", at, "expected a pprint dispatch table");
            }
            table
        }
    };
    let table = runtime.root(table);
    let mut current: Vec<(RootSlot, RootSlot, f64)> = dispatch_entries(table.slot.get(), runtime)?
        .into_iter()
        .filter(|(existing, _, _)| !equal(runtime, existing.get(), spec.slot.get(), at))
        .collect();
    if function.slot.get() != Word::NIL {
        current.push((
            runtime.root_slot(spec.slot.get()),
            runtime.root_slot(function.slot.get()),
            priority_value,
        ));
        current.sort_by(|left, right| right.2.total_cmp(&left.2));
    }
    let mut list = runtime.nil();
    for (entry_spec, entry_function, entry_priority) in current.into_iter().rev() {
        let priority_word = if entry_priority == entry_priority.trunc() {
            Word::fixnum(entry_priority as isize).unwrap_or(Word::NIL)
        } else {
            runtime
                .heap
                .allocate_number(&crate::number::Number::Real(crate::number::Real::Double(
                    entry_priority,
                )))
                .map_err(|error| EvalError::out_of_memory(at, error))?
        };
        let priority_root = runtime.root(priority_word);
        let tail = runtime
            .cons(&priority_root, &runtime.nil())
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        let function_root = runtime.root(entry_function.get());
        let rest = runtime
            .cons(&function_root, &tail)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        let spec_root = runtime.root(entry_spec.get());
        let entry = runtime
            .cons(&spec_root, &rest)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        list = runtime
            .cons(&entry, &list)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    runtime.heap.set_cons_cdr(table.slot.get(), list.slot.get());
    Ok(Word::NIL)
}

fn equal(runtime: &PrintContext<'_>, left: Word, right: Word, at: usize) -> bool {
    let _ = at;
    super::super::equality::equal(&runtime.heap, left, right)
}

#[derive(Clone, Copy)]
pub(crate) enum PprintPrimitive {
    Pprint,
    PprintFill,
    PprintLinear,
    PprintTabular,
    PprintTab,
    PprintIndent,
    PprintNewline,
    PprintDispatch,
    CopyPprintDispatch,
    SetPprintDispatch,
    BlockStart,
    BlockEnd,
    LengthCheck,
    PrintUnreadable,
    PrintToStream,
    DispatchValues,
}

impl PprintPrimitive {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Pprint => count == 1 || count == 2,
            Self::PprintFill | Self::PprintLinear => (2..=4).contains(&count),
            Self::PprintTabular => (2..=5).contains(&count),
            Self::PprintTab => (3..=4).contains(&count),
            Self::PprintIndent => (2..=3).contains(&count),
            Self::PprintNewline => count == 1 || count == 2,
            Self::PprintDispatch => count == 1 || count == 2,
            Self::CopyPprintDispatch => count <= 1,
            Self::SetPprintDispatch => (2..=4).contains(&count),
            Self::BlockStart => count == 6,
            Self::BlockEnd => count == 3,
            Self::LengthCheck => count == 3,
            Self::PrintUnreadable => count == 5,
            Self::PrintToStream => count == 1 || count == 2,
            Self::DispatchValues => count == 1 || count == 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Lisp wrappers.  PPRINT returns no values and PPRINT-DISPATCH returns two
// values; both are installed as compiled Lisp functions so that the VM's
// multiple-value protocol applies.

fn user_symbol(runtime: &PrintContext<'_>, name: &str, at: usize) -> Result<Word, Fault> {
    runtime
        .intern_package(Package::USER, name, at)
        .map(|(word, _)| word)
        .map_err(Into::into)
}

fn bind_keyword_function(
    runtime: &PrintContext<'_>,
    keyword: &str,
    primitive: &str,
    at: usize,
) -> Result<(), Fault> {
    let symbol = runtime.intern(&Name::keyword(keyword), at)?;
    let function = super::super::Primitive::named(primitive)
        .ok_or_else(|| error(at, format!("missing primitive {primitive}")))?;
    runtime.heap.set_symbol_cell(symbol, true, function);
    Ok(())
}

fn compile_native_lambda(
    runtime: &PrintContext<'_>,
    source: &str,
    at: usize,
) -> Result<Word, Fault> {
    let form = crate::read::read(runtime, source)
        .map_err(|error| Fault::plain(Class::ProgramError, error))?;
    let Some([_, parameters, body @ ..]) = form.list() else {
        return Err(error(at, "invalid pprint wrapper source"));
    };
    let (code, _) = crate::compile::compile_lambda(runtime, parameters, body, at)
        .map_err(|error| Fault::plain(Class::ProgramError, error))?;
    code.instantiate(runtime)
        .map_err(|error| Fault::plain(Class::ProgramError, error))
}

pub(crate) fn install_functions(runtime: &PrintContext<'_>, at: usize) -> Result<(), Fault> {
    let flag = user_symbol(runtime, "*%PPRINT-FUNCTIONS*", at)?;
    if runtime.heap.symbol_cell(flag, false) == Some(Word::T) {
        return Ok(());
    }
    bind_keyword_function(runtime, "%PPRINT-IMPL", "%PPRINT-IMPL", at)?;
    bind_keyword_function(
        runtime,
        "%PPRINT-DISPATCH-VALUES",
        "%PPRINT-DISPATCH-VALUES",
        at,
    )?;
    let pprint = compile_native_lambda(
        runtime,
        "(lambda (object &optional (output-stream nil)) \
           (let ((*print-pretty* t) (*print-escape* t)) \
             (:%pprint-impl object output-stream)) (values))",
        at,
    )?;
    let dispatch = compile_native_lambda(
        runtime,
        "(lambda (object &optional (table nil)) \
           (values-list (:%pprint-dispatch-values object table)))",
        at,
    )?;
    for (name, function) in [("PPRINT", pprint), ("PPRINT-DISPATCH", dispatch)] {
        let symbol = runtime
            .find_named_symbol(&Name::common_lisp(name))
            .ok_or_else(|| error(at, format!("missing symbol {name}")))?;
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    runtime.heap.set_symbol_cell(flag, false, Word::T);
    Ok(())
}

fn dispatch_values(
    runtime: &PrintContext<'_>,
    object: Word,
    table: Word,
    at: usize,
) -> Result<Word, Fault> {
    let object = runtime.root(object);
    let table = dispatch_table(runtime, (table != Word::NIL).then_some(table), at)?;
    let (function, found) = match find_dispatch_function(runtime, object.slot.get(), table, at)? {
        Some(function) => (function, Word::T),
        None => (
            runtime
                .find_named_symbol(&Name::common_lisp("WRITE"))
                .ok_or_else(|| error(at, "missing WRITE"))?,
            Word::NIL,
        ),
    };
    let function = runtime.root(function);
    let found = runtime.root(found);
    let tail = runtime
        .cons(&found, &runtime.nil())
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let list = runtime
        .cons(&function, &tail)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(list.slot.get())
}

// ---------------------------------------------------------------------------
// PRINT-UNREADABLE-OBJECT support.

pub(crate) fn print_unreadable(
    runtime: &PrintContext<'_>,
    object: Word,
    stream: Word,
    show_type: Word,
    show_identity: Word,
    thunk: Word,
    at: usize,
) -> Result<Word, Fault> {
    if boolean(special(runtime, "*PRINT-READABLY*", at)?) {
        return Err(Fault::print_not_readable(runtime, 
            object,
            EvalError::new(at, "cannot print an unreadable object readably"),
        ));
    }
    let object = runtime.root(object);
    let thunk = runtime.root(thunk);
    let target = runtime.root(output_stream(runtime, stream, at)?);
    write(runtime, target.slot.get(), "#<", at)?;
    let show_type = boolean(show_type);
    let show_identity = boolean(show_identity);
    if show_type {
        let type_of = runtime
            .find_named_symbol(&Name::common_lisp("TYPE-OF"))
            .ok_or_else(|| error(at, "missing TYPE-OF"))?;
        let values = call1(runtime, type_of, object.slot.get(), at)?;
        let name = values
            .first()
            .map(|value| value.slot.get())
            .unwrap_or(Word::NIL);
        let name = runtime.root(name);
        let column = stream_column(runtime, target.slot.get(), at)?;
        let package = printer::current_package(runtime, at)?;
        let mut options = read_pretty_controls(runtime, column, at)?;
        options.pretty = false;
        options.level = None;
        options.length = None;
        options.circle = false;
        let text = printer::render(
            runtime,
            package,
            name.slot.get(),
            &options,
            None,
            printer::PrintDispatchMode::current_depth(runtime),
            at,
        )?;
        write(runtime, target.slot.get(), &text, at)?;
        write(runtime, target.slot.get(), " ", at)?;
    }
    if thunk.slot.get() != Word::NIL {
        call0(runtime, thunk.slot.get(), at)?;
    }
    if show_identity {
        if thunk.slot.get() != Word::NIL || !show_type {
            write(runtime, target.slot.get(), " ", at)?;
        }
        let sxhash = runtime
            .find_named_symbol(&Name::common_lisp("SXHASH"))
            .ok_or_else(|| error(at, "missing SXHASH"))?;
        let values = call1(runtime, sxhash, object.slot.get(), at)?;
        let hash = values
            .first()
            .and_then(|value| value.slot.get().as_fixnum())
            .unwrap_or(0) as usize;
        write(runtime, target.slot.get(), &format!("{{{hash:X}}}"), at)?;
    }
    write(runtime, target.slot.get(), ">", at)?;
    Ok(Word::NIL)
}

// ---------------------------------------------------------------------------
// Primitive entry point.

pub(crate) fn run(
    runtime: &PrintContext<'_>,
    frame: &crate::heap::RootSlots,
    op: PprintPrimitive,
    args: std::ops::Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    install_functions(runtime, at)?;
    let count = args.len();
    let arg = |index: usize| frame.get(args.start + index);
    match op {
        PprintPrimitive::Pprint => {
            let stream = if count == 2 { arg(1) } else { Word::NIL };
            pprint(runtime, arg(0), stream, at)
        }
        PprintPrimitive::PprintFill | PprintPrimitive::PprintLinear => {
            let colon = if count >= 3 {
                arg(2) != Word::NIL
            } else {
                true
            };
            let style = if matches!(op, PprintPrimitive::PprintFill) {
                ListLayout::Fill
            } else {
                ListLayout::Linear
            };
            pprint_list(runtime, arg(0), arg(1), colon, style, at)
        }
        PprintPrimitive::PprintTabular => {
            let colon = if count >= 3 {
                arg(2) != Word::NIL
            } else {
                true
            };
            let tabsize = if count >= 5 && arg(4) != Word::NIL {
                Some(non_negative(runtime, arg(4), "tabsize", at)?)
            } else {
                None
            };
            pprint_list(
                runtime,
                arg(0),
                arg(1),
                colon,
                ListLayout::Tabular(tabsize),
                at,
            )
        }
        PprintPrimitive::PprintTab => {
            let stream = if count == 4 { arg(3) } else { Word::NIL };
            pprint_tab(runtime, arg(0), arg(1), arg(2), stream, at)
        }
        PprintPrimitive::PprintIndent => {
            let stream = if count == 3 { arg(2) } else { Word::NIL };
            pprint_indent(runtime, arg(0), arg(1), stream, at)
        }
        PprintPrimitive::PprintNewline => {
            let stream = if count == 2 { arg(1) } else { Word::NIL };
            pprint_newline(runtime, arg(0), stream, at)
        }
        PprintPrimitive::PprintDispatch => {
            let table = if count == 2 { arg(1) } else { Word::NIL };
            dispatch_values(runtime, arg(0), table, at)
        }
        PprintPrimitive::CopyPprintDispatch => {
            let table = if count == 1 { arg(0) } else { Word::NIL };
            copy_pprint_dispatch(runtime, table, at)
        }
        PprintPrimitive::SetPprintDispatch => {
            let priority = (count >= 3).then(|| arg(2));
            let table = (count == 4).then(|| arg(3));
            set_pprint_dispatch(runtime, arg(0), arg(1), priority, table, at)
        }
        PprintPrimitive::BlockStart => {
            block_start(runtime, arg(0), arg(1), arg(2), arg(3), arg(4), arg(5), at)
        }
        PprintPrimitive::BlockEnd => block_end(runtime, arg(0), arg(1), arg(2), at),
        PprintPrimitive::LengthCheck => length_check(runtime, arg(0), arg(1), arg(2), at),
        PprintPrimitive::PrintUnreadable => {
            print_unreadable(runtime, arg(0), arg(1), arg(2), arg(3), arg(4), at)
        }
        PprintPrimitive::PrintToStream => {
            let stream = if count == 2 { arg(1) } else { Word::NIL };
            pprint(runtime, arg(0), stream, at)
        }
        PprintPrimitive::DispatchValues => {
            let table = if count == 2 { arg(1) } else { Word::NIL };
            dispatch_values(runtime, arg(0), table, at)
        }
    }
}

// ---------------------------------------------------------------------------
// Hooks used by FORMAT.

pub(crate) fn pretty_enabled(runtime: &PrintContext<'_>) -> bool {
    special_optional(runtime, "*PRINT-PRETTY*").is_some_and(|value| value != Word::NIL)
}

pub(crate) fn limits(
    runtime: &PrintContext<'_>,
    at: usize,
) -> Result<(Option<usize>, Option<usize>), Fault> {
    Ok((
        numeric_limit(runtime, "*PRINT-RIGHT-MARGIN*", at)?,
        numeric_limit(runtime, "*PRINT-MISER-WIDTH*", at)?,
    ))
}

/// Width of TEXT once pretty-printer markers are removed, with the column
/// reset at every literal newline.
pub(crate) fn visible_width(text: &str) -> usize {
    if !text.contains(FORMAT_MARK) {
        let tail = text.rsplit_once('\n').map_or(text, |(_, tail)| tail);
        return tail.chars().count();
    }
    let items = parse_format_markers(text);
    layout_trailing_width(&items).1
}

pub(crate) fn layout_text(
    text: &str,
    column: usize,
    margin: Option<usize>,
    miser: Option<usize>,
) -> String {
    let items = parse_format_markers(text);
    render_layout(&items, column, margin, miser)
}
