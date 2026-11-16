use std::ops::Range;

use num_bigint::Sign;
use num_traits::ToPrimitive;

use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, ListSpine, RootFrame, RootSlots, TreelistKind, Word};
use crate::{EvalError, Runtime};

use super::sequence_types::ResultSequence;
use super::{arrays, named_keyword_arguments};

pub(super) mod matching;
mod selection;
pub(super) use selection::{Elements, Selection};

pub(super) struct Indices {
    range: Range<usize>,
    from_end: bool,
}

impl Indices {
    pub(super) fn new(range: Range<usize>, from_end: Option<Word>) -> Self {
        Self {
            range,
            from_end: from_end.is_some_and(|word| word != Word::NIL),
        }
    }

    pub(super) fn scans_from_end(&self) -> bool {
        self.from_end
    }
}

impl Iterator for Indices {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.from_end {
            self.range.next_back()
        } else {
            self.range.next()
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Copy,
    Subseq,
    SetSubseq,
    Fill,
    Replace,
    Reverse,
    /// The destructive form may return a fresh sequence; the standard permits
    /// NREVERSE to modify its argument, not to require modification.
    NReverse,
    Concatenate,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("COPY-SEQ", Op::Copy),
    ("SUBSEQ", Op::Subseq),
    ("(SETF SUBSEQ)", Op::SetSubseq),
    ("FILL", Op::Fill),
    ("REPLACE", Op::Replace),
    ("REVERSE", Op::Reverse),
    ("NREVERSE", Op::NReverse),
    ("CONCATENATE", Op::Concatenate),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Copy | Self::Reverse | Self::NReverse => count == 1,
            Self::Subseq => (2..=3).contains(&count),
            Self::SetSubseq => (3..=4).contains(&count),
            Self::Fill | Self::Replace => count >= 2,
            Self::Concatenate => count >= 1,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::Copy | Op::Subseq => {
            let sequence = Sequence::new(runtime, frame, args.start, at)?;
            let [start, end] = positional_bounds(frame, args.start + 1..args.end);
            let range = bounds(runtime, sequence.len, start, end, at)?;
            sequence.copy(runtime, range, at)
        }
        Op::SetSubseq => {
            let target = Sequence::new(runtime, frame, args.start + 1, at)?;
            let source = Sequence::new(runtime, frame, args.start, at)?;
            let [start, end] = positional_bounds(frame, args.start + 2..args.end);
            let range = bounds(runtime, target.len, start, end, at)?;
            replace(runtime, &target, range, &source, 0..source.len, at)?;
            Ok(source.word())
        }
        Op::Fill => {
            let [start, end] = named_keyword_arguments(
                runtime,
                frame,
                args.start + 2..args.end,
                ["START", "END"],
                at,
            )?
            .map(|reg| reg.map(|reg| frame.get(reg)));
            let sequence = Sequence::new(runtime, frame, args.start, at)?;
            let item = frame.get(args.start + 1);
            let range = bounds(runtime, sequence.len, start, end, at)?;
            match sequence.kind {
                Kind::List => {
                    let mut tail = sequence.list_tail(runtime, range.start);
                    for _ in range {
                        runtime.heap.set_cons_car(tail, item);
                        tail = runtime
                            .heap
                            .cons_fields(tail)
                            .expect("validated fill cons")
                            .1;
                    }
                }
                Kind::Vector(_) => {
                    for index in range {
                        arrays::store(runtime, sequence.word(), index, item, at)?;
                    }
                }
                Kind::Treelist(TreelistKind::Persistent) => {
                    super::treelist::expect_mutable_treelist(runtime, sequence.word(), at)?;
                }
                Kind::Treelist(TreelistKind::Mutable) => {
                    let seq = runtime.root_slot(sequence.word());
                    let item = runtime.root_slot(item);
                    for index in range {
                        runtime
                            .heap
                            .mutable_treelist_set(&seq, index, &item)
                            .map_err(|error| EvalError::out_of_memory(at, error))?;
                    }
                }
            }
            Ok(sequence.word())
        }
        Op::Replace => {
            let [start1, end1, start2, end2] = named_keyword_arguments(
                runtime,
                frame,
                args.start + 2..args.end,
                ["START1", "END1", "START2", "END2"],
                at,
            )?
            .map(|reg| reg.map(|reg| frame.get(reg)));
            let target = Sequence::new(runtime, frame, args.start, at)?;
            let source = Sequence::new(runtime, frame, args.start + 1, at)?;
            let target_range = bounds(runtime, target.len, start1, end1, at)?;
            let source_range = bounds(runtime, source.len, start2, end2, at)?;
            replace(runtime, &target, target_range, &source, source_range, at)?;
            Ok(target.word())
        }
        Op::Reverse => reverse(runtime, frame, args.start, at),
        Op::NReverse => nreverse(runtime, frame, args.start, at),
        Op::Concatenate => concatenate(runtime, frame, args, at),
    }
}

