use crate::OutOfMemory;

use super::object::{HeapObject, WordSlot};
use super::word::{Header, ObjectKind};
use super::{Heap, Word};

#[repr(C)]
pub(super) struct Cons {
    header: Header,
    pub(super) car: WordSlot,
    pub(super) cdr: WordSlot,
}

// SAFETY: `Cons` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Cons`.
unsafe impl HeapObject for Cons {
    const KIND: ObjectKind = ObjectKind::Cons;
}

impl Cons {
    #[inline]
    fn new(header: Header, car: Word, cdr: Word) -> Self {
        Self {
            header,
            car: WordSlot::new(car),
            cdr: WordSlot::new(cdr),
        }
    }
}

#[derive(Debug)]
pub(crate) struct CircularList {
    pub(crate) period: usize,
}

/// An incremental Brent cycle check. The caller owns the checkpoint so it
/// can keep that value rooted if the traversal invokes Lisp callbacks.
pub(crate) struct SpineCycle {
    interval: usize,
    steps: usize,
}

impl SpineCycle {
    pub(crate) fn new() -> Self {
        Self {
            interval: 1,
            steps: 0,
        }
    }

    /// Check one cdr advance. Success with `true` replaces the checkpoint
    /// with `next`. The initial checkpoint is the head of the spine.
    #[inline]
    pub(crate) fn step(&mut self, next: Word, checkpoint: Word) -> Result<bool, CircularList> {
        if next == checkpoint {
            return Err(CircularList {
                period: self.steps + 1,
            });
        }
        self.steps += 1;
        if self.steps == self.interval {
            self.interval *= 2;
            self.steps = 0;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// An allocation-free walk over the cdr chain of a Lisp value.
/// Do not hold this cursor across an allocation that can move conses.
pub(crate) struct ListSpine {
    tail: Word,
    checkpoint: Word,
    cycle: SpineCycle,
}

impl ListSpine {
    pub(crate) fn new(list: Word) -> Self {
        Self {
            tail: list,
            checkpoint: list,
            cycle: SpineCycle::new(),
        }
    }

    #[inline]
    pub(crate) fn next(&mut self, heap: &Heap) -> Result<Option<(Word, Word)>, CircularList> {
        let Some((car, cdr)) = heap.cons_fields(self.tail) else {
            return Ok(None);
        };
        // On a detected cycle the cursor remains on the cons before the repeated tail.
        if self.cycle.step(cdr, self.checkpoint)? {
            self.checkpoint = cdr;
        }
        self.tail = cdr;
        Ok(Some((car, cdr)))
    }

    pub(crate) fn tail(&self) -> Word {
        self.tail
    }
}

impl Heap {
    #[inline]
    pub(crate) fn cons(&self, fields: impl FnOnce() -> (Word, Word)) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            let (car, cdr) = fields();
            Cons::new(header, car, cdr)
        })
    }

    #[inline]
    pub(crate) fn try_cons_without_collection(
        &self,
        car: Word,
        cdr: Word,
    ) -> Result<Option<Word>, OutOfMemory> {
        self.try_alloc_object(|header| Cons::new(header, car, cdr))
    }

    #[inline]
    pub(crate) fn cons_fields(&self, word: Word) -> Option<(Word, Word)> {
        let cons = self.object_ref::<Cons>(word)?;
        Some((cons.car.get(), cons.cdr.get()))
    }

    #[inline(always)]
    pub(crate) fn cons_car(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<Cons>(word)?.car.get())
    }

    #[inline(always)]
    pub(crate) fn cons_cdr(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<Cons>(word)?.cdr.get())
    }

    #[inline]
    pub(crate) fn list_field(&self, word: Word, car: bool) -> Option<Word> {
        if word == Word::NIL {
            return Some(word);
        }
        if car {
            self.cons_car(word)
        } else {
            self.cons_cdr(word)
        }
    }

    pub(crate) fn set_cons_car(&self, word: Word, value: Word) {
        let cons = self.expect_object::<Cons>(word, "cons object");
        self.write_slot_object(cons, &cons.car, value);
    }

    pub(crate) fn set_cons_cdr(&self, word: Word, value: Word) {
        let cons = self.expect_object::<Cons>(word, "cons object");
        self.write_slot_object(cons, &cons.cdr, value);
    }

    #[inline(always)]
    pub(crate) fn atomic_load_cons_car(&self, word: Word, order: std::sync::atomic::Ordering) -> Option<Word> {
        Some(self.object_ref::<Cons>(word)?.car.load_ordered(order))
    }

    #[inline(always)]
    pub(crate) fn atomic_load_cons_cdr(&self, word: Word, order: std::sync::atomic::Ordering) -> Option<Word> {
        Some(self.object_ref::<Cons>(word)?.cdr.load_ordered(order))
    }

    pub(crate) fn atomic_store_cons_car(
        &self,
        word: Word,
        value: Word,
        order: std::sync::atomic::Ordering,
    ) -> Option<()> {
        let cons = self.object_ref::<Cons>(word)?;
        self.atomic_store_slot(cons, &cons.car, value, order);
        Some(())
    }

    pub(crate) fn atomic_store_cons_cdr(
        &self,
        word: Word,
        value: Word,
        order: std::sync::atomic::Ordering,
    ) -> Option<()> {
        let cons = self.object_ref::<Cons>(word)?;
        self.atomic_store_slot(cons, &cons.cdr, value, order);
        Some(())
    }

    pub(crate) fn atomic_cas_cons_car(
        &self,
        word: Word,
        expected: Word,
        new: Word,
        success: std::sync::atomic::Ordering,
        failure: std::sync::atomic::Ordering,
    ) -> Option<Result<Word, Word>> {
        let cons = self.object_ref::<Cons>(word)?;
        Some(self.atomic_cas_slot(cons, &cons.car, expected, new, success, failure))
    }

    pub(crate) fn atomic_cas_cons_cdr(
        &self,
        word: Word,
        expected: Word,
        new: Word,
        success: std::sync::atomic::Ordering,
        failure: std::sync::atomic::Ordering,
    ) -> Option<Result<Word, Word>> {
        let cons = self.object_ref::<Cons>(word)?;
        Some(self.atomic_cas_slot(cons, &cons.cdr, expected, new, success, failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gc_cons_leaves_collection_and_relocated_inputs_to_the_caller() {
        let mut config = Heap::builder();
        config.stress_bytes = Some(1);
        config.always_defrag = true;
        let heap = Heap::start(config).unwrap();
        let first = heap.root_slot(
            heap.try_cons_without_collection(Word::T, Word::NIL)
                .unwrap()
                .unwrap(),
        );
        let original = first.get();
        assert_eq!(heap.collections(), 0);
        assert_eq!(
            heap.try_cons_without_collection(original, original),
            Ok(None)
        );
        assert_eq!(heap.collections(), 0);

        let second = heap.cons(|| (first.get(), first.get())).unwrap();
        assert_ne!(heap.collections(), 0);
        assert_ne!(first.get(), original);
        assert_eq!(heap.cons_fields(second), Some((first.get(), first.get())));
        assert_eq!(heap.cons_fields(first.get()), Some((Word::T, Word::NIL)));
    }
}
