use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::sequence_types::ResultSequence;
use super::{Continuation, ContinuationStep, Resume, Values, arrays, designator, sequence};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Map,
    MapInto,
    Predicate(Predicate),
}

#[derive(Clone, Copy)]
pub(crate) enum Predicate {
    Every,
    Some,
    NotAny,
    NotEvery,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAP", Op::Map),
    ("MAP-INTO", Op::MapInto),
    ("EVERY", Op::Predicate(Predicate::Every)),
    ("SOME", Op::Predicate(Predicate::Some)),
    ("NOTANY", Op::Predicate(Predicate::NotAny)),
    ("NOTEVERY", Op::Predicate(Predicate::NotEvery)),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        count >= if matches!(self, Self::Map) { 3 } else { 2 }
    }
}

impl Predicate {
    fn decide(self, value: Word) -> Option<Word> {
        match (self, value == Word::NIL) {
            (Self::Every, true) | (Self::NotAny, false) => Some(Word::NIL),
            (Self::NotEvery, true) => Some(Word::T),
            (Self::Some, false) => Some(value),
            _ => None,
        }
    }

    fn exhausted(self) -> Word {
        match self {
            Self::Every | Self::NotAny => Word::T,
            Self::Some | Self::NotEvery => Word::NIL,
        }
    }
}

#[derive(Clone, Copy)]
enum SequenceInputKind {
    List,
    Vector,
    Treelist,
}

enum Destination {
    List(RootFrame),
    Vector { has_fill_pointer: bool },
    MutableTreelist,
}

enum Output {
    Discard,
    Create(ResultSequence, RootFrame),
    Into(RootSlot, Destination),
    Predicate(Predicate),
}

pub(super) struct SequenceMap {
    function: RootSlot,
    inputs: Vec<SequenceInputKind>,
    cursors: RootFrame,
    output: Output,
    index: usize,
    len: usize,
    resume: Resume,
    at: usize,
}

impl SequenceMap {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let function_arg = args.start + usize::from(!matches!(op, Op::Predicate(_)));
        let function = runtime.root_slot(designator(runtime, frame.get(function_arg), at)?);
        let sequence_args = function_arg + 1..args.end;
        let mut inputs = Vec::with_capacity(sequence_args.len());
        let cursors = runtime.root_frame(sequence_args.len());
        // MAP-INTO can take no inputs, in which case the destination alone sets the length.
        let mut len = usize::MAX;
        for (index, register) in sequence_args.enumerate() {
            let word = frame.get(register);
            cursors.set(index, word);
            let (kind, size) = if let Some(size) = arrays::vector_length(runtime, word) {
                (SequenceInputKind::Vector, size)
            } else if let Some(size) = runtime.heap.treelist_len(word) {
                (SequenceInputKind::Treelist, size)
            } else if matches!(op, Op::Map) {
                let size = sequence::list_length_or_cycle(runtime, word, at)?;
                (SequenceInputKind::List, size.unwrap_or(usize::MAX))
            } else {
                (
                    SequenceInputKind::List,
                    sequence::list_length(runtime, word, at)?,
                )
            };
            inputs.push(kind);
            len = len.min(size);
        }

