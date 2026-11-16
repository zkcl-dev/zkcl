use std::cell::Cell as LocalCell;
use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::OutOfMemory;
use crate::package::Package;
use crate::symbol::SymbolAttributes;

use super::object::{HeapObject, WordSlot};
use super::word::{Header, ObjectKind, object_address};
use super::{Heap, Word};

#[repr(C)]
pub(super) struct Symbol {
    pub(super) header: Header,
    pub(super) value: WordSlot,
    pub(super) function: WordSlot,
    pub(super) setter: WordSlot,
    pub(super) name: WordSlot,
    pub(super) variable_doc: WordSlot,
    pub(super) symbol_macro: WordSlot,
    pub(super) attributes: std::cell::Cell<SymbolAttributes>,
    pub(super) function_revision: LocalCell<usize>,
    pub(super) tls_index: AtomicU32,
}

// SAFETY: `Symbol` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Symbol`.
unsafe impl HeapObject for Symbol {
    const KIND: ObjectKind = ObjectKind::Symbol;
}

#[repr(C)]
pub(super) struct Cell {
    header: Header,
    pub(super) value: WordSlot,
}

// SAFETY: `Cell` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Cell`.
unsafe impl HeapObject for Cell {
    const KIND: ObjectKind = ObjectKind::Cell;
}

pub(crate) const CELL_VALUE_OFFSET: usize = offset_of!(Cell, value);

impl Heap {
    pub(crate) fn symbol(
        &self,
        name: &str,
        attributes: SymbolAttributes,
    ) -> Result<Word, OutOfMemory> {
        let name = self.root_slot(self.string(name)?);
        self.alloc_object(|header| Symbol {
            header,
            value: WordSlot::unbound(),
            function: WordSlot::unbound(),
            setter: WordSlot::unbound(),
            name: WordSlot::new(name.get()),
            variable_doc: WordSlot::nil(),
            symbol_macro: WordSlot::unbound(),
            attributes: std::cell::Cell::new(attributes),
            function_revision: LocalCell::new(0),
            tls_index: AtomicU32::new(0),
        })
    }

