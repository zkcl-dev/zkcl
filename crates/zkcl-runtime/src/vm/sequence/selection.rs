use crate::condition::Fault;
use crate::heap::{ArrayElement, ListSpine, RootFrame, RootSlot, TreelistKind, Word};
use crate::vm::arrays;
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
enum SequenceKind {
    List,
    String,
    Vector(ArrayElement),
    Treelist(TreelistKind),
}

/// A rooted view of the elements. Lists keep their conses, so the car is
/// read at visit time. Vectors and strings keep their original elements.
pub(in crate::vm) struct Elements {
    original: RootSlot,
    slots: RootFrame,
    kind: SequenceKind,
}

impl Elements {
    pub(in crate::vm) fn new(runtime: &Runtime, word: Word, at: usize) -> Result<Self, Fault> {
        let original = runtime.root_slot(word);
        let (kind, slots) = snapshot(runtime, original.get(), at)?;
        Ok(Self {
            original,
            slots,
            kind,
        })
    }

    pub(in crate::vm) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(in crate::vm) fn get(&self, runtime: &Runtime, index: usize) -> Word {
        let word = self.slots.get(index);
        match self.kind {
            SequenceKind::List => runtime.heap.cons_fields(word).expect("rooted cons").0,
            SequenceKind::String | SequenceKind::Vector(_) | SequenceKind::Treelist(_) => word,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElementEdit {
    Keep,
    Discard,
    Replace,
}

/// The retained positions of an element snapshot. Destructive list edits
/// relink the original conses.
pub(in crate::vm) struct Selection {
    elements: Elements,
    edits: Vec<ElementEdit>,
    changed: bool,
}

impl Selection {
    pub(in crate::vm) fn new(elements: Elements) -> Self {
        Self {
            edits: vec![ElementEdit::Keep; elements.len()],
            elements,
            changed: false,
        }
    }

    pub(in crate::vm) fn elements(&self) -> &Elements {
        &self.elements
    }

    pub(in crate::vm) fn is_kept(&self, index: usize) -> bool {
        self.edits[index] == ElementEdit::Keep
    }

    pub(in crate::vm) fn discard(&mut self, index: usize) {
        self.edits[index] = ElementEdit::Discard;
        self.changed = true;
    }

    pub(in crate::vm) fn replace(&mut self, index: usize) {
        self.edits[index] = ElementEdit::Replace;
        self.changed = true;
    }

    pub(in crate::vm) fn result(
        &self,
        runtime: &Runtime,
        destructive: bool,
        replacement: Option<Word>,
        at: usize,
    ) -> Result<Word, Fault> {
        if !self.changed {
            return Ok(self.elements.original.get());
        }
        let replacement_root = replacement.map(|word| runtime.root_slot(word));
        let value_at = |index: usize| match self.edits[index] {
            ElementEdit::Replace => replacement_root
                .as_ref()
                .expect("substitute value present")
                .get(),
            ElementEdit::Keep => self.elements.get(runtime, index),
            ElementEdit::Discard => unreachable!("discarded element should be skipped"),
        };
        if matches!(self.elements.kind, SequenceKind::String)
            && let Some(rep) = &replacement_root
            && rep.get().as_character().is_none()
        {
            return Err(Fault::type_error(runtime, 
                rep.get(),
                "CHARACTER",
                EvalError::new(at, "expected a character for string substitution"),
            ));
        }
        if let SequenceKind::Vector(element) = self.elements.kind
            && let Some(rep) = &replacement_root
        {
            arrays::checked_element(runtime, element, rep.get(), at)?;
        }

        let indices =
            (0..self.edits.len()).filter(|&index| self.edits[index] != ElementEdit::Discard);
        let oom = |error| EvalError::out_of_memory(at, error);
        match self.elements.kind {
            SequenceKind::List if destructive => {
                let mut tail = Word::NIL;
                for index in indices.rev() {
                    let cell = self.elements.slots.get(index);
                    if self.edits[index] == ElementEdit::Replace {
                        runtime.heap.set_cons_car(
                            cell,
                            replacement_root.as_ref().expect("substitute value").get(),
                        );
                    }
                    runtime.heap.set_cons_cdr(cell, tail);
                    tail = cell;
                }
                Ok(tail)
            }
            SequenceKind::List => {
                let tail = runtime.root_frame(1);
                for index in indices.rev() {
                    let cell = runtime
                        .heap
                        .cons(|| (value_at(index), tail.get(0)))
                        .map_err(oom)?;
                    tail.set(0, cell);
                }
                Ok(tail.get(0))
            }
            SequenceKind::String | SequenceKind::Vector(_)
                if destructive && replacement_root.is_some() =>
            {
                for (index, &edit) in self.edits.iter().enumerate() {
                    if edit == ElementEdit::Replace {
                        runtime
                            .heap
                            .set_array_slot(self.elements.original.get(), index, value_at(index))
                            .map_err(oom)?;
                    }
                }
                Ok(self.elements.original.get())
            }
            SequenceKind::String => {
                let text: String = indices
                    .map(|index| value_at(index).as_character().expect("character"))
                    .collect();
                runtime.heap.string(&text).map_err(oom).map_err(Fault::from)
            }
            SequenceKind::Vector(element) => {
                let indices = indices.collect::<Vec<_>>();
                runtime
                    .heap
                    .array(&[indices.len()], element, false, None, |pos| {
                        value_at(indices[pos])
                    })
                    .map_err(oom)
                    .map_err(Fault::from)
            }
            SequenceKind::Treelist(TreelistKind::Mutable) if destructive => {
                if let Some(rep) = &replacement_root {
                    for (index, &edit) in self.edits.iter().enumerate() {
                        if edit == ElementEdit::Replace {
                            runtime
                                .heap
                                .mutable_treelist_set(&self.elements.original, index, rep)
                                .map_err(oom)?;
                        }
                    }
                    return Ok(self.elements.original.get());
                }
                let indices: Vec<_> = indices.collect();
                let slots = runtime.root_frame(indices.len());
                for (pos, idx) in indices.into_iter().enumerate() {
                    slots.set(pos, value_at(idx));
                }
                runtime
                    .heap
                    .mutable_treelist_reset_from_slots(&self.elements.original, &slots, slots.len())
                    .map_err(oom)?;
                Ok(self.elements.original.get())
            }
            SequenceKind::Treelist(kind) => {
                let indices: Vec<_> = indices.collect();
                let slots = runtime.root_frame(indices.len());
                for (pos, idx) in indices.into_iter().enumerate() {
                    slots.set(pos, value_at(idx));
                }
                match kind {
                    TreelistKind::Persistent => runtime
                        .heap
                        .treelist_from_slots(&slots, slots.len())
                        .map_err(oom)
                        .map_err(Fault::from),
                    TreelistKind::Mutable => runtime
                        .heap
                        .mutable_treelist_from_slots(&slots, slots.len())
                        .map_err(oom)
                        .map_err(Fault::from),
                }
            }
        }
    }
}

fn snapshot(runtime: &Runtime, word: Word, at: usize) -> Result<(SequenceKind, RootFrame), Fault> {
    if let Some(values) = runtime.heap.with_string_slice(word, |text| {
        let values = runtime.root_frame(text.chars().count());
        for (index, character) in text.chars().enumerate() {
            values.set(index, Word::character(character));
        }
        values
    }) {
        return Ok((SequenceKind::String, values));
    }
    let invalid = || {
        Fault::type_error(runtime, 
            word,
            "SEQUENCE",
            EvalError::new(at, "expected a proper sequence"),
        )
    };
    if let Some(info) = runtime.heap.array_info(word) {
        if info.rank != 1 {
            return Err(invalid());
        }
        let values = arrays::sequence_elements(runtime, word, at)?;
        return Ok((SequenceKind::Vector(info.element), values));
    }
    if let Some(kind) = runtime.heap.treelist_kind(word) {
        let values = arrays::sequence_elements(runtime, word, at)?;
        return Ok((SequenceKind::Treelist(kind), values));
    }
    let mut list = ListSpine::new(word);
    let mut cells = Vec::new();
    loop {
        let cell = list.tail();
        if list.next(&runtime.heap).map_err(|_| invalid())?.is_none() {
            if cell != Word::NIL {
                return Err(if cell == word {
                    invalid()
                } else {
                    super::improper_list_tail(runtime, cell, at)
                });
            }
            let values = runtime.root_frame(cells.len());
            for (index, word) in cells.into_iter().enumerate() {
                values.set(index, word);
            }
            return Ok((SequenceKind::List, values));
        }
        cells.push(cell);
    }
}
