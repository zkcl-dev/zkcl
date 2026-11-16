use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use super::super::{Heap, Word, WordSlot};
use super::{CHUNK_CAPACITY, MutableTreelist, Treelist, TreelistChunk, TreelistNode, TreelistSpan};

pub(in crate::heap) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    if heap.object_ref::<Treelist>(word).is_some() {
        return Some(("treelist", vec![]));
    }
    if let Some(x) = heap.object_ref::<MutableTreelist>(word) {
        return Some(("mutable-treelist", vec![x.owner.load(Ordering::Relaxed)]));
    }
    if let Some(x) = heap.object_ref::<TreelistNode>(word) {
        return Some((
            "treelist-node",
            vec![
                x.owner.load(Ordering::Relaxed),
                x.weight.load(Ordering::Relaxed) as u64,
                x.depth as u64,
            ],
        ));
    }
    if let Some(x) = heap.object_ref::<TreelistSpan>(word) {
        return Some((
            "treelist-span",
            vec![
                x.owner.load(Ordering::Relaxed),
                x.weight.load(Ordering::Relaxed) as u64,
                x.start.load(Ordering::Relaxed) as u64,
                x.len.load(Ordering::Relaxed) as u64,
            ],
        ));
    }
    heap.object_ref::<TreelistChunk>(word)
        .map(|x| ("treelist-chunk", vec![x.owner.load(Ordering::Relaxed)]))
}
pub(in crate::heap) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if matches!(
        kind,
        "mutable-treelist" | "treelist-node" | "treelist-span" | "treelist-chunk"
    ) {
        let owner = *data.first().ok_or_else(super::super::image::invalid)?;
        let next = owner
            .checked_add(1)
            .ok_or_else(super::super::image::invalid)?;
        heap.next_treelist_owner
            .set(heap.next_treelist_owner.get().max(next));
    }
    let mut d = super::super::image::Data(data);
    let word = match kind {
        "treelist" => {
            d.done()?;
            heap.alloc_object(|header| Treelist {
                header,
                node: WordSlot::nil(),
            })
        }
        "mutable-treelist" => {
            let owner = d.number()? as u64;
            d.done()?;
            heap.alloc_object(|header| MutableTreelist {
                header,
                owner: AtomicU64::new(owner),
                node: WordSlot::nil(),
            })
        }
        "treelist-node" => {
            let owner = d.number()? as u64;
            let weight = d.number()?;
            let depth = u8::try_from(d.number()?).map_err(|_| super::super::image::invalid())?;
            d.done()?;
            heap.alloc_object(|header| TreelistNode {
                header,
                owner: AtomicU64::new(owner),
                weight: AtomicUsize::new(weight),
                depth,
                front: WordSlot::nil(),
                middle: WordSlot::nil(),
                back: WordSlot::nil(),
            })
        }
        "treelist-span" => {
            let owner = d.number()? as u64;
            let weight = d.number()?;
            let start = d.number()?;
            let len = d.number()?;
            d.done()?;
            if start >= CHUNK_CAPACITY || !(1..=CHUNK_CAPACITY).contains(&len) {
                return Err(super::super::image::invalid());
            }
            heap.alloc_object(|header| TreelistSpan {
                header,
                owner: AtomicU64::new(owner),
                weight: AtomicUsize::new(weight),
                start: AtomicU8::new(start as u8),
                len: AtomicU8::new(len as u8),
                chunk: WordSlot::nil(),
            })
        }
        "treelist-chunk" => {
            let owner = d.number()? as u64;
            d.done()?;
            heap.alloc_object(|header| TreelistChunk {
                header,
                owner: AtomicU64::new(owner),
                slots: std::array::from_fn(|_| WordSlot::nil()),
            })
        }
        _ => return Ok(None),
    };
    word.map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
