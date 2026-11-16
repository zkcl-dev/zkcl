//! VM primitives and package initialization for `ZK-TREELIST`.

use std::ops::Range;

use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, ArrayValue, RootSlots, TreelistKind, Word};
use crate::package::Package;
use crate::{EvalError, Runtime};

use super::{Primitive, Values, arrays, named_keyword_arguments};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Treelist,
    MutableTreelist,
    Make,
    Empty,
    TreelistP,
    MutableTreelistP,
    EmptyP,
    Length,
    Ref,
    First,
    Last,
    Set,
    PushFront,
    PushBack,
    PopFront,
    PopBack,
    Insert,
    Delete,
    Append,
    Subseq,
    Split,
    SetBang,
    PushFrontBang,
    PushBackBang,
    PopFrontBang,
    PopBackBang,
    InsertBang,
    DeleteBang,
    AppendBang,
    SplitBang,
    ClearBang,
    Copy,
    Thaw,
    Snapshot,
    Freeze,
    FromSequence,
    ToList,
    ToVector,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("ZK-TREELIST:TREELIST", Op::Treelist),
    ("ZK-TREELIST:MUTABLE-TREELIST", Op::MutableTreelist),
    ("ZK-TREELIST:MAKE", Op::Make),
    ("ZK-TREELIST:EMPTY", Op::Empty),
    ("ZK-TREELIST:TREELIST-P", Op::TreelistP),
    ("ZK-TREELIST:MUTABLE-TREELIST-P", Op::MutableTreelistP),
    ("ZK-TREELIST:EMPTY-P", Op::EmptyP),
    ("ZK-TREELIST:LENGTH", Op::Length),
    ("ZK-TREELIST:REF", Op::Ref),
    ("ZK-TREELIST:FIRST", Op::First),
    ("ZK-TREELIST:LAST", Op::Last),
    ("ZK-TREELIST:SET", Op::Set),
    ("ZK-TREELIST:PUSH-FRONT", Op::PushFront),
    ("ZK-TREELIST:PUSH-BACK", Op::PushBack),
    ("ZK-TREELIST:POP-FRONT", Op::PopFront),
    ("ZK-TREELIST:POP-BACK", Op::PopBack),
    ("ZK-TREELIST:INSERT", Op::Insert),
    ("ZK-TREELIST:DELETE", Op::Delete),
    ("ZK-TREELIST:APPEND", Op::Append),
    ("ZK-TREELIST:SUBSEQ", Op::Subseq),
    ("ZK-TREELIST:SPLIT", Op::Split),
    ("ZK-TREELIST:SET!", Op::SetBang),
    ("ZK-TREELIST:PUSH-FRONT!", Op::PushFrontBang),
    ("ZK-TREELIST:PUSH-BACK!", Op::PushBackBang),
    ("ZK-TREELIST:POP-FRONT!", Op::PopFrontBang),
    ("ZK-TREELIST:POP-BACK!", Op::PopBackBang),
    ("ZK-TREELIST:INSERT!", Op::InsertBang),
    ("ZK-TREELIST:DELETE!", Op::DeleteBang),
    ("ZK-TREELIST:APPEND!", Op::AppendBang),
    ("ZK-TREELIST:SPLIT!", Op::SplitBang),
    ("ZK-TREELIST:CLEAR!", Op::ClearBang),
    ("ZK-TREELIST:COPY", Op::Copy),
    ("ZK-TREELIST:THAW", Op::Thaw),
    ("ZK-TREELIST:SNAPSHOT", Op::Snapshot),
    ("ZK-TREELIST:FREEZE", Op::Freeze),
    ("ZK-TREELIST:FROM-SEQUENCE", Op::FromSequence),
    ("ZK-TREELIST:TO-LIST", Op::ToList),
    ("ZK-TREELIST:TO-VECTOR", Op::ToVector),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Treelist | Self::MutableTreelist | Self::Append => true,
            Self::Empty => count == 0,
            Self::TreelistP
            | Self::MutableTreelistP
            | Self::EmptyP
            | Self::Length
            | Self::First
            | Self::Last
            | Self::PopFront
            | Self::PopBack
            | Self::PopFrontBang
            | Self::PopBackBang
            | Self::ClearBang
            | Self::Copy
            | Self::Thaw
            | Self::Snapshot
            | Self::Freeze
            | Self::FromSequence
            | Self::ToList => count == 1,
            Self::Make | Self::ToVector => count >= 1,
            Self::Ref
            | Self::PushFront
            | Self::PushBack
            | Self::Delete
            | Self::Split
            | Self::PushFrontBang
            | Self::PushBackBang
            | Self::DeleteBang
            | Self::AppendBang
            | Self::SplitBang => count == 2,
            Self::Subseq => (2..=3).contains(&count),
            Self::Set | Self::Insert | Self::SetBang | Self::InsertBang => count == 3,
        }
    }
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    {
        let mut packages = runtime.packages.borrow_mut();
        packages.reserve_symbols(Package::TREELIST, PRIMITIVES.len() + 8);
    }

    let export_treelist = |name: &str| -> Result<Word, EvalError> {
        let symbol = runtime.intern_package(Package::TREELIST, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .insert(Package::TREELIST, name.to_owned(), symbol, true);
        Ok(symbol)
    };

    for &(full_name, _) in PRIMITIVES {
        let local = full_name
            .strip_prefix("ZK-TREELIST:")
            .expect("treelist primitive prefix");
        let symbol = export_treelist(local)?;
        let function = Primitive::named(full_name).expect("registered treelist primitive");
        runtime.heap.set_symbol_cell(symbol, true, function);
    }

    for extra in ["WITH-TRANSIENT", "DO-ELEMENTS", "EQUAL-P"] {
        export_treelist(extra)?;
    }

    for name in [
        "TREELIST",
        "MUTABLE-TREELIST",
        "TREELIST-P",
        "MUTABLE-TREELIST-P",
    ] {
        let symbol = runtime.intern_package(Package::TREELIST, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .insert(Package::ZK, name.to_owned(), symbol, true);
    }

    runtime.load_builtin_source(include_str!("../../lisp/runtime/treelist.lisp"))?;
    Ok(())
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

fn oom(at: usize, error: crate::OutOfMemory) -> Fault {
    EvalError::out_of_memory(at, error).into()
}

fn expect_treelist(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    if let Some(TreelistKind::Persistent) = runtime.heap.treelist_kind(word) {
        Ok(runtime.heap.treelist_len(word).expect("treelist length"))
    } else {
        let datum = runtime.root_slot(word);
        let expected = runtime.intern_package(Package::TREELIST, "TREELIST", at)?.0;
        Err(Fault::type_error_specifier(
            runtime,
            datum.get(),
            expected,
            EvalError::new(at, "expected an immutable treelist"),
        ))
    }
}

pub(in crate::vm) fn expect_mutable_treelist(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<usize, Fault> {
    if let Some(TreelistKind::Mutable) = runtime.heap.treelist_kind(word) {
        Ok(runtime.heap.treelist_len(word).expect("treelist length"))
    } else {
        let datum = runtime.root_slot(word);
        let expected = runtime
            .intern_package(Package::TREELIST, "MUTABLE-TREELIST", at)?
            .0;
        Err(Fault::type_error_specifier(
            runtime,
            datum.get(),
            expected,
            EvalError::new(at, "expected a mutable-treelist"),
        ))
    }
}

fn expect_any_treelist(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    runtime.heap.treelist_len(word).ok_or_else(|| {
        Fault::type_error(
            runtime,
            word,
            "SEQUENCE",
            EvalError::new(at, "expected a treelist or mutable-treelist"),
        )
    })
}

fn non_negative_fixnum(
    runtime: &Runtime,
    word: Word,
    what: &str,
    at: usize,
) -> Result<usize, Fault> {
    word.as_fixnum()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| Fault::non_negative_integer(runtime, word, EvalError::new(at, what)))
}

fn check_index_lt(
    runtime: &Runtime,
    raw_index: Word,
    len: usize,
    at: usize,
) -> Result<usize, Fault> {
    let idx = non_negative_fixnum(
        runtime,
        raw_index,
        "expected a non-negative treelist index",
        at,
    )?;
    if idx < len {
        return Ok(idx);
    }
    let host = EvalError::new(at, "treelist index is out of bounds");
    if len == 0 {
        Err(Fault::type_error(runtime, raw_index, "NIL", host))
    } else if let Ok(upper) = isize::try_from(len - 1) {
        Err(Fault::integer_range(runtime, raw_index, 0, upper, host))
    } else {
        Err(Fault::non_negative_integer(runtime, raw_index, host))
    }
}

fn check_index_le(
    runtime: &Runtime,
    raw_index: Word,
    len: usize,
    at: usize,
) -> Result<usize, Fault> {
    let idx = non_negative_fixnum(
        runtime,
        raw_index,
        "expected a non-negative treelist index",
        at,
    )?;
    if idx <= len {
        return Ok(idx);
    }
    let host = EvalError::new(at, "treelist index is out of bounds");
    if let Ok(upper) = isize::try_from(len) {
        Err(Fault::integer_range(runtime, raw_index, 0, upper, host))
    } else {
        Err(Fault::non_negative_integer(runtime, raw_index, host))
    }
}

fn check_subseq_bounds(
    runtime: &Runtime,
    start_word: Word,
    end_word: Option<Word>,
    len: usize,
    at: usize,
) -> Result<(usize, usize), Fault> {
    let start = non_negative_fixnum(
        runtime,
        start_word,
        "expected a non-negative start index",
        at,
    )?;
    let end = match end_word.filter(|&w| w != Word::NIL) {
        Some(w) => non_negative_fixnum(runtime, w, "expected a non-negative end index", at)?,
        None => len,
    };
    if start > end || end > len {
        return Err(Fault::plain(
            Class::Error,
            EvalError::new(at, "bounding indices are outside the treelist"),
        ));
    }
    Ok((start, end))
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let heap = &runtime.heap;
    let arg = |i| frame.get(args.start + i);

    let single = match op {
        Op::Treelist | Op::MutableTreelist => {
            let slots = frame.slice(args.clone());
            if matches!(op, Op::Treelist) {
                heap.treelist_from_slots(slots, slots.len())
            } else {
                heap.mutable_treelist_from_slots(slots, slots.len())
            }
            .map_err(|err| oom(at, err))?
        }
        Op::Make => {
            let [initial_element, mutable] = named_keyword_arguments(
                runtime,
                frame,
                args.start + 1..args.end,
                ["INITIAL-ELEMENT", "MUTABLE"],
                at,
            )?;
            let count = non_negative_fixnum(runtime, arg(0), "expected a non-negative count", at)?;
            let init = runtime.root_slot(initial_element.map_or(Word::NIL, |reg| frame.get(reg)));
            let is_mut = mutable.is_some_and(|reg| frame.get(reg) != Word::NIL);
            if is_mut {
                heap.mutable_treelist_repeat(count, &init)
                    .map_err(|err| oom(at, err))?
            } else {
                heap.treelist_repeat(count, &init)
                    .map_err(|err| oom(at, err))?
            }
        }
        Op::Empty => heap.treelist_empty().map_err(|err| oom(at, err))?,
        Op::TreelistP => truth(heap.is_treelist(arg(0))),
        Op::MutableTreelistP => truth(heap.is_mutable_treelist(arg(0))),
        Op::EmptyP => {
            let len = expect_any_treelist(runtime, arg(0), at)?;
            truth(len == 0)
        }
        Op::Length => {
            let len = expect_any_treelist(runtime, arg(0), at)?;
            isize::try_from(len)
                .ok()
                .and_then(Word::fixnum)
                .ok_or_else(|| EvalError::new(at, "treelist length exceeds fixnum range"))?
        }
        Op::Ref => {
            let seq = arg(0);
            let len = expect_any_treelist(runtime, seq, at)?;
            let index = check_index_lt(runtime, arg(1), len, at)?;
            heap.treelist_get(seq, index).expect("validated index")
        }
        Op::First => {
            let seq = arg(0);
            let len = expect_any_treelist(runtime, seq, at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot take FIRST of an empty treelist"),
                ));
            }
            heap.treelist_get(seq, 0).expect("non-empty treelist")
        }
        Op::Last => {
            let seq = arg(0);
            let len = expect_any_treelist(runtime, seq, at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot take LAST of an empty treelist"),
                ));
            }
            heap.treelist_get(seq, len - 1).expect("non-empty treelist")
        }
        Op::Set => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            let index = check_index_lt(runtime, arg(1), len, at)?;
            let val = runtime.root_slot(arg(2));
            heap.treelist_set(&tl, index, &val)
                .map_err(|err| oom(at, err))?
        }
        Op::PushFront => {
            let tl = runtime.root_slot(arg(0));
            expect_treelist(runtime, tl.get(), at)?;
            let val = runtime.root_slot(arg(1));
            heap.treelist_push_front(&tl, &val)
                .map_err(|err| oom(at, err))?
        }
        Op::PushBack => {
            let tl = runtime.root_slot(arg(0));
            expect_treelist(runtime, tl.get(), at)?;
            let val = runtime.root_slot(arg(1));
            heap.treelist_push_back(&tl, &val)
                .map_err(|err| oom(at, err))?
        }
        Op::PopFront => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot pop an empty treelist"),
                ));
            }
            let (head, rest) = heap.treelist_pop_front(&tl).map_err(|err| oom(at, err))?;
            values.resize(2);
            values.slots.set(0, head);
            values.slots.set(1, rest);
            return Ok(());
        }
        Op::PopBack => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot pop an empty treelist"),
                ));
            }
            let (tail, rest) = heap.treelist_pop_back(&tl).map_err(|err| oom(at, err))?;
            values.resize(2);
            values.slots.set(0, tail);
            values.slots.set(1, rest);
            return Ok(());
        }
        Op::Insert => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            let index = check_index_le(runtime, arg(1), len, at)?;
            let val = runtime.root_slot(arg(2));
            heap.treelist_insert(&tl, index, &val)
                .map_err(|err| oom(at, err))?
        }
        Op::Delete => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            let index = check_index_lt(runtime, arg(1), len, at)?;
            heap.treelist_delete(&tl, index)
                .map_err(|err| oom(at, err))?
        }
        Op::Append => {
            for i in 0..args.len() {
                expect_treelist(runtime, arg(i), at)?;
            }
            if args.is_empty() {
                heap.treelist_empty().map_err(|err| oom(at, err))?
            } else {
                let mut acc = runtime.root_slot(arg(0));
                for i in 1..args.len() {
                    let rhs = runtime.root_slot(arg(i));
                    acc = runtime.root_slot(
                        heap.treelist_concat(&acc, &rhs)
                            .map_err(|err| oom(at, err))?,
                    );
                }
                acc.get()
            }
        }
        Op::Subseq => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            let end_word = (args.len() == 3).then(|| arg(2));
            let (start, end) = check_subseq_bounds(runtime, arg(1), end_word, len, at)?;
            heap.treelist_subseq(&tl, start, end)
                .map_err(|err| oom(at, err))?
        }
        Op::Split => {
            let tl = runtime.root_slot(arg(0));
            let len = expect_treelist(runtime, tl.get(), at)?;
            let index = check_index_le(runtime, arg(1), len, at)?;
            let (left, right) = heap
                .treelist_split(&tl, index)
                .map_err(|err| oom(at, err))?;
            values.resize(2);
            values.slots.set(0, left);
            values.slots.set(1, right);
            return Ok(());
        }
        Op::SetBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            let index = check_index_lt(runtime, arg(1), len, at)?;
            let val = runtime.root_slot(arg(2));
            heap.mutable_treelist_set(&mtl, index, &val)
                .map_err(|err| oom(at, err))?;
            val.get()
        }
        Op::PushFrontBang => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            let val = runtime.root_slot(arg(1));
            heap.mutable_treelist_push_front(&mtl, &val)
                .map_err(|err| oom(at, err))?;
            mtl.get()
        }
        Op::PushBackBang => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            let val = runtime.root_slot(arg(1));
            heap.mutable_treelist_push_back(&mtl, &val)
                .map_err(|err| oom(at, err))?;
            mtl.get()
        }
        Op::PopFrontBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot pop an empty mutable-treelist"),
                ));
            }
            heap.mutable_treelist_pop_front(&mtl)
                .map_err(|err| oom(at, err))?
        }
        Op::PopBackBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            if len == 0 {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "cannot pop an empty mutable-treelist"),
                ));
            }
            heap.mutable_treelist_pop_back(&mtl)
                .map_err(|err| oom(at, err))?
        }
        Op::InsertBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            let index = check_index_le(runtime, arg(1), len, at)?;
            let val = runtime.root_slot(arg(2));
            heap.mutable_treelist_insert(&mtl, index, &val)
                .map_err(|err| oom(at, err))?;
            mtl.get()
        }
        Op::DeleteBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            let index = check_index_lt(runtime, arg(1), len, at)?;
            heap.mutable_treelist_delete(&mtl, index)
                .map_err(|err| oom(at, err))?;
            mtl.get()
        }
        Op::AppendBang => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            let other = runtime.root_slot(arg(1));
            expect_any_treelist(runtime, other.get(), at)?;
            heap.mutable_treelist_append(&mtl, &other)
                .map_err(|err| oom(at, err))?;
            mtl.get()
        }
        Op::SplitBang => {
            let mtl = runtime.root_slot(arg(0));
            let len = expect_mutable_treelist(runtime, mtl.get(), at)?;
            let index = check_index_le(runtime, arg(1), len, at)?;
            heap.mutable_treelist_split(&mtl, index)
                .map_err(|err| oom(at, err))?
        }
        Op::ClearBang => {
            let mtl = arg(0);
            expect_mutable_treelist(runtime, mtl, at)?;
            heap.mutable_treelist_clear(mtl);
            mtl
        }
        Op::Copy => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            heap.mutable_treelist_copy(&mtl)
                .map_err(|err| oom(at, err))?
        }
        Op::Thaw => {
            let tl = runtime.root_slot(arg(0));
            expect_treelist(runtime, tl.get(), at)?;
            heap.treelist_thaw(&tl).map_err(|err| oom(at, err))?
        }
        Op::Snapshot => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            heap.treelist_snapshot(&mtl).map_err(|err| oom(at, err))?
        }
        Op::Freeze => {
            let mtl = runtime.root_slot(arg(0));
            expect_mutable_treelist(runtime, mtl.get(), at)?;
            heap.treelist_freeze(&mtl).map_err(|err| oom(at, err))?
        }
        Op::FromSequence => {
            let seq = arg(0);
            if heap.is_treelist(seq) {
                seq
            } else if heap.is_mutable_treelist(seq) {
                let mtl = runtime.root_slot(seq);
                heap.treelist_snapshot(&mtl).map_err(|err| oom(at, err))?
            } else {
                let elements = arrays::sequence_elements(runtime, seq, at)?;
                heap.treelist_from_slots(&elements, elements.len())
                    .map_err(|err| oom(at, err))?
            }
        }
        Op::ToList => {
            let seq = runtime.root_slot(arg(0));
            let len = expect_any_treelist(runtime, seq.get(), at)?;
            let elements = runtime.root_frame(len.min(512));
            let list = runtime.root_frame(1);
            let mut end = len;
            while end > 0 {
                let start = end.saturating_sub(elements.len());
                heap.treelist_copy_into_frame(seq.get(), start, end, &elements, 0);
                for index in (0..end - start).rev() {
                    let cell = heap
                        .cons(|| (elements.get(index), list.get(0)))
                        .map_err(|err| oom(at, err))?;
                    list.set(0, cell);
                }
                end = start;
            }
            list.get(0)
        }
        Op::ToVector => {
            let seq = runtime.root_slot(arg(0));
            let len = expect_any_treelist(runtime, seq.get(), at)?;
            let [element_type] = named_keyword_arguments(
                runtime,
                frame,
                args.start + 1..args.end,
                ["ELEMENT-TYPE"],
                at,
            )?;
            let element = match element_type {
                Some(reg) => match arrays::upgraded_type(runtime, frame.get(reg), at)? {
                    Some(elem) => elem,
                    None => {
                        if len == 0 {
                            return heap
                                .nil_element_array(&[0], false, None, 0, |_| '\0')
                                .map(|word| values.one(word))
                                .map_err(|err| oom(at, err));
                        }
                        return Err(Fault::type_error(
                            runtime,
                            heap.treelist_get(seq.get(), 0).expect("non-empty"),
                            "NIL",
                            EvalError::new(at, "treelist element cannot fit (ARRAY NIL)"),
                        ));
                    }
                },
                None => ArrayElement::General,
            };
            arrays::total_size(&[len], at)?;
            if element != ArrayElement::General {
                let minimum_bytes = match element {
                    ArrayElement::Bit => Some(len.div_ceil(u8::BITS as usize)),
                    ArrayElement::Octet | ArrayElement::Character => Some(len),
                    ArrayElement::SingleFloat => len.checked_mul(size_of::<f32>()),
                    ArrayElement::DoubleFloat => len.checked_mul(size_of::<f64>()),
                    ArrayElement::General => unreachable!(),
                };
                if minimum_bytes.is_none_or(|bytes| bytes > heap.max_heap_bytes()) {
                    return Err(oom(at, crate::OutOfMemory));
                }
                let invalid = heap
                    .treelist_iter(seq.get())
                    .find(|&word| ArrayValue::from_word(heap, element, word).is_none());
                if let Some(word) = invalid {
                    return Err(arrays::invalid_element(runtime, element, word, at));
                }
            }
            let mut cursor = None;
            heap.array(&[len], element, false, None, |index| {
                let items = cursor.get_or_insert_with(|| heap.treelist_iter(seq.get()));
                let word = items.next().expect("treelist element");
                // UTF-8 sizing and copying run on opposite sides of an allocation.
                if index + 1 == len {
                    cursor = None;
                }
                word
            })
            .map_err(|err| oom(at, err))?
        }
    };

    values.one(single);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treelist_and_mutable_treelist_api_and_cow_snapshots() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                r#"(progn
                     (assert (member :zk-treelist *features*))
                     (assert (eq 'zk:treelist 'zk-treelist:treelist))
                     (assert (eq 'zk:mutable-treelist 'zk-treelist:mutable-treelist))
                     (assert (eq 'zk:treelist-p 'zk-treelist:treelist-p))
                     (assert (eq 'zk:mutable-treelist-p 'zk-treelist:mutable-treelist-p))
                     (let* ((tl (zk-treelist:treelist 10 20 30))
                            (tl2 (zk-treelist:push-front (zk-treelist:push-back tl 40) 5)))
                       (assert (zk-treelist:treelist-p tl))
                       (assert (not (zk-treelist:mutable-treelist-p tl)))
                       (assert (typep tl 'zk-treelist:treelist))
                       (assert (typep tl 'sequence))
                       (assert (equal '(t t) (multiple-value-list (subtypep 'zk-treelist:treelist 'sequence))))
                       (assert (equal '(t t) (multiple-value-list (subtypep 'zk-treelist:mutable-treelist 'sequence))))
                       (assert (equal '(10 20 30) (zk-treelist:to-list tl)))
                       (assert (equal '(5 10 20 30 40) (zk-treelist:to-list tl2)))
                       (assert (equal tl (zk-treelist:treelist 10 20 30)))
                       (assert (= (sxhash tl) (sxhash (zk-treelist:treelist 10 20 30))))
                       (let ((mtl (zk-treelist:thaw tl2)))
                         (setf (zk-treelist:ref mtl 2) 99)
                         (zk-treelist:push-back! mtl 50)
                         (let ((snap (zk-treelist:snapshot mtl)))
                           (zk-treelist:pop-front! mtl)
                           (assert (equal '(5 10 20 30 40) (zk-treelist:to-list tl2)))
                           (assert (equal '(5 10 99 30 40 50) (zk-treelist:to-list snap)))
                           (assert (equal '(10 99 30 40 50) (zk-treelist:to-list mtl))))))
                     (let ((built (zk-treelist:with-transient (acc (zk-treelist:treelist 1 2))
                                    (zk-treelist:push-back! acc 3)
                                    (zk-treelist:push-front! acc 0))))
                       (assert (zk-treelist:treelist-p built))
                       (assert (equal '(0 1 2 3) (zk-treelist:to-list built))))
                     t)"#,
            )
            .unwrap();
        assert!(values[0].is_t());
    }

    #[test]
    fn treelist_cl_sequence_and_zk_match_integration() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                r#"(let* ((tl (zk:treelist 1 2 3 4 5))
                          (mtl (zk-treelist:thaw tl)))
                     (assert (= 5 (length tl)))
                     (assert (= 3 (elt tl 2)))
                     (setf (elt mtl 2) 30)
                     (assert (= 30 (elt mtl 2)))
                     (assert (= 3 (elt tl 2)))
                     (assert (equal '(2 3 4) (zk-treelist:to-list (subseq tl 1 4))))
                     (assert (equal '(5 4 3 2 1) (zk-treelist:to-list (reverse tl))))
                     (assert (equal '(10 20 30)
                                    (zk-treelist:to-list
                                     (map 'zk:treelist (lambda (x) (* x 10)) '(1 2 3)))))
                     (assert (equal '(1 2 3 4)
                                    (zk-treelist:to-list
                                     (concatenate 'zk:treelist '(1 2) #(3 4)))))
                     (assert (equal '(1 2 3)
                                    (zk-treelist:to-list (coerce #(1 2 3) 'zk:treelist))))
                     (assert (= 15 (reduce #'+ tl)))
                     (assert (equal '(1 (2 3 4) 5)
                                    (zk:match tl
                                      ((zk:treelist a (:rest mid) b)
                                       (list a (zk-treelist:to-list mid) b))
                                      (_ nil))))
                     t)"#,
            )
            .unwrap();
        assert!(values[0].is_t());
    }

    #[test]
    fn treelist_conversions_and_comparisons_follow_chunk_order() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                r#"(let* ((seq (zk-treelist:make 1031 :initial-element 0 :mutable t)))
                     (setf (elt seq 511) 1 (elt seq 512) 2 (elt seq 1030) 3)
                     (let* ((frozen (zk-treelist:snapshot seq))
                            (vector (zk-treelist:to-vector frozen))
                            (rebuilt (zk-treelist:from-sequence vector)))
                       (assert (equal frozen rebuilt))
                       (assert (= (sxhash frozen) (sxhash rebuilt)))
                       (assert (equalp seq rebuilt))
                       (assert (equal (zk-treelist:to-list frozen) (coerce vector 'list)))
                       (assert (= (aref vector 511) 1))
                       (assert (= (aref vector 512) 2))
                       (assert (= (aref vector 1030) 3))
                       (setf (elt seq 1030) 4)
                       (assert (not (equalp seq rebuilt))))
                     (assert (equal "aλ文"
                                    (zk-treelist:to-vector (zk:treelist #\a #\λ #\文)
                                                           :element-type 'character)))
                     (assert (equal #*10011
                                    (zk-treelist:to-vector (zk:treelist 1 0 0 1 1)
                                                           :element-type 'bit)))
                     (assert (handler-case
                                 (progn (zk-treelist:to-vector (zk:treelist #\a 1)
                                                               :element-type 'character)
                                        nil)
                               (type-error () t)))
                     t)"#,
            )
            .unwrap();
        assert!(values[0].is_t());
    }

    #[test]
    fn treelist_reference_model_oracle_and_load_form() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                r#"(let ((seed 123456789))
                     (labels ((next-rand (limit)
                                (setf seed (logand (+ (* seed 1103515245) 12345) #x7fffffff))
                                (if (zerop limit) 0 (mod seed limit))))
                       (let ((tl (zk-treelist:empty))
                             (mtl (zk-treelist:mutable-treelist))
                             (tl-model nil)
                             (mtl-model nil)
                             (history nil))
                         (dotimes (step 320)
                           (let ((op (next-rand 12))
                                 (val (+ (* step 10) (next-rand 7))))
                             (case op
                               (0
                                (setf tl (zk-treelist:push-front tl val)
                                      tl-model (cons val tl-model))
                                (zk-treelist:push-front! mtl val)
                                (setf mtl-model (cons val mtl-model)))
                               (1
                                (setf tl (zk-treelist:push-back tl val)
                                      tl-model (append tl-model (list val)))
                                (zk-treelist:push-back! mtl val)
                                (setf mtl-model (append mtl-model (list val))))
                               (2
                                (when tl-model
                                  (multiple-value-bind (h rest) (zk-treelist:pop-front tl)
                                    (assert (eql h (car tl-model)))
                                    (setf tl rest tl-model (cdr tl-model))))
                                (when mtl-model
                                  (let ((h (zk-treelist:pop-front! mtl)))
                                    (assert (eql h (car mtl-model)))
                                    (setf mtl-model (cdr mtl-model)))))
                               (3
                                (when tl-model
                                  (multiple-value-bind (tail-val rest) (zk-treelist:pop-back tl)
                                    (assert (eql tail-val (car (last tl-model))))
                                    (setf tl rest tl-model (butlast tl-model))))
                                (when mtl-model
                                  (let ((tail-val (zk-treelist:pop-back! mtl)))
                                    (assert (eql tail-val (car (last mtl-model))))
                                    (setf mtl-model (butlast mtl-model)))))
                               (4
                                (when tl-model
                                  (let ((idx (next-rand (length tl-model))))
                                    (setf tl (zk-treelist:set tl idx val))
                                    (setf (nth idx tl-model) val)))
                                (when mtl-model
                                  (let ((idx (next-rand (length mtl-model))))
                                    (setf (zk-treelist:ref mtl idx) val)
                                    (setf (nth idx mtl-model) val))))
                               (5
                                (let ((idx (next-rand (1+ (length tl-model)))))
                                  (setf tl (zk-treelist:insert tl idx val)
                                        tl-model (append (subseq tl-model 0 idx)
                                                         (cons val (nthcdr idx tl-model)))))
                                (let ((idx (next-rand (1+ (length mtl-model)))))
                                  (zk-treelist:insert! mtl idx val)
                                  (setf mtl-model (append (subseq mtl-model 0 idx)
                                                          (cons val (nthcdr idx mtl-model))))))
                               (6
                                (when tl-model
                                  (let ((idx (next-rand (length tl-model))))
                                    (setf tl (zk-treelist:delete tl idx)
                                          tl-model (append (subseq tl-model 0 idx)
                                                           (nthcdr (1+ idx) tl-model)))))
                                (when mtl-model
                                  (let ((idx (next-rand (length mtl-model))))
                                    (zk-treelist:delete! mtl idx)
                                    (setf mtl-model (append (subseq mtl-model 0 idx)
                                                            (nthcdr (1+ idx) mtl-model))))))
                               (7
                                (let ((idx (next-rand (1+ (length tl-model)))))
                                  (multiple-value-bind (l r) (zk-treelist:split tl idx)
                                    (assert (equal (zk-treelist:to-list l) (subseq tl-model 0 idx)))
                                    (assert (equal (zk-treelist:to-list r) (nthcdr idx tl-model)))
                                    (setf tl (zk-treelist:append l r)))))
                               (8
                                (let* ((len (length tl-model))
                                       (a (next-rand (1+ len)))
                                       (b (next-rand (1+ len)))
                                       (start (min a b))
                                       (end (max a b)))
                                  (assert (equal (zk-treelist:to-list (zk-treelist:subseq tl start end))
                                                 (subseq tl-model start end)))))
                               (9
                                (push (cons (zk-treelist:snapshot mtl) (copy-list mtl-model)) history)
                                (push (cons tl (copy-list tl-model)) history))
                               (10
                                (let ((c (zk-treelist:copy mtl)))
                                  (zk-treelist:push-back! c -999)
                                  (assert (equal (zk-treelist:to-list c)
                                                 (append mtl-model (list -999))))))
                               (11
                                (let ((idx (next-rand (1+ (length mtl-model)))))
                                  (let ((right (zk-treelist:split! mtl idx)))
                                    (assert (equal (zk-treelist:to-list mtl) (subseq mtl-model 0 idx)))
                                    (assert (equal (zk-treelist:to-list right) (nthcdr idx mtl-model)))
                                    (zk-treelist:append! mtl right)))))
                             (assert (equal (zk-treelist:to-list tl) tl-model))
                             (assert (equal (zk-treelist:to-list mtl) mtl-model))))
                         (dolist (entry history)
                           (assert (equal (zk-treelist:to-list (car entry)) (cdr entry))))
                         (assert (equal tl-model
                                        (zk-treelist:to-list (eval (make-load-form tl)))))
                         t)))"#,
            )
            .unwrap();
        assert!(values[0].is_t());
    }
}