        let output = match op {
            Op::Map => {
                // A circular input consumes elements only up to the shortest finite one.
                if len == usize::MAX {
                    return Err(sequence::improper_sequence(runtime, cursors.get(0), at));
                }
                let result_type = frame.get(args.start);
                if result_type == Word::NIL {
                    Output::Discard
                } else {
                    Output::Create(
                        ResultSequence::parse(runtime, result_type, at)?,
                        runtime.root_frame(len),
                    )
                }
            }
            Op::MapInto => {
                let word = frame.get(args.start);
                let (destination, capacity) = if let Some(info) =
                    runtime.heap.array_info(word).filter(|info| info.rank == 1)
                {
                    // Output uses all slots, including the ones past its fill pointer.
                    (
                        Destination::Vector {
                            has_fill_pointer: info.fill_pointer.is_some(),
                        },
                        info.size,
                    )
                } else if let Some(capacity) = runtime.heap.treelist_len(word) {
                    super::treelist::expect_mutable_treelist(runtime, word, at)?;
                    (Destination::MutableTreelist, capacity)
                } else {
                    let capacity = sequence::list_length(runtime, word, at)?;
                    let cursor = runtime.root_frame(1);
                    cursor.set(0, word);
                    (Destination::List(cursor), capacity)
                };
                len = len.min(capacity);
                Output::Into(runtime.root_slot(word), destination)
            }
            Op::Predicate(predicate) => Output::Predicate(predicate),
        };
        Ok(Self {
            function,
            inputs,
            cursors,
            output,
            index: 0,
            len,
            resume,
            at,
        })
    }

    fn result(&self, runtime: &Runtime) -> Result<Word, Fault> {
        match &self.output {
            Output::Discard => Ok(Word::NIL),
            Output::Create(result, elements) => {
                result.build(runtime, elements, 0..self.len, self.at)
            }
            Output::Into(original, destination) => {
                let word = original.get();
                if matches!(
                    destination,
                    Destination::Vector {
                        has_fill_pointer: true
                    }
                ) {
                    runtime
                        .heap
                        .set_array_fill_pointer(word, self.len)
                        .ok_or_else(|| {
                            EvalError::new(self.at, "invalid destination fill pointer")
                        })?;
                }
                Ok(word)
            }
            Output::Predicate(predicate) => Ok(predicate.exhausted()),
        }
    }
}

fn list_cell(runtime: &Runtime, word: Word, at: usize) -> Result<(Word, Word), Fault> {
    runtime.heap.cons_fields(word).ok_or_else(|| {
        Fault::type_error(runtime, 
            word,
            "CONS",
            EvalError::new(at, "list traversal did not reach a cons"),
        )
    })
}

impl Continuation for SequenceMap {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if returned {
            let value = values.get(0);
            match &self.output {
                Output::Discard => {}
                Output::Create(_, elements) => elements.set(self.index, value),
                Output::Into(original, destination) => match destination {
                    Destination::List(cursor) => {
                        let cell = cursor.get(0);
                        let (_, next) = list_cell(runtime, cell, self.at)?;
                        runtime.heap.set_cons_car(cell, value);
                        cursor.set(0, next);
                    }
                    Destination::Vector { .. } => {
                        arrays::store(runtime, original.get(), self.index, value, self.at)?;
                    }
                    Destination::MutableTreelist => {
                        let value = runtime.root_slot(value);
                        runtime
                            .heap
                            .mutable_treelist_set(original, self.index, &value)
                            .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                    }
                },
                Output::Predicate(predicate) => {
                    if let Some(result) = predicate.decide(value) {
                        return Ok(ContinuationStep::Done(result));
                    }
                }
            }
            self.index += 1;
        }

        if self.index == self.len {
            return self.result(runtime).map(ContinuationStep::Done);
        }

        // Read each row just before its call. MAP-INTO may share storage with
        // the inputs, so this row can observe the preceding write.
        let args = runtime.root_frame(self.inputs.len());
        for (input, kind) in self.inputs.iter().enumerate() {
            let cursor = self.cursors.get(input);
            let value = match kind {
                SequenceInputKind::List => {
                    let (value, next) = list_cell(runtime, cursor, self.at)?;
                    self.cursors.set(input, next);
                    value
                }
                SequenceInputKind::Vector => arrays::slot(runtime, cursor, self.index, self.at)?,
                SequenceInputKind::Treelist => runtime
                    .heap
                    .treelist_get(cursor, self.index)
                    .expect("validated treelist index"),
            };
            args.set(input, value);
        }
        Ok(ContinuationStep::Call {
            target: self.function.get(),
            args,
            at: self.at,
        })
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
