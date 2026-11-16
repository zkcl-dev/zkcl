use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::OutOfMemory;

use super::super::{Heap, RootSlot, Word, WordSlot};
use super::{CHUNK_CAPACITY, CHUNK_MASK, TreelistChunk, TreelistSpan, can_mutate};

#[inline]
pub(super) fn span_weight(heap: &Heap, span: Word) -> usize {
    if span == Word::NIL {
        0
    } else {
        heap.expect_object::<TreelistSpan>(span, "treelist span")
            .weight
            .load(Ordering::Relaxed)
    }
}

#[inline]
pub(super) fn span_len(heap: &Heap, span: Word) -> usize {
    if span == Word::NIL {
        0
    } else {
        usize::from(
            heap.expect_object::<TreelistSpan>(span, "treelist span")
                .len
                .load(Ordering::Relaxed),
        )
    }
}

pub(super) fn span_get(heap: &Heap, mut span_word: Word, mut depth: u8, mut index: usize) -> Word {
    loop {
        let span = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
        let start = usize::from(span.start.load(Ordering::Relaxed));
        let len = usize::from(span.len.load(Ordering::Relaxed));
        let chunk = heap.expect_object::<TreelistChunk>(span.chunk.get(), "treelist chunk");
        if depth == 0 {
            return chunk.slots[(start + index) & CHUNK_MASK].get();
        }
        for i in 0..len {
            let child = chunk.slots[(start + i) & CHUNK_MASK].get();
            let cw = span_weight(heap, child);
            if index < cw {
                span_word = child;
                depth -= 1;
                break;
            }
            index -= cw;
        }
    }
}

pub(super) fn span_for_each(heap: &Heap, span_word: Word, depth: u8, f: &mut impl FnMut(Word)) {
    if span_word == Word::NIL {
        return;
    }
    let span = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
    let start = usize::from(span.start.load(Ordering::Relaxed));
    let len = usize::from(span.len.load(Ordering::Relaxed));
    let chunk = heap.expect_object::<TreelistChunk>(span.chunk.get(), "treelist chunk");
    if depth == 0 {
        for i in 0..len {
            f(chunk.slots[(start + i) & CHUNK_MASK].get());
        }
    } else {
        for i in 0..len {
            let child = chunk.slots[(start + i) & CHUNK_MASK].get();
            span_for_each(heap, child, depth - 1, f);
        }
    }
}

pub(super) fn span_for_each_range(
    heap: &Heap,
    span_word: Word,
    depth: u8,
    mut start: usize,
    mut end: usize,
    f: &mut impl FnMut(Word),
) {
    if span_word == Word::NIL || start >= end {
        return;
    }
    let span = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
    let s_start = usize::from(span.start.load(Ordering::Relaxed));
    let len = usize::from(span.len.load(Ordering::Relaxed));
    let chunk = heap.expect_object::<TreelistChunk>(span.chunk.get(), "treelist chunk");
    if depth == 0 {
        for i in start..end {
            f(chunk.slots[(s_start + i) & CHUNK_MASK].get());
        }
        return;
    }
    for i in 0..len {
        let child = chunk.slots[(s_start + i) & CHUNK_MASK].get();
        let cw = span_weight(heap, child);
        if start < cw && end > 0 {
            span_for_each_range(heap, child, depth - 1, start, end.min(cw), f);
        }
        if end <= cw {
            break;
        }
        start = start.saturating_sub(cw);
        end -= cw;
    }
}

pub(super) fn alloc_chunk_from_fn(
    heap: &Heap,
    owner: u64,
    len: usize,
    mut item_at: impl FnMut(usize) -> Word,
) -> Result<Word, OutOfMemory> {
    // Evaluate `item_at` inside the post-GC initializer so any `Word` read from a
    // `RootSlot`/`RootFrame` reflects addresses moved during collection.
    heap.alloc_object(|header| {
        let items: [Word; CHUNK_CAPACITY] = std::array::from_fn(|index| {
            if index < len {
                item_at(index)
            } else {
                Word::NIL
            }
        });
        TreelistChunk {
            header,
            owner: AtomicU64::new(owner),
            slots: items.map(WordSlot::new),
        }
    })
}

pub(super) fn alloc_span(
    heap: &Heap,
    owner: u64,
    weight: usize,
    start: u8,
    len: u8,
    chunk: &RootSlot,
) -> Result<Word, OutOfMemory> {
    heap.alloc_object(|header| TreelistSpan {
        header,
        owner: AtomicU64::new(owner),
        weight: AtomicUsize::new(weight),
        start: AtomicU8::new(start),
        len: AtomicU8::new(len),
        chunk: WordSlot::new(chunk.get()),
    })
}

