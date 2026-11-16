use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::OutOfMemory;

use super::super::{Heap, RootFrame, RootSlot, Word, WordSlot};
use super::span::{
    alloc_chunk_from_fn, alloc_single_item_span, alloc_span, span_for_each, span_for_each_range,
    span_get, span_item_at, span_len, span_merge, span_pop_back, span_pop_front, span_push_back,
    span_push_front, span_set_at, span_set_slot, span_split, span_weight,
};
use super::{CHUNK_CAPACITY, PERSISTENT_OWNER, TreelistNode, can_mutate};

#[inline]
pub(super) fn node_weight(heap: &Heap, node: Word) -> usize {
    if node == Word::NIL {
        0
    } else {
        heap.expect_object::<TreelistNode>(node, "treelist node")
            .weight
            .load(Ordering::Relaxed)
    }
}

pub(super) fn node_get(heap: &Heap, mut node: Word, mut index: usize) -> Word {
    loop {
        let obj = heap.expect_object::<TreelistNode>(node, "treelist node");
        let front = obj.front.get();
        let fw = span_weight(heap, front);
        if index < fw {
            return span_get(heap, front, obj.depth, index);
        }
        index -= fw;
        let middle = obj.middle.get();
        let mw = node_weight(heap, middle);
        if index < mw {
            node = middle;
        } else {
            return span_get(heap, obj.back.get(), obj.depth, index - mw);
        }
    }
}

pub(super) fn node_for_each(heap: &Heap, node: Word, f: &mut impl FnMut(Word)) {
    if node == Word::NIL {
        return;
    }
    let obj = heap.expect_object::<TreelistNode>(node, "treelist node");
    let depth = obj.depth;
    let front = obj.front.get();
    let middle = obj.middle.get();
    let back = obj.back.get();
    span_for_each(heap, front, depth, f);
    node_for_each(heap, middle, f);
    span_for_each(heap, back, depth, f);
}

pub(super) fn node_for_each_range(
    heap: &Heap,
    node: Word,
    start: usize,
    end: usize,
    f: &mut impl FnMut(Word),
) {
    if node == Word::NIL || start >= end {
        return;
    }
    let obj = heap.expect_object::<TreelistNode>(node, "treelist node");
    let depth = obj.depth;
    let front = obj.front.get();
    let middle = obj.middle.get();
    let back = obj.back.get();
    let fw = span_weight(heap, front);
    if start < fw {
        span_for_each_range(heap, front, depth, start, end.min(fw), f);
    }
    let mw = node_weight(heap, middle);
    let mid_end = fw + mw;
    if start < mid_end && end > fw {
        node_for_each_range(
            heap,
            middle,
            start.saturating_sub(fw),
            (end - fw).min(mw),
            f,
        );
    }
    if end > mid_end {
        span_for_each_range(
            heap,
            back,
            depth,
            start.saturating_sub(mid_end),
            end - mid_end,
            f,
        );
    }
}

pub(super) fn alloc_node(
    heap: &Heap,
    owner: u64,
    depth: u8,
    front: &RootSlot,
    middle: &RootSlot,
    back: &RootSlot,
) -> Result<Word, OutOfMemory> {
    let fw = span_weight(heap, front.get());
    let mw = node_weight(heap, middle.get());
    let bw = span_weight(heap, back.get());
    let total = fw + mw + bw;
    if total == 0 {
        return Ok(Word::NIL);
    }
    heap.alloc_object(|header| TreelistNode {
        header,
        owner: AtomicU64::new(owner),
        weight: AtomicUsize::new(total),
        depth,
        front: WordSlot::new(front.get()),
        middle: WordSlot::new(middle.get()),
        back: WordSlot::new(back.get()),
    })
}

