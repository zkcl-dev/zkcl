use std::num::NonZeroUsize;

use rustc_hash::FxHashMap;

use super::{
    Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, RootSlot, ValueSlot, Word, WordSlot,
};

#[derive(Default)]
pub(super) struct WeakObjects {
    pub(super) pointers: Vec<Address>,
    pub(super) tables: Vec<Address>,
}

/// The index uses unmarked source addresses. Each conditional slot belongs
/// to an already marked bucket array, so its own address is stable until sweep.
#[derive(Default)]
pub(super) struct Ephemerons {
    waiting: FxHashMap<Word, NonZeroUsize>,
    deferred: Vec<DeferredEdge>,
    pub(super) ready: Vec<ValueSlot>,
}

struct DeferredEdge {
    slot: ValueSlot,
    next: Option<NonZeroUsize>,
}

impl Ephemerons {
    pub(super) fn clear(&mut self) {
        self.ready.clear();
        self.waiting.clear();
        self.deferred.clear();
    }

    pub(super) fn register(&mut self, source: Word, marked: bool, dependent: ValueSlot) {
        if marked {
            self.ready.push(dependent);
        } else {
            let index = NonZeroUsize::new(self.deferred.len() + 1).unwrap();
            let next = self.waiting.insert(source, index);
            self.deferred.push(DeferredEdge {
                slot: dependent,
                next,
            });
        }
    }

    #[inline]
    pub(super) fn marked(&mut self, source: Word) {
        if !self.waiting.is_empty()
            && let Some(mut index) = self.waiting.remove(&source)
        {
            loop {
                let edge = &self.deferred[index.get() - 1];
                self.ready.push(edge.slot);
                let Some(next) = edge.next else {
                    break;
                };
                index = next;
            }
        }
    }
}

#[repr(C)]
struct WeakPointer {
    header: Header,
    // UNBOUND distinguishes a cleared pointer from a live reference to NIL.
    referent: WordSlot,
}

// SAFETY: `WeakPointer` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::WeakPointer`.
unsafe impl HeapObject for WeakPointer {
    const KIND: ObjectKind = ObjectKind::WeakPointer;
}

impl Heap {
    pub(crate) fn weak_pointer(&self, target: &RootSlot) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| WeakPointer {
            header,
            referent: WordSlot::new(target.get()),
        })
    }

    pub(crate) fn is_weak_pointer(&self, word: Word) -> bool {
        self.object_ref::<WeakPointer>(word).is_some()
    }

    pub(crate) fn weak_pointer_value(&self, word: Word) -> Option<Word> {
        self.object_ref::<WeakPointer>(word)?.referent.get_bound()
    }
}

pub(super) fn update(base: Address, live: impl FnOnce(Word) -> Option<Word>) {
    // SAFETY: the collector recorded the destination of this live weak pointer.
    let pointer = unsafe { &*base.to_ptr::<WeakPointer>() };
    let current = pointer.referent.get();
    pointer
        .referent
        .set_unbarriered(live(current).unwrap_or(Word::UNBOUND));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::HeapConfig;

    #[test]
    fn weak_targets_follow_evacuation_and_clear_before_reuse() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            stress_bytes: Some(1),
            ..HeapConfig::default()
        })
        .unwrap();
        let target = heap.root_slot(heap.string("target").unwrap());
        let pointer = heap.root_slot(heap.weak_pointer(&target).unwrap());
        let nil = heap.root_slot(heap.weak_pointer(&heap.root_slot(Word::NIL)).unwrap());
        let large = heap.root_slot(heap.string(&"a".repeat(9000)).unwrap());
        let large_pointer = heap.root_slot(heap.weak_pointer(&large).unwrap());

        let before = target.get();
        heap.collect();
        assert_ne!(before, target.get());
        assert_eq!(heap.weak_pointer_value(pointer.get()), Some(target.get()));
        assert_eq!(
            heap.weak_pointer_value(large_pointer.get()),
            Some(large.get())
        );
        drop(target);
        drop(large);
        // Allocation pressure takes the same weak-processing path as explicit GC.
        heap.string("first allocation after explicit GC").unwrap();
        heap.string("collection on the next allocation").unwrap();
        for _ in 0..2 {
            assert_eq!(heap.weak_pointer_value(pointer.get()), None);
            assert_eq!(heap.weak_pointer_value(large_pointer.get()), None);
            assert_eq!(heap.weak_pointer_value(nil.get()), Some(Word::NIL));
            heap.collect();
        }
    }
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    heap.object_ref::<WeakPointer>(word)?;
    Some(("weak-pointer", vec![]))
}
pub(super) fn image_slot(word: Word) -> Option<ValueSlot> {
    let base = super::object_address(word, ObjectKind::WeakPointer)?;
    Some(ValueSlot(
        base + std::mem::offset_of!(WeakPointer, referent),
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if kind != "weak-pointer" {
        return Ok(None);
    }
    super::image::Data(data).done()?;
    heap.weak_pointer(&heap.root_slot(Word::UNBOUND))
        .map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