pub(super) fn alloc_single_item_span(
    heap: &Heap,
    item: &RootSlot,
    item_weight: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    let chunk = heap.alloc_object(|header| {
        let mut slots = [const { Word::NIL }; CHUNK_CAPACITY];
        slots[0] = item.get();
        TreelistChunk {
            header,
            owner: AtomicU64::new(owner),
            slots: slots.map(WordSlot::new),
        }
    })?;
    let chunk_slot = heap.root_slot(chunk);
    alloc_span(heap, owner, item_weight, 0, 1, &chunk_slot)
}

pub(super) fn clone_chunk_contiguous(
    heap: &Heap,
    chunk_slot: &RootSlot,
    start: usize,
    len: usize,
    target_offset: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    heap.alloc_object(|header| {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        let slots: [Word; CHUNK_CAPACITY] = std::array::from_fn(|index| {
            if index >= target_offset && index - target_offset < len {
                let src_idx = (start + (index - target_offset)) & CHUNK_MASK;
                chunk.slots[src_idx].get()
            } else {
                Word::NIL
            }
        });
        TreelistChunk {
            header,
            owner: AtomicU64::new(owner),
            slots: slots.map(WordSlot::new),
        }
    })
}

pub(super) fn span_set_slot(
    heap: &Heap,
    span_slot: &RootSlot,
    slot_idx: usize,
    new_item: &RootSlot,
    weight_delta: isize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (span_owner, start, len, old_weight, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            span.owner.load(Ordering::Relaxed),
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.weight.load(Ordering::Relaxed),
            span.chunk.get(),
        )
    };
    let chunk_slot = heap.root_slot(chunk_word);
    let chunk_owner = heap
        .expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
        .owner
        .load(Ordering::Relaxed);
    let (final_chunk, final_start) = if can_mutate(chunk_owner, op_owner) {
        let physical = (start + slot_idx) & CHUNK_MASK;
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        heap.write_slot_object(chunk, &chunk.slots[physical], new_item.get());
        (chunk_slot.get(), start as u8)
    } else {
        let new_chunk = clone_chunk_contiguous(heap, &chunk_slot, start, len, 0, op_owner)?;
        heap.expect_object::<TreelistChunk>(new_chunk, "treelist chunk")
            .slots[slot_idx]
            .set_unbarriered(new_item.get());
        (new_chunk, 0u8)
    };
    let new_weight = (old_weight as isize + weight_delta) as usize;
    if can_mutate(span_owner, op_owner) {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        span.weight.store(new_weight, Ordering::Relaxed);
        span.start.store(final_start, Ordering::Relaxed);
        heap.write_slot_object(span, &span.chunk, final_chunk);
        Ok(span_slot.get())
    } else {
        let final_chunk_slot = heap.root_slot(final_chunk);
        alloc_span(
            heap,
            op_owner,
            new_weight,
            final_start,
            len as u8,
            &final_chunk_slot,
        )
    }
}

pub(super) fn span_set_at(
    heap: &Heap,
    span_slot: &RootSlot,
    depth: u8,
    mut index: usize,
    value: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    if depth == 0 {
        return span_set_slot(heap, span_slot, index, value, 0, op_owner);
    }
    let (start, len, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.chunk.get(),
        )
    };
    let mut target_slot = 0usize;
    let mut child_word = Word::NIL;
    {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_word, "treelist chunk");
        for i in 0..len {
            let child = chunk.slots[(start + i) & CHUNK_MASK].get();
            let cw = span_weight(heap, child);
            if index < cw {
                target_slot = i;
                child_word = child;
                break;
            }
            index -= cw;
        }
    }
    let child_slot = heap.root_slot(child_word);
    let updated_child = span_set_at(heap, &child_slot, depth - 1, index, value, op_owner)?;
    let updated_slot = heap.root_slot(updated_child);
    span_set_slot(heap, span_slot, target_slot, &updated_slot, 0, op_owner)
}