pub(super) fn build_node_from_slots(
    heap: &Heap,
    slots: &RootFrame,
    len: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    if len == 0 {
        return Ok(Word::NIL);
    }
    let chunk_count = len.div_ceil(CHUNK_CAPACITY);
    let spans = heap.root_frame(chunk_count);
    for c in 0..chunk_count {
        let base = c * CHUNK_CAPACITY;
        let count = (len - base).min(CHUNK_CAPACITY);
        let chunk = alloc_chunk_from_fn(heap, owner, count, |i| slots.get(base + i))?;
        let chunk_slot = heap.root_slot(chunk);
        let span = alloc_span(heap, owner, count, 0, count as u8, &chunk_slot)?;
        spans.set(c, span);
    }
    build_tree_from_spans(heap, &spans, chunk_count, 0, owner)
}

pub(super) fn build_repeated_node(
    heap: &Heap,
    count: usize,
    element: &RootSlot,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    if count == 0 {
        return Ok(Word::NIL);
    }
    let chunk_count = count.div_ceil(CHUNK_CAPACITY);
    let spans = heap.root_frame(chunk_count);
    for c in 0..chunk_count {
        let base = c * CHUNK_CAPACITY;
        let item_count = (count - base).min(CHUNK_CAPACITY);
        let chunk = alloc_chunk_from_fn(heap, owner, item_count, |_| element.get())?;
        let chunk_slot = heap.root_slot(chunk);
        let span = alloc_span(heap, owner, item_count, 0, item_count as u8, &chunk_slot)?;
        spans.set(c, span);
    }
    build_tree_from_spans(heap, &spans, chunk_count, 0, owner)
}

pub(super) fn build_tree_from_spans(
    heap: &Heap,
    spans: &RootFrame,
    count: usize,
    depth: u8,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    if count == 0 {
        return Ok(Word::NIL);
    }
    if count == 1 {
        let front = heap.root_slot(spans.get(0));
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, owner, depth, &front, &nil, &nil);
    }
    if count == 2 {
        let front = heap.root_slot(spans.get(0));
        let nil = heap.root_slot(Word::NIL);
        let back = heap.root_slot(spans.get(1));
        return alloc_node(heap, owner, depth, &front, &nil, &back);
    }
    let inner_count = count - 2;
    let parent_count = inner_count.div_ceil(CHUNK_CAPACITY);
    let parent_spans = heap.root_frame(parent_count);
    for p in 0..parent_count {
        let base = 1 + p * CHUNK_CAPACITY;
        let items = (count - 1 - base).min(CHUNK_CAPACITY);
        let mut weight = 0usize;
        for i in 0..items {
            weight += span_weight(heap, spans.get(base + i));
        }
        let chunk = alloc_chunk_from_fn(heap, owner, items, |i| spans.get(base + i))?;
        let chunk_slot = heap.root_slot(chunk);
        let span = alloc_span(heap, owner, weight, 0, items as u8, &chunk_slot)?;
        parent_spans.set(p, span);
    }
    let middle = build_tree_from_spans(heap, &parent_spans, parent_count, depth + 1, owner)?;
    let front = heap.root_slot(spans.get(0));
    let middle_slot = heap.root_slot(middle);
    let back = heap.root_slot(spans.get(count - 1));
    alloc_node(heap, owner, depth, &front, &middle_slot, &back)
}

pub(super) fn node_set_at(
    heap: &Heap,
    node_slot: &RootSlot,
    index: usize,
    value: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (node_owner, depth, front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        (
            node.owner.load(Ordering::Relaxed),
            node.depth,
            node.front.get(),
            node.middle.get(),
            node.back.get(),
        )
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);
    let fw = span_weight(heap, front.get());
    if index < fw {
        let new_front = span_set_at(heap, &front, depth, index, value, op_owner)?;
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, new_front);
            return Ok(node_slot.get());
        }
        let new_front_slot = heap.root_slot(new_front);
        return alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back);
    }
    let mw = node_weight(heap, middle.get());
    if index < fw + mw {
        let new_middle = node_set_at(heap, &middle, index - fw, value, op_owner)?;
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.middle, new_middle);
            return Ok(node_slot.get());
        }
        let new_middle_slot = heap.root_slot(new_middle);
        return alloc_node(heap, op_owner, depth, &front, &new_middle_slot, &back);
    }
    let new_back = span_set_at(heap, &back, depth, index - fw - mw, value, op_owner)?;
    if can_mutate(node_owner, op_owner) {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        heap.write_slot_object(node, &node.back, new_back);
        return Ok(node_slot.get());
    }
    let new_back_slot = heap.root_slot(new_back);
    alloc_node(heap, op_owner, depth, &front, &middle, &new_back_slot)
}

