use std::cell::Cell;

use super::{
    Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor, Word, WordSlot, offset_of,
    visit_offsets,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadtableCase {
    Upcase,
    Downcase,
    Preserve,
    Invert,
}

impl ReadtableCase {
    pub(crate) fn inverted(letters: impl Iterator<Item = char>) -> Self {
        let mut lower = false;
        let mut upper = false;
        for ch in letters {
            lower |= ch.is_ascii_lowercase();
            upper |= ch.is_ascii_uppercase();
            if lower && upper {
                return Self::Preserve;
            }
        }
        match (lower, upper) {
            (true, false) => Self::Upcase,
            (false, true) => Self::Downcase,
            _ => Self::Preserve,
        }
    }

    pub(crate) fn fold(self, ch: char) -> char {
        match self {
            Self::Upcase => ch.to_ascii_uppercase(),
            Self::Downcase => ch.to_ascii_lowercase(),
            Self::Preserve => ch,
            Self::Invert => unreachable!("resolve case inversion before folding characters"),
        }
    }
}

#[repr(C)]
struct LispReadtable {
    header: Header,
    case: Cell<ReadtableCase>,
    // Alist of character macro definitions.  See readtables.rs for the
    // representation; the table owns the list through this traced slot.
    macros: WordSlot,
    // Alist of non-macro character syntax overrides installed by
    // SET-SYNTAX-FROM-CHAR.  Characters without an entry use the standard
    // syntax, so a fresh readtable needs no initialization here.
    syntax: WordSlot,
}

// SAFETY: `LispReadtable` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Readtable`.
unsafe impl HeapObject for LispReadtable {
    const KIND: ObjectKind = ObjectKind::Readtable;
}

impl Heap {
    pub(crate) fn readtable(&self, case: ReadtableCase) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| LispReadtable {
            header,
            case: Cell::new(case),
            macros: WordSlot::nil(),
            syntax: WordSlot::nil(),
        })
    }

    pub(crate) fn readtable_case(&self, word: Word) -> Option<ReadtableCase> {
        Some(self.object_ref::<LispReadtable>(word)?.case.get())
    }

    pub(crate) fn standard_readtable_case(&self, word: Word) -> Option<ReadtableCase> {
        let table = self.object_ref::<LispReadtable>(word)?;
        (table.macros.get() == Word::NIL && table.syntax.get() == Word::NIL)
            .then(|| table.case.get())
    }

    pub(crate) fn set_readtable_case(&self, word: Word, case: ReadtableCase) {
        self.expect_object::<LispReadtable>(word, "readtable")
            .case
            .set(case);
    }

    pub(crate) fn readtable_macros(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<LispReadtable>(word)?.macros.get())
    }

    pub(crate) fn set_readtable_macros(&self, word: Word, macros: Word) {
        let table = self.expect_object::<LispReadtable>(word, "readtable");
        self.write_slot_object(table, &table.macros, macros);
    }

    pub(crate) fn readtable_syntax(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<LispReadtable>(word)?.syntax.get())
    }

    pub(crate) fn set_readtable_syntax(&self, word: Word, syntax: Word) {
        let table = self.expect_object::<LispReadtable>(word, "readtable");
        self.write_slot_object(table, &table.syntax, syntax);
    }
}

pub(super) fn trace_slots(base: super::Address, visitor: &mut impl SlotVisitor) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(LispReadtable, macros),
            offset_of!(LispReadtable, syntax),
        ],
    );
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    let value = heap.object_ref::<LispReadtable>(word)?;
    Some(("readtable", vec![value.case.get() as u64]))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if kind != "readtable" {
        return Ok(None);
    }
    let mut d = super::image::Data(data);
    let case = match d.number()? {
        0 => ReadtableCase::Upcase,
        1 => ReadtableCase::Downcase,
        2 => ReadtableCase::Preserve,
        3 => ReadtableCase::Invert,
        _ => return Err(super::image::invalid()),
    };
    d.done()?;
    heap.readtable(case)
        .map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
