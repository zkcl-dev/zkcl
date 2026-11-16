//! Explicit traversal stack for rendering aggregate objects across Lisp calls.
use std::{collections::HashSet, fmt::Write as _, rc::Rc};

use super::{
    PREVIEW_CHARS, PrintControls, PrintDispatchMode, SharingMarker, UnreadableObject,
    next_sharing_marker, print_atom, print_cycle, print_name, print_number, print_symbol,
    sharing_scope, uses_print_object,
};
use crate::EvalError;
use crate::condition::Fault;
use crate::heap::{ArrayElement, ArrayValue, Heap, RootSlot, Word};
use crate::number::{Number, Real};
use crate::package::Package;
use crate::vm::{printer::PrintContext, structures::StructureDefinition};

enum PrintTask {
    Object {
        word: Word,
        depth: usize,
    },
    ListTail {
        word: Word,
        depth: usize,
        count: usize,
        label_checked: bool,
    },
    StructureSlot {
        word: Word,
        depth: usize,
        index: usize,
        definition: Rc<StructureDefinition>,
    },
    ArrayDimension {
        word: Word,
        depth: usize,
        dimensions: Rc<[(usize, usize)]>,
        axis: usize,
        index: usize,
        offset: usize,
    },
    LeaveObject(Word),
    CloseParen {
        path_start: usize,
    },
}

impl PrintTask {
    fn operand_mut(&mut self) -> Option<&mut Word> {
        match self {
            Self::Object { word, .. }
            | Self::ListTail { word, .. }
            | Self::StructureSlot { word, .. }
            | Self::ArrayDimension { word, .. }
            | Self::LeaveObject(word) => Some(word),
            Self::CloseParen { .. } => None,
        }
    }
}

/// Save only the explicit traversal operands while Lisp can allocate. The
/// cycle path uses stable identities and needs no updating after collection.
fn with_rooted_print_tasks<T>(
    runtime: &crate::Runtime,
    pending_tasks: &mut [PrintTask],
    call: impl FnOnce() -> Result<T, Fault>,
) -> Result<T, Fault> {
    let roots = runtime.root_frame(pending_tasks.len());
    for (index, task) in pending_tasks.iter_mut().enumerate() {
        if let Some(word) = task.operand_mut() {
            roots.set(index, *word);
        }
    }
    let result = call()?;
    for (index, task) in pending_tasks.iter_mut().enumerate() {
        if let Some(word) = task.operand_mut() {
            *word = roots.get(index);
        }
    }
    Ok(result)
}

/// STREAM receives the accumulated prefix before each method call. The
/// caller writes the remaining suffix. Host diagnostics can render without
/// calling Lisp when reporting an error during dispatch itself.
pub(crate) fn render(
    runtime: &PrintContext<'_>,
    package: Package,
    object: Word,
    options: &PrintControls,
    stream: Option<&RootSlot>,
    mode: PrintDispatchMode,
    at: usize,
) -> Result<String, Fault> {
    let depth = match mode {
        PrintDispatchMode::Dispatch(depth) => depth,
        PrintDispatchMode::DefaultMethod => crate::vm::format::pprint::logical_block_depth(runtime),
        PrintDispatchMode::Diagnostic | PrintDispatchMode::Preview => 0,
    };
    let (object, dispatch_table) = if options.pretty && mode.calls_lisp() {
        let object = runtime.root_slot(object);
        let table = crate::vm::format::pprint::active_dispatch_table(runtime, None, at)?;
        (object.get(), table)
    } else {
        (object, None)
    };
    let _sharing = sharing_scope(runtime, object, options);
    let mut traversal = PrintTraversal {
        runtime,
        package,
        options,
        stream,
        mode,
        at,
        dispatch_table,
        text: String::new(),
        tasks: Vec::new(),
        active: HashSet::new(),
        open_aggregates: Vec::new(),
    };
    traversal.object(object, depth, true)?;
    loop {
        if mode == PrintDispatchMode::Preview
            && traversal.text.len() > PREVIEW_CHARS
            && let Some((byte, _)) = traversal.text.char_indices().nth(PREVIEW_CHARS)
        {
            traversal.text.truncate(byte);
            traversal.text.push('…');
            break;
        }
        let Some(task) = traversal.tasks.pop() else {
            break;
        };
        match task {
            PrintTask::Object { word, depth } => traversal.object(word, depth, false)?,
            PrintTask::ListTail {
                word,
                depth,
                count,
                label_checked,
            } => traversal.list_tail(word, depth, count, label_checked)?,
            PrintTask::StructureSlot {
                word,
                depth,
                index,
                definition,
            } => traversal.structure_slot(word, depth, index, definition),
            PrintTask::ArrayDimension {
                word,
                depth,
                dimensions,
                axis,
                index,
                offset,
            } => traversal.array_dimension(word, depth, dimensions, axis, index, offset)?,
            PrintTask::LeaveObject(word) => {
                traversal.active.remove(&Heap::word_identity_hash(word));
            }
            PrintTask::CloseParen { path_start } => traversal.close_paren(path_start),
        }
    }
    Ok(traversal.text)
}

