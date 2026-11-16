//! A recursive READ returns an ordinary Lisp object to its reader macro. An
//! unfinished #n= therefore needs an identity that can survive arbitrary Lisp
//! allocation until the enclosing object's value is available.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use super::Token;
use crate::heap::{ArrayElement, RootSlot, Word};
use crate::{EvalError, Root, Runtime, vm};

#[derive(Default)]
pub(super) struct Labels {
    by_number: HashMap<u64, usize>,
    definitions: Vec<(u64, Label)>,
}

enum Label {
    Reading(Option<RootSlot>),
    Complete(RootSlot),
}

impl Labels {
    pub(super) fn begin(&mut self, label: u64, token: &Token<'_>) -> Result<usize, EvalError> {
        match self.by_number.entry(label) {
            Entry::Occupied(_) => Err(token.error("duplicate #n= label in one form")),
            Entry::Vacant(entry) => {
                let index = self.definitions.len();
                entry.insert(index);
                self.definitions.push((label, Label::Reading(None)));
                Ok(index)
            }
        }
    }

    pub(super) fn reference(
        &mut self,
        runtime: &Runtime,
        label: u64,
        token: &Token<'_>,
    ) -> Result<Word, EvalError> {
        let index = self
            .by_number
            .get(&label)
            .ok_or_else(|| token.error("#n# requires an earlier #n= label"))?;
        match &mut self.definitions[*index].1 {
            Label::Complete(value) | Label::Reading(Some(value)) => Ok(value.get()),
            Label::Reading(marker) => {
                let (_, word) = runtime.fresh_symbol("READER-LABEL", token.at)?;
                *marker = Some(runtime.root_slot(word));
                Ok(word)
            }
        }
    }

    pub(super) fn complete(
        &mut self,
        runtime: &Runtime,
        index: usize,
        value: &Root<'_>,
        at: usize,
    ) -> Result<(), EvalError> {
        let previous = std::mem::replace(
            &mut self.definitions[index].1,
            Label::Complete(runtime.root_slot(value.slot.get())),
        );
        if let Label::Reading(Some(marker)) = previous {
            let marker = marker.get();
            let target = value.slot.get();
            if target == marker {
                return Err(EvalError::read(at, "a label cannot name its own reference"));
            }
            // A nested #m= can return the unresolved parent, or retain it
            // even if its reader macro discarded that object from the result.
            let roots = self.definitions[index..]
                .iter_mut()
                .filter_map(|(_, entry)| {
                    let Label::Complete(root) = entry else {
                        return None;
                    };
                    if root.get() == marker {
                        *root = runtime.root_slot(target);
                    }
                    Some(root.get())
                });
            substitute(runtime, roots.collect(), marker, target, at)?;
        }
        Ok(())
    }

    pub(super) fn abandon(&mut self, index: usize) {
        for (label, _) in self.definitions.drain(index..) {
            self.by_number.remove(&label);
        }
    }
}

/// Walk containers by identity; a reader macro can return a graph that is
/// already circular. Only general array slots can contain an object marker.
/// Everything before the rehash phase uses host memory and cannot collect.
fn substitute(
    runtime: &Runtime,
    mut work: Vec<Word>,
    marker: Word,
    target: Word,
    at: usize,
) -> Result<(), EvalError> {
    let heap = &runtime.heap;
    let mut seen = HashSet::new();
    let mut tables = Vec::new();
    while let Some(word) = work.pop() {
        if word.is_immediate() || !seen.insert(word) {
            continue;
        }
        let mut replacement = |child: Word| {
            if child == marker {
                Some(target)
            } else {
                work.push(child);
                None
            }
        };
        if let Some((car, cdr)) = heap.cons_fields(word) {
            if let Some(value) = replacement(car) {
                heap.set_cons_car(word, value);
            }
            if let Some(value) = replacement(cdr) {
                heap.set_cons_cdr(word, value);
            }
        } else if let Some(info) = heap.array_info(word) {
            if info.element == ArrayElement::General {
                for index in 0..info.size {
                    let child = heap
                        .array_value(word, index)
                        .expect("general array slot")
                        .expect_word();
                    if let Some(value) = replacement(child) {
                        heap.set_array_slot(word, index, value)
                            .map_err(|error| EvalError::out_of_memory(at, error))?;
                    }
                }
            }
        } else if let Some((_, count)) = heap.structure_layout(word) {
            for index in 0..count {
                let child = heap.structure_slot(word, index).expect("structure slot");
                if let Some(value) = replacement(child) {
                    heap.set_structure_slot(word, index, value);
                }
            }
        } else if heap.instance_class(word).is_some() {
            let mut index = 0;
            while let Some(child) = heap.instance_slot(word, index) {
                if let Some(value) = replacement(child) {
                    heap.set_instance_slot(word, index, value);
                }
                index += 1;
            }
        } else if let Some(info) = heap.hash_table_info(word) {
            // EQUAL/EQUALP keys can contain a marker below the key itself.
            // Recompute their hashes after all nested objects are fixed.
            let mut entries = Vec::with_capacity(info.count);
            let mut cursor = 0;
            while let Some((key, value)) = heap.hash_table_next_entry(word, &mut cursor) {
                entries.push((
                    runtime.root_slot(replacement(key).unwrap_or(key)),
                    runtime.root_slot(replacement(value).unwrap_or(value)),
                ));
            }
            tables.push((runtime.root_slot(word), info.test, entries));
        }
    }
    for (table, test, entries) in tables {
        heap.hash_table_clear(table.get());
        for (key, value) in entries {
            vm::hash_tables::put(heap, table.get(), test, key.get(), value.get())
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
    }
    Ok(())
}
