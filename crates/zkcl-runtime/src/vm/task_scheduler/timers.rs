use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::TaskId;
use super::cancellation::BoundarySignal;

const NONE: u32 = u32::MAX;
const ARITY: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimerKey {
    deadline: u64,
    sequence: u64,
}

pub(super) enum TimerEvent {
    Wake(TaskId),
    Timeout(TaskId, Rc<BoundarySignal>),
}

struct TimerSlot {
    generation: u64,
    state: SlotState,
}

enum SlotState {
    Free { next_free: u32 },
    Single(TimerEvent),
    InHeap { heap_index: u32, event: TimerEvent },
}

#[derive(Default)]
struct TimerEntries {
    slots: Vec<TimerSlot>,
    free_head: Option<u32>,
    single: Option<(TimerKey, u32)>,
    heap: Vec<(TimerKey, u32)>,
}

impl TimerEntries {
    fn alloc_slot(&mut self, state: SlotState) -> (u32, u64) {
        if let Some(index) = self.free_head {
            let slot = &mut self.slots[index as usize];
            let SlotState::Free { next_free } = slot.state else {
                unreachable!("free timer slot list corrupted");
            };
            self.free_head = (next_free != NONE).then_some(next_free);
            slot.state = state;
            return (index, slot.generation);
        }
        let index = u32::try_from(self.slots.len()).expect("timer slot count fits in u32");
        self.slots.push(TimerSlot {
            generation: 0,
            state,
        });
        (index, 0)
    }

    fn free_slot(&mut self, index: u32) -> TimerEvent {
        let next_free = self.free_head.unwrap_or(NONE);
        let slot = &mut self.slots[index as usize];
        slot.generation = slot.generation.wrapping_add(1);
        let event = match std::mem::replace(&mut slot.state, SlotState::Free { next_free }) {
            SlotState::Single(event) | SlotState::InHeap { event, .. } => event,
            SlotState::Free { .. } => unreachable!("freeing already-free timer slot"),
        };
        self.free_head = Some(index);
        event
    }

    fn set_heap_index(&mut self, slot_index: u32, heap_index: usize) {
        match &mut self.slots[slot_index as usize].state {
            SlotState::InHeap {
                heap_index: pos, ..
            } => *pos = heap_index as u32,
            _ => unreachable!("heap entry points to non-heap slot"),
        }
    }

    fn push_heap(&mut self, key: TimerKey, slot_index: u32) {
        let pos = self.heap.len();
        self.heap.push((key, slot_index));
        self.sift_up(pos);
    }

    fn sift_up(&mut self, mut pos: usize) {
        let entry = self.heap[pos];
        while pos > 0 {
            let parent = (pos - 1) / ARITY;
            if entry.0 >= self.heap[parent].0 {
                break;
            }
            let moved = self.heap[parent];
            self.heap[pos] = moved;
            self.set_heap_index(moved.1, pos);
            pos = parent;
        }
        self.heap[pos] = entry;
        self.set_heap_index(entry.1, pos);
    }

    fn sift_down(&mut self, mut pos: usize) {
        let len = self.heap.len();
        let entry = self.heap[pos];
        loop {
            let first_child = pos * ARITY + 1;
            if first_child >= len {
                break;
            }
            let end = (first_child + ARITY).min(len);
            let mut best = first_child;
            for child in (first_child + 1)..end {
                if self.heap[child].0 < self.heap[best].0 {
                    best = child;
                }
            }
            if entry.0 <= self.heap[best].0 {
                break;
            }
            let moved = self.heap[best];
            self.heap[pos] = moved;
            self.set_heap_index(moved.1, pos);
            pos = best;
        }
        self.heap[pos] = entry;
        self.set_heap_index(entry.1, pos);
    }

    fn remove_heap_at(&mut self, pos: usize) {
        let last = self.heap.pop().expect("heap is non-empty");
        if pos < self.heap.len() {
            let replacing_key = self.heap[pos].0;
            self.heap[pos] = last;
            self.set_heap_index(last.1, pos);
            if last.0 < replacing_key {
                self.sift_up(pos);
            } else {
                self.sift_down(pos);
            }
        }
    }

    fn min_key(&self) -> Option<TimerKey> {
        self.single
            .map(|(key, _)| key)
            .or_else(|| self.heap.first().map(|(key, _)| *key))
    }
}

#[derive(Default)]
struct TimerQueue {
    first: Cell<Option<TimerKey>>,
    entries: RefCell<TimerEntries>,
}

impl TimerQueue {
    fn insert(&self, key: TimerKey, event: TimerEvent) -> (u32, u64) {
        let mut entries = self.entries.borrow_mut();
        if self.first.get().is_none() {
            let (slot_index, generation) = entries.alloc_slot(SlotState::Single(event));
            entries.single = Some((key, slot_index));
            self.first.set(Some(key));
            return (slot_index, generation);
        }
        if let Some((single_key, single_slot)) = entries.single.take() {
            // Once timers overlap, promote the inline single entry into the 4-ary heap.
            let slot = &mut entries.slots[single_slot as usize];
            let SlotState::Single(single_event) =
                std::mem::replace(&mut slot.state, SlotState::Free { next_free: NONE })
            else {
                unreachable!("single entry holds a Single slot");
            };
            slot.state = SlotState::InHeap {
                heap_index: 0,
                event: single_event,
            };
            entries.push_heap(single_key, single_slot);
        }
        let heap_pos = entries.heap.len() as u32;
        let (slot_index, generation) = entries.alloc_slot(SlotState::InHeap {
            heap_index: heap_pos,
            event,
        });
        entries.push_heap(key, slot_index);
        self.first.set(entries.min_key());
        (slot_index, generation)
    }

