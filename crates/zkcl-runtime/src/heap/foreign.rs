use std::ptr::NonNull;

use super::roots::ForeignStorage;
use super::{
    Address, ForeignEdge, Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor, Word,
};

#[repr(C)]
pub(crate) struct LispForeign {
    header: Header,
    pub(crate) id: u64,
    edge: Option<NonNull<ForeignStorage>>,
}

// SAFETY: `LispForeign` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Foreign`.
unsafe impl HeapObject for LispForeign {
    const KIND: ObjectKind = ObjectKind::Foreign;
}

impl Heap {
    #[cfg(test)]
    pub(crate) fn allocate_foreign(&self, id: u64) -> Result<Word, OutOfMemory> {
        self.allocate_foreign_with_edge(id, None)
    }

    pub(crate) fn allocate_foreign_with_edge(
        &self,
        id: u64,
        edge: Option<&ForeignEdge>,
    ) -> Result<Word, OutOfMemory> {
        // ForeignObjectStorage retains this Rc allocation until the Lisp wrapper dies.
        let edge = edge.map(|e| {
            assert!(e.belongs_to(self));
            e.slot_storage()
        });
        self.alloc_object(|header| LispForeign { header, id, edge })
    }

    pub(crate) fn foreign_id(&self, word: Word) -> Option<u64> {
        Some(self.object_ref::<LispForeign>(word)?.id)
    }
}

fn storage(base: Address) -> Option<NonNull<ForeignStorage>> {
    // SAFETY: object scanning supplies an initialized LispForeign header.
    unsafe { (*base.to_ptr::<LispForeign>()).edge }
}

pub(super) fn scan_edge(base: Address, visitor: &mut impl SlotVisitor) {
    if let Some(storage) = storage(base) {
        // SAFETY: the foreign value owns the storage while its wrapper is live.
        unsafe { storage.as_ref() }.visit_slots(visitor);
    }
}

pub(super) fn trace_edge(
    base: Address,
    visitor: &mut impl SlotVisitor,
) -> Option<NonNull<ForeignStorage>> {
    // SAFETY: the collector supplied a live foreign wrapper. Its host-table
    // entry owns the slot through this entire synchronous collection.
    let storage = storage(base)?;
    unsafe { storage.as_ref() }.trace(false, visitor);
    Some(storage)
}