struct PrintTraversal<'a, 'rt> {
    runtime: &'a PrintContext<'rt>,
    package: Package,
    options: &'a PrintControls,
    stream: Option<&'a RootSlot>,
    mode: PrintDispatchMode,
    at: usize,
    dispatch_table: Option<RootSlot>,
    text: String,
    tasks: Vec<PrintTask>,
    /// Stable identities on the current path, including array objects.
    active: HashSet<usize>,
    /// Identities to remove when closing the enclosing list or structure.
    open_aggregates: Vec<usize>,
}

impl PrintTraversal<'_, '_> {
    fn unreadable(&self, error: UnreadableObject) -> Fault {
        Fault::print_not_readable(self.runtime, 
            error.object,
            EvalError::new(
                self.at,
                format!("cannot print {} readably", error.description),
            ),
        )
    }

    fn cycle(&mut self, word: Word, description: &'static str) -> Result<(), Fault> {
        print_cycle(word, description, self.options, &mut self.text).map_err(|e| self.unreadable(e))
    }

    /// A reference completes the object here. A definition labels the object
    /// whose contents will immediately follow.
    fn write_sharing_reference(&mut self, word: Word) -> bool {
        match next_sharing_marker(self.runtime, self.options, word) {
            Some(SharingMarker::Reference(id)) => {
                write!(self.text, "#{id}#").expect("writing to String");
                true
            }
            Some(SharingMarker::Definition(id)) => {
                write!(self.text, "#{id}=").expect("writing to String");
                false
            }
            None => false,
        }
    }

    fn truncate_depth(&mut self, depth: usize) -> bool {
        let reached = self.options.level_reached(depth);
        if reached {
            self.text.push('#');
        }
        reached
    }

    fn object(&mut self, mut word: Word, depth: usize, root: bool) -> Result<(), Fault> {
        if !root && let Some(table) = &self.dispatch_table {
            let object = self.runtime.root_slot(word);
            let dispatched = with_rooted_print_tasks(self.runtime, &mut self.tasks, || {
                crate::vm::format::pprint::dispatch_into(
                    self.runtime,
                    object.get(),
                    table,
                    self.options,
                    self.stream,
                    &mut self.text,
                    depth,
                    self.at,
                )
            })?;
            if dispatched {
                return Ok(());
            }
            word = object.get();
        }
        let default_method = root && self.mode == PrintDispatchMode::DefaultMethod;
        if self.mode.calls_lisp() && !default_method && uses_print_object(self.runtime, word) {
            if self.truncate_depth(depth) || self.write_sharing_reference(word) {
                return Ok(());
            }
            return with_rooted_print_tasks(self.runtime, &mut self.tasks, || {
                crate::vm::printer::call_print_object(
                    self.runtime,
                    word,
                    self.stream,
                    depth,
                    self.options.start_column,
                    &mut self.text,
                    self.at,
                )
            });
        }
        if self.runtime.heap.cons_fields(word).is_some() {
            if self.truncate_depth(depth) || self.write_sharing_reference(word) {
                return Ok(());
            }
            if self.active.contains(&Heap::word_identity_hash(word)) {
                return self.cycle(word, "CIRCULAR-CONS");
            }
            self.start_list(word, depth);
        } else if let Some((name, count)) = self.runtime.heap.structure_layout(word) {
            self.structure(word, depth, name, count, default_method)?;
        } else if let Some(info) = self.runtime.heap.array_info(word)
            && !(info.rank == 1 && info.element == ArrayElement::Character)
        {
            if !self.options.readably && !self.options.array {
                self.text.push_str("#<ARRAY>");
            } else if info.rank == 1 && info.element == ArrayElement::Bit {
                self.bit_vector(word, info.fill_pointer.unwrap_or(info.size));
            } else {
                self.array(word, depth, info.rank, info.element, info.fill_pointer)?;
            }
        } else {
            if !default_method
                && self.options.circle
                && (self.runtime.heap.is_string(word)
                    || (self.runtime.heap.is_symbol(word)
                        && self.runtime.heap.symbol_home(word).is_none()
                        && word != Word::T
                        && word != Word::NIL))
                && self.write_sharing_reference(word)
            {
                return Ok(());
            }
            print_atom(
                self.runtime,
                self.package,
                word,
                self.options,
                &mut self.text,
                self.mode,
                depth,
            )
            .map_err(|e| self.unreadable(e))?;
        }
        Ok(())
    }