fn reverse(runtime: &Runtime, frame: &RootSlots, reg: usize, at: usize) -> Result<Word, Fault> {
    let sequence = Sequence::new(runtime, frame, reg, at)?;
    let len = sequence.len;
    let elements = sequence.snapshot(runtime, 0..len, at)?;
    let reversed = runtime.root_frame(len);
    for index in 0..len {
        reversed.set(index, elements.get(len - 1 - index));
    }
    assemble(runtime, sequence.kind, &reversed, at)
}

fn nreverse(runtime: &Runtime, frame: &RootSlots, reg: usize, at: usize) -> Result<Word, Fault> {
    let sequence = Sequence::new(runtime, frame, reg, at)?;
    if matches!(
        sequence.kind,
        Kind::List | Kind::Treelist(TreelistKind::Persistent)
    ) {
        return reverse(runtime, frame, reg, at);
    }
    if matches!(sequence.kind, Kind::Treelist(TreelistKind::Mutable)) {
        let seq = runtime.root_slot(sequence.word());
        runtime
            .heap
            .mutable_treelist_reverse_in_place(&seq)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        return Ok(seq.get());
    }
    // NREVERSE may modify its argument; reversing the vector in place keeps its
    // type, fill pointer and adjustability intact.
    let elements = sequence.snapshot(runtime, 0..sequence.len, at)?;
    for index in 0..sequence.len {
        let value = elements.get(sequence.len - 1 - index);
        arrays::store(runtime, sequence.word(), index, value, at)?;
    }
    Ok(sequence.word())
}

fn concatenate(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let result = ResultSequence::parse(runtime, frame.get(args.start), at)?;
    let mut sequences = Vec::with_capacity(args.len() - 1);
    let mut total = 0usize;
    for reg in args.start + 1..args.end {
        let sequence = Sequence::new(runtime, frame, reg, at)?;
        total = total
            .checked_add(sequence.len)
            .ok_or_else(|| bounds_error(at))?;
        sequences.push(sequence);
    }
    result.check_length(runtime, total, at)?;
    let elements = runtime.root_frame(total);
    let mut offset = 0;
    for sequence in sequences {
        sequence.copy_into(runtime, 0..sequence.len, &elements, offset, at)?;
        offset += sequence.len;
    }
    result.build(runtime, &elements, 0..total, at)
}