pub(super) fn node_push_front(
    heap: &Heap,
    node_slot: &RootSlot,
    depth: u8,
    item: &RootSlot,
    item_weight: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if node_slot.get() == Word::NIL {
        let front = heap.root_slot(alloc_single_item_span(heap, item, item_weight, op_owner)?);
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, &front, &nil, &nil);
    }
    let (node_owner, old_weight, front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        debug_assert_eq!(node.depth, depth);
        (
            node.owner.load(Ordering::Relaxed),
            node.weight.load(Ordering::Relaxed),
            node.front.get(),
            node.middle.get(),
            node.back.get(),
        )
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);

    if depth >= 1 {
        let first_child = heap.root_slot(span_item_at(heap, front.get(), 0));
        if span_len(heap, item.get()) + span_len(heap, first_child.get()) <= CHUNK_CAPACITY {
            let merged = heap.root_slot(span_merge(heap, item, &first_child, op_owner)?);
            let new_front =
                span_set_slot(heap, &front, 0, &merged, item_weight as isize, op_owner)?;
            if can_mutate(node_owner, op_owner) {
                let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
                heap.write_slot_object(node, &node.front, new_front);
                node.weight
                    .store(old_weight + item_weight, Ordering::Relaxed);
                return Ok(node_slot.get());
            }
            let new_front_slot = heap.root_slot(new_front);
            return alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back);
        }
    }

    if span_len(heap, front.get()) < CHUNK_CAPACITY {
        let new_front = span_push_front(heap, &front, item, item_weight, op_owner)?;
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, new_front);
            node.weight
                .store(old_weight + item_weight, Ordering::Relaxed);
            return Ok(node_slot.get());
        }
        let new_front_slot = heap.root_slot(new_front);
        return alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back);
    }

    // `front` is full (`CHUNK_CAPACITY` items). Keep <= 2K flat when `middle` & `back` are empty.
    let new_front = heap.root_slot(alloc_single_item_span(heap, item, item_weight, op_owner)?);
    if middle.get() == Word::NIL && back.get() == Word::NIL {
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.back, front.get());
            heap.write_slot_object(node, &node.front, new_front.get());
            node.weight
                .store(old_weight + item_weight, Ordering::Relaxed);
            return Ok(node_slot.get());
        }
        return alloc_node(heap, op_owner, depth, &new_front, &middle, &front);
    }

    let fw = span_weight(heap, front.get());
    let new_middle = node_push_front(heap, &middle, depth + 1, &front, fw, op_owner)?;
    if can_mutate(node_owner, op_owner) {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        heap.write_slot_object(node, &node.front, new_front.get());
        heap.write_slot_object(node, &node.middle, new_middle);
        node.weight
            .store(old_weight + item_weight, Ordering::Relaxed);
        return Ok(node_slot.get());
    }
    let new_middle_slot = heap.root_slot(new_middle);
    alloc_node(heap, op_owner, depth, &new_front, &new_middle_slot, &back)
}

