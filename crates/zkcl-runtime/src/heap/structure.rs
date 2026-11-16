use super::{
    Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor, ValueSlot, Word,
    WordSlot, offset_of, trailing_base,
};

#[repr(C)]
struct Structure {
    header: Header,
    name: WordSlot,
    count: usize,
}

// SAFETY: `Structure` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Structure`.
unsafe impl HeapObject for Structure {
    const KIND: ObjectKind = ObjectKind::Structure;
}

impl Heap {
    /// Allocate a structure, then read its traced fields from the caller's roots.
    /// The field callbacks must not allocate Lisp objects.
    pub(crate) fn structure(
        &self,
        count: usize,
        name: impl FnOnce() -> Word,
        slot: impl Fn(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            count,
            |header| Structure {
                header,
                name: WordSlot::new(name()),
                count,
            },
            |index| WordSlot::new(slot(index)),
        )
    }

    pub(crate) fn structure_layout(&self, word: Word) -> Option<(Word, usize)> {
        let instance = self.object_ref::<Structure>(word)?;
        Some((instance.name.get(), instance.count))
    }

    pub(crate) fn structure_slot(&self, word: Word, index: usize) -> Option<Word> {
        self.trailing_ref::<Structure, WordSlot>(word, |s| s.count, index)
            .map(WordSlot::get)
    }

    pub(crate) fn set_structure_slot(&self, word: Word, index: usize, value: Word) -> Option<()> {
        let slot = self.trailing_ref::<Structure, WordSlot>(word, |s| s.count, index)?;
        self.write_slot(word.object_address().expect("structure"), slot, value);
        Some(())
    }
}

pub(super) fn scan<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(Structure, name)));
    // SAFETY: The collector supplies a live structure with initialized slots.
    let count = unsafe { (*base.to_ptr::<Structure>()).count };
    visitor.visit_array(trailing_base::<Structure>(base), count);
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    Some((
        "structure",
        vec![heap.object_ref::<Structure>(word)?.count as u64],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if kind != "structure" {
        return Ok(None);
    }
    let mut d = super::image::Data(data);
    let count = d.number()?;
    d.done()?;
    heap.structure(count, || Word::NIL, |_| Word::NIL)
        .map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