    fn start_list(&mut self, word: Word, depth: usize) {
        self.text.push('(');
        self.tasks.push(PrintTask::CloseParen {
            path_start: self.open_aggregates.len(),
        });
        self.tasks.push(PrintTask::ListTail {
            word,
            depth,
            count: 0,
            label_checked: true,
        });
    }

    fn close_paren(&mut self, path_start: usize) {
        for identity in self.open_aggregates.drain(path_start..) {
            self.active.remove(&identity);
        }
        self.text.push(')');
    }

    fn list_tail(
        &mut self,
        word: Word,
        depth: usize,
        count: usize,
        label_checked: bool,
    ) -> Result<(), Fault> {
        let Some((car, cdr)) = self.runtime.heap.cons_fields(word) else {
            if word != Word::NIL {
                self.text.push_str(" . ");
                self.tasks.push(PrintTask::Object {
                    word,
                    depth: depth + 1,
                });
            }
            return Ok(());
        };
        if self.options.length_reached(count) {
            if count > 0 {
                self.text.push(' ');
            }
            self.text.push_str("...");
            return Ok(());
        }
        if !label_checked && let Some(marker) = next_sharing_marker(self.runtime, self.options, word) {
            self.text.push_str(" . ");
            match marker {
                SharingMarker::Reference(id) => {
                    write!(self.text, "#{id}#").expect("writing to String")
                }
                SharingMarker::Definition(id) => {
                    write!(self.text, "#{id}=").expect("writing to String");
                    self.start_list(word, depth);
                }
            }
            return Ok(());
        }
        let identity = Heap::word_identity_hash(word);
        if !self.active.insert(identity) {
            self.text.push_str(" . ");
            return self.cycle(word, "CIRCULAR-CONS");
        }
        if count > 0 {
            self.text.push(' ');
        }
        self.open_aggregates.push(identity);
        self.tasks.push(PrintTask::ListTail {
            word: cdr,
            depth,
            count: count + 1,
            label_checked: false,
        });
        self.tasks.push(PrintTask::Object {
            word: car,
            depth: depth + 1,
        });
        Ok(())
    }

    fn structure(
        &mut self,
        word: Word,
        depth: usize,
        name: Word,
        count: usize,
        default_method: bool,
    ) -> Result<(), Fault> {
        if self.truncate_depth(depth) || !default_method && self.write_sharing_reference(word) {
            return Ok(());
        }
        if !self.active.insert(Heap::word_identity_hash(word)) {
            return self.cycle(word, "CIRCULAR-STRUCTURE");
        }
        let definition = self
            .runtime
            .structure_definition(name)
            .filter(|definition| definition.slots.len() == count);
        let spelling = self.runtime.heap.symbol_name(name).expect("structure name");
        let readable = definition.as_ref().is_some_and(|definition| {
            definition.keyword_constructor.get() != Word::NIL
                && self.runtime.heap.symbol_home(name).is_some()
        });
        if self.options.readably && !readable {
            return Err(self.unreadable(UnreadableObject {
                object: word,
                description: "STRUCTURE-OBJECT",
            }));
        }
        if let Some(definition) = definition {
            self.text.push_str("#S(");
            // The structure name is a symbol inside the #S form and
            // must stay package-qualified even when *PRINT-ESCAPE*
            // is false, so that the printed form can be read back
            // from a package that does not contain the name.
            let mut name_options = self.options.clone();
            name_options.escape = true;
            print_symbol(
                self.runtime,
                self.package,
                name,
                &spelling,
                &name_options,
                &mut self.text,
            );
            self.tasks.push(PrintTask::CloseParen {
                path_start: self.open_aggregates.len(),
            });
            self.open_aggregates.push(Heap::word_identity_hash(word));
            self.tasks.push(PrintTask::StructureSlot {
                word,
                depth,
                index: 0,
                definition,
            });
        } else {
            self.active.remove(&Heap::word_identity_hash(word));
            self.text.push_str("#<STRUCTURE ");
            print_symbol(
                self.runtime,
                self.package,
                name,
                &spelling,
                self.options,
                &mut self.text,
            );
            self.text.push('>');
        }
        Ok(())
    }

    fn structure_slot(
        &mut self,
        word: Word,
        depth: usize,
        index: usize,
        definition: Rc<StructureDefinition>,
    ) {
        let Some(slot) = definition.slots.get(index) else {
            return;
        };
        if self.options.length_reached(index) {
            self.text.push_str(" ...");
            return;
        }
        let name = self
            .runtime
            .heap
            .symbol_name(slot.name.get())
            .expect("slot name");
        self.text.push_str(" :");
        print_name(self.runtime, &name, self.options, &mut self.text);
        self.text.push(' ');
        self.tasks.push(PrintTask::StructureSlot {
            word,
            depth,
            index: index + 1,
            definition,
        });
        self.tasks.push(PrintTask::Object {
            word: self
                .runtime
                .heap
                .structure_slot(word, index)
                .expect("structure slot"),
            depth: depth + 1,
        });
    }