pub(super) fn node_push_back(
    heap: &Heap,
    node_slot: &RootSlot,
    depth: u8,
    item: &RootSlot,
    item_weight: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if node_slot.get() == Word::NIL {
        let front = heap.root_slot(alloc_single_item_span(heap, item, item_weight, op_owner)?);
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, &front, &nil, &nil);
    }
    let (node_owner, old_weight, front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        debug_assert_eq!(node.depth, depth);
        (
            node.owner.load(Ordering::Relaxed),
            node.weight.load(Ordering::Relaxed),
            node.front.get(),
            node.middle.get(),
            node.back.get(),
        )
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);

    // If `back == NIL` and `middle == NIL`, check if `front` can absorb directly or promote to `back`.
    if back.get() == Word::NIL {
        debug_assert_eq!(middle.get(), Word::NIL);
        if depth >= 1 {
            let fl = span_len(heap, front.get());
            let last_child = heap.root_slot(span_item_at(heap, front.get(), fl - 1));
            if span_len(heap, last_child.get()) + span_len(heap, item.get()) <= CHUNK_CAPACITY {
                let merged = heap.root_slot(span_merge(heap, &last_child, item, op_owner)?);
                let new_front = span_set_slot(
                    heap,
                    &front,
                    fl - 1,
                    &merged,
                    item_weight as isize,
                    op_owner,
                )?;
                if can_mutate(node_owner, op_owner) {
                    let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
                    heap.write_slot_object(node, &node.front, new_front);
                    node.weight
                        .store(old_weight + item_weight, Ordering::Relaxed);
                    return Ok(node_slot.get());
                }
                let new_front_slot = heap.root_slot(new_front);
                return alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back);
            }
        }
        if span_len(heap, front.get()) < CHUNK_CAPACITY {
            let new_front = span_push_back(heap, &front, item, item_weight, op_owner)?;
            if can_mutate(node_owner, op_owner) {
                let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
                heap.write_slot_object(node, &node.front, new_front);
                node.weight
                    .store(old_weight + item_weight, Ordering::Relaxed);
                return Ok(node_slot.get());
            }
            let new_front_slot = heap.root_slot(new_front);
            return alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back);
        }
        let new_back = heap.root_slot(alloc_single_item_span(heap, item, item_weight, op_owner)?);
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.back, new_back.get());
            node.weight
                .store(old_weight + item_weight, Ordering::Relaxed);
            return Ok(node_slot.get());
        }
        return alloc_node(heap, op_owner, depth, &front, &middle, &new_back);
    }

    if depth >= 1 {
        let bl = span_len(heap, back.get());
        let last_child = heap.root_slot(span_item_at(heap, back.get(), bl - 1));
        if span_len(heap, last_child.get()) + span_len(heap, item.get()) <= CHUNK_CAPACITY {
            let merged = heap.root_slot(span_merge(heap, &last_child, item, op_owner)?);
            let new_back =
                span_set_slot(heap, &back, bl - 1, &merged, item_weight as isize, op_owner)?;
            if can_mutate(node_owner, op_owner) {
                let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
                heap.write_slot_object(node, &node.back, new_back);
                node.weight
                    .store(old_weight + item_weight, Ordering::Relaxed);
                return Ok(node_slot.get());
            }
            let new_back_slot = heap.root_slot(new_back);
            return alloc_node(heap, op_owner, depth, &front, &middle, &new_back_slot);
        }
    }

    if span_len(heap, back.get()) < CHUNK_CAPACITY {
        let new_back = span_push_back(heap, &back, item, item_weight, op_owner)?;
        if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.back, new_back);
            node.weight
                .store(old_weight + item_weight, Ordering::Relaxed);
            return Ok(node_slot.get());
        }
        let new_back_slot = heap.root_slot(new_back);
        return alloc_node(heap, op_owner, depth, &front, &middle, &new_back_slot);
    }

    let bw = span_weight(heap, back.get());
    let new_middle = node_push_back(heap, &middle, depth + 1, &back, bw, op_owner)?;
    let new_middle_slot = heap.root_slot(new_middle);
    let new_back = heap.root_slot(alloc_single_item_span(heap, item, item_weight, op_owner)?);
    if can_mutate(node_owner, op_owner) {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        heap.write_slot_object(node, &node.middle, new_middle_slot.get());
        heap.write_slot_object(node, &node.back, new_back.get());
        node.weight
            .store(old_weight + item_weight, Ordering::Relaxed);
        return Ok(node_slot.get());
    }
    alloc_node(heap, op_owner, depth, &front, &new_middle_slot, &new_back)
}

