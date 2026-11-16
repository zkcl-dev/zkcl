use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crate::OutOfMemory;

use super::super::{Heap, RootSlot, Word, WordSlot};
use super::{
    CHUNK_CAPACITY, CHUNK_MASK, PERSISTENT_OWNER, TreelistChunk, TreelistSpan, can_mutate,
};

struct SpanView<'a> {
    span: &'a TreelistSpan,
    chunk: &'a TreelistChunk,
    start: usize,
    len: usize,
}

impl<'a> SpanView<'a> {
    fn new(heap: &'a Heap, word: Word) -> Self {
        let span = heap.expect_object::<TreelistSpan>(word, "treelist span");
        Self {
            span,
            chunk: heap.expect_object(span.chunk.get(), "treelist chunk"),
            start: usize::from(span.start.load(Ordering::Relaxed)),
            len: usize::from(span.len.load(Ordering::Relaxed)),
        }
    }

    fn slot(&self, index: usize) -> &WordSlot {
        &self.chunk.slots[(self.start + index) & CHUNK_MASK]
    }

    fn item(&self, index: usize) -> Word {
        self.slot(index).get()
    }

    fn weight(&self) -> usize {
        self.span.weight.load(Ordering::Relaxed)
    }

    fn owned_by(&self, owner: u64) -> bool {
        can_mutate(self.span.owner.load(Ordering::Relaxed), owner)
            && can_mutate(self.chunk.owner.load(Ordering::Relaxed), owner)
    }

    fn locate_child(&self, heap: &Heap, mut index: usize) -> (usize, Word, usize) {
        for slot in 0..self.len {
            let child = self.item(slot);
            let weight = span_weight(heap, child);
            if index < weight {
                return (slot, child, index);
            }
            index -= weight;
        }
        unreachable!("treelist index must be within the span weight")
    }
}

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

fn leaf_at(heap: &Heap, mut span: Word, depth: u8, mut index: usize) -> (SpanView<'_>, usize) {
    for _ in 0..depth {
        let (_, child, relative) = SpanView::new(heap, span).locate_child(heap, index);
        span = child;
        index = relative;
    }
    (SpanView::new(heap, span), index)
}

pub(super) fn span_get(heap: &Heap, span: Word, depth: u8, index: usize) -> Word {
    let (leaf, index) = leaf_at(heap, span, depth, index);
    leaf.item(index)
}