pub(super) fn span_push_front(
    heap: &Heap,
    span_slot: &RootSlot,
    item: &RootSlot,
    item_weight: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (span_owner, start, len, old_weight, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            span.owner.load(Ordering::Relaxed),
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.weight.load(Ordering::Relaxed),
            span.chunk.get(),
        )
    };
    debug_assert!(len < CHUNK_CAPACITY);
    let chunk_slot = heap.root_slot(chunk_word);
    let chunk_owner = heap
        .expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
        .owner
        .load(Ordering::Relaxed);
    let (final_chunk, new_start) = if can_mutate(chunk_owner, op_owner) {
        let new_start = (start + CHUNK_CAPACITY - 1) & CHUNK_MASK;
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        heap.write_slot_object(chunk, &chunk.slots[new_start], item.get());
        (chunk_slot.get(), new_start as u8)
    } else {
        let new_chunk = clone_chunk_contiguous(heap, &chunk_slot, start, len, 1, op_owner)?;
        heap.expect_object::<TreelistChunk>(new_chunk, "treelist chunk")
            .slots[0]
            .set_unbarriered(item.get());
        (new_chunk, 0u8)
    };
    let new_len = (len + 1) as u8;
    let new_weight = old_weight + item_weight;
    if can_mutate(span_owner, op_owner) {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        span.weight.store(new_weight, Ordering::Relaxed);
        span.start.store(new_start, Ordering::Relaxed);
        span.len.store(new_len, Ordering::Relaxed);
        heap.write_slot_object(span, &span.chunk, final_chunk);
        Ok(span_slot.get())
    } else {
        let final_chunk_slot = heap.root_slot(final_chunk);
        alloc_span(
            heap,
            op_owner,
            new_weight,
            new_start,
            new_len,
            &final_chunk_slot,
        )
    }
}

pub(super) fn span_push_back(
    heap: &Heap,
    span_slot: &RootSlot,
    item: &RootSlot,
    item_weight: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (span_owner, start, len, old_weight, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            span.owner.load(Ordering::Relaxed),
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.weight.load(Ordering::Relaxed),
            span.chunk.get(),
        )
    };
    debug_assert!(len < CHUNK_CAPACITY);
    let chunk_slot = heap.root_slot(chunk_word);
    let chunk_owner = heap
        .expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
        .owner
        .load(Ordering::Relaxed);
    let (final_chunk, new_start) = if can_mutate(chunk_owner, op_owner) {
        let slot = (start + len) & CHUNK_MASK;
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        heap.write_slot_object(chunk, &chunk.slots[slot], item.get());
        (chunk_slot.get(), start as u8)
    } else {
        let new_chunk = clone_chunk_contiguous(heap, &chunk_slot, start, len, 0, op_owner)?;
        heap.expect_object::<TreelistChunk>(new_chunk, "treelist chunk")
            .slots[len]
            .set_unbarriered(item.get());
        (new_chunk, 0u8)
    };
    let new_len = (len + 1) as u8;
    let new_weight = old_weight + item_weight;
    if can_mutate(span_owner, op_owner) {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        span.weight.store(new_weight, Ordering::Relaxed);
        span.start.store(new_start, Ordering::Relaxed);
        span.len.store(new_len, Ordering::Relaxed);
        heap.write_slot_object(span, &span.chunk, final_chunk);
        Ok(span_slot.get())
    } else {
        let final_chunk_slot = heap.root_slot(final_chunk);
        alloc_span(
            heap,
            op_owner,
            new_weight,
            new_start,
            new_len,
            &final_chunk_slot,
        )
    }
}

pub(super) fn span_pop_front(
    heap: &Heap,
    span_slot: &RootSlot,
    depth: u8,
    op_owner: u64,
) -> Result<(Word, usize, Word), OutOfMemory> {
    let (span_owner, start, len, old_weight, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            span.owner.load(Ordering::Relaxed),
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.weight.load(Ordering::Relaxed),
            span.chunk.get(),
        )
    };
    let chunk_slot = heap.root_slot(chunk_word);
    let (chunk_owner, popped) = {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        (
            chunk.owner.load(Ordering::Relaxed),
            chunk.slots[start].get(),
        )
    };
    let popped_w = if depth == 0 {
        1
    } else {
        span_weight(heap, popped)
    };
    let popped_slot = heap.root_slot(popped);
    if len == 1 {
        if can_mutate(chunk_owner, op_owner) {
            heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
                .slots[start]
                .set_unbarriered(Word::NIL);
        }
        return Ok((popped_slot.get(), popped_w, Word::NIL));
    }
    let new_start = ((start + 1) & CHUNK_MASK) as u8;
    let new_len = (len - 1) as u8;
    let new_weight = old_weight - popped_w;
    if can_mutate(chunk_owner, op_owner) {
        heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
            .slots[start]
            .set_unbarriered(Word::NIL);
    }
    let rest_span = if can_mutate(span_owner, op_owner) {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        span.weight.store(new_weight, Ordering::Relaxed);
        span.start.store(new_start, Ordering::Relaxed);
        span.len.store(new_len, Ordering::Relaxed);
        span_slot.get()
    } else {
        alloc_span(heap, op_owner, new_weight, new_start, new_len, &chunk_slot)?
    };
    Ok((popped_slot.get(), popped_w, rest_span))
}