pub(super) fn node_pop_front(
    heap: &Heap,
    node_slot: &RootSlot,
    op_owner: u64,
) -> Result<(Word, usize, Word), OutOfMemory> {
    let (node_owner, depth, old_weight, front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        (
            node.owner.load(Ordering::Relaxed),
            node.depth,
            node.weight.load(Ordering::Relaxed),
            node.front.get(),
            node.middle.get(),
            node.back.get(),
        )
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);
    let (popped, popped_w, new_front) = span_pop_front(heap, &front, depth, op_owner)?;
    let popped_slot = heap.root_slot(popped);
    let new_front_slot = heap.root_slot(new_front);

    if new_front_slot.get() != Word::NIL {
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, new_front_slot.get());
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            alloc_node(heap, op_owner, depth, &new_front_slot, &middle, &back)?
        };
        return Ok((popped_slot.get(), popped_w, rest_node));
    }

    if middle.get() != Word::NIL {
        let (promoted, _, new_middle) = node_pop_front(heap, &middle, op_owner)?;
        let promoted_slot = heap.root_slot(promoted);
        let new_middle_slot = heap.root_slot(new_middle);
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, promoted_slot.get());
            heap.write_slot_object(node, &node.middle, new_middle_slot.get());
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            alloc_node(
                heap,
                op_owner,
                depth,
                &promoted_slot,
                &new_middle_slot,
                &back,
            )?
        };
        return Ok((popped_slot.get(), popped_w, rest_node));
    }

    if back.get() != Word::NIL {
        let nil = heap.root_slot(Word::NIL);
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, back.get());
            node.back.set_unbarriered(Word::NIL);
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            alloc_node(heap, op_owner, depth, &back, &nil, &nil)?
        };
        return Ok((popped_slot.get(), popped_w, rest_node));
    }

    Ok((popped_slot.get(), popped_w, Word::NIL))
}

pub(super) fn node_pop_back(
    heap: &Heap,
    node_slot: &RootSlot,
    op_owner: u64,
) -> Result<(Word, Word, usize), OutOfMemory> {
    let (node_owner, depth, old_weight, front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        (
            node.owner.load(Ordering::Relaxed),
            node.depth,
            node.weight.load(Ordering::Relaxed),
            node.front.get(),
            node.middle.get(),
            node.back.get(),
        )
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);

    if back.get() == Word::NIL {
        debug_assert_eq!(middle.get(), Word::NIL);
        let (new_front, popped, popped_w) = span_pop_back(heap, &front, depth, op_owner)?;
        let popped_slot = heap.root_slot(popped);
        let new_front_slot = heap.root_slot(new_front);
        if new_front_slot.get() == Word::NIL {
            return Ok((Word::NIL, popped_slot.get(), popped_w));
        }
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.front, new_front_slot.get());
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            let nil = heap.root_slot(Word::NIL);
            alloc_node(heap, op_owner, depth, &new_front_slot, &nil, &nil)?
        };
        return Ok((rest_node, popped_slot.get(), popped_w));
    }

    let (new_back, popped, popped_w) = span_pop_back(heap, &back, depth, op_owner)?;
    let popped_slot = heap.root_slot(popped);
    let new_back_slot = heap.root_slot(new_back);

    if new_back_slot.get() != Word::NIL {
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.back, new_back_slot.get());
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            alloc_node(heap, op_owner, depth, &front, &middle, &new_back_slot)?
        };
        return Ok((rest_node, popped_slot.get(), popped_w));
    }

    if middle.get() != Word::NIL {
        let (new_middle, promoted, _) = node_pop_back(heap, &middle, op_owner)?;
        let promoted_slot = heap.root_slot(promoted);
        let new_middle_slot = heap.root_slot(new_middle);
        let rest_node = if can_mutate(node_owner, op_owner) {
            let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
            heap.write_slot_object(node, &node.middle, new_middle_slot.get());
            heap.write_slot_object(node, &node.back, promoted_slot.get());
            node.weight.store(old_weight - popped_w, Ordering::Relaxed);
            node_slot.get()
        } else {
            alloc_node(
                heap,
                op_owner,
                depth,
                &front,
                &new_middle_slot,
                &promoted_slot,
            )?
        };
        return Ok((rest_node, popped_slot.get(), popped_w));
    }

    let nil = heap.root_slot(Word::NIL);
    let rest_node = if can_mutate(node_owner, op_owner) {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        node.back.set_unbarriered(Word::NIL);
        node.weight.store(old_weight - popped_w, Ordering::Relaxed);
        node_slot.get()
    } else {
        alloc_node(heap, op_owner, depth, &front, &nil, &nil)?
    };
    Ok((rest_node, popped_slot.get(), popped_w))
}

