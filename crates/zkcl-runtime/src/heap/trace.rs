//! Lisp's object graph and non-heap resources observed during collection.

use rustc_hash::FxHashSet;

use super::gray_packet::{GrayPacket, GrayPacketPool};
use super::immix::Collection;
use super::roots::HeapRoots;
use super::weak::{Ephemerons, WeakObjects};
use super::{Address, Header, SlotVisitor, ValueSlot, Word, scan_object_slots};
use crate::code_store::CodeId;

const SLOT_BATCH: usize = 512;

struct SlotSpan {
    start: Address,
    remaining: usize,
}

#[derive(Default)]
pub(super) struct HeapTrace {
    pool: GrayPacketPool,
    spans: Vec<SlotSpan>,
    weak: WeakObjects,
    ephemerons: Ephemerons,
    pub(super) foreign_epoch: u64,
    pub(super) foreign_ids: FxHashSet<u64>,
}

impl HeapTrace {
    pub(super) fn run(
        &mut self,
        collection: &mut Collection<'_>,
        roots: &HeapRoots,
        tls_roots: &mut [Word],
        remembered: &mut Vec<Address>,
    ) -> Option<FxHashSet<CodeId>> {
        let minor = collection.is_minor();
        let mut live_code = FxHashSet::default();
        let mut scratch_foreigns = FxHashSet::default();
        if !minor {
            self.foreign_ids.clear();
            remembered.clear();
        }
        let foreign_ids = if minor {
            &mut scratch_foreigns
        } else {
            &mut self.foreign_ids
        };

        let mut in_packet = self.pool.pop_empty();
        let out_packet = self.pool.pop_empty();
        let mut visitor = TraceSlots {
            collection,
            pool: &self.pool,
            out_packet,
            spans: &mut self.spans,
            ephemerons: &mut self.ephemerons,
        };
        roots.visit_slots(|slot| visitor.visit_slot(slot));
        const _: () = assert!(
            size_of::<Word>() == size_of::<std::sync::atomic::AtomicUsize>()
                && align_of::<Word>() == align_of::<std::sync::atomic::AtomicUsize>()
        );
        if !tls_roots.is_empty() {
            visitor.visit_array(Address::from_ptr(tls_roots.as_ptr()), tls_roots.len());
        }
        if minor {
            for owner in remembered.drain(..) {
                // Restore UNLOGGED before scanning slots; surviving young targets are promoted
                // to mature, so `owner` only needs re-logging on future young writes.
                // SAFETY: `owner` stays allocated with a valid header until the next major GC.
                unsafe { (*owner.to_ptr::<Header>()).mark_unlogged() };
                scan_object_slots(owner, &mut visitor, &mut self.weak, foreign_ids);
            }
        }

        // Each newly reachable weak table may add edges after its bucket link
        // is forwarded. Close both the strong and conditional graphs first.
        let mut registered_tables = 0;
        loop {
            loop {
                while let Some(address) = in_packet.pop() {
                    if let Some(id) =
                        scan_object_slots(address, &mut visitor, &mut self.weak, foreign_ids)
                    {
                        live_code.insert(id);
                    }
                }
                if let Some(full) = visitor.pool.pop_full() {
                    let empty = std::mem::replace(&mut in_packet, full);
                    visitor.pool.push_empty(empty);
                } else if !visitor.out_packet.is_empty() {
                    std::mem::swap(&mut in_packet, &mut visitor.out_packet);
                } else {
                    break;
                }
            }
            if visitor.trace_next_batch() {
                continue;
            }
            for &table in &self.weak.tables[registered_tables..] {
                super::hash_table::register_weak_entries(
                    table,
                    |word| visitor.collection.live_word(word).is_some(),
                    visitor.ephemerons,
                );
            }
            registered_tables = self.weak.tables.len();
            while let Some(slot) = visitor.ephemerons.ready.pop() {
                visitor.visit_slot(slot);
            }
            if visitor.out_packet.is_empty() {
                if let Some(full) = visitor.pool.pop_full() {
                    let empty = std::mem::replace(&mut in_packet, full);
                    visitor.pool.push_empty(empty);
                } else {
                    break;
                }
            }
        }
        let TraceSlots {
            collection,
            out_packet,
            ephemerons,
            ..
        } = visitor;
        self.pool.push_empty(in_packet);
        self.pool.push_empty(out_packet);
        for table in self.weak.tables.drain(..) {
            super::hash_table::sweep_weak_entries(table, |word| collection.live_word(word));
        }
        for pointer in self.weak.pointers.drain(..) {
            super::weak::update(pointer, |word| collection.live_word(word));
        }
        ephemerons.clear();
        if minor {
            None
        } else {
            self.foreign_epoch = self.foreign_epoch.saturating_add(1);
            Some(live_code)
        }
    }
}

