//! Lisp's object graph and non-heap resources observed during collection.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rustc_hash::FxHashSet;

use super::immix::{Collection, YoungMarkView};
use super::roots::{ForeignStorage, HeapRoots};
use super::weak::{Ephemerons, WeakObjects};
use super::{Address, Header, RememberedSet, SlotVisitor, ValueSlot, Word, scan_object_slots};
use crate::code_store::CodeId;

const SLOT_BATCH: usize = 512;
const OBJECT_BATCH: usize = 128;

struct SlotSpan {
    start: Address,
    remaining: usize,
}

fn take_slot_batch(spans: &mut Vec<SlotSpan>) -> Option<SlotSpan> {
    let span = spans.last_mut()?;
    let count = span.remaining.min(SLOT_BATCH);
    let batch = SlotSpan {
        start: span.start,
        remaining: count,
    };
    span.start = span.start + count * ValueSlot::STRIDE;
    span.remaining -= count;
    if span.remaining == 0 {
        spans.pop();
    }
    Some(batch)
}

#[derive(Default)]
pub(super) struct HeapTrace {
    pending: Vec<Address>,
    spans: Vec<SlotSpan>,
    weak: WeakObjects,
    ephemerons: Ephemerons,
    foreign_edges: Vec<NonNull<ForeignStorage>>,
    pub(super) foreign_epoch: u64,
    pub(super) foreign_ids: FxHashSet<u64>,
    #[cfg(test)]
    parallel_minor_workers: usize,
}

impl HeapTrace {
    pub(super) fn run(
        &mut self,
        collection: &mut Collection<'_>,
        roots: &HeapRoots,
        tls_roots: &mut [Word],
        remembered: &mut RememberedSet,
    ) -> Option<FxHashSet<CodeId>> {
        let minor = collection.is_minor();
        let mut live_code = (!minor).then(FxHashSet::default);
        let mut foreign_ids = if minor {
            None
        } else {
            self.foreign_ids.clear();
            self.foreign_edges.clear();
            remembered.objects.clear();
            for card in remembered.cards.drain(..) {
                card.clear();
            }
            Some(&mut self.foreign_ids)
        };

        let mut visitor = TraceSlots {
            collection,
            pending: &mut self.pending,
            spans: &mut self.spans,
            ephemerons: &mut self.ephemerons,
            foreign_edges: &mut self.foreign_edges,
        };
        roots.visit_slots(&mut visitor);
        const _: () = assert!(
            size_of::<Word>() == size_of::<std::sync::atomic::AtomicUsize>()
                && align_of::<Word>() == align_of::<std::sync::atomic::AtomicUsize>()
        );
        if !tls_roots.is_empty() {
            visitor.visit_array(Address::from_ptr(tls_roots.as_mut_ptr()), tls_roots.len());
        }
        if minor {
            // Host writes dirty their own storage. Its foreign owner remains
            // allocated until a major collection clears this list.
            for index in 0..visitor.foreign_edges.len() {
                let storage = visitor.foreign_edges[index];
                // SAFETY: the host table still owns this mature foreign edge.
                unsafe { storage.as_ref() }.trace(true, &mut visitor);
            }
            for card in remembered.cards.drain(..) {
                card.visit_slots(&mut visitor);
                card.clear();
            }
            for owner in remembered.objects.drain(..) {
                // Restore UNLOGGED before scanning slots; surviving young targets are promoted
                // to mature, so `owner` only needs re-logging on future young writes.
                // SAFETY: `owner` stays allocated with a valid header until the next major GC.
                unsafe { (*owner.to_ptr::<Header>()).mark_unlogged() };
                scan_object_slots(owner, &mut visitor, &mut self.weak, None);
            }
        }

        // Each newly reachable weak table may add edges after its bucket link
        // is forwarded. Close both the strong and conditional graphs first.
        let mut registered_tables = 0;
        let mut parallel_workers = visitor.collection.minor_workers();
        loop {
            if parallel_workers > 1 && visitor.has_parallel_frontier() {
                let workers = visitor.mark_parallel_young(&mut self.weak, parallel_workers);
                #[cfg(test)]
                {
                    self.parallel_minor_workers += workers;
                }
                let _ = workers;
                parallel_workers = 1;
            }
            while let Some(address) = visitor.pending.pop() {
                let code = scan_object_slots(
                    address,
                    &mut visitor,
                    &mut self.weak,
                    foreign_ids.as_deref_mut(),
                );
                if let (Some(id), Some(live_code)) = (code, &mut live_code) {
                    live_code.insert(id);
                }
            }
            if parallel_workers > 1 && visitor.has_parallel_frontier() {
                continue;
            }
            if visitor.trace_next_batch() {
                continue;
            }
            // Ephemeron registration and the edges it exposes are owned by the
            // collecting thread. The parallel phase only visits strong edges.
            parallel_workers = 1;
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
            if visitor.pending.is_empty() {
                break;
            }
        }
        let TraceSlots {
            collection,
            ephemerons,
            ..
        } = visitor;
        for table in self.weak.tables.drain(..) {
            super::hash_table::sweep_weak_entries(table, |word| collection.live_word(word));
        }
        for pointer in self.weak.pointers.drain(..) {
            super::weak::update(pointer, |word| collection.live_word(word));
        }
        ephemerons.clear();
        if !minor {
            self.foreign_epoch = self.foreign_epoch.saturating_add(1);
        }
        live_code
    }
}