fn assemble(runtime: &Runtime, kind: Kind, elements: &RootSlots, at: usize) -> Result<Word, Fault> {
    let len = elements.len();
    match kind {
        Kind::List => {
            let result = runtime.root_frame(1);
            for index in (0..len).rev() {
                let pair = runtime
                    .heap
                    .cons(|| (elements.get(index), result.get(0)))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                result.set(0, pair);
            }
            Ok(result.get(0))
        }
        Kind::Vector(ArrayElement::Character) => {
            let mut text = String::with_capacity(len);
            for index in 0..len {
                let value = elements.get(index);
                text.push(
                    value
                        .as_character()
                        .ok_or_else(|| arrays::type_error(runtime, value, "CHARACTER", at))?,
                );
            }
            runtime
                .heap
                .string(&text)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        Kind::Vector(element) => runtime
            .heap
            .array(&[len], element, false, None, |index| elements.get(index))
            .map_err(|error| EvalError::out_of_memory(at, error).into()),
        Kind::Treelist(TreelistKind::Persistent) => {
            let slots = runtime.root_frame(len);
            for index in 0..len {
                slots.set(index, elements.get(index));
            }
            runtime
                .heap
                .treelist_from_slots(&slots, len)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        Kind::Treelist(TreelistKind::Mutable) => {
            let slots = runtime.root_frame(len);
            for index in 0..len {
                slots.set(index, elements.get(index));
            }
            runtime
                .heap
                .mutable_treelist_from_slots(&slots, len)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    List,
    Vector(ArrayElement),
    Treelist(TreelistKind),
}

pub(super) struct Sequence<'args> {
    frame: &'args RootSlots,
    reg: usize,
    pub(super) kind: Kind,
    pub(super) len: usize,
}

impl<'args> Sequence<'args> {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &'args RootSlots,
        reg: usize,
        at: usize,
    ) -> Result<Self, Fault> {
        let word = frame.get(reg);
        let (kind, len) =
            if let Some(info) = runtime.heap.array_info(word).filter(|info| info.rank == 1) {
                (
                    Kind::Vector(info.element),
                    info.fill_pointer.unwrap_or(info.size),
                )
            } else if let Some(kind) = runtime.heap.treelist_kind(word) {
                (
                    Kind::Treelist(kind),
                    runtime.heap.treelist_len(word).expect("treelist length"),
                )
            } else {
                (Kind::List, list_length(runtime, word, at)?)
            };
        Ok(Self {
            frame,
            reg,
            kind,
            len,
        })
    }

    fn word(&self) -> Word {
        self.frame.get(self.reg)
    }

    pub(super) fn list_tail(&self, runtime: &Runtime, start: usize) -> Word {
        let mut tail = self.word();
        for _ in 0..start {
            tail = runtime
                .heap
                .cons_fields(tail)
                .expect("validated list index")
                .1;
        }
        tail
    }

    pub(super) fn snapshot(
        &self,
        runtime: &Runtime,
        range: Range<usize>,
        at: usize,
    ) -> Result<RootFrame, Fault> {
        let elements = runtime.root_frame(range.len());
        self.copy_into(runtime, range, &elements, 0, at)?;
        Ok(elements)
    }

    pub(super) fn copy_into(
        &self,
        runtime: &Runtime,
        range: Range<usize>,
        elements: &RootSlots,
        offset: usize,
        at: usize,
    ) -> Result<(), Fault> {
        match self.kind {
            Kind::List => {
                let mut tail = self.list_tail(runtime, range.start);
                for index in 0..range.len() {
                    let (car, cdr) = runtime
                        .heap
                        .cons_fields(tail)
                        .expect("validated list index");
                    elements.set(offset + index, car);
                    tail = cdr;
                }
            }
            Kind::Vector(ArrayElement::Character) => {
                runtime
                    .heap
                    .with_array_character_slice(self.word(), range.start, range.len(), |text| {
                        for (index, character) in text.chars().enumerate() {
                            elements.set(offset + index, Word::character(character));
                        }
                    })
                    .ok_or_else(|| bounds_error(at))?;
            }
            Kind::Vector(_) => {
                for (slot, index) in range.enumerate() {
                    let value = runtime
                        .heap
                        .array_slot(self.word(), index)
                        .map_err(|error| EvalError::out_of_memory(at, error))?
                        .ok_or_else(|| bounds_error(at))?;
                    elements.set(offset + slot, value);
                }
            }
            Kind::Treelist(_) => {
                runtime.heap.treelist_copy_into_frame(
                    self.word(),
                    range.start,
                    range.end,
                    elements,
                    offset,
                );
            }
        }
        Ok(())
    }

    fn copy(&self, runtime: &Runtime, range: Range<usize>, at: usize) -> Result<Word, Fault> {
        if let Kind::Treelist(kind) = self.kind {
            let seq = runtime.root_slot(self.word());
            return match kind {
                TreelistKind::Persistent => runtime
                    .heap
                    .treelist_subseq(&seq, range.start, range.end)
                    .map_err(|error| EvalError::out_of_memory(at, error).into()),
                TreelistKind::Mutable => runtime
                    .heap
                    .mutable_treelist_subseq(&seq, range.start, range.end)
                    .map_err(|error| EvalError::out_of_memory(at, error).into()),
            };
        }
        let elements = self.snapshot(runtime, range, at)?;
        assemble(runtime, self.kind, &elements, at)
    }

    pub(super) fn character_text(
        &self,
        runtime: &Runtime,
        range: Range<usize>,
        at: usize,
    ) -> Result<String, Fault> {
        if let Kind::Vector(ArrayElement::Character) = self.kind {
            return runtime
                .heap
                .with_array_character_slice(self.word(), range.start, range.len(), str::to_owned)
                .ok_or_else(|| bounds_error(at));
        }
        let len = range.len();
        let elements = self.snapshot(runtime, range, at)?;
        let mut text = String::with_capacity(len);
        for index in 0..len {
            let value = elements.get(index);
            let character = value.as_character().ok_or_else(|| {
                Fault::type_error(runtime, 
                    value,
                    "CHARACTER",
                    EvalError::new(at, "expected a character"),
                )
            })?;
            text.push(character);
        }
        Ok(text)
    }
}

pub(super) fn list_length(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    list_length_or_cycle(runtime, word, at)?.ok_or_else(|| improper_sequence(runtime, word, at))
}

pub(super) fn list_length_or_cycle(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<Option<usize>, Fault> {
    let mut len = 0;
    let mut spine = ListSpine::new(word);
    loop {
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => len += 1,
            Ok(None) => break,
            Err(_) => return Ok(None),
        }
    }
    let tail = spine.tail();
    if tail != Word::NIL {
        return Err(if tail == word {
            improper_sequence(runtime, word, at)
        } else {
            improper_list_tail(runtime, tail, at)
        });
    }
    Ok(Some(len))
}

pub(super) fn improper_sequence(runtime: &Runtime, word: Word, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        word,
        "SEQUENCE",
        EvalError::new(at, "expected a proper sequence"),
    )
}

pub(super) fn improper_list_tail(runtime: &Runtime, tail: Word, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        tail,
        "LIST",
        EvalError::new(at, "expected a proper sequence"),
    )
}