    fn remove(&self, slot_index: u32, generation: u64) {
        let mut entries = self.entries.borrow_mut();
        let Some(slot) = entries.slots.get(slot_index as usize) else {
            return;
        };
        if slot.generation != generation {
            return;
        }
        match slot.state {
            SlotState::Free { .. } => return,
            SlotState::Single(_) => {
                entries.single = None;
                self.first.set(None);
            }
            SlotState::InHeap { heap_index, .. } => {
                entries.remove_heap_at(heap_index as usize);
                self.first.set(entries.min_key());
            }
        }
        drop(entries.free_slot(slot_index));
    }

    fn pop_due(&self, now: u64) -> Option<TimerEvent> {
        if self.first.get()?.deadline > now {
            return None;
        }
        let mut entries = self.entries.borrow_mut();
        let slot_index = if let Some((_, single_slot)) = entries.single.take() {
            self.first.set(None);
            single_slot
        } else {
            let (_, slot_index) = entries.heap[0];
            entries.remove_heap_at(0);
            self.first.set(entries.min_key());
            slot_index
        };
        Some(entries.free_slot(slot_index))
    }
}

/// Dropping a wait or timeout removes its entry in O(log_4 N) without allocating.
pub(super) struct TimerRegistration {
    queue: Rc<TimerQueue>,
    slot_index: u32,
    generation: u64,
}

impl Drop for TimerRegistration {
    fn drop(&mut self) {
        self.queue.remove(self.slot_index, self.generation);
    }
}

#[derive(Default)]
pub(super) struct Timers {
    queue: Option<Rc<TimerQueue>>,
    sequence: u64,
}

impl Timers {
    pub(super) fn next_deadline(&self) -> Option<u64> {
        Some(self.queue.as_ref()?.first.get()?.deadline)
    }

    pub(super) fn register(&mut self, deadline: u64, event: TimerEvent) -> TimerRegistration {
        let key = TimerKey {
            deadline,
            sequence: self.sequence,
        };
        self.sequence += 1;
        let queue = self.queue.get_or_insert_with(Rc::default);
        let (slot_index, generation) = queue.insert(key, event);
        TimerRegistration {
            queue: Rc::clone(queue),
            slot_index,
            generation,
        }
    }

    pub(super) fn pop_due(&self, now: u64) -> Option<TimerEvent> {
        self.queue.as_ref()?.pop_due(now)
    }
}

#[cfg(test)]
mod tests {
    use super::{TimerEvent, Timers};

    #[test]
    fn cancelling_and_expiring_the_minimum_keep_deadline_order() {
        let mut timers = Timers::default();
        let later = timers.register(20, TimerEvent::Wake(1));
        let cancelled = timers.register(10, TimerEvent::Wake(2));
        let first = timers.register(15, TimerEvent::Wake(3));
        let second = timers.register(15, TimerEvent::Wake(4));
        assert_eq!(timers.next_deadline(), Some(10));
        drop(cancelled);
        assert_eq!(timers.next_deadline(), Some(15));
        assert!(timers.pop_due(14).is_none());
        assert!(matches!(timers.pop_due(15), Some(TimerEvent::Wake(3))));
        drop(first);
        assert_eq!(timers.next_deadline(), Some(15));
        assert!(matches!(timers.pop_due(15), Some(TimerEvent::Wake(4))));
        drop(second);
        assert_eq!(timers.next_deadline(), Some(20));
        drop(later);
        assert_eq!(timers.next_deadline(), None);
        assert!(timers.pop_due(u64::MAX).is_none());
    }

    #[test]
    fn four_ary_heap_reuses_slots_and_preserves_order_across_many_cancellations() {
        let mut timers = Timers::default();
        let mut active = Vec::new();
        for i in 0u64..2000 {
            let deadline = ((i * 7919) % 1000) + 1;
            active.push((
                deadline,
                i + 1,
                timers.register(deadline, TimerEvent::Wake(i + 1)),
            ));
        }
        // Cancel every 3rd entry out of order.
        let mut kept = Vec::new();
        for (idx, item) in active.into_iter().enumerate() {
            if idx % 3 == 0 {
                drop(item.2);
            } else {
                kept.push(item);
            }
        }
        // Register new batch to verify free-list reuse and generation safety.
        for j in 0u64..500 {
            let deadline = ((j * 104729) % 500) + 1;
            let id = 10_000 + j;
            kept.push((
                deadline,
                id,
                timers.register(deadline, TimerEvent::Wake(id)),
            ));
        }
        kept.sort_by_key(|&(deadline, id, _)| {
            (deadline, if id >= 10_000 { 2000 + id } else { id })
        });
        let mut popped = Vec::new();
        while let Some(TimerEvent::Wake(id)) = timers.pop_due(u64::MAX) {
            popped.push(id);
        }
        assert_eq!(popped.len(), kept.len());
        // Deadlines must be strictly non-decreasing.
        let id_to_deadline: std::collections::HashMap<u64, u64> =
            kept.iter().map(|&(d, id, _)| (id, d)).collect();
        for pair in popped.windows(2) {
            assert!(id_to_deadline[&pair[0]] <= id_to_deadline[&pair[1]]);
        }
        drop(kept);
        assert_eq!(timers.next_deadline(), None);
    }
}
