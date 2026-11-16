//! Persistent and transient Sek-backed sequence storage on the Immix heap.
//!
//! Each sequence uses a three-part chunked tree (`front`, `middle`, `back`)
//! with fixed-capacity circular backing buffers (`CHUNK_CAPACITY = 32`) and
//! monotonic `owner` IDs for zero-refcount ephemeral in-place updates with
//! copy-on-write on snapshot/thaw sharing.

use std::mem::offset_of;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::OutOfMemory;

use super::{
    Address, Header, Heap, HeapObject, ObjectKind, RootFrame, RootSlot, SlotVisitor, ValueSlot,
    Word, WordSlot, visit_offsets,
};

mod image;
mod node;
mod span;
#[cfg(test)]
mod tests;

pub(in crate::heap) use image::{image_allocate, image_shape};
use node::*;

pub(crate) const CHUNK_CAPACITY: usize = 32;
pub(super) const CHUNK_MASK: usize = CHUNK_CAPACITY - 1;
pub(super) const PERSISTENT_OWNER: u64 = 0;

#[inline(always)]
const fn can_mutate(object_owner: u64, op_owner: u64) -> bool {
    op_owner != PERSISTENT_OWNER && object_owner == op_owner
}

#[repr(C)]
pub(super) struct Treelist {
    header: Header,
    pub(super) node: WordSlot,
}

// SAFETY: `Treelist` is `#[repr(C)]` with `header: Header` first and `ObjectKind::Treelist`.
unsafe impl HeapObject for Treelist {
    const KIND: ObjectKind = ObjectKind::Treelist;
}

#[repr(C)]
pub(super) struct MutableTreelist {
    header: Header,
    pub(super) owner: AtomicU64,
    pub(super) node: WordSlot,
}

// SAFETY: `MutableTreelist` is `#[repr(C)]` with `header: Header` first and `ObjectKind::MutableTreelist`.
unsafe impl HeapObject for MutableTreelist {
    const KIND: ObjectKind = ObjectKind::MutableTreelist;
}

#[repr(C)]
pub(super) struct TreelistNode {
    header: Header,
    pub(super) owner: AtomicU64,
    pub(super) weight: AtomicUsize,
    pub(super) depth: u8,
    pub(super) front: WordSlot,
    pub(super) middle: WordSlot,
    pub(super) back: WordSlot,
}

// SAFETY: `TreelistNode` is `#[repr(C)]` with `header: Header` first and `ObjectKind::TreelistNode`.
unsafe impl HeapObject for TreelistNode {
    const KIND: ObjectKind = ObjectKind::TreelistNode;
}

#[repr(C)]
pub(super) struct TreelistSpan {
    header: Header,
    pub(super) owner: AtomicU64,
    pub(super) weight: AtomicUsize,
    pub(super) start: AtomicU8,
    pub(super) len: AtomicU8,
    pub(super) chunk: WordSlot,
}

// SAFETY: `TreelistSpan` is `#[repr(C)]` with `header: Header` first and `ObjectKind::TreelistSpan`.
unsafe impl HeapObject for TreelistSpan {
    const KIND: ObjectKind = ObjectKind::TreelistSpan;
}

#[repr(C)]
pub(super) struct TreelistChunk {
    header: Header,
    pub(super) owner: AtomicU64,
    pub(super) slots: [WordSlot; CHUNK_CAPACITY],
}

// SAFETY: `TreelistChunk` is `#[repr(C)]` with `header: Header` first and `ObjectKind::TreelistChunk`.
unsafe impl HeapObject for TreelistChunk {
    const KIND: ObjectKind = ObjectKind::TreelistChunk;
}

pub(in crate::heap) fn scan_treelist<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(Treelist, node)));
}

pub(in crate::heap) fn scan_mutable_treelist<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(MutableTreelist, node)));
}

pub(in crate::heap) fn scan_node<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(TreelistNode, front),
            offset_of!(TreelistNode, middle),
            offset_of!(TreelistNode, back),
        ],
    );
}

pub(in crate::heap) fn scan_span<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(TreelistSpan, chunk)));
}

pub(in crate::heap) fn scan_chunk<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_array(base + offset_of!(TreelistChunk, slots), CHUNK_CAPACITY);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreelistKind {
    Persistent,
    Mutable,
}

impl Heap {
    fn alloc_treelist_owner(&self) -> u64 {
        let id = self.next_treelist_owner.get();
        let next = id.checked_add(1).expect("treelist owner ID overflow");
        self.next_treelist_owner.set(next);
        id
    }

