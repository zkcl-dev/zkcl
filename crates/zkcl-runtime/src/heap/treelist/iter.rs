use std::sync::atomic::Ordering;

use smallvec::SmallVec;

use super::super::{Heap, Word, WordSlot};
use super::{CHUNK_CAPACITY, CHUNK_MASK, TreelistChunk, TreelistNode, TreelistSpan};

enum Step<'a> {
    Node(Word),
    Span(Word, u8),
    Chunk {
        slots: &'a [WordSlot; CHUNK_CAPACITY],
        next: usize,
        remaining: usize,
        depth: u8,
    },
}

/// A depth-first cursor. The caller keeps the tree live and unchanged and
/// finishes using the cursor before any safepoint.
pub(crate) struct TreelistIter<'a> {
    heap: &'a Heap,
    pending: SmallVec<[Step<'a>; 8]>,
}

impl<'a> TreelistIter<'a> {
    pub(super) fn new(heap: &'a Heap, node: Word) -> Self {
        let mut pending = SmallVec::new();
        if node != Word::NIL {
            pending.push(Step::Node(node));
        }
        Self { heap, pending }
    }
}

impl Iterator for TreelistIter<'_> {
    type Item = Word;

    fn next(&mut self) -> Option<Word> {
        loop {
            let step = self.pending.last_mut()?;
            match step {
                Step::Node(word) => {
                    let node = self
                        .heap
                        .expect_object::<TreelistNode>(*word, "treelist node");
                    self.pending.pop();
                    if let Some(back) = node.back.get_non_nil() {
                        self.pending.push(Step::Span(back, node.depth));
                    }
                    if let Some(middle) = node.middle.get_non_nil() {
                        self.pending.push(Step::Node(middle));
                    }
                    self.pending.push(Step::Span(node.front.get(), node.depth));
                }
                Step::Span(word, depth) => {
                    let span = self
                        .heap
                        .expect_object::<TreelistSpan>(*word, "treelist span");
                    let chunk = self
                        .heap
                        .expect_object::<TreelistChunk>(span.chunk.get(), "treelist chunk");
                    *step = Step::Chunk {
                        slots: &chunk.slots,
                        next: usize::from(span.start.load(Ordering::Relaxed)),
                        remaining: usize::from(span.len.load(Ordering::Relaxed)),
                        depth: *depth,
                    };
                }
                Step::Chunk {
                    slots,
                    next,
                    remaining,
                    depth,
                } => {
                    let depth = *depth;
                    let word = slots[*next].get();
                    *next = (*next + 1) & CHUNK_MASK;
                    *remaining -= 1;
                    if *remaining == 0 {
                        self.pending.pop();
                    }
                    if depth == 0 {
                        return Some(word);
                    }
                    self.pending.push(Step::Span(word, depth - 1));
                }
            }
        }
    }
}