struct TraceSlots<'trace, 'heap> {
    collection: &'trace mut Collection<'heap>,
    ephemerons: &'trace mut Ephemerons,
    pool: &'trace GrayPacketPool,
    out_packet: GrayPacket,
    spans: &'trace mut Vec<SlotSpan>,
}

impl TraceSlots<'_, '_> {
    #[inline]
    fn visit(&mut self, slot: ValueSlot) {
        if let Some((source, address)) = self.collection.trace_slot(slot) {
            self.ephemerons.marked(source);
            if !self.out_packet.push(address) {
                let full = std::mem::replace(&mut self.out_packet, self.pool.pop_empty());
                self.pool.push_full(full);
                let pushed = self.out_packet.push(address);
                debug_assert!(pushed);
            }
        }
    }

    #[inline]
    fn trace_next_batch(&mut self) -> bool {
        let Some(span) = self.spans.last_mut() else {
            return false;
        };
        // The span belongs to an already marked object, so its slots keep
        // their addresses until sweep. Drain the object worklist between batches.
        let count = span.remaining.min(SLOT_BATCH);
        let start = span.start;
        span.start = start + count * ValueSlot::STRIDE;
        span.remaining -= count;
        if span.remaining == 0 {
            self.spans.pop();
        }
        for slot in ValueSlot::array(start, count) {
            self.visit_slot(slot);
        }
        true
    }
}

impl SlotVisitor for TraceSlots<'_, '_> {
    #[inline]
    fn visit_slot(&mut self, slot: ValueSlot) {
        self.visit(slot);
    }

    #[inline]
    fn visit_array(&mut self, start: Address, count: usize) {
        if count <= SLOT_BATCH {
            for slot in ValueSlot::array(start, count) {
                self.visit_slot(slot);
            }
        } else {
            self.spans.push(SlotSpan {
                start,
                remaining: count,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SLOT_BATCH;
    use crate::heap::{ArrayElement, Heap, HeapConfig, RootSlot, Word};

    #[test]
    fn deferred_arrays_trace_cycles_and_late_strong_edges_before_weak_sweep() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let make_array = |len| {
            heap.root_slot(
                heap.array(&[len], ArrayElement::General, false, None, |_| Word::NIL)
                    .unwrap(),
            )
        };
        let outer = make_array(8 * SLOT_BATCH + 1);
        let inner = make_array(SLOT_BATCH + 1);
        let target = heap.root_slot(heap.string("reachable through the last batch").unwrap());
        let weak = heap.root_slot(heap.weak_pointer(&target).unwrap());
        let set = |array: &RootSlot, index, value| {
            assert!(heap.set_array_slot(array.get(), index, value).unwrap());
        };
        set(&outer, 0, weak.get());
        set(&outer, 8 * SLOT_BATCH, inner.get());
        set(&inner, 0, outer.get());
        set(&inner, SLOT_BATCH, target.get());
        let old_inner = inner.get();
        drop((inner, target, weak));

        heap.collect();
        let slot = |array, index| heap.array_slot(array, index).unwrap().unwrap();
        let inner = slot(outer.get(), 8 * SLOT_BATCH);
        assert_ne!(inner, old_inner);
        assert_eq!(slot(inner, 0), outer.get());
        let target = slot(inner, SLOT_BATCH);
        let weak = slot(outer.get(), 0);
        assert_eq!(heap.weak_pointer_value(weak), Some(target));
        assert!(heap.set_array_slot(inner, SLOT_BATCH, Word::NIL).unwrap());

        heap.collect();
        let weak = slot(outer.get(), 0);
        assert_eq!(heap.weak_pointer_value(weak), None);
    }
}