    pub(crate) fn treelist_kind(&self, word: Word) -> Option<TreelistKind> {
        if self.object_ref::<Treelist>(word).is_some() {
            Some(TreelistKind::Persistent)
        } else if self.object_ref::<MutableTreelist>(word).is_some() {
            Some(TreelistKind::Mutable)
        } else {
            None
        }
    }

    pub(crate) fn is_treelist(&self, word: Word) -> bool {
        self.object_ref::<Treelist>(word).is_some()
    }

    pub(crate) fn is_mutable_treelist(&self, word: Word) -> bool {
        self.object_ref::<MutableTreelist>(word).is_some()
    }

    fn treelist_node(&self, word: Word) -> Option<Word> {
        if let Some(treelist) = self.object_ref::<Treelist>(word) {
            return Some(treelist.node.get());
        }
        Some(self.object_ref::<MutableTreelist>(word)?.node.get())
    }

    pub(crate) fn treelist_len(&self, word: Word) -> Option<usize> {
        Some(node_weight(self, self.treelist_node(word)?))
    }

    pub(crate) fn treelist_get(&self, word: Word, index: usize) -> Option<Word> {
        let node = self.treelist_node(word)?;
        if index >= node_weight(self, node) {
            return None;
        }
        Some(node_get(self, node, index))
    }

    fn wrap_treelist(&self, node: Word) -> Result<Word, OutOfMemory> {
        let node_slot = self.root_slot(node);
        self.alloc_object(|header| Treelist {
            header,
            node: WordSlot::new(node_slot.get()),
        })
    }

    fn wrap_mutable_treelist(&self, owner: u64, node: Word) -> Result<Word, OutOfMemory> {
        let node_slot = self.root_slot(node);
        self.alloc_object(|header| MutableTreelist {
            header,
            owner: AtomicU64::new(owner),
            node: WordSlot::new(node_slot.get()),
        })
    }

    fn persistent_node(&self, tl: &RootSlot) -> RootSlot {
        self.root_slot(
            self.expect_object::<Treelist>(tl.get(), "treelist")
                .node
                .get(),
        )
    }

    fn update_mutable_treelist<R>(
        &self,
        mtl: &RootSlot,
        op: impl FnOnce(u64, &RootSlot) -> Result<(Word, R), OutOfMemory>,
    ) -> Result<R, OutOfMemory> {
        let (owner, node) = {
            let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
            (obj.owner.load(Ordering::Relaxed), obj.node.get())
        };
        let node_slot = self.root_slot(node);
        let (new_node, extra) = op(owner, &node_slot)?;
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        self.write_slot_object(obj, &obj.node, new_node);
        Ok(extra)
    }