pub(super) fn node_push_front_span(
    heap: &Heap,
    node_slot: &RootSlot,
    depth: u8,
    span_slot: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if node_slot.get() == Word::NIL {
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, span_slot, &nil, &nil);
    }
    let (front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        debug_assert_eq!(node.depth, depth);
        (node.front.get(), node.middle.get(), node.back.get())
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);
    if span_len(heap, span_slot.get()) + span_len(heap, front.get()) <= CHUNK_CAPACITY {
        let merged = heap.root_slot(span_merge(heap, span_slot, &front, op_owner)?);
        return alloc_node(heap, op_owner, depth, &merged, &middle, &back);
    }
    if middle.get() == Word::NIL && back.get() == Word::NIL {
        return alloc_node(heap, op_owner, depth, span_slot, &middle, &front);
    }
    let fw = span_weight(heap, front.get());
    let new_middle = heap.root_slot(node_push_front(
        heap,
        &middle,
        depth + 1,
        &front,
        fw,
        op_owner,
    )?);
    alloc_node(heap, op_owner, depth, span_slot, &new_middle, &back)
}

pub(super) fn node_push_back_span(
    heap: &Heap,
    node_slot: &RootSlot,
    depth: u8,
    span_slot: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if node_slot.get() == Word::NIL {
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, span_slot, &nil, &nil);
    }
    let (front_w, middle_w, back_w) = {
        let node = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        debug_assert_eq!(node.depth, depth);
        (node.front.get(), node.middle.get(), node.back.get())
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);
    if back.get() == Word::NIL {
        debug_assert_eq!(middle.get(), Word::NIL);
        if span_len(heap, front.get()) + span_len(heap, span_slot.get()) <= CHUNK_CAPACITY {
            let merged = heap.root_slot(span_merge(heap, &front, span_slot, op_owner)?);
            return alloc_node(heap, op_owner, depth, &merged, &middle, &back);
        }
        return alloc_node(heap, op_owner, depth, &front, &middle, span_slot);
    }
    if span_len(heap, back.get()) + span_len(heap, span_slot.get()) <= CHUNK_CAPACITY {
        let merged = heap.root_slot(span_merge(heap, &back, span_slot, op_owner)?);
        return alloc_node(heap, op_owner, depth, &front, &middle, &merged);
    }
    let bw = span_weight(heap, back.get());
    let new_middle = heap.root_slot(node_push_back(
        heap,
        &middle,
        depth + 1,
        &back,
        bw,
        op_owner,
    )?);
    alloc_node(heap, op_owner, depth, &front, &new_middle, span_slot)
}