fn replace(
    runtime: &Runtime,
    target: &Sequence<'_>,
    target_range: Range<usize>,
    source: &Sequence<'_>,
    source_range: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    let count = target_range.len().min(source_range.len());
    if count == 0 {
        return Ok(());
    }
    let source_range = source_range.start..source_range.start + count;
    if let Kind::Vector(ArrayElement::Character) = target.kind {
        let text = source.character_text(runtime, source_range, at)?;
        let stored = runtime
            .heap
            .set_array_string_range(target.word(), target_range.start, &text)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        return if stored {
            Ok(())
        } else {
            Err(bounds_error(at))
        };
    }
    // The source and target may overlap. Read the source before any write,
    // so writes always see the source's original elements.
    let elements = source.snapshot(runtime, source_range, at)?;
    match target.kind {
        Kind::List => {
            let mut tail = target.list_tail(runtime, target_range.start);
            for index in 0..count {
                let (_, cdr) = runtime
                    .heap
                    .cons_fields(tail)
                    .expect("validated list index");
                runtime.heap.set_cons_car(tail, elements.get(index));
                tail = cdr;
            }
        }
        Kind::Vector(_) => {
            for index in 0..count {
                arrays::store(
                    runtime,
                    target.word(),
                    target_range.start + index,
                    elements.get(index),
                    at,
                )?;
            }
        }
        Kind::Treelist(TreelistKind::Persistent) => {
            super::treelist::expect_mutable_treelist(runtime, target.word(), at)?;
        }
        Kind::Treelist(TreelistKind::Mutable) => {
            let target_slot = runtime.root_slot(target.word());
            for index in 0..count {
                let value = runtime.root_slot(elements.get(index));
                runtime
                    .heap
                    .mutable_treelist_set(&target_slot, target_range.start + index, &value)
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
            }
        }
    }
    Ok(())
}

fn positional_bounds(frame: &RootSlots, mut registers: Range<usize>) -> [Option<Word>; 2] {
    std::array::from_fn(|_| registers.next().map(|reg| frame.get(reg)))
}

pub(super) fn bounds(
    runtime: &Runtime,
    len: usize,
    start: Option<Word>,
    end: Option<Word>,
    at: usize,
) -> Result<Range<usize>, Fault> {
    let start = start
        .map(|word| index(runtime, word, at))
        .transpose()?
        .unwrap_or(0);
    let end = end
        .filter(|&word| word != Word::NIL)
        .map(|word| index(runtime, word, at))
        .transpose()?
        .unwrap_or(len);
    if start > end || end > len {
        return Err(bounds_error(at));
    }
    Ok(start..end)
}

fn index(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    let invalid =
        || Fault::non_negative_integer(runtime, word, EvalError::new(at, "expected a non-negative index"));
    if let Some(number) = word.as_fixnum() {
        return usize::try_from(number).map_err(|_| invalid());
    }
    let integer = runtime
        .heap
        .integer(word)
        .filter(|value| value.sign() != Sign::Minus)
        .ok_or_else(invalid)?;
    integer.to_usize().ok_or_else(|| bounds_error(at))
}

