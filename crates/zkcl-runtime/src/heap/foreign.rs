use std::cell::RefCell;
use std::ptr::NonNull;

use super::roots::ForeignSlots;
use super::{
    Address, ForeignEdge, Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor,
    ValueSlot, Word,
};

#[repr(C)]
pub(crate) struct LispForeign {
    header: Header,
    pub(crate) id: u64,
    edge: Option<NonNull<RefCell<ForeignSlots>>>,
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

pub(super) fn scan_edge(base: Address, visitor: &mut impl SlotVisitor) {
    // SAFETY: the collector supplied a live foreign wrapper. Its host-table
    // entry owns the slot through this entire synchronous collection.
    if let Some(edge) = unsafe { (*base.to_ptr::<LispForeign>()).edge } {
        let storage = unsafe { edge.as_ref() };
        let slots = storage.borrow();
        for slice in slots.slices() {
            for slot in slice {
                visitor.visit_slot(ValueSlot(Address::from_ptr(slot)));
            }
        }
    }
}