pub(super) fn span_pop_back(
    heap: &Heap,
    span_slot: &RootSlot,
    depth: u8,
    op_owner: u64,
) -> Result<(Word, Word, usize), OutOfMemory> {
    let (span_owner, start, len, old_weight, chunk_word) = {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            span.owner.load(Ordering::Relaxed),
            usize::from(span.start.load(Ordering::Relaxed)),
            usize::from(span.len.load(Ordering::Relaxed)),
            span.weight.load(Ordering::Relaxed),
            span.chunk.get(),
        )
    };
    let back_phys = (start + len - 1) & CHUNK_MASK;
    let chunk_slot = heap.root_slot(chunk_word);
    let (chunk_owner, popped) = {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        (
            chunk.owner.load(Ordering::Relaxed),
            chunk.slots[back_phys].get(),
        )
    };
    let popped_w = if depth == 0 {
        1
    } else {
        span_weight(heap, popped)
    };
    let popped_slot = heap.root_slot(popped);
    if len == 1 {
        if can_mutate(chunk_owner, op_owner) {
            heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
                .slots[back_phys]
                .set_unbarriered(Word::NIL);
        }
        return Ok((Word::NIL, popped_slot.get(), popped_w));
    }
    let new_len = (len - 1) as u8;
    let new_weight = old_weight - popped_w;
    if can_mutate(chunk_owner, op_owner) {
        heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk")
            .slots[back_phys]
            .set_unbarriered(Word::NIL);
    }
    let rest_span = if can_mutate(span_owner, op_owner) {
        let span = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        span.weight.store(new_weight, Ordering::Relaxed);
        span.len.store(new_len, Ordering::Relaxed);
        span_slot.get()
    } else {
        alloc_span(
            heap,
            op_owner,
            new_weight,
            start as u8,
            new_len,
            &chunk_slot,
        )?
    };
    Ok((rest_span, popped_slot.get(), popped_w))
}

pub(super) fn span_merge(
    heap: &Heap,
    left_slot: &RootSlot,
    right_slot: &RootSlot,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (l_start, l_len, l_w, l_chunk_w) = {
        let s = heap.expect_object::<TreelistSpan>(left_slot.get(), "treelist span");
        (
            usize::from(s.start.load(Ordering::Relaxed)),
            usize::from(s.len.load(Ordering::Relaxed)),
            s.weight.load(Ordering::Relaxed),
            s.chunk.get(),
        )
    };
    let (r_start, r_len, r_w, r_chunk_w) = {
        let s = heap.expect_object::<TreelistSpan>(right_slot.get(), "treelist span");
        (
            usize::from(s.start.load(Ordering::Relaxed)),
            usize::from(s.len.load(Ordering::Relaxed)),
            s.weight.load(Ordering::Relaxed),
            s.chunk.get(),
        )
    };
    let total_len = l_len + r_len;
    debug_assert!(total_len <= CHUNK_CAPACITY);
    let l_chunk_slot = heap.root_slot(l_chunk_w);
    let r_chunk_slot = heap.root_slot(r_chunk_w);
    let merged_chunk = alloc_chunk_from_fn(heap, op_owner, total_len, |index| {
        if index < l_len {
            let l_chunk = heap.expect_object::<TreelistChunk>(l_chunk_slot.get(), "treelist chunk");
            l_chunk.slots[(l_start + index) & CHUNK_MASK].get()
        } else {
            let r_chunk = heap.expect_object::<TreelistChunk>(r_chunk_slot.get(), "treelist chunk");
            r_chunk.slots[(r_start + (index - l_len)) & CHUNK_MASK].get()
        }
    })?;
    let merged_chunk_slot = heap.root_slot(merged_chunk);
    alloc_span(
        heap,
        op_owner,
        l_w + r_w,
        0,
        total_len as u8,
        &merged_chunk_slot,
    )
}

pub(super) fn span_item_at(heap: &Heap, span_word: Word, idx: usize) -> Word {
    let span = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
    let start = usize::from(span.start.load(Ordering::Relaxed));
    let chunk = heap.expect_object::<TreelistChunk>(span.chunk.get(), "treelist chunk");
    chunk.slots[(start + idx) & CHUNK_MASK].get()
}