pub(super) fn span_set_owned_element(
    heap: &Heap,
    span: Word,
    depth: u8,
    index: usize,
    value: Word,
    owner: u64,
) -> bool {
    let (leaf, index) = leaf_at(heap, span, depth, index);
    if !leaf.owned_by(owner) {
        return false;
    }
    heap.write_slot_object(leaf.chunk, leaf.slot(index), value);
    true
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

fn chunk_slots(len: usize, mut item_at: impl FnMut(usize) -> Word) -> [WordSlot; CHUNK_CAPACITY] {
    let mut slots = [const { WordSlot::nil() }; CHUNK_CAPACITY];
    for (index, slot) in slots[..len].iter_mut().enumerate() {
        *slot = WordSlot::new(item_at(index));
    }
    slots
}

fn alloc_span_with_slots(
    heap: &Heap,
    owner: u64,
    weight: usize,
    len: usize,
    slots: impl FnOnce() -> [WordSlot; CHUNK_CAPACITY],
) -> Result<Word, OutOfMemory> {
    // Read source slots inside the initializer, after a possible moving collection.
    let chunk = heap.root_frame(1);
    chunk.set(
        0,
        heap.alloc_object(|header| TreelistChunk {
            header,
            owner: AtomicU64::new(owner),
            slots: slots(),
        })?,
    );
    heap.alloc_object(|header| TreelistSpan {
        header,
        owner: AtomicU64::new(owner),
        weight: AtomicUsize::new(weight),
        start: AtomicU8::new(0),
        len: AtomicU8::new(len as u8),
        chunk: WordSlot::new(chunk.get(0)),
    })
}

pub(super) fn alloc_span_from_fn(
    heap: &Heap,
    owner: u64,
    weight: usize,
    len: usize,
    item_at: impl FnMut(usize) -> Word,
) -> Result<Word, OutOfMemory> {
    alloc_span_with_slots(heap, owner, weight, len, || chunk_slots(len, item_at))
}

pub(super) fn alloc_single_item_span(
    heap: &Heap,
    item: &RootSlot,
    item_weight: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    alloc_span_from_fn(heap, owner, item_weight, 1, |_| item.get())
}

fn copy_span(
    heap: &Heap,
    source: &RootSlot,
    owner: u64,
    weight: usize,
    len: usize,
    mut item_at: impl FnMut(&SpanView<'_>, usize) -> Word,
) -> Result<Word, OutOfMemory> {
    alloc_span_with_slots(heap, owner, weight, len, || {
        let source = SpanView::new(heap, source.get());
        chunk_slots(len, |index| item_at(&source, index))
    })
}

pub(super) fn span_set_slot(
    heap: &Heap,
    span: &RootSlot,
    index: usize,
    item: &RootSlot,
    added_weight: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    let view = SpanView::new(heap, span.get());
    let weight = view.weight() + added_weight;
    if view.owned_by(owner) {
        heap.write_slot_object(view.chunk, view.slot(index), item.get());
        view.span.weight.store(weight, Ordering::Relaxed);
        return Ok(span.get());
    }
    copy_span(heap, span, owner, weight, view.len, |source, position| {
        if position == index {
            item.get()
        } else {
            source.item(position)
        }
    })
}

pub(super) fn span_set_at(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    index: usize,
    value: &RootSlot,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    if depth == 0 {
        return span_set_slot(heap, span, index, value, 0, owner);
    }
    let (position, child, relative) = SpanView::new(heap, span.get()).locate_child(heap, index);
    let child = heap.root_slot(child);
    let updated = heap.root_slot(span_set_at(
        heap,
        &child,
        depth - 1,
        relative,
        value,
        owner,
    )?);
    span_set_slot(heap, span, position, &updated, 0, owner)
}

#[derive(Clone, Copy)]
enum End {
    Front,
    Back,
}

fn span_push(
    heap: &Heap,
    span: &RootSlot,
    item: &RootSlot,
    item_weight: usize,
    owner: u64,
    end: End,
) -> Result<Word, OutOfMemory> {
    let view = SpanView::new(heap, span.get());
    debug_assert!(view.len < CHUNK_CAPACITY);
    let weight = view.weight() + item_weight;
    let len = view.len + 1;
    if view.owned_by(owner) {
        let position = if matches!(end, End::Front) {
            CHUNK_MASK
        } else {
            view.len
        };
        heap.write_slot_object(view.chunk, view.slot(position), item.get());
        if matches!(end, End::Front) {
            view.span.start.store(
                ((view.start + CHUNK_MASK) & CHUNK_MASK) as u8,
                Ordering::Relaxed,
            );
        }
        view.span.len.store(len as u8, Ordering::Relaxed);
        view.span.weight.store(weight, Ordering::Relaxed);
        return Ok(span.get());
    }
    let position = if matches!(end, End::Front) {
        0
    } else {
        view.len
    };
    copy_span(heap, span, owner, weight, len, |source, index| {
        if index == position {
            item.get()
        } else {
            source.item(index - usize::from(index > position))
        }
    })
}

pub(super) fn span_push_front(
    heap: &Heap,
    span: &RootSlot,
    item: &RootSlot,
    weight: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    span_push(heap, span, item, weight, owner, End::Front)
}

pub(super) fn span_push_back(
    heap: &Heap,
    span: &RootSlot,
    item: &RootSlot,
    weight: usize,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    span_push(heap, span, item, weight, owner, End::Back)
}

fn span_pop(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    owner: u64,
    end: End,
) -> Result<(Word, usize, Word), OutOfMemory> {
    let view = SpanView::new(heap, span.get());
    let len = view.len - 1;
    let front = matches!(end, End::Front);
    let position = if front { 0 } else { len };
    let popped = view.item(position);
    let popped_weight = if depth == 0 {
        1
    } else {
        span_weight(heap, popped)
    };
    if len == 0 {
        // The node drops the entire span after any fallible middle promotion.
        return Ok((popped, popped_weight, Word::NIL));
    }
    let weight = view.weight() - popped_weight;
    if view.owned_by(owner) {
        view.slot(position).set_unbarriered(Word::NIL);
        if front {
            view.span
                .start
                .store(((view.start + 1) & CHUNK_MASK) as u8, Ordering::Relaxed);
        }
        view.span.len.store(len as u8, Ordering::Relaxed);
        view.span.weight.store(weight, Ordering::Relaxed);
        return Ok((popped, popped_weight, span.get()));
    }
    let popped = heap.root_slot(popped);
    let offset = usize::from(front);
    let rest = copy_span(heap, span, owner, weight, len, |source, index| {
        source.item(offset + index)
    })?;
    Ok((popped.get(), popped_weight, rest))
}

pub(super) fn span_pop_front(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    owner: u64,
) -> Result<(Word, usize, Word), OutOfMemory> {
    span_pop(heap, span, depth, owner, End::Front)
}

pub(super) fn span_pop_back(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    owner: u64,
) -> Result<(Word, Word, usize), OutOfMemory> {
    let (popped, weight, rest) = span_pop(heap, span, depth, owner, End::Back)?;
    Ok((rest, popped, weight))
}

pub(super) fn span_merge(
    heap: &Heap,
    left: &RootSlot,
    right: &RootSlot,
    owner: u64,
) -> Result<Word, OutOfMemory> {
    let left_view = SpanView::new(heap, left.get());
    let right_view = SpanView::new(heap, right.get());
    let len = left_view.len + right_view.len;
    let weight = left_view.weight() + right_view.weight();
    alloc_span_with_slots(heap, owner, weight, len, || {
        let left = SpanView::new(heap, left.get());
        let right = SpanView::new(heap, right.get());
        chunk_slots(len, |index| {
            if index < left.len {
                left.item(index)
            } else {
                right.item(index - left.len)
            }
        })
    })
}

pub(super) fn span_item_at(heap: &Heap, span: Word, index: usize) -> Word {
    SpanView::new(heap, span).item(index)
}

fn span_cut(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    index: usize,
    retain: End,
) -> Result<Word, OutOfMemory> {
    if index == 0 {
        debug_assert!(matches!(retain, End::Back));
        return Ok(span.get());
    }
    let view = SpanView::new(heap, span.get());
    let weight = view.weight();
    if index == weight {
        debug_assert!(matches!(retain, End::Front));
        return Ok(span.get());
    }
    debug_assert!(index < weight);
    let (boundary, child, relative) = if depth == 0 {
        (index, Word::NIL, 0)
    } else {
        view.locate_child(heap, index)
    };
    let (range, weight) = match retain {
        End::Front => (0..boundary + usize::from(relative != 0), index),
        End::Back => (boundary..view.len, weight - index),
    };
    let partial = if relative == 0 {
        RootSlot::nil()
    } else {
        let child = heap.root_slot(child);
        heap.root_slot(span_cut(heap, &child, depth - 1, relative, retain)?)
    };
    copy_span(
        heap,
        span,
        PERSISTENT_OWNER,
        weight,
        range.len(),
        |source, i| {
            let position = range.start + i;
            if position == boundary && relative != 0 {
                partial.get()
            } else {
                source.item(position)
            }
        },
    )
}

pub(super) fn span_prefix(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    index: usize,
) -> Result<Word, OutOfMemory> {
    span_cut(heap, span, depth, index, End::Front)
}

pub(super) fn span_suffix(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    index: usize,
) -> Result<Word, OutOfMemory> {
    span_cut(heap, span, depth, index, End::Back)
}

pub(super) fn span_split(
    heap: &Heap,
    span: &RootSlot,
    depth: u8,
    index: usize,
    owner: u64,
) -> Result<(Word, Word), OutOfMemory> {
    let view = SpanView::new(heap, span.get());
    let weight = view.weight();
    debug_assert!(index <= weight);
    if index == 0 {
        return Ok((Word::NIL, span.get()));
    }
    if index == weight {
        return Ok((span.get(), Word::NIL));
    }
    let len = view.len;
    let (boundary, child, relative) = if depth == 0 {
        (index, Word::NIL, 0)
    } else {
        view.locate_child(heap, index)
    };
    // Only the cut span is copied. Its new backing chunks contain exactly the
    // retained slots, so discarded child spans are no longer strong GC edges.
    if relative == 0 {
        let left = heap.root_slot(copy_span(
            heap,
            span,
            owner,
            index,
            boundary,
            |source, i| source.item(i),
        )?);
        let right = copy_span(
            heap,
            span,
            owner,
            weight - index,
            len - boundary,
            |source, i| source.item(boundary + i),
        )?;
        return Ok((left.get(), right));
    }

    let child = heap.root_slot(child);
    let (left_child, right_child) = span_split(heap, &child, depth - 1, relative, owner)?;
    let left_child = heap.root_slot(left_child);
    let right_child = heap.root_slot(right_child);
    let left = heap.root_slot(copy_span(
        heap,
        span,
        owner,
        index,
        boundary + 1,
        |source, i| {
            if i == boundary {
                left_child.get()
            } else {
                source.item(i)
            }
        },
    )?);
    let right = copy_span(
        heap,
        span,
        owner,
        weight - index,
        len - boundary,
        |source, i| {
            if i == 0 {
                right_child.get()
            } else {
                source.item(boundary + i)
            }
        },
    )?;
    Ok((left.get(), right))
}