struct TraceSlots<'trace, 'heap> {
    collection: &'trace mut Collection<'heap>,
    ephemerons: &'trace mut Ephemerons,
    pending: &'trace mut Vec<Address>,
    spans: &'trace mut Vec<SlotSpan>,
    foreign_edges: &'trace mut Vec<NonNull<ForeignStorage>>,
}

impl TraceSlots<'_, '_> {
    fn has_parallel_frontier(&self) -> bool {
        self.pending.len() >= 2 * OBJECT_BATCH
            || self
                .spans
                .iter()
                .any(|span| span.remaining >= 2 * SLOT_BATCH)
    }

    fn mark_parallel_young(&mut self, weak: &mut WeakObjects, workers: usize) -> usize {
        let mut spans = Vec::new();
        while let Some(batch) = take_slot_batch(self.spans) {
            spans.push(batch);
        }

        let view = self.collection.young_mark_view();
        let next = AtomicUsize::new(0);
        let pending = self.pending.as_slice();
        let mark = || mark_young_jobs(&view, pending, &spans, &next);
        let worker_count = workers.min(pending.len().div_ceil(OBJECT_BATCH) + spans.len());
        let (worker_count, results) = thread::scope(|scope| {
            let mut running = Vec::with_capacity(worker_count - 1);
            for _ in 1..worker_count {
                match thread::Builder::new().spawn_scoped(scope, mark) {
                    Ok(worker) => running.push(worker),
                    Err(_) => break,
                }
            }
            let local = mark();
            let worker_count = 1 + running.len();
            let results = std::iter::once(local)
                .chain(running.into_iter().map(|worker| worker.join().unwrap()))
                .collect::<Vec<_>>();
            (worker_count, results)
        });
        self.pending.clear();
        for marked in results {
            self.collection
                .finish_young_mark(marked.bytes, marked.large_objects);
            weak.pointers.extend(marked.weak.pointers);
            weak.tables.extend(marked.weak.tables);
            for address in marked.foreign_edges {
                self.visit_foreign_edge(address);
            }
        }
        worker_count
    }

    #[inline]
    fn visit(&mut self, slot: ValueSlot) {
        if let Some((source, address)) = self.collection.trace_slot(slot) {
            self.ephemerons.marked(source);
            self.pending.push(address);
        }
    }

    #[inline]
    fn trace_next_batch(&mut self) -> bool {
        let Some(batch) = take_slot_batch(self.spans) else {
            return false;
        };
        for slot in ValueSlot::array(batch.start, batch.remaining) {
            self.visit_slot(slot);
        }
        true
    }
}

#[derive(Default)]
struct YoungMarked {
    bytes: usize,
    large_objects: Vec<Address>,
    foreign_edges: Vec<Address>,
    weak: WeakObjects,
}

struct YoungSlots<'a> {
    view: &'a YoungMarkView<'a>,
    pending: Vec<Address>,
    spans: Vec<SlotSpan>,
    marked: YoungMarked,
}

impl SlotVisitor for YoungSlots<'_> {
    fn visit_slot(&mut self, slot: ValueSlot) {
        if let Some((address, bytes)) = self.view.trace_slot(slot) {
            self.marked.bytes += bytes;
            if bytes == 0 {
                self.marked.large_objects.push(address);
            }
            self.pending.push(address);
        }
    }

    fn visit_foreign_edge(&mut self, base: Address) {
        // The foreign slot storage can be shared by several wrappers and
        // uses a non-atomic RefCell borrow flag. Scan it after joining.
        self.marked.foreign_edges.push(base);
    }

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