pub(super) fn node_concat(
    heap: &Heap,
    left_slot: &RootSlot,
    right_slot: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if left_slot.get() == Word::NIL {
        return Ok(right_slot.get());
    }
    if right_slot.get() == Word::NIL {
        return Ok(left_slot.get());
    }
    let (depth, lf_w, lm_w, lb_w) = {
        let l = heap.expect_object::<TreelistNode>(left_slot.get(), "treelist node");
        (l.depth, l.front.get(), l.middle.get(), l.back.get())
    };
    let (r_depth, rf_w, rm_w, rb_w) = {
        let r = heap.expect_object::<TreelistNode>(right_slot.get(), "treelist node");
        (r.depth, r.front.get(), r.middle.get(), r.back.get())
    };
    debug_assert_eq!(depth, r_depth);

    let lf = heap.root_slot(lf_w);
    let lm = heap.root_slot(lm_w);
    let lb = heap.root_slot(lb_w);
    let rf = heap.root_slot(rf_w);
    let rm = heap.root_slot(rm_w);
    let rb = heap.root_slot(rb_w);

    if lb.get() == Word::NIL {
        debug_assert_eq!(lm.get(), Word::NIL);
        return node_push_front_span(heap, right_slot, depth, &lf, op_owner);
    }
    if rb.get() == Word::NIL {
        debug_assert_eq!(rm.get(), Word::NIL);
        return node_push_back_span(heap, left_slot, depth, &rf, op_owner);
    }

    let combined = span_len(heap, lb.get()) + span_len(heap, rf.get());
    let (lm_final, rm_final) = if combined <= CHUNK_CAPACITY {
        let merged = heap.root_slot(span_merge(heap, &lb, &rf, op_owner)?);
        let mw = span_weight(heap, merged.get());
        let lm_next = heap.root_slot(node_push_back(heap, &lm, depth + 1, &merged, mw, op_owner)?);
        (lm_next, rm)
    } else {
        let lb_w = span_weight(heap, lb.get());
        let rf_w = span_weight(heap, rf.get());
        let lm_next = heap.root_slot(node_push_back(heap, &lm, depth + 1, &lb, lb_w, op_owner)?);
        let rm_next = heap.root_slot(node_push_front(heap, &rm, depth + 1, &rf, rf_w, op_owner)?);
        (lm_next, rm_next)
    };
    let out_middle = heap.root_slot(node_concat(heap, &lm_final, &rm_final, op_owner)?);
    alloc_node(heap, op_owner, depth, &lf, &out_middle, &rb)
}

pub(super) fn node_from_front_and_middle(
    heap: &Heap,
    depth: u8,
    front: &RootSlot,
    middle: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if middle.get() == Word::NIL {
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, front, &nil, &nil);
    }
    let (new_middle, new_back, _) = node_pop_back(heap, middle, op_owner)?;
    let new_middle_slot = heap.root_slot(new_middle);
    let new_back_slot = heap.root_slot(new_back);
    alloc_node(
        heap,
        op_owner,
        depth,
        front,
        &new_middle_slot,
        &new_back_slot,
    )
}

pub(super) fn node_from_middle_and_back(
    heap: &Heap,
    depth: u8,
    middle: &RootSlot,
    back: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if middle.get() == Word::NIL {
        let nil = heap.root_slot(Word::NIL);
        return alloc_node(heap, op_owner, depth, back, &nil, &nil);
    }
    let (new_front, _, new_middle) = node_pop_front(heap, middle, op_owner)?;
    let new_front_slot = heap.root_slot(new_front);
    let new_middle_slot = heap.root_slot(new_middle);
    alloc_node(
        heap,
        op_owner,
        depth,
        &new_front_slot,
        &new_middle_slot,
        back,
    )
}