    pub(crate) fn cell(&self, value: impl FnOnce() -> Word) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| Cell {
            header,
            value: WordSlot::new(value()),
        })
    }

    pub(crate) fn symbol_name(&self, word: Word) -> Option<String> {
        match word {
            Word::NIL => return Some("NIL".into()),
            Word::T => return Some("T".into()),
            _ => {}
        }
        self.string_contents(self.symbol_name_word(word)?)
    }

    pub(crate) fn symbol_name_word(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<Symbol>(word)?.name.get())
    }

    pub(crate) fn symbol_attributes(&self, word: Word) -> Option<SymbolAttributes> {
        if matches!(word, Word::NIL | Word::T) {
            return Some(SymbolAttributes::new(
                usize::from(word == Word::T),
                Some(Package::COMMON_LISP),
                true,
                true,
            ));
        }
        Some(self.object_ref::<Symbol>(word)?.attributes.get())
    }

    pub(crate) fn symbol_home(&self, word: Word) -> Option<Package> {
        self.symbol_attributes(word)?.home
    }

    pub(crate) fn variable_documentation(&self, word: Word) -> Word {
        self.object_ref::<Symbol>(word)
            .map_or(Word::NIL, |symbol| symbol.variable_doc.get())
    }

    pub(crate) fn set_variable_documentation(&self, word: Word, doc: Word) {
        // The immediate constants NIL and T have no symbol payload.
        if let Some(symbol) = self.object_ref::<Symbol>(word) {
            self.write_slot_object(symbol, &symbol.variable_doc, doc);
            self.namespace_changed();
        }
    }

    pub(crate) fn symbol_macro_expander(&self, word: Word) -> Option<Word> {
        self.object_ref::<Symbol>(word)?.symbol_macro.get_bound()
    }

    pub(crate) fn set_symbol_macro_expander(&self, word: Word, expander: Word) {
        let symbol = self.expect_object::<Symbol>(word, "variable symbol");
        self.write_slot_object(symbol, &symbol.symbol_macro, expander);
    }

    pub(crate) fn set_symbol_home(&self, word: Word, home: Option<Package>) {
        let slot = &self.expect_object::<Symbol>(word, "heap symbol").attributes;
        let mut attributes = slot.get();
        attributes.home = home;
        slot.set(attributes);
    }

    #[inline(always)]
    fn tls_load(&self, tls_index: u32) -> Word {
        self.tls_values
            .borrow()
            .get(tls_index as usize)
            .copied()
            .unwrap_or(Word::NO_TLS_VALUE)
    }

    #[inline]
    fn tls_store(&self, tls_index: u32, value: Word) {
        let index = tls_index as usize;
        let mut values = self.tls_values.borrow_mut();
        if index >= values.len() {
            values.resize(index + 1, Word::NO_TLS_VALUE);
        }
        values[index] = value;
    }

    fn ensure_tls_index(&self, symbol: &Symbol) -> u32 {
        let current = symbol.tls_index.load(Ordering::Acquire);
        if current != 0 {
            return current;
        }
        let fresh = self.next_tls_index.fetch_add(1, Ordering::Relaxed);
        assert_ne!(fresh, 0, "symbol TLS index space exhausted");
        match symbol
            .tls_index
            .compare_exchange(0, fresh, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => fresh,
            Err(existing) => existing,
        }
    }

    pub(crate) fn bind_tls_symbol(&self, word: Word, value: Word) -> Word {
        let symbol = self.expect_object::<Symbol>(word, "special variable symbol");
        let index = self.ensure_tls_index(symbol);
        let previous = self.tls_load(index);
        self.tls_store(index, value);
        previous
    }

    pub(crate) fn swap_tls_symbol(&self, word: Word, value: Word) -> Word {
        self.bind_tls_symbol(word, value)
    }

    pub(crate) fn restore_tls_symbol(&self, word: Word, previous: Word) {
        let symbol = self.expect_object::<Symbol>(word, "special variable symbol");
        let index = symbol.tls_index.load(Ordering::Acquire);
        debug_assert_ne!(index, 0, "restoring unbound TLS symbol");
        self.tls_store(index, previous);
    }

    pub(crate) fn symbol_cell(&self, word: Word, function: bool) -> Option<Word> {
        if !function && matches!(word, Word::NIL | Word::T) {
            return Some(word);
        }
        let symbol = self.object_ref::<Symbol>(word)?;
        let slot = if function {
            if symbol.attributes.get().has_flag(SymbolAttributes::MACRO) {
                return None;
            }
            &symbol.function
        } else {
            let tls_index = symbol.tls_index.load(Ordering::Acquire);
            if tls_index != 0 {
                let tls = self.tls_load(tls_index);
                if tls != Word::NO_TLS_VALUE {
                    return (tls != Word::UNBOUND).then_some(tls);
                }
            }
            &symbol.value
        };
        slot.get_bound()
    }

    pub(crate) fn function_epoch(&self) -> usize {
        self.function_epoch.get()
    }

    /// Last change to this symbol's ordinary function binding or macro role.
    /// The symbol itself keeps the revision when the collector moves it.
    pub(crate) fn symbol_function_revision(&self, word: Word) -> Option<usize> {
        Some(self.object_ref::<Symbol>(word)?.function_revision.get())
    }

    pub(crate) fn namespace_epoch(&self) -> u64 {
        self.namespace_epoch.get()
    }

    pub(crate) fn namespace_changed(&self) {
        self.namespace_epoch.update(|epoch| epoch.wrapping_add(1));
    }

    fn function_binding_changed(&self, symbol: &Symbol) {
        let revision = self
            .function_epoch()
            .checked_add(1)
            .expect("function binding revision space exhausted");
        symbol.function_revision.set(revision);
        self.function_epoch.set(revision);
    }

    pub(crate) fn set_symbol_cell(&self, word: Word, function: bool, value: Word) {
        let symbol = self.expect_object::<Symbol>(word, "symbol object");
        let slot = if function {
            self.function_binding_changed(symbol);
            symbol
                .attributes
                .update(|attributes| attributes.with_flag(SymbolAttributes::MACRO, false));
            &symbol.function
        } else {
            let tls_index = symbol.tls_index.load(Ordering::Acquire);
            if tls_index != 0 && self.tls_load(tls_index) != Word::NO_TLS_VALUE {
                self.tls_store(tls_index, value);
                return;
            }
            if (symbol.value.get() == Word::UNBOUND) != (value == Word::UNBOUND) {
                self.namespace_changed();
            }
            &symbol.value
        };
        self.write_slot_object(symbol, slot, value);
    }

    pub(crate) fn symbol_setter(&self, word: Word) -> Option<Word> {
        self.object_ref::<Symbol>(word)?.setter.get_bound()
    }

    pub(crate) fn set_symbol_setter(&self, word: Word, value: Word) {
        let symbol = self.expect_object::<Symbol>(word, "setter name symbol");
        self.write_slot_object(symbol, &symbol.setter, value);
    }

    pub(crate) fn macro_expander(&self, word: Word) -> Option<Word> {
        let symbol = self.object_ref::<Symbol>(word)?;
        symbol
            .attributes
            .get()
            .has_flag(SymbolAttributes::MACRO)
            .then(|| symbol.function.get())
    }

    pub(crate) fn set_macro_expander(&self, word: Word, expander: Word) {
        let symbol = self.expect_object::<Symbol>(word, "macro symbol");
        self.function_binding_changed(symbol);
        self.write_slot_object(symbol, &symbol.function, expander);
        symbol
            .attributes
            .update(|attributes| attributes.with_flag(SymbolAttributes::MACRO, true));
    }

    pub(crate) fn is_symbol(&self, word: Word) -> bool {
        matches!(word, Word::NIL | Word::T) || object_address(word, ObjectKind::Symbol).is_some()
    }

    pub(crate) fn is_variable(&self, word: Word) -> bool {
        self.symbol_attributes(word)
            .is_some_and(|attributes| !attributes.constant)
    }

    pub(crate) fn proclaim_constant(&self, word: Word) {
        self.expect_object::<Symbol>(word, "heap symbol")
            .attributes
            .update(|mut attributes| {
                attributes.defined_constant = true;
                attributes
            });
        self.namespace_changed();
    }

    pub(crate) fn is_special(&self, word: Word) -> bool {
        self.object_ref::<Symbol>(word)
            .is_some_and(|symbol| symbol.attributes.get().has_flag(SymbolAttributes::SPECIAL))
    }

    pub(crate) fn proclaim_special(&self, word: Word) {
        self.expect_object::<Symbol>(word, "special symbol")
            .attributes
            .update(|attributes| attributes.with_flag(SymbolAttributes::SPECIAL, true));
    }

    pub(crate) fn read_cell(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<Cell>(word)?.value.get())
    }

    pub(crate) fn write_cell(&self, word: Word, value: Word) -> Option<()> {
        let cell = self.object_ref::<Cell>(word)?;
        self.write_slot_object(cell, &cell.value, value);
        Some(())
    }

    pub(crate) fn atomic_load_symbol_value(
        &self,
        word: Word,
        order: std::sync::atomic::Ordering,
    ) -> Option<Word> {
        if matches!(word, Word::NIL | Word::T) {
            // Preserve the atomic fence semantics if an Acquire/SeqCst load was requested on NIL/T.
            if order != std::sync::atomic::Ordering::Relaxed {
                std::sync::atomic::fence(order);
            }
            return Some(word);
        }
        let symbol = self.object_ref::<Symbol>(word)?;
        let value = symbol.value.load_ordered(order);
        (value != Word::UNBOUND).then_some(value)
    }

    pub(crate) fn atomic_store_symbol_value(
        &self,
        word: Word,
        value: Word,
        order: std::sync::atomic::Ordering,
    ) -> Option<()> {
        debug_assert_ne!(value, Word::UNBOUND);
        let symbol = self.object_ref::<Symbol>(word)?;
        let was_unbound = symbol.value.load_ordered(std::sync::atomic::Ordering::Relaxed)
            == Word::UNBOUND;
        self.atomic_store_slot(symbol, &symbol.value, value, order);
        if was_unbound {
            self.namespace_changed();
        }
        Some(())
    }

    pub(crate) fn atomic_cas_symbol_value(
        &self,
        word: Word,
        expected: Word,
        new: Word,
        success: std::sync::atomic::Ordering,
        failure: std::sync::atomic::Ordering,
    ) -> Option<Result<Word, Word>> {
        debug_assert_ne!(expected, Word::UNBOUND);
        debug_assert_ne!(new, Word::UNBOUND);
        let symbol = self.object_ref::<Symbol>(word)?;
        Some(self.atomic_cas_slot(symbol, &symbol.value, expected, new, success, failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::HeapConfig;

    #[test]
    fn tls_bound_young_objects_relocate_and_survive_defrag_gc() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let sym = heap.root_slot(
            heap.symbol("*TLS-RELOC*", SymbolAttributes::new(100, None, false, false))
                .unwrap(),
        );
        let global_val = Word::fixnum(7).unwrap();
        heap.set_symbol_cell(sym.get(), false, global_val);

        let young_cons = heap
            .cons(|| (Word::fixnum(11).unwrap(), Word::fixnum(22).unwrap()))
            .unwrap();
        let prev = heap.bind_tls_symbol(sym.get(), young_cons);
        assert_eq!(prev, Word::NO_TLS_VALUE);

        heap.collect();

        let relocated = heap.symbol_cell(sym.get(), false).unwrap();
        assert_ne!(relocated, young_cons, "TLS root must track defrag move");
        assert_eq!(
            heap.cons_fields(relocated),
            Some((Word::fixnum(11).unwrap(), Word::fixnum(22).unwrap()))
        );
        assert_eq!(
            heap.atomic_load_symbol_value(sym.get(), Ordering::SeqCst),
            Some(global_val)
        );

        heap.restore_tls_symbol(sym.get(), prev);
        assert_eq!(heap.symbol_cell(sym.get(), false), Some(global_val));
    }
}
