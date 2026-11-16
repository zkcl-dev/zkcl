use super::super::Heap;
use super::span::span_len;
use super::*;
use crate::heap::HeapConfig;

fn stress_heap() -> Heap {
    Heap::start(HeapConfig {
        always_defrag: true,
        stress_bytes: Some(1),
        ..HeapConfig::default()
    })
    .unwrap()
}

fn collect_vec(heap: &Heap, word: Word) -> Vec<isize> {
    let mut out = Vec::new();
    heap.treelist_for_each(word, |w| out.push(w.as_fixnum().unwrap()));
    out
}

fn verify_node_invariants(heap: &Heap, node: Word, expected_depth: u8) -> usize {
    if node == Word::NIL {
        return 0;
    }
    let n = heap.expect_object::<TreelistNode>(node, "treelist node");
    assert_eq!(n.depth, expected_depth);
    let front = n.front.get();
    let middle = n.middle.get();
    let back = n.back.get();
    assert_ne!(front, Word::NIL, "non-empty node must have non-nil front");
    if middle != Word::NIL {
        assert_ne!(back, Word::NIL, "node with middle must have non-nil back");
    }
    let fw = verify_span_invariants(heap, front, expected_depth);
    let mw = verify_node_invariants(heap, middle, expected_depth + 1);
    let bw = verify_span_invariants(heap, back, expected_depth);
    assert_eq!(n.weight.load(Ordering::Relaxed), fw + mw + bw);
    if expected_depth >= 1 {
        // Every adjacent pair in `middle` (depth >= 1) should not merge down.
        verify_middle_span_pair(heap, front);
        verify_middle_span_pair(heap, back);
    }
    fw + mw + bw
}

fn verify_middle_span_pair(heap: &Heap, span_word: Word) {
    if span_word == Word::NIL {
        return;
    }
    let s = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
    let len = usize::from(s.len.load(Ordering::Relaxed));
    let start = usize::from(s.start.load(Ordering::Relaxed));
    let chunk = heap.expect_object::<TreelistChunk>(s.chunk.get(), "treelist chunk");
    for i in 0..len.saturating_sub(1) {
        let a = chunk.slots[(start + i) & CHUNK_MASK].get();
        let b = chunk.slots[(start + i + 1) & CHUNK_MASK].get();
        assert!(
            span_len(heap, a) + span_len(heap, b) > CHUNK_CAPACITY,
            "adjacent middle spans should satisfy density invariant: {} + {}",
            span_len(heap, a),
            span_len(heap, b)
        );
    }
}

fn verify_span_invariants(heap: &Heap, span_word: Word, depth: u8) -> usize {
    if span_word == Word::NIL {
        return 0;
    }
    let s = heap.expect_object::<TreelistSpan>(span_word, "treelist span");
    let len = usize::from(s.len.load(Ordering::Relaxed));
    let start = usize::from(s.start.load(Ordering::Relaxed));
    assert!((1..=CHUNK_CAPACITY).contains(&len));
    assert!(start < CHUNK_CAPACITY);
    let chunk = heap.expect_object::<TreelistChunk>(s.chunk.get(), "treelist chunk");
    let expected_w = if depth == 0 {
        len
    } else {
        let mut sum = 0;
        for i in 0..len {
            let child = chunk.slots[(start + i) & CHUNK_MASK].get();
            sum += verify_span_invariants(heap, child, depth - 1);
        }
        sum
    };
    assert_eq!(s.weight.load(Ordering::Relaxed), expected_w);
    expected_w
}

fn verify_treelist(heap: &Heap, word: Word, expected: &[isize]) {
    assert_eq!(heap.treelist_len(word), Some(expected.len()));
    assert_eq!(collect_vec(heap, word), expected);
    for (i, &val) in expected.iter().enumerate() {
        assert_eq!(heap.treelist_get(word, i).unwrap().as_fixnum(), Some(val));
    }
    assert!(heap.treelist_get(word, expected.len()).is_none());
    let node = if let Some(tl) = heap.object_ref::<Treelist>(word) {
        tl.node.get()
    } else {
        heap.expect_object::<MutableTreelist>(word, "mutable treelist")
            .node
            .get()
    };
    assert_eq!(verify_node_invariants(heap, node, 0), expected.len());
}