pub(super) fn node_split(
    heap: &Heap,
    node_slot: &RootSlot,
    index: usize,
    op_owner: u64,
) -> Result<(Word, Word), OutOfMemory> {
    if node_slot.get() == Word::NIL {
        return Ok((Word::NIL, Word::NIL));
    }
    let total = node_weight(heap, node_slot.get());
    debug_assert!(index <= total);
    if index == 0 {
        return Ok((Word::NIL, node_slot.get()));
    }
    if index == total {
        return Ok((node_slot.get(), Word::NIL));
    }

    let (depth, front_w, middle_w, back_w) = {
        let n = heap.expect_object::<TreelistNode>(node_slot.get(), "treelist node");
        (n.depth, n.front.get(), n.middle.get(), n.back.get())
    };
    let front = heap.root_slot(front_w);
    let middle = heap.root_slot(middle_w);
    let back = heap.root_slot(back_w);
    let nil = heap.root_slot(Word::NIL);

    let fw = span_weight(heap, front.get());
    if index <= fw {
        if index == fw {
            let left = heap.root_slot(alloc_node(heap, op_owner, depth, &front, &nil, &nil)?);
            let right = if back.get() == Word::NIL {
                Word::NIL
            } else {
                node_from_middle_and_back(heap, depth, &middle, &back, op_owner)?
            };
            return Ok((left.get(), right));
        }
        let (fl, fr) = span_split(heap, &front, depth, index, op_owner)?;
        let fl_slot = heap.root_slot(fl);
        let fr_slot = heap.root_slot(fr);
        let left = heap.root_slot(alloc_node(heap, op_owner, depth, &fl_slot, &nil, &nil)?);
        let right = alloc_node(heap, op_owner, depth, &fr_slot, &middle, &back)?;
        return Ok((left.get(), right));
    }

    let mw = node_weight(heap, middle.get());
    if index <= fw + mw {
        let mid_idx = index - fw;
        if mid_idx == mw {
            let left = heap.root_slot(node_from_front_and_middle(
                heap, depth, &front, &middle, op_owner,
            )?);
            let right = alloc_node(heap, op_owner, depth, &back, &nil, &nil)?;
            return Ok((left.get(), right));
        }
        let (mid_left, mid_right) = node_split(heap, &middle, mid_idx, op_owner)?;
        let mid_left_slot = heap.root_slot(mid_left);
        let mid_right_slot = heap.root_slot(mid_right);
        let left = heap.root_slot(node_from_front_and_middle(
            heap,
            depth,
            &front,
            &mid_left_slot,
            op_owner,
        )?);
        let right = node_from_middle_and_back(heap, depth, &mid_right_slot, &back, op_owner)?;
        return Ok((left.get(), right));
    }

    let back_idx = index - fw - mw;
    let (bl, br) = span_split(heap, &back, depth, back_idx, op_owner)?;
    let bl_slot = heap.root_slot(bl);
    let br_slot = heap.root_slot(br);
    let left = heap.root_slot(alloc_node(
        heap, op_owner, depth, &front, &middle, &bl_slot,
    )?);
    let right = alloc_node(heap, op_owner, depth, &br_slot, &nil, &nil)?;
    Ok((left.get(), right))
}

pub(super) fn node_subseq(
    heap: &Heap,
    node_slot: &RootSlot,
    start: usize,
    end: usize,
) -> Result<Word, OutOfMemory> {
    let total = node_weight(heap, node_slot.get());
    if start == end {
        return Ok(Word::NIL);
    }
    if start == 0 && end == total {
        return Ok(node_slot.get());
    }
    let (prefix, _) = node_split(heap, node_slot, end, PERSISTENT_OWNER)?;
    if start == 0 {
        return Ok(prefix);
    }
    let prefix_slot = heap.root_slot(prefix);
    let (_, slice) = node_split(heap, &prefix_slot, start, PERSISTENT_OWNER)?;
    Ok(slice)
}

pub(super) fn node_insert(
    heap: &Heap,
    node_slot: &RootSlot,
    index: usize,
    value: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let len = node_weight(heap, node_slot.get());
    if index == 0 {
        return node_push_front(heap, node_slot, 0, value, 1, op_owner);
    }
    if index == len {
        return node_push_back(heap, node_slot, 0, value, 1, op_owner);
    }
    let (left, right) = node_split(heap, node_slot, index, PERSISTENT_OWNER)?;
    let left_slot = heap.root_slot(left);
    let right_slot = heap.root_slot(right);
    let left_pushed = heap.root_slot(node_push_back(heap, &left_slot, 0, value, 1, op_owner)?);
    node_concat(heap, &left_pushed, &right_slot, op_owner)
}

pub(super) fn node_delete(
    heap: &Heap,
    node_slot: &RootSlot,
    index: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let len = node_weight(heap, node_slot.get());
    if index == 0 {
        return Ok(node_pop_front(heap, node_slot, op_owner)?.2);
    }
    if index + 1 == len {
        return Ok(node_pop_back(heap, node_slot, op_owner)?.0);
    }
    let (left, rest) = node_split(heap, node_slot, index, PERSISTENT_OWNER)?;
    let left_slot = heap.root_slot(left);
    let rest_slot = heap.root_slot(rest);
    let (_, _, right) = node_pop_front(heap, &rest_slot, PERSISTENT_OWNER)?;
    let right_slot = heap.root_slot(right);
    node_concat(heap, &left_slot, &right_slot, op_owner)
}