    pub(crate) fn treelist_empty(&self) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| Treelist {
            header,
            node: WordSlot::nil(),
        })
    }

    pub(crate) fn mutable_treelist_empty(&self) -> Result<Word, OutOfMemory> {
        self.wrap_mutable_treelist(self.alloc_treelist_owner(), Word::NIL)
    }

    pub(crate) fn treelist_from_slots(
        &self,
        slots: &RootFrame,
        len: usize,
    ) -> Result<Word, OutOfMemory> {
        let node = build_node_from_slots(self, slots, len, PERSISTENT_OWNER)?;
        self.wrap_treelist(node)
    }

    pub(crate) fn mutable_treelist_from_slots(
        &self,
        slots: &RootFrame,
        len: usize,
    ) -> Result<Word, OutOfMemory> {
        let owner = self.alloc_treelist_owner();
        let node = build_node_from_slots(self, slots, len, owner)?;
        self.wrap_mutable_treelist(owner, node)
    }

    pub(crate) fn treelist_repeat(
        &self,
        count: usize,
        element: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let node = build_repeated_node(self, count, element, PERSISTENT_OWNER)?;
        self.wrap_treelist(node)
    }

    pub(crate) fn mutable_treelist_repeat(
        &self,
        count: usize,
        element: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let owner = self.alloc_treelist_owner();
        let node = build_repeated_node(self, count, element, owner)?;
        self.wrap_mutable_treelist(owner, node)
    }

    pub(crate) fn treelist_set(
        &self,
        tl: &RootSlot,
        index: usize,
        value: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let node_slot = self.persistent_node(tl);
        let new_node = node_set_at(self, &node_slot, index, value, PERSISTENT_OWNER)?;
        self.wrap_treelist(new_node)
    }

    pub(crate) fn mutable_treelist_set(
        &self,
        mtl: &RootSlot,
        index: usize,
        value: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        self.update_mutable_treelist(mtl, |owner, node_slot| {
            Ok((node_set_at(self, node_slot, index, value, owner)?, ()))
        })
    }

    pub(crate) fn treelist_push_front(
        &self,
        tl: &RootSlot,
        value: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let node_slot = self.persistent_node(tl);
        let new_node = node_push_front(self, &node_slot, 0, value, 1, PERSISTENT_OWNER)?;
        self.wrap_treelist(new_node)
    }

    pub(crate) fn treelist_push_back(
        &self,
        tl: &RootSlot,
        value: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let node_slot = self.persistent_node(tl);
        let new_node = node_push_back(self, &node_slot, 0, value, 1, PERSISTENT_OWNER)?;
        self.wrap_treelist(new_node)
    }

    pub(crate) fn mutable_treelist_push_front(
        &self,
        mtl: &RootSlot,
        value: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        self.update_mutable_treelist(mtl, |owner, node_slot| {
            Ok((node_push_front(self, node_slot, 0, value, 1, owner)?, ()))
        })
    }

    pub(crate) fn mutable_treelist_push_back(
        &self,
        mtl: &RootSlot,
        value: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        self.update_mutable_treelist(mtl, |owner, node_slot| {
            Ok((node_push_back(self, node_slot, 0, value, 1, owner)?, ()))
        })
    }

    pub(crate) fn treelist_pop_front(&self, tl: &RootSlot) -> Result<(Word, Word), OutOfMemory> {
        let node_slot = self.persistent_node(tl);
        let (popped, _, rest_node) = node_pop_front(self, &node_slot, PERSISTENT_OWNER)?;
        let popped_slot = self.root_slot(popped);
        let rest_tl = self.wrap_treelist(rest_node)?;
        Ok((popped_slot.get(), rest_tl))
    }

    pub(crate) fn treelist_pop_back(&self, tl: &RootSlot) -> Result<(Word, Word), OutOfMemory> {
        let node_slot = self.persistent_node(tl);
        let (rest_node, popped, _) = node_pop_back(self, &node_slot, PERSISTENT_OWNER)?;
        let popped_slot = self.root_slot(popped);
        let rest_tl = self.wrap_treelist(rest_node)?;
        Ok((popped_slot.get(), rest_tl))
    }

    pub(crate) fn mutable_treelist_pop_front(&self, mtl: &RootSlot) -> Result<Word, OutOfMemory> {
        self.update_mutable_treelist(mtl, |owner, node_slot| {
            let (popped, _, rest_node) = node_pop_front(self, node_slot, owner)?;
            Ok((rest_node, popped))
        })
    }

    pub(crate) fn mutable_treelist_pop_back(&self, mtl: &RootSlot) -> Result<Word, OutOfMemory> {
        self.update_mutable_treelist(mtl, |owner, node_slot| {
            let (rest_node, popped, _) = node_pop_back(self, node_slot, owner)?;
            Ok((rest_node, popped))
        })
    }

    pub(crate) fn treelist_concat(
        &self,
        left: &RootSlot,
        right: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let ln = self.persistent_node(left);
        let rn = self.persistent_node(right);
        let merged = node_concat(self, &ln, &rn, PERSISTENT_OWNER)?;
        self.wrap_treelist(merged)
    }

    pub(crate) fn mutable_treelist_append(
        &self,
        mtl: &RootSlot,
        other: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        let (owner, left_node) = {
            let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
            (obj.owner.load(Ordering::Relaxed), obj.node.get())
        };
        let other_node = if let Some(tl) = self.object_ref::<Treelist>(other.get()) {
            tl.node.get()
        } else {
            // If appending another MutableTreelist, bump its owner so shared chunks remain safe.
            let other_mtl = self.expect_object::<MutableTreelist>(other.get(), "mutable treelist");
            let node = other_mtl.node.get();
            if mtl.get() == other.get() {
                other_mtl
                    .owner
                    .store(self.alloc_treelist_owner(), Ordering::Relaxed);
                let ln = self.root_slot(left_node);
                let rn = self.root_slot(node);
                let merged = node_concat(self, &ln, &rn, PERSISTENT_OWNER)?;
                let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
                obj.owner
                    .store(self.alloc_treelist_owner(), Ordering::Relaxed);
                self.write_slot_object(obj, &obj.node, merged);
                return Ok(());
            }
            other_mtl
                .owner
                .store(self.alloc_treelist_owner(), Ordering::Relaxed);
            node
        };
        let ln = self.root_slot(left_node);
        let rn = self.root_slot(other_node);
        let new_owner = self.alloc_treelist_owner();
        let merged = node_concat(self, &ln, &rn, owner)?;
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        // Refresh owner after concatenation in case `other` was a persistent treelist or shared chunks.
        obj.owner.store(new_owner, Ordering::Relaxed);
        self.write_slot_object(obj, &obj.node, merged);
        Ok(())
    }

    pub(crate) fn treelist_split(
        &self,
        tl: &RootSlot,
        index: usize,
    ) -> Result<(Word, Word), OutOfMemory> {
        let node = self.persistent_node(tl);
        let (left_node, right_node) = node_split(self, &node, index, PERSISTENT_OWNER)?;
        let right_slot = self.root_slot(right_node);
        let left_tl = self.root_slot(self.wrap_treelist(left_node)?);
        let right_tl = self.wrap_treelist(right_slot.get())?;
        Ok((left_tl.get(), right_tl))
    }

    pub(crate) fn mutable_treelist_split(
        &self,
        mtl: &RootSlot,
        index: usize,
    ) -> Result<Word, OutOfMemory> {
        let node = self.root_slot(
            self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist")
                .node
                .get(),
        );
        // Split cleanly in persistent mode so neither side shares mutable chunk ownership.
        let (left_node, right_node) = node_split(self, &node, index, PERSISTENT_OWNER)?;
        let right_slot = self.root_slot(right_node);
        let left_owner = self.alloc_treelist_owner();
        let right_owner = self.alloc_treelist_owner();
        let mtl_obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        mtl_obj.owner.store(left_owner, Ordering::Relaxed);
        self.write_slot_object(mtl_obj, &mtl_obj.node, left_node);
        self.wrap_mutable_treelist(right_owner, right_slot.get())
    }

    pub(crate) fn treelist_subseq(
        &self,
        tl: &RootSlot,
        start: usize,
        end: usize,
    ) -> Result<Word, OutOfMemory> {
        let len = self.treelist_len(tl.get()).expect("treelist");
        if start == 0 && end == len {
            return Ok(tl.get());
        }
        if start == end {
            return self.treelist_empty();
        }
        let node = self.persistent_node(tl);
        let sub_node = node_subseq(self, &node, start, end)?;
        self.wrap_treelist(sub_node)
    }

    pub(crate) fn mutable_treelist_subseq(
        &self,
        mtl: &RootSlot,
        start: usize,
        end: usize,
    ) -> Result<Word, OutOfMemory> {
        let len = self.treelist_len(mtl.get()).expect("mutable treelist");
        if start == end {
            return self.mutable_treelist_empty();
        }
        let owner = self.alloc_treelist_owner();
        let mtl_obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        mtl_obj.owner.store(owner, Ordering::Relaxed);
        let node = self.root_slot(mtl_obj.node.get());
        let sub_node = if start == 0 && end == len {
            node.get()
        } else {
            node_subseq(self, &node, start, end)?
        };
        let right_owner = self.alloc_treelist_owner();
        self.wrap_mutable_treelist(right_owner, sub_node)
    }

    pub(crate) fn treelist_insert(
        &self,
        tl: &RootSlot,
        index: usize,
        value: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let node = self.persistent_node(tl);
        let new_node = node_insert(self, &node, index, value, PERSISTENT_OWNER)?;
        self.wrap_treelist(new_node)
    }

    pub(crate) fn mutable_treelist_insert(
        &self,
        mtl: &RootSlot,
        index: usize,
        value: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        let (owner, node) = {
            let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
            (obj.owner.load(Ordering::Relaxed), obj.node.get())
        };
        let node_slot = self.root_slot(node);
        let new_node = node_insert(self, &node_slot, index, value, owner)?;
        let new_owner = self.alloc_treelist_owner();
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        if index != 0 && index != node_weight(self, node_slot.get()) {
            obj.owner.store(new_owner, Ordering::Relaxed);
        }
        self.write_slot_object(obj, &obj.node, new_node);
        Ok(())
    }

    pub(crate) fn treelist_delete(&self, tl: &RootSlot, index: usize) -> Result<Word, OutOfMemory> {
        let node = self.persistent_node(tl);
        let new_node = node_delete(self, &node, index, PERSISTENT_OWNER)?;
        self.wrap_treelist(new_node)
    }

    pub(crate) fn mutable_treelist_delete(
        &self,
        mtl: &RootSlot,
        index: usize,
    ) -> Result<(), OutOfMemory> {
        let (owner, node) = {
            let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
            (obj.owner.load(Ordering::Relaxed), obj.node.get())
        };
        let node_slot = self.root_slot(node);
        let old_len = node_weight(self, node);
        let new_node = node_delete(self, &node_slot, index, owner)?;
        let new_owner = self.alloc_treelist_owner();
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        if index != 0 && index + 1 != old_len {
            obj.owner.store(new_owner, Ordering::Relaxed);
        }
        self.write_slot_object(obj, &obj.node, new_node);
        Ok(())
    }

    pub(crate) fn mutable_treelist_clear(&self, mtl: Word) {
        let obj = self.expect_object::<MutableTreelist>(mtl, "mutable treelist");
        obj.owner
            .store(self.alloc_treelist_owner(), Ordering::Relaxed);
        obj.node.set_unbarriered(Word::NIL);
    }

    pub(crate) fn mutable_treelist_copy(&self, mtl: &RootSlot) -> Result<Word, OutOfMemory> {
        let owner1 = self.alloc_treelist_owner();
        let owner2 = self.alloc_treelist_owner();
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        obj.owner.store(owner1, Ordering::Relaxed);
        self.wrap_mutable_treelist(owner2, obj.node.get())
    }

    pub(crate) fn treelist_thaw(&self, tl: &RootSlot) -> Result<Word, OutOfMemory> {
        let owner = self.alloc_treelist_owner();
        let node_slot = self.persistent_node(tl);
        self.wrap_mutable_treelist(owner, node_slot.get())
    }

    pub(crate) fn treelist_snapshot(&self, mtl: &RootSlot) -> Result<Word, OutOfMemory> {
        let new_owner = self.alloc_treelist_owner();
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        obj.owner.store(new_owner, Ordering::Relaxed);
        self.wrap_treelist(obj.node.get())
    }

    pub(crate) fn treelist_freeze(&self, mtl: &RootSlot) -> Result<Word, OutOfMemory> {
        let new_owner = self.alloc_treelist_owner();
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        let node = obj.node.get();
        obj.node.set_unbarriered(Word::NIL);
        obj.owner.store(new_owner, Ordering::Relaxed);
        self.wrap_treelist(node)
    }

    /// Visit every element in sequence order without safepoints.
    pub(crate) fn treelist_for_each(&self, word: Word, mut f: impl FnMut(Word)) {
        if let Some(node) = self.treelist_node(word) {
            node_for_each(self, node, &mut f);
        }
    }

    /// Visit the first `count` elements of a treelist without safepoints.
    pub(crate) fn treelist_for_each_prefix(
        &self,
        word: Word,
        count: usize,
        mut f: impl FnMut(Word),
    ) {
        if let Some(node) = self.treelist_node(word) {
            node_for_each_range(self, node, 0, count, &mut f);
        }
    }

    pub(crate) fn mutable_treelist_reverse_in_place(
        &self,
        mtl: &RootSlot,
    ) -> Result<(), OutOfMemory> {
        let len = self.treelist_len(mtl.get()).expect("mutable treelist");
        if len <= 1 {
            return Ok(());
        }
        let slots = self.root_frame(len);
        let mut cursor = len;
        self.treelist_for_each(mtl.get(), |word| {
            cursor -= 1;
            slots.set(cursor, word);
        });
        self.mutable_treelist_reset_from_slots(mtl, &slots, len)
    }

    pub(crate) fn mutable_treelist_reset_from_slots(
        &self,
        mtl: &RootSlot,
        slots: &RootFrame,
        len: usize,
    ) -> Result<(), OutOfMemory> {
        let owner = self.alloc_treelist_owner();
        let node = build_node_from_slots(self, slots, len, owner)?;
        let obj = self.expect_object::<MutableTreelist>(mtl.get(), "mutable treelist");
        obj.owner.store(owner, Ordering::Relaxed);
        self.write_slot_object(obj, &obj.node, node);
        Ok(())
    }

    pub(crate) fn treelist_copy_into_frame(
        &self,
        word: Word,
        start: usize,
        end: usize,
        dst: &crate::heap::RootSlots,
        dst_offset: usize,
    ) {
        let Some(node) = self.treelist_node(word) else {
            return;
        };
        let mut out = dst_offset;
        node_for_each_range(self, node, start, end, &mut |value| {
            dst.set(out, value);
            out += 1;
        });
    }
}