#[test]
fn persistent_and_transient_roundtrip_under_gc_stress() {
    let heap = stress_heap();
    let mtl = heap.root_slot(heap.mutable_treelist_empty().unwrap());
    let mut model = Vec::new();

    for i in 0..160isize {
        let v = heap.root_slot(Word::fixnum(i).unwrap());
        if i % 2 == 0 {
            heap.mutable_treelist_push_back(&mtl, &v).unwrap();
            model.push(i);
        } else {
            heap.mutable_treelist_push_front(&mtl, &v).unwrap();
            model.insert(0, i);
        }
    }
    verify_treelist(&heap, mtl.get(), &model);

    let snap1 = heap.root_slot(heap.treelist_snapshot(&mtl).unwrap());
    let model_snap1 = model.clone();

    for i in 0..50isize {
        let idx = (i as usize * 7) % model.len();
        let v = heap.root_slot(Word::fixnum(1000 + i).unwrap());
        heap.mutable_treelist_set(&mtl, idx, &v).unwrap();
        model[idx] = 1000 + i;
    }
    verify_treelist(&heap, snap1.get(), &model_snap1);
    verify_treelist(&heap, mtl.get(), &model);

    // Split, subseq, and concat on persistent snapshots.
    let (left, right) = heap.treelist_split(&snap1, 73).unwrap();
    let left = heap.root_slot(left);
    let right = heap.root_slot(right);
    verify_treelist(&heap, left.get(), &model_snap1[..73]);
    verify_treelist(&heap, right.get(), &model_snap1[73..]);

    let rejoined = heap.root_slot(heap.treelist_concat(&left, &right).unwrap());
    verify_treelist(&heap, rejoined.get(), &model_snap1);

    let sub = heap.root_slot(heap.treelist_subseq(&rejoined, 20, 125).unwrap());
    verify_treelist(&heap, sub.get(), &model_snap1[20..125]);

    // Freeze & thaw.
    let frozen = heap.root_slot(heap.treelist_freeze(&mtl).unwrap());
    verify_treelist(&heap, mtl.get(), &[]);
    verify_treelist(&heap, frozen.get(), &model);

    let thawed = heap.root_slot(heap.treelist_thaw(&frozen).unwrap());
    heap.mutable_treelist_insert(&thawed, 40, &heap.root_slot(Word::fixnum(-77).unwrap()))
        .unwrap();
    let mut thawed_model = model.clone();
    thawed_model.insert(40, -77);
    verify_treelist(&heap, frozen.get(), &model);
    verify_treelist(&heap, thawed.get(), &thawed_model);

    heap.mutable_treelist_delete(&thawed, 40).unwrap();
    verify_treelist(&heap, thawed.get(), &model);
}

#[test]
fn deep_tree_split_concat_and_pop_all_under_stress() {
    let heap = stress_heap();
    let mut tl = heap.root_slot(heap.treelist_empty().unwrap());
    let mut model = Vec::new();
    for i in 0..1200isize {
        let v = heap.root_slot(Word::fixnum(i).unwrap());
        let next = if i % 3 == 0 {
            model.insert(0, i);
            heap.treelist_push_front(&tl, &v).unwrap()
        } else {
            model.push(i);
            heap.treelist_push_back(&tl, &v).unwrap()
        };
        tl = heap.root_slot(next);
    }
    verify_treelist(&heap, tl.get(), &model);

    for split_at in [0, 1, 31, 32, 33, 64, 511, 1024, 1199, 1200] {
        let (l, r) = heap.treelist_split(&tl, split_at).unwrap();
        let l = heap.root_slot(l);
        let r = heap.root_slot(r);
        verify_treelist(&heap, l.get(), &model[..split_at]);
        verify_treelist(&heap, r.get(), &model[split_at..]);
        let back = heap.root_slot(heap.treelist_concat(&l, &r).unwrap());
        verify_treelist(&heap, back.get(), &model);
    }

    let mtl = heap.root_slot(heap.treelist_thaw(&tl).unwrap());
    let mut mtl_model = model.clone();
    while !mtl_model.is_empty() {
        if mtl_model.len() % 2 == 0 {
            let got = heap.mutable_treelist_pop_front(&mtl).unwrap();
            let exp = mtl_model.remove(0);
            assert_eq!(got.as_fixnum(), Some(exp));
        } else {
            let got = heap.mutable_treelist_pop_back(&mtl).unwrap();
            let exp = mtl_model.pop().unwrap();
            assert_eq!(got.as_fixnum(), Some(exp));
        }
    }
    verify_treelist(&heap, mtl.get(), &[]);
    verify_treelist(&heap, tl.get(), &model);
}