fn bounds_error(at: usize) -> Fault {
    Fault::plain(
        Class::Error,
        EvalError::new(at, "bounding indices are outside the sequence"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayValue, Heap};
    use crate::package::Package;
    use crate::vm::{Continuation, ContinuationStep, Primitive, Resume, Values, reduce, sets};

    fn runtime_with_gc_on_allocation() -> Runtime {
        let mut config = Heap::builder();
        config.stress_bytes = Some(1);
        config.always_defrag = true;
        Runtime::with_heap(Heap::start(config).unwrap())
    }

    #[test]
    fn reduce_calls_its_reducer_with_rooted_arguments_during_collection() {
        let runtime = runtime_with_gc_on_allocation();
        let arguments = runtime.root_frame(4);
        arguments.set(0, Primitive::named("CONS").expect("CONS primitive"));
        let floats = [1.25, -2.5, 3.75];
        arguments.set(
            1,
            runtime
                .heap
                .array_from_values(&[3], ArrayElement::DoubleFloat, false, None, |index| {
                    ArrayValue::DoubleFloat(floats[index])
                })
                .unwrap(),
        );
        let (keyword, _) = runtime
            .intern_package(Package::KEYWORD, "INITIAL-VALUE", 0)
            .unwrap();
        arguments.set(2, keyword);
        arguments.set(3, runtime.heap.string("initial accumulator").unwrap());

        let mut reduction = reduce::Reduction::new(
            &runtime,
            &arguments,
            reduce::Op::Reduce,
            0..4,
            Resume::Continuation,
            0,
        )
        .expect("REDUCE construction failed");
        let mut values = Values::new(&runtime);
        for (index, expected) in floats.into_iter().enumerate() {
            let ContinuationStep::Call { args, .. } = reduction
                .step(&runtime, &values, index != 0)
                .expect("REDUCE step failed")
            else {
                panic!("REDUCE should call the reducer for each float");
            };
            assert_eq!(args.get(0), arguments.get(3));
            assert_eq!(runtime.heap.double_float(args.get(1)), Some(expected));
            values.one(args.get(0));
        }
    }

    #[test]
    fn destructive_string_edits_return_the_relocated_input() {
        let runtime = runtime_with_gc_on_allocation();
        let arguments = runtime.root_frame(2);

        arguments.set(0, runtime.heap.string("xy").unwrap());
        arguments.set(1, Word::character('雪'));
        let filled = run(&runtime, &arguments, Op::Fill, 0..2, 0)
            .expect("FILL failed under collection stress");
        assert_eq!(filled, arguments.get(0));
        assert_eq!(
            runtime.heap.string_contents(filled).as_deref(),
            Some("雪雪")
        );

        let elements =
            Elements::new(&runtime, arguments.get(0), 0).expect("sequence selection failed");
        let mut selection = Selection::new(elements);
        selection.replace(0);
        selection.replace(1);
        let substituted = selection
            .result(&runtime, true, Some(Word::character('z')), 0)
            .expect("destructive string substitution failed");
        assert_eq!(substituted, arguments.get(0));
        assert_eq!(
            runtime.heap.string_contents(substituted).as_deref(),
            Some("zz")
        );
    }

    #[test]
    fn newly_built_lists_contain_the_relocated_element() {
        let runtime = runtime_with_gc_on_allocation();
        let arguments = runtime.root_frame(2);
        let item = runtime.root(runtime.heap.string("kept object").unwrap());
        let tail = runtime.cons(&item, &runtime.nil()).unwrap();
        let list = runtime.cons(&runtime.nil(), &tail).unwrap();
        let elements = Elements::new(&runtime, list.slot.get(), 0).expect("list selection failed");
        let mut selection = Selection::new(elements);
        selection.discard(0);
        let filtered = selection
            .result(&runtime, false, None, 0)
            .expect("non-destructive list result failed");
        assert_eq!(
            runtime.heap.cons_fields(filtered),
            Some((item.slot.get(), Word::NIL))
        );

        arguments.set(0, Word::NIL);
        arguments.set(1, tail.slot.get());
        let mut union = sets::SetOperation::new(
            &runtime,
            &arguments,
            sets::Op::Union,
            0..2,
            Resume::Continuation,
            0,
        )
        .expect("UNION construction failed");
        let ContinuationStep::Done(unioned) = union
            .step(&runtime, &Values::new(&runtime), false)
            .expect("UNION step failed")
        else {
            panic!("default UNION should not need a Lisp callback");
        };
        assert_eq!(
            runtime.heap.cons_fields(unioned),
            Some((item.slot.get(), Word::NIL))
        );
    }
}