fn mark_young_jobs(
    view: &YoungMarkView<'_>,
    pending: &[Address],
    spans: &[SlotSpan],
    next: &AtomicUsize,
) -> YoungMarked {
    let mut visitor = YoungSlots {
        view,
        pending: Vec::new(),
        spans: Vec::new(),
        marked: YoungMarked::default(),
    };
    let mut weak = WeakObjects::default();
    let object_batches = pending.len().div_ceil(OBJECT_BATCH);
    loop {
        let job = next.fetch_add(1, Ordering::Relaxed);
        if job < object_batches {
            let start = job * OBJECT_BATCH;
            let end = (start + OBJECT_BATCH).min(pending.len());
            visitor.pending.extend_from_slice(&pending[start..end]);
        } else if let Some(span) = spans.get(job - object_batches) {
            visitor.visit_array(span.start, span.remaining);
        } else {
            break;
        }
        loop {
            while let Some(address) = visitor.pending.pop() {
                scan_object_slots(address, &mut visitor, &mut weak, None);
            }
            let Some(batch) = take_slot_batch(&mut visitor.spans) else {
                break;
            };
            visitor.visit_array(batch.start, batch.remaining);
        }
    }
    visitor.marked.weak = weak;
    visitor.marked
}

impl SlotVisitor for TraceSlots<'_, '_> {
    #[inline]
    fn visit_slot(&mut self, slot: ValueSlot) {
        self.visit(slot);
    }

    fn visit_foreign_edge(&mut self, base: Address) {
        if let Some(storage) = super::foreign::trace_edge(base, self) {
            self.foreign_edges.push(storage);
        }
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
    use std::num::NonZeroUsize;

    use super::SLOT_BATCH;
    use crate::RuntimeBuilder;
    use crate::heap::{ArrayElement, ForeignQueue, Heap, HeapConfig, RootSlot, Word};

    #[test]
    fn minor_collection_revisits_mature_foreign_slots() {
        let runtime = RuntimeBuilder::new().start().unwrap();
        let heap = &runtime.heap;
        let queue = ForeignQueue::new(&runtime);
        let wrapper = runtime.foreign_with_edge((), Some(queue.edge())).unwrap();
        heap.collect_minor();

        let value = runtime.make_string("sent after promotion").unwrap();
        let weak = heap.root_slot(heap.weak_pointer(&value.slot).unwrap());
        assert!(queue.try_push(value.slot.get(), None));
        drop(value);
        heap.collect_minor();

        let value = queue.pop().unwrap();
        assert_eq!(heap.weak_pointer_value(weak.get()), Some(value));
        assert_eq!(
            heap.string_contents(value).as_deref(),
            Some("sent after promotion")
        );
        heap.collect();
        assert_eq!(heap.weak_pointer_value(weak.get()), None);
        drop(wrapper);
    }

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

    #[test]
    fn parallel_minor_mark_matches_serial_for_wide_cyclic_and_weak_edges() {
        fn run(workers: NonZeroUsize) -> (usize, usize) {
            let heap = Heap::start(HeapConfig {
                gc_workers: workers,
                ..HeapConfig::default()
            })
            .unwrap();
            let len = 8 * SLOT_BATCH + 1;
            let array = heap.root_slot(
                heap.array(&[len], ArrayElement::General, false, None, |_| Word::NIL)
                    .unwrap(),
            );
            let live = heap.root_slot(heap.string(&"a".repeat(9000)).unwrap());
            let dead = heap.root_slot(heap.string("only weakly reachable").unwrap());
            let weak_live = heap.root_slot(heap.weak_pointer(&live).unwrap());
            let weak_dead = heap.root_slot(heap.weak_pointer(&dead).unwrap());

            let set = |index, value| {
                assert!(heap.set_array_slot(array.get(), index, value).unwrap());
            };
            set(0, weak_live.get());
            set(1, weak_dead.get());
            set(2, array.get());
            for index in (SLOT_BATCH..len).step_by(SLOT_BATCH) {
                set(index, live.get());
            }
            let target = live.get();
            drop((live, dead, weak_live, weak_dead));
            heap.collect_minor();

            let slot = |index| heap.array_slot(array.get(), index).unwrap().unwrap();
            assert_eq!(slot(2), array.get());
            assert_eq!(slot(len - 1), target);
            assert_eq!(heap.weak_pointer_value(slot(0)), Some(target));
            assert_eq!(heap.weak_pointer_value(slot(1)), None);
            let workers = heap.trace.borrow().parallel_minor_workers;
            (heap.stats().live_bytes, workers)
        }

        let (serial_bytes, serial_workers) = run(NonZeroUsize::MIN);
        let (parallel_bytes, parallel_workers) = run(NonZeroUsize::new(4).unwrap());
        assert_eq!(serial_workers, 0);
        assert_eq!(parallel_bytes, serial_bytes);
        if std::thread::available_parallelism().is_ok_and(|workers| workers.get() > 1) {
            assert!(parallel_workers > 1);
        }
    }
}