    fn bit_vector(&mut self, word: Word, length: usize) {
        self.text.push_str("#*");
        for index in 0..length.min(self.mode.atom_limit()) {
            let bit = self
                .runtime
                .heap
                .array_value(word, index)
                .expect("valid bit slot")
                .expect_word();
            self.text
                .push(if bit.as_fixnum() == Some(0) { '0' } else { '1' });
        }
    }

    fn array(
        &mut self,
        word: Word,
        depth: usize,
        rank: usize,
        element: ArrayElement,
        fill_pointer: Option<usize>,
    ) -> Result<(), Fault> {
        if self.truncate_depth(depth) || self.write_sharing_reference(word) {
            return Ok(());
        }
        if !self.active.insert(Heap::word_identity_hash(word)) {
            return self.cycle(word, "CIRCULAR-ARRAY");
        }
        let mut dimensions = self
            .runtime
            .heap
            .array_dimensions(word)
            .expect("array dimensions");
        if let Some(fill) = fill_pointer {
            dimensions[0] = fill;
        }
        if self.options.readably
            && (element != ArrayElement::General
                || dimensions
                    .iter()
                    .position(|&n| n == 0)
                    .is_some_and(|zero| dimensions[zero + 1..].iter().any(|&n| n != 0)))
        {
            return Err(self.unreadable(UnreadableObject {
                object: word,
                description: "ARRAY",
            }));
        }
        if rank == 1 {
            self.text.push('#');
        } else {
            write!(self.text, "#{}A", rank).expect("writing to String");
        }
        let mut stride = 1usize;
        let mut strides = Vec::with_capacity(dimensions.len());
        for dimension in dimensions.into_iter().rev() {
            strides.push((dimension, stride));
            stride = stride.saturating_mul(dimension);
        }
        strides.reverse();
        self.tasks.push(PrintTask::LeaveObject(word));
        self.tasks.push(PrintTask::ArrayDimension {
            word,
            depth,
            dimensions: strides.into(),
            axis: 0,
            index: 0,
            offset: 0,
        });
        Ok(())
    }

    fn array_dimension(
        &mut self,
        word: Word,
        depth: usize,
        dimensions: Rc<[(usize, usize)]>,
        axis: usize,
        index: usize,
        offset: usize,
    ) -> Result<(), Fault> {
        let Some(&(length, stride)) = dimensions.get(axis) else {
            return self.array_element(word, depth, offset);
        };
        if index == 0 {
            if self.truncate_depth(depth) {
                return Ok(());
            }
            self.text.push('(');
        }
        if index == length {
            self.text.push(')');
            return Ok(());
        }
        if index > 0 {
            self.text.push(' ');
        }
        if self.options.length_reached(index) {
            self.text.push_str("...)");
            return Ok(());
        }
        self.tasks.push(PrintTask::ArrayDimension {
            word,
            depth,
            dimensions: Rc::clone(&dimensions),
            axis,
            index: index + 1,
            offset,
        });
        self.tasks.push(PrintTask::ArrayDimension {
            word,
            depth: depth + 1,
            dimensions,
            axis: axis + 1,
            index: 0,
            offset: offset + index * stride,
        });
        Ok(())
    }

    fn array_element(&mut self, array: Word, depth: usize, offset: usize) -> Result<(), Fault> {
        let value = self
            .runtime
            .heap
            .array_value(array, offset)
            .expect("valid array slot");
        if let Some(word) = match value {
            ArrayValue::Word(word) => Some(word),
            ArrayValue::Octet(byte) => Some(Word::fixnum(isize::from(byte)).expect("octet fixnum")),
            _ => None,
        } {
            self.tasks.push(PrintTask::Object { word, depth });
            return Ok(());
        }
        if self.dispatch_table.is_some() {
            let word = with_rooted_print_tasks(self.runtime, &mut self.tasks, || {
                value
                    .boxed(&self.runtime.heap)
                    .map_err(|error| EvalError::out_of_memory(self.at, error).into())
            })?;
            self.tasks.push(PrintTask::Object { word, depth });
            return Ok(());
        }
        let real = match value {
            ArrayValue::SingleFloat(value) => Real::Single(value),
            ArrayValue::DoubleFloat(value) => Real::Double(value),
            ArrayValue::Word(_) | ArrayValue::Octet(_) => {
                unreachable!("tagged and octet elements use object tasks")
            }
        };
        print_number(
            self.runtime,
            array,
            &Number::Real(real),
            self.options,
            &mut self.text,
        )
        .map_err(|error| self.unreadable(error))
    }
}