pub(super) fn span_slice_range(
    heap: &Heap,
    span_slot: &RootSlot,
    depth: u8,
    offset: usize,
    count: usize,
    op_owner: u64,
) -> Result<Word, OutOfMemory> {
    let (start, len, old_w, chunk_w) = {
        let s = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            usize::from(s.start.load(Ordering::Relaxed)),
            usize::from(s.len.load(Ordering::Relaxed)),
            s.weight.load(Ordering::Relaxed),
            s.chunk.get(),
        )
    };
    debug_assert!(offset + count <= len);
    debug_assert!(count > 0);
    if offset == 0 && count == len {
        return Ok(span_slot.get());
    }
    let chunk_slot = heap.root_slot(chunk_w);
    let new_start = ((start + offset) & CHUNK_MASK) as u8;
    let weight = if depth == 0 {
        count
    } else if count == len {
        old_w
    } else {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_slot.get(), "treelist chunk");
        let mut total = 0usize;
        for i in 0..count {
            total += span_weight(
                heap,
                chunk.slots[(usize::from(new_start) + i) & CHUNK_MASK].get(),
            );
        }
        total
    };
    alloc_span(heap, op_owner, weight, new_start, count as u8, &chunk_slot)
}

pub(super) fn span_split(
    heap: &Heap,
    span_slot: &RootSlot,
    depth: u8,
    index: usize,
    op_owner: u64,
) -> Result<(Word, Word), OutOfMemory> {
    let w = span_weight(heap, span_slot.get());
    debug_assert!(index <= w);
    if index == 0 {
        return Ok((Word::NIL, span_slot.get()));
    }
    if index == w {
        return Ok((span_slot.get(), Word::NIL));
    }
    let (start, len, chunk_w) = {
        let s = heap.expect_object::<TreelistSpan>(span_slot.get(), "treelist span");
        (
            usize::from(s.start.load(Ordering::Relaxed)),
            usize::from(s.len.load(Ordering::Relaxed)),
            s.chunk.get(),
        )
    };
    if depth == 0 {
        let left = heap.root_slot(span_slice_range(heap, span_slot, 0, 0, index, op_owner)?);
        let right = span_slice_range(heap, span_slot, 0, index, len - index, op_owner)?;
        return Ok((left.get(), right));
    }
    let mut split_slot = 0usize;
    let mut child_word = Word::NIL;
    let mut rem = index;
    {
        let chunk = heap.expect_object::<TreelistChunk>(chunk_w, "treelist chunk");
        for i in 0..len {
            let child = chunk.slots[(start + i) & CHUNK_MASK].get();
            let cw = span_weight(heap, child);
            if rem < cw {
                split_slot = i;
                child_word = child;
                break;
            }
            rem -= cw;
        }
    }
    let child_slot = heap.root_slot(child_word);
    if rem == 0 {
        // Exact boundary before `split_slot` (`split_slot > 0` because `index > 0`).
        let left = heap.root_slot(span_slice_range(
            heap, span_slot, depth, 0, split_slot, op_owner,
        )?);
        let right = span_slice_range(
            heap,
            span_slot,
            depth,
            split_slot,
            len - split_slot,
            op_owner,
        )?;
        return Ok((left.get(), right));
    }
    let (c_left, c_right) = span_split(heap, &child_slot, depth - 1, rem, op_owner)?;
    let c_left_slot = heap.root_slot(c_left);
    let c_right_slot = heap.root_slot(c_right);

    let left = if split_slot == 0 {
        heap.root_slot(alloc_single_item_span(
            heap,
            &c_left_slot,
            span_weight(heap, c_left_slot.get()),
            op_owner,
        )?)
    } else {
        let prefix = heap.root_slot(span_slice_range(
            heap, span_slot, depth, 0, split_slot, op_owner,
        )?);
        heap.root_slot(span_push_back(
            heap,
            &prefix,
            &c_left_slot,
            span_weight(heap, c_left_slot.get()),
            op_owner,
        )?)
    };

    let right = if split_slot + 1 == len {
        alloc_single_item_span(
            heap,
            &c_right_slot,
            span_weight(heap, c_right_slot.get()),
            op_owner,
        )?
    } else {
        let suffix = heap.root_slot(span_slice_range(
            heap,
            span_slot,
            depth,
            split_slot + 1,
            len - split_slot - 1,
            op_owner,
        )?);
        span_push_front(
            heap,
            &suffix,
            &c_right_slot,
            span_weight(heap, c_right_slot.get()),
            op_owner,
        )?
    };
    Ok((left.get(), right))
}
