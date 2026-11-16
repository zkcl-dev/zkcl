use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::mem::DropGuard;

use crate::condition::Fault;
use crate::heap::{ArrayElement, Heap, ReadtableCase, RootSlot, Word};
use crate::number::{Number, NumberKind, Real};
use crate::package::{Package, SymbolStatus};
use crate::pathname;
use crate::vm::format::pprint;
use crate::vm::printer::PrintContext;
use crate::{EvalError, Runtime};

mod traversal;
pub(crate) use traversal::render;

const PREVIEW_CHARS: usize = 200;

#[derive(Clone, Copy)]
pub(crate) enum PrintCase {
    Upcase,
    Downcase,
    Capitalize,
}

#[derive(Clone)]
pub(crate) struct PrintControls {
    pub(crate) base: u32,
    pub(crate) case: PrintCase,
    pub(crate) escape: bool,
    pub(crate) gensym: bool,
    pub(crate) length: Option<usize>,
    pub(crate) level: Option<usize>,
    pub(crate) miser_width: Option<usize>,
    pub(crate) right_margin: Option<usize>,
    pub(crate) radix: bool,
    pub(crate) readably: bool,
    pub(crate) array: bool,
    pub(crate) pretty: bool,
    pub(crate) circle: bool,
    /// Column at which the rendered text begins; used by the pretty printer.
    pub(crate) start_column: usize,
}

impl Default for PrintControls {
    fn default() -> Self {
        Self {
            base: 10,
            case: PrintCase::Upcase,
            escape: true,
            gensym: true,
            length: None,
            level: None,
            miser_width: None,
            right_margin: None,
            radix: false,
            readably: false,
            array: true,
            pretty: false,
            circle: false,
            start_column: 0,
        }
    }
}

impl PrintControls {
    pub(crate) fn level_reached(&self, depth: usize) -> bool {
        !self.readably && self.level.is_some_and(|limit| depth >= limit)
    }

    fn length_reached(&self, count: usize) -> bool {
        !self.readably && self.length.is_some_and(|limit| count >= limit)
    }

    pub(crate) fn escape(&self) -> bool {
        self.escape || self.readably
    }
}

pub(crate) fn initial(name: &str, upcase: Word) -> Option<Word> {
    match name {
        "*PRINT-BASE*" => Word::fixnum(10),
        "*PRINT-CASE*" => Some(upcase),
        "*PRINT-ESCAPE*" | "*PRINT-GENSYM*" | "*PRINT-ARRAY*" => Some(Word::T),
        "*PRINT-LENGTH*"
        | "*PRINT-LEVEL*"
        | "*PRINT-RADIX*"
        | "*PRINT-READABLY*"
        | "*PRINT-CIRCLE*"
        | "*PRINT-LINES*"
        | "*PRINT-MISER-WIDTH*"
        | "*PRINT-PPRINT-DISPATCH*"
        | "*PRINT-PRETTY*"
        | "*PRINT-RIGHT-MARGIN*" => Some(Word::NIL),
        _ => None,
    }
}

pub(crate) fn current_package(runtime: &Runtime, at: usize) -> Result<Package, EvalError> {
    if runtime.packages_initialized.get() {
        runtime.current_package_at(at)
    } else {
        Ok(Package::USER)
    }
}

pub(crate) struct UnreadableObject {
    pub(crate) object: Word,
    pub(crate) description: &'static str,
}

thread_local! {
    static PRINT_CALLBACK: RefCell<Option<(RootSlot, usize)>> = const { RefCell::new(None) };
}

pub(crate) fn uses_print_object(runtime: &Runtime, word: Word) -> bool {
    word.is_heap_object()
        && (runtime.heap.instance_class(word).is_some()
            || (runtime.heap.structure_layout(word).is_some()
                && crate::vm::printer::structure_has_print_method(runtime, word))
            || runtime.heap.condition_class(word).is_some()
            || runtime.heap.restart_name(word).is_some())
}

/// Writes to the stream supplied to PRINT-OBJECT share its labels. A write
/// to a different destination starts its own sharing pass.
pub(crate) fn output_scope(stream: Word) -> impl Drop {
    let stream = pprint::canonical_destination(stream);
    let separate = PRINT_CALLBACK.with_borrow(|method| {
        method
            .as_ref()
            .is_some_and(|(target, _)| target.get() != stream)
    });
    DropGuard::new(separate.then(|| SHARING_SCOPES.take()), |labels| {
        if let Some(labels) = labels {
            SHARING_SCOPES.set(labels);
        }
    })
}

/// A same-stream write from a method prints a component. An enclosing logical
/// block inside that method may already have descended to the component depth.
pub(crate) fn write_depth<'a>(
    runtime: &'a PrintContext<'_>,
    stream: Word,
) -> Option<impl Drop + use<'a>> {
    let stream = pprint::canonical_destination(stream);
    let depth = pprint::logical_block_depth(runtime);
    let descends = PRINT_CALLBACK.with_borrow(|method| {
        method
            .as_ref()
            .is_some_and(|(target, entry)| target.get() == stream && *entry == depth)
    });
    descends.then(|| pprint::bind_logical_block_depth(runtime, depth + 1))
}

pub(crate) fn with_method<T>(
    runtime: &PrintContext<'_>,
    stream: Word,
    depth: usize,
    f: impl FnOnce() -> T,
) -> T {
    let stream = pprint::canonical_destination(stream);
    let _restore = DropGuard::new(
        PRINT_CALLBACK.replace(Some((runtime.root_slot(stream), depth))),
        |previous| PRINT_CALLBACK.set(previous),
    );
    let _depth = (depth != pprint::logical_block_depth(runtime))
        .then(|| pprint::bind_logical_block_depth(runtime, depth));
    f()
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PrintDispatchMode {
    Dispatch(usize),
    DefaultMethod,
    Diagnostic,
    Preview,
}

impl PrintDispatchMode {
    pub(crate) fn current_depth(runtime: &Runtime) -> Self {
        Self::Dispatch(pprint::logical_block_depth(runtime))
    }

    fn calls_lisp(self) -> bool {
        matches!(self, Self::Dispatch(_) | Self::DefaultMethod)
    }

    fn atom_limit(self) -> usize {
        if self == Self::Preview {
            PREVIEW_CHARS + 1
        } else {
            usize::MAX
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SharingMarker {
    Definition(usize),
    Reference(usize),
}

struct SharingLabelEntry {
    object: RootSlot,
    number: usize,
    // True on a repeated visit during discovery, or after the label definition was emitted.
    marked: bool,
}

#[derive(Default)]
struct SharingLabelTable {
    detecting_sharing: bool,
    entries: Vec<SharingLabelEntry>,
    index_by_identity: HashMap<usize, Vec<usize>>,
}

impl SharingLabelTable {
    fn insert(&mut self, runtime: &Runtime, word: Word, number: usize) {
        let index = self.entries.len();
        let identity = Heap::word_identity_hash(word);
        self.entries.push(SharingLabelEntry {
            object: runtime.root_slot(word),
            number,
            marked: false,
        });
        self.index_by_identity
            .entry(identity)
            .or_default()
            .push(index);
    }

    fn find(&self, word: Word) -> Option<&SharingLabelEntry> {
        let identity = Heap::word_identity_hash(word);
        let indices = self.index_by_identity.get(&identity)?;
        indices
            .iter()
            .map(|&index| &self.entries[index])
            .find(|entry| entry.object.get() == word)
    }

    fn find_mut(&mut self, word: Word) -> Option<&mut SharingLabelEntry> {
        let identity = Heap::word_identity_hash(word);
        let indices = self.index_by_identity.get(&identity)?;
        let index = indices
            .iter()
            .copied()
            .find(|&index| self.entries[index].object.get() == word)?;
        Some(&mut self.entries[index])
    }
}

thread_local! {
    static SHARING_SCOPES: RefCell<Vec<SharingLabelTable>> = const { RefCell::new(Vec::new()) };
}

/// Logical blocks can outlive a dispatch slice while their Lisp bodies yield.
/// Their buffers and sharing labels move together with the task.
#[derive(Default)]
pub(crate) struct PrinterTaskState {
    sharing: Vec<SharingLabelTable>,
    logical_blocks: Vec<pprint::LogicalBlockBuffer>,
}

impl PrinterTaskState {
    pub(crate) fn activate(&mut self) -> impl Drop + use<'_> {
        self.swap_active_scopes();
        DropGuard::new(self, Self::swap_active_scopes)
    }

    fn swap_active_scopes(&mut self) {
        SHARING_SCOPES.with_borrow_mut(|active| std::mem::swap(active, &mut self.sharing));
        pprint::swap_logical_block_buffers(&mut self.logical_blocks);
    }
}

fn sharing_scope_guard(pushed: bool) -> impl Drop {
    DropGuard::new(pushed, |pushed| {
        if pushed {
            SHARING_SCOPES.with_borrow_mut(|stack| {
                stack.pop();
            });
        }
    })
}

/// Discover sharing by following what the methods actually print. Slots may be
/// hidden by a method, or it may print a computed or global object twice.
pub(crate) fn discover_shared_objects(
    runtime: &Runtime,
    options: &PrintControls,
    print_for_discovery: impl FnOnce() -> Result<(), Fault>,
) -> Result<impl Drop, Fault> {
    if !options.circle || SHARING_SCOPES.with_borrow(|stack| !stack.is_empty()) {
        return Ok(sharing_scope_guard(false));
    }
    let label_table = SharingLabelTable {
        detecting_sharing: true,
        ..SharingLabelTable::default()
    };
    SHARING_SCOPES.with_borrow_mut(|stack| stack.push(label_table));
    let scope = sharing_scope_guard(true);
    print_for_discovery()?;
    SHARING_SCOPES.with_borrow_mut(|stack| {
        let label_table = stack.last_mut().expect("active printer discovery");
        let mut labels = SharingLabelTable::default();
        for entry in &label_table.entries {
            if entry.marked {
                labels.insert(runtime, entry.object.get(), labels.entries.len() + 1);
            }
        }
        *label_table = labels;
    });
    Ok(scope)
}

/// Establish a sharing-detection scope for one printing operation.  Scopes
/// nest: an enclosing logical block owns the labels for everything printed
/// inside it.
pub(crate) fn sharing_scope(
    runtime: &Runtime,
    object: Word,
    options: &PrintControls,
) -> impl Drop + use<> {
    sharing_scope_guard(push_sharing_labels(runtime, object, options))
}

/// Push sharing-detection labels for OBJECT unless a scope is already active.
/// Returns whether a scope was pushed; the caller must call pop_sharing_labels.
pub(crate) fn push_sharing_labels(
    runtime: &Runtime,
    object: Word,
    options: &PrintControls,
) -> bool {
    if !options.circle || SHARING_SCOPES.with(|stack| !stack.borrow().is_empty()) {
        return false;
    }
    let mut occurrence_counts = HashMap::new();
    let mut expanded = HashSet::new();
    let mut first_seen_order = Vec::new();
    scan_printed_object(
        runtime,
        object,
        0,
        options,
        &mut occurrence_counts,
        &mut expanded,
        &mut first_seen_order,
    );
    let mut label_table = SharingLabelTable::default();
    let mut next_label_number = 1usize;
    for word in first_seen_order {
        if occurrence_counts.get(&word).copied().unwrap_or(0) > 1 {
            label_table.insert(runtime, word, next_label_number);
            next_label_number += 1;
        }
    }
    SHARING_SCOPES.with(|stack| stack.borrow_mut().push(label_table));
    true
}

/// Pop the innermost sharing-detection scope.
pub(crate) fn pop_sharing_labels() {
    SHARING_SCOPES.with(|stack| {
        stack.borrow_mut().pop();
    });
}

/// Check whether a list tail needs its own sharing marker. During discovery,
/// remember a new tail so a later visit can detect that it is shared.
pub(crate) fn list_tail_needs_label(
    runtime: &Runtime,
    options: &PrintControls,
    word: Word,
) -> bool {
    options.circle
        && SHARING_SCOPES.with_borrow_mut(|stack| {
            let Some(label_table) = stack.last_mut() else {
                return false;
            };
            if label_table.find(word).is_some() {
                return true;
            }
            if label_table.detecting_sharing {
                // A previously unseen cdr is printed by continuing along the list.
                label_table.insert(runtime, word, 0);
            }
            false
        })
}

/// Mark a visit and return the label definition or reference to print.
pub(crate) fn next_sharing_marker(
    runtime: &Runtime,
    options: &PrintControls,
    word: Word,
) -> Option<SharingMarker> {
    if !options.circle {
        return None;
    }
    SHARING_SCOPES.with(|stack| {
        let mut stack = stack.borrow_mut();
        let label_table = stack.last_mut()?;
        if label_table.detecting_sharing {
            if let Some(entry) = label_table.find_mut(word) {
                entry.marked = true;
                return Some(SharingMarker::Reference(0));
            }
            label_table.insert(runtime, word, 0);
            return None;
        }
        let entry = label_table.find_mut(word)?;
        if !entry.marked {
            entry.marked = true;
            Some(SharingMarker::Definition(entry.number))
        } else {
            Some(SharingMarker::Reference(entry.number))
        }
    })
}

fn record_occurrence(
    word: Word,
    occurrence_counts: &mut HashMap<Word, usize>,
    expanded: &mut HashSet<Word>,
    first_seen_order: &mut Vec<Word>,
) -> bool {
    let count = occurrence_counts.entry(word).or_insert(0);
    *count += 1;
    if *count > 1 {
        return false;
    }
    first_seen_order.push(word);
    expanded.insert(word);
    true
}

fn scan_printed_object(
    runtime: &Runtime,
    word: Word,
    depth: usize,
    options: &PrintControls,
    occurrence_counts: &mut HashMap<Word, usize>,
    expanded: &mut HashSet<Word>,
    first_seen_order: &mut Vec<Word>,
) {
    if let Some((car, cdr)) = runtime.heap.cons_fields(word) {
        if !options.readably && options.level.is_some_and(|limit| depth >= limit) {
            return;
        }
        if !record_occurrence(word, occurrence_counts, expanded, first_seen_order) {
            return;
        }
        scan_printed_object(
            runtime,
            car,
            depth + 1,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
        scan_list_tail(
            runtime,
            cdr,
            depth,
            1,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
        return;
    }
    if let Some((name, count)) = runtime.heap.structure_layout(word) {
        if !options.readably && options.level.is_some_and(|limit| depth >= limit) {
            return;
        }
        if !record_occurrence(word, occurrence_counts, expanded, first_seen_order) {
            return;
        }
        let layout_matches = runtime
            .structure_definition(name)
            .is_some_and(|definition| definition.slots.len() == count);
        if !layout_matches {
            return;
        }
        for index in 0..count {
            if !options.readably && options.length.is_some_and(|limit| index >= limit) {
                break;
            }
            if let Some(slot) = runtime.heap.structure_slot(word, index) {
                scan_printed_object(
                    runtime,
                    slot,
                    depth + 1,
                    options,
                    occurrence_counts,
                    expanded,
                    first_seen_order,
                );
            }
        }
        return;
    }
    if let Some(layout) = runtime.heap.instance_layout_of(word) {
        if !options.readably && options.level.is_some_and(|limit| depth >= limit) {
            return;
        }
        if !record_occurrence(word, occurrence_counts, expanded, first_seen_order) {
            return;
        }
        if let Some(entries) = runtime.heap.instance_layout_entries(layout) {
            for (index, (_, cell)) in entries.into_iter().enumerate() {
                let slot = match cell {
                    Some(cell) => runtime.heap.read_cell(cell),
                    None => runtime.heap.instance_slot(word, index),
                };
                if let Some(slot) = slot {
                    scan_printed_object(
                        runtime,
                        slot,
                        depth + 1,
                        options,
                        occurrence_counts,
                        expanded,
                        first_seen_order,
                    );
                }
            }
        }
        return;
    }
    if let Some(info) = runtime.heap.array_info(word) {
        if info.rank == 1 && info.element == ArrayElement::Character {
            return;
        }
        if !options.readably && !options.array {
            return;
        }
        if info.rank == 1 && info.element == ArrayElement::Bit {
            return;
        }
        if !options.readably && options.level.is_some_and(|limit| depth >= limit) {
            return;
        }
        if !record_occurrence(word, occurrence_counts, expanded, first_seen_order) {
            return;
        }
        if info.element != ArrayElement::General {
            return;
        }
        let mut dimensions = runtime
            .heap
            .array_dimensions(word)
            .expect("array dimensions");
        if let Some(fill) = info.fill_pointer {
            dimensions[0] = fill;
        }
        scan_array_dimension(
            runtime,
            word,
            depth,
            &dimensions,
            0,
            0,
            0,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
        return;
    }
    if runtime.heap.symbol_name(word).is_some() && runtime.heap.symbol_home(word).is_none() {
        record_occurrence(word, occurrence_counts, expanded, first_seen_order);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal printer scanner threads traversal state explicitly"
)]
fn scan_list_tail(
    runtime: &Runtime,
    word: Word,
    depth: usize,
    count: usize,
    options: &PrintControls,
    occurrence_counts: &mut HashMap<Word, usize>,
    expanded: &mut HashSet<Word>,
    first_seen_order: &mut Vec<Word>,
) {
    if let Some((car, cdr)) = runtime.heap.cons_fields(word) {
        if !options.readably && options.length.is_some_and(|limit| count >= limit) {
            return;
        }
        if !record_occurrence(word, occurrence_counts, expanded, first_seen_order) {
            return;
        }
        scan_printed_object(
            runtime,
            car,
            depth + 1,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
        scan_list_tail(
            runtime,
            cdr,
            depth,
            count + 1,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
    } else if word != Word::NIL {
        scan_printed_object(
            runtime,
            word,
            depth + 1,
            options,
            occurrence_counts,
            expanded,
            first_seen_order,
        );
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal printer scanner threads array traversal state explicitly"
)]
fn scan_array_dimension(
    runtime: &Runtime,
    word: Word,
    depth: usize,
    dimensions: &[usize],
    axis: usize,
    index: usize,
    offset: usize,
    options: &PrintControls,
    occurrence_counts: &mut HashMap<Word, usize>,
    expanded: &mut HashSet<Word>,
    first_seen_order: &mut Vec<Word>,
) {
    let Some(&length) = dimensions.get(axis) else {
        if let Some(element) = runtime.heap.array_value(word, offset) {
            scan_printed_object(
                runtime,
                element.expect_word(),
                depth,
                options,
                occurrence_counts,
                expanded,
                first_seen_order,
            );
        }
        return;
    };
    if index >= length {
        return;
    }
    if !options.readably && options.length.is_some_and(|limit| index >= limit) {
        return;
    }
    let stride = dimensions[axis + 1..]
        .iter()
        .fold(1usize, |product, dimension| {
            product.saturating_mul(*dimension)
        });
    scan_array_dimension(
        runtime,
        word,
        depth + 1,
        dimensions,
        axis + 1,
        0,
        offset + index * stride,
        options,
        occurrence_counts,
        expanded,
        first_seen_order,
    );
    scan_array_dimension(
        runtime,
        word,
        depth,
        dimensions,
        axis,
        index + 1,
        offset,
        options,
        occurrence_counts,
        expanded,
        first_seen_order,
    );
}

fn print_cycle(
    word: Word,
    description: &'static str,
    options: &PrintControls,
    text: &mut String,
) -> Result<(), UnreadableObject> {
    if options.readably {
        return Err(UnreadableObject {
            object: word,
            description,
        });
    }
    text.push_str("#<");
    text.push_str(description);
    text.push('>');
    Ok(())
}

fn print_atom(
    runtime: &Runtime,
    package: Package,
    value: Word,
    options: &PrintControls,
    text: &mut String,
    mode: PrintDispatchMode,
    depth: usize,
) -> Result<(), UnreadableObject> {
    if let Some(integer) = value.as_fixnum() {
        print_integer(runtime, integer, options, text);
        return Ok(());
    }
    if mode == PrintDispatchMode::Preview
        && let Some(kind) = oversized_preview_number(&runtime.heap, value)
    {
        write!(text, "#<{kind} …>").expect("writing to String");
        return Ok(());
    }
    if let Some(number) = runtime.heap.number(value) {
        print_number(runtime, value, &number, options, text)?;
        return Ok(());
    }
    if let Some(character) = value.as_character() {
        if options.escape() {
            text.push_str("#\\");
            if character.is_control() {
                let name = crate::character::name(character);
                text.push_str(&name);
                return Ok(());
            }
        }
        text.push(character);
        return Ok(());
    }
    if let Some(name) = runtime.heap.symbol_name(value) {
        print_symbol(runtime, package, value, &name, options, text);
        return Ok(());
    }
    if runtime
        .heap
        .with_string_slice(value, |string| {
            print_string(string, options.escape(), text, mode)
        })
        .is_some()
    {
        return Ok(());
    }
    if let Some(pathname) = pathname::from_word(runtime, value) {
        let namestring = pathname.namestring();
        if let Some(name) = namestring
            && (!options.readably
                || pathname::default_pathname(runtime, 0).is_ok_and(|defaults| {
                    pathname::parse_namestring(
                        runtime,
                        &name,
                        None,
                        defaults.logical_host.as_deref(),
                    )
                    .is_ok_and(|parsed| parsed == pathname)
                }))
        {
            if options.escape() {
                text.push_str("#P");
            }
            print_string(&name, options.escape(), text, mode);
            return Ok(());
        }
        if options.readably {
            return Err(UnreadableObject {
                object: value,
                description: "PATHNAME",
            });
        }
        text.push_str("#<PATHNAME>");
        return Ok(());
    }

    if let Some(state) = runtime.heap.random_state_value(value) {
        text.push_str("#Y\"");
        for word in state.words() {
            write!(text, "{word:016X}").unwrap();
        }
        text.push('"');
        return Ok(());
    }
    if let Some(len) = runtime.heap.treelist_len(value) {
        if mode == PrintDispatchMode::Preview && options.level_reached(depth) {
            text.push('#');
            return Ok(());
        }
        let is_mut = runtime.heap.is_mutable_treelist(value);
        let desc = if is_mut {
            "ZK-TREELIST:MUTABLE-TREELIST"
        } else {
            "ZK-TREELIST:TREELIST"
        };
        if options.readably {
            return print_cycle(value, desc, options, text);
        }
        write!(text, "#<{desc} ({len})").unwrap();
        let limit = options.length.unwrap_or(8);
        runtime
            .heap
            .treelist_for_each_prefix(value, len.min(limit), |elem| {
                if mode == PrintDispatchMode::Preview
                    && text.len() > PREVIEW_CHARS
                    && text.chars().nth(PREVIEW_CHARS).is_some()
                {
                    return;
                }
                text.push(' ');
                if print_atom(runtime, package, elem, options, text, mode, depth + 1).is_err() {
                    text.push_str("#<UNREADABLE>");
                }
            });
        if len > limit {
            text.push_str(" ...");
        }
        text.push('>');
        return Ok(());
    }
    if let Some(pack) = runtime.heap.simd_pack(value) {
        if options.readably {
            return print_cycle(value, "ZK-SIMD:PACK", options, text);
        }
        write!(text, "#<ZK-SIMD:{}", pack.shape().name()).unwrap();
        for number in crate::vm::simd::printable_lanes(&pack) {
            text.push(' ');
            print_number(runtime, value, &Number::Real(number), options, text)?;
        }
        text.push('>');
        return Ok(());
    }
    if let Some(mask) = runtime.heap.simd_mask(value) {
        if options.readably {
            return print_cycle(value, "ZK-SIMD:MASK", options, text);
        }
        write!(text, "#<ZK-SIMD:MASK {} #x{:X}>", mask.lanes(), mask.bits()).unwrap();
        return Ok(());
    }

    if let Some(class) = runtime
        .heap
        .instance_class(value)
        .and_then(|class| runtime.heap.class_info(class))
    {
        if options.readably {
            return Err(UnreadableObject {
                object: value,
                description: "STANDARD-OBJECT",
            });
        }
        let name = runtime
            .heap
            .symbol_name(class.name)
            .expect("class name symbol");
        text.push_str("#<");
        print_symbol(runtime, package, class.name, &name, options, text);
        if let Some(info) = runtime.heap.class_info(value)
            && info.name != Word::NIL
        {
            let name = runtime
                .heap
                .symbol_name(info.name)
                .expect("class name symbol");
            text.push(' ');
            print_symbol(runtime, package, info.name, &name, options, text);
        }
        text.push('>');
        return Ok(());
    }
    if let Some(name) = crate::condition::instance_name(runtime, value) {
        if options.readably {
            return Err(UnreadableObject {
                object: value,
                description: "condition",
            });
        }
        if !options.escape()
            && let Some(report) = crate::condition::constant_report(runtime, value)
        {
            text.push_str(&report);
        } else {
            text.push_str("#<");
            if name == Word::NIL {
                text.push_str("CONDITION");
            } else {
                text.push_str(
                    &runtime
                        .heap
                        .symbol_name(name)
                        .expect("condition class name"),
                );
            }
            text.push('>');
        }
        return Ok(());
    }
    let class = runtime.heap.class_info(value);
    let description = if let Some(class) = class {
        class.kind.name()
    } else if runtime.heap.generic_info(value).is_some() {
        "STANDARD-GENERIC-FUNCTION"
    } else if runtime.heap.method_info(value).is_some() {
        "STANDARD-METHOD"
    } else if runtime.heap.eql_specializer_object(value).is_some() {
        "EQL-SPECIALIZER"
    } else if let Some(slot) = runtime.heap.slot_definition_info(value) {
        if slot.direct {
            "STANDARD-DIRECT-SLOT-DEFINITION"
        } else {
            "STANDARD-EFFECTIVE-SLOT-DEFINITION"
        }
    } else if Package::from_word(value).is_some() {
        "PACKAGE"
    } else if runtime.heap.hash_table_info(value).is_some() {
        "HASH-TABLE"
    } else if runtime.heap.is_weak_pointer(value) {
        "WEAK-POINTER"
    } else if runtime.heap.readtable_case(value).is_some() {
        "READTABLE"
    } else if let Some(stream) = runtime.heap.stream_info(value) {
        match stream.kind {
            crate::heap::StreamKind::StringInput => "STRING-INPUT-STREAM",
            crate::heap::StreamKind::StringOutput | crate::heap::StreamKind::StringOutputTo => {
                "STRING-OUTPUT-STREAM"
            }
            _ => "STREAM",
        }
    } else if runtime.heap.restart_name(value).is_some() {
        "RESTART"
    } else if let Some(class) = runtime.heap.condition_class(value) {
        class.name()
    } else if value.primitive_index().is_some() {
        "BUILTIN-FUNCTION"
    } else if runtime.heap.foreign_id(value).is_some() {
        "FOREIGN-OBJECT"
    } else if value.native_index().is_some() || runtime.heap.is_host_function(value) {
        "NATIVE-FUNCTION"
    } else if runtime.heap.function_code(value).is_some() {
        "FUNCTION"
    } else {
        "OBJECT"
    };
    if options.readably {
        return Err(UnreadableObject {
            object: value,
            description,
        });
    }
    text.push_str("#<");
    text.push_str(description);
    if let Some(class) = class {
        let name = runtime
            .heap
            .symbol_name(class.name)
            .expect("class name symbol");
        text.push(' ');
        print_symbol(runtime, package, class.name, &name, options, text);
    } else if let Some(package) = Package::from_word(value) {
        if let Some(name) = runtime.packages.borrow().name(package) {
            text.push(' ');
            print_string(name, true, text, mode);
        } else {
            text.push_str(" DELETED");
        }
    } else if let Some(name) = runtime.heap.restart_name(value)
        && name != Word::NIL
        && let Some(spelling) = runtime.heap.symbol_name(name)
    {
        text.push(' ');
        print_symbol(runtime, package, name, &spelling, options, text);
    }
    text.push('>');
    Ok(())
}

fn print_string(value: &str, escape: bool, text: &mut String, mode: PrintDispatchMode) {
    if !escape {
        let end = if mode == PrintDispatchMode::Preview {
            value
                .char_indices()
                .nth(mode.atom_limit())
                .map_or(value.len(), |(idx, _)| idx)
        } else {
            value.len()
        };
        text.push_str(&value[..end]);
        return;
    }
    text.push('"');
    for ch in value.chars().take(mode.atom_limit()) {
        if matches!(ch, '"' | '\\') {
            text.push('\\');
        }
        text.push(ch);
    }
    text.push('"');
}

fn oversized_preview_number(heap: &Heap, word: Word) -> Option<&'static str> {
    // Convert moderately large values so a clipped preview still shows their
    // leading digits. Avoid copying or decimal conversion beyond 1024 bits.
    const MAX_BIGNUM_DIGITS: usize = 32;
    let oversized_component = |(left, right)| {
        oversized_preview_number(heap, left).is_some()
            || oversized_preview_number(heap, right).is_some()
    };
    match heap.number_kind(word)? {
        NumberKind::Bignum => {
            (heap.bignum_info(word)?.digits > MAX_BIGNUM_DIGITS).then_some("BIGNUM")
        }
        NumberKind::Ratio => oversized_component(heap.ratio_fields(word)?).then_some("RATIO"),
        NumberKind::Complex => oversized_component(heap.complex_fields(word)?).then_some("COMPLEX"),
        _ => None,
    }
}

fn print_integer(runtime: &Runtime, value: isize, options: &PrintControls, text: &mut String) {
    let base = options.base;
    let radix = print_radix(runtime, options);
    if base == 10 {
        write!(text, "{value}").expect("writing to String");
        if radix {
            text.push('.');
        }
        return;
    }
    if radix {
        radix_prefix(base, text);
    }
    if value < 0 {
        text.push('-');
    }
    let mut number = value.unsigned_abs();
    let mut digits = [0_u8; usize::BITS as usize];
    let mut first_digit = digits.len();
    loop {
        let digit = number % base as usize;
        first_digit -= 1;
        digits[first_digit] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"[digit];
        number /= base as usize;
        if number == 0 {
            break;
        }
    }
    text.push_str(std::str::from_utf8(&digits[first_digit..]).expect("ASCII integer digits"));
}

fn read_base(runtime: &Runtime) -> u32 {
    reader_setting(runtime, "*READ-BASE*")
        .and_then(Word::as_fixnum)
        .and_then(|base| u32::try_from(base).ok())
        .filter(|base| (2..=36).contains(base))
        .unwrap_or(10)
}

fn reader_setting(runtime: &Runtime, name: &str) -> Option<Word> {
    let symbol = runtime
        .packages
        .borrow()
        .present(Package::COMMON_LISP, name)?
        .0;
    runtime.heap.symbol_cell(symbol, false)
}

fn print_radix(runtime: &Runtime, options: &PrintControls) -> bool {
    options.radix || (options.readably && options.base != read_base(runtime))
}

fn radix_prefix(base: u32, text: &mut String) {
    match base {
        2 => text.push_str("#b"),
        8 => text.push_str("#o"),
        16 => text.push_str("#x"),
        _ => write!(text, "#{base}r").expect("writing to String"),
    }
}

fn print_number(
    runtime: &Runtime,
    word: Word,
    number: &Number,
    options: &PrintControls,
    text: &mut String,
) -> Result<(), UnreadableObject> {
    let default_double = || {
        reader_setting(runtime, "*READ-DEFAULT-FLOAT-FORMAT*")
            .and_then(|value| runtime.heap.symbol_name(value))
            .is_some_and(|name| matches!(name.as_str(), "DOUBLE-FLOAT" | "LONG-FLOAT"))
    };
    let real = |value: &Real, text: &mut String| match value {
        Real::Integer(value) => {
            let radix = print_radix(runtime, options);
            if radix && options.base != 10 {
                radix_prefix(options.base, text);
            }
            text.push_str(&value.to_str_radix(options.base).to_ascii_uppercase());
            if radix && options.base == 10 {
                text.push('.');
            }
            true
        }
        Real::Ratio(value) => {
            if print_radix(runtime, options) {
                radix_prefix(options.base, text);
            }
            text.push_str(
                &value
                    .numer()
                    .to_str_radix(options.base)
                    .to_ascii_uppercase(),
            );
            text.push('/');
            text.push_str(
                &value
                    .denom()
                    .to_str_radix(options.base)
                    .to_ascii_uppercase(),
            );
            true
        }
        Real::Single(value) => {
            if !value.is_finite() {
                return false;
            }
            let marker = (options.readably || default_double()).then_some('f');
            print_float(*value, f64::from(value.abs()), marker, text);
            true
        }
        Real::Double(value) => {
            if !value.is_finite() {
                return false;
            }
            let marker = (options.readably || !default_double()).then_some('d');
            print_float(*value, value.abs(), marker, text);
            true
        }
    };
    let start = text.len();
    let finite = match number {
        Number::Real(value) => real(value, text),
        Number::Complex(re, im) => {
            text.push_str("#C(");
            let real_finite = real(re, text);
            text.push(' ');
            let imag_finite = real(im, text);
            text.push(')');
            real_finite && imag_finite
        }
    };
    if !finite {
        text.truncate(start);
        if options.readably {
            return Err(UnreadableObject {
                object: word,
                description: "non-finite number",
            });
        }
        text.push_str("#<NON-FINITE-NUMBER>");
    }
    Ok(())
}

fn print_float(
    value: impl std::fmt::Display + std::fmt::LowerExp,
    magnitude: f64,
    marker: Option<char>,
    text: &mut String,
) {
    if magnitude == 0.0 || (1e-3..1e7).contains(&magnitude) {
        let start = text.len();
        write!(text, "{value}").expect("writing to String");
        if !text[start..].contains('.') {
            text.push_str(".0");
        }
        if let Some(marker) = marker {
            text.push(marker);
            text.push('0');
        }
    } else {
        let scientific = format!("{value:e}");
        let (fraction, exponent) = scientific.split_once('e').expect("scientific notation");
        text.push_str(fraction);
        if !fraction.contains('.') {
            text.push_str(".0");
        }
        text.push(marker.unwrap_or('e'));
        text.push_str(exponent);
    }
}

fn print_symbol(
    runtime: &Runtime,
    current: Package,
    value: Word,
    name: &str,
    options: &PrintControls,
    text: &mut String,
) {
    if options.escape() {
        let registry = runtime.packages.borrow();
        match runtime.heap.symbol_home(value) {
            Some(Package::KEYWORD) => text.push(':'),
            Some(home)
                if !registry
                    .find(current, name)
                    .is_some_and(|(symbol, _)| symbol == value)
                    && runtime.packages_initialized.get() =>
            {
                let package_name = registry
                    .prefix_for(current, home)
                    .expect("symbol's live home package");
                print_name(runtime, package_name, options, text);
                if matches!(
                    registry.present(home, name),
                    Some((_, SymbolStatus::External))
                ) {
                    text.push(':');
                } else {
                    text.push_str("::");
                }
            }
            None if options.gensym || options.readably => text.push_str("#:"),
            _ => {}
        }
    }
    print_name(runtime, name, options, text);
}

fn print_name(runtime: &Runtime, name: &str, options: &PrintControls, text: &mut String) {
    let read_case = reader_setting(runtime, "*READTABLE*")
        .and_then(|table| runtime.heap.readtable_case(table))
        .unwrap_or(ReadtableCase::Upcase);
    if options.escape()
        && (!bare_name(name, options.base)
            || (options.readably && !bare_name(name, read_base(runtime)))
            || (options.readably || matches!(read_case, ReadtableCase::Upcase))
                && name.bytes().any(|ch| ch.is_ascii_lowercase())
            || matches!(read_case, ReadtableCase::Downcase)
                && name.bytes().any(|ch| ch.is_ascii_uppercase()))
    {
        text.push('|');
        for ch in name.chars() {
            if matches!(ch, '|' | '\\')
                || (ch.is_ascii_control() && !matches!(ch, '\t' | '\n' | '\r' | '\x0c'))
            {
                text.push('\\');
            }
            text.push(ch);
        }
        text.push('|');
        return;
    }
    match read_case {
        ReadtableCase::Preserve => {
            text.push_str(name);
            return;
        }
        ReadtableCase::Invert => {
            let fold = ReadtableCase::inverted(name.chars());
            text.extend(name.chars().map(|ch| fold.fold(ch)));
            return;
        }
        _ => {}
    }
    let mut start = true;
    for ch in name.chars() {
        let changes_case = match read_case {
            ReadtableCase::Upcase => ch.is_ascii_uppercase(),
            ReadtableCase::Downcase => ch.is_ascii_lowercase(),
            _ => unreachable!("preserve and invert are handled above"),
        };
        text.push(match (changes_case, options.case) {
            (true, PrintCase::Upcase) => ch.to_ascii_uppercase(),
            (true, PrintCase::Downcase) => ch.to_ascii_lowercase(),
            (true, PrintCase::Capitalize) if start => ch.to_ascii_uppercase(),
            (true, PrintCase::Capitalize) => ch.to_ascii_lowercase(),
            (false, _) => ch,
        });
        start = !ch.is_alphanumeric();
    }
}

fn bare_name(name: &str, base: u32) -> bool {
    !name.is_empty()
        && !name.starts_with('#')
        && !name.bytes().all(|byte| byte == b'.')
        && !crate::read::numeric_or_reserved(name, base)
        && name.chars().all(crate::symbol::is_symbol_constituent)
}
