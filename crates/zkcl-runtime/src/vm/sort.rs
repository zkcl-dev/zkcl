use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::sequence::matching::{Key, call};
use super::sequence::{Kind, Sequence};
use super::sequence_types::ResultSequence;
use super::{
    Continuation, ContinuationStep, Resume, Values, arrays, designator, named_keyword_arguments,
};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Sort,
    Merge,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("SORT", Op::Sort),
    ("STABLE-SORT", Op::Sort),
    ("MERGE", Op::Merge),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Sort => count >= 2,
            Self::Merge => count >= 4,
        }
    }
}

enum SortDestination {
    Sort {
        sequence: RootSlot,
        cells: Option<Vec<RootSlot>>,
    },
    Merge {
        result_type: ResultSequence,
        first_len: usize,
    },
}

struct MergeRuns {
    run_width: usize,
    run_start: usize,
    right_start: usize,
    run_end: usize,
    left_cursor: usize,
    right_cursor: usize,
    merged_indices: Vec<usize>,
}

enum SortStep {
    Start,
    Key { index: usize, state: MergeRuns },
    Compare(MergeRuns),
}

pub(super) struct SortOperation {
    target: SortDestination,
    elements: RootFrame,
    keys: Option<RootFrame>,
    order: Vec<usize>,
    total: usize,
    predicate: RootSlot,
    key: Key,
    stage: SortStep,
    resume: Resume,
    at: usize,
}

impl SortOperation {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let (target, elements, total, predicate_function, key_function) = match op {
            Op::Sort => {
                let [key] =
                    named_keyword_arguments(runtime, frame, args.start + 2..args.end, ["KEY"], at)?;
                let seq = Sequence::new(runtime, frame, args.start, at)?;
                let len = seq.len;
                let elements = seq.snapshot(runtime, 0..len, at)?;
                let cells = if matches!(seq.kind, Kind::List) {
                    let mut cells = Vec::with_capacity(len);
                    let mut cursor = frame.get(args.start);
                    for _ in 0..len {
                        cells.push(runtime.root_slot(cursor));
                        cursor = runtime
                            .heap
                            .cons_fields(cursor)
                            .expect("validated list cell")
                            .1;
                    }
                    Some(cells)
                } else {
                    None
                };
                let predicate = designator(runtime, frame.get(args.start + 1), at)?;
                let key = Key::resolve(runtime, key.map(|register| frame.get(register)), at)?;
                (
                    SortDestination::Sort {
                        sequence: runtime.root_slot(frame.get(args.start)),
                        cells,
                    },
                    elements,
                    len,
                    predicate,
                    key,
                )
            }
            Op::Merge => {
                let result_type = ResultSequence::parse(runtime, frame.get(args.start), at)?;
                let [key] =
                    named_keyword_arguments(runtime, frame, args.start + 4..args.end, ["KEY"], at)?;
                let first_sequence = Sequence::new(runtime, frame, args.start + 1, at)?;
                let second_sequence = Sequence::new(runtime, frame, args.start + 2, at)?;
                let first_len = first_sequence.len;
                let second_len = second_sequence.len;
                let total = first_len
                    .checked_add(second_len)
                    .ok_or_else(|| Fault::from(EvalError::new(at, "merged sequence too long")))?;
                result_type.check_length(runtime, total, at)?;
                let elements = runtime.root_frame(total);
                first_sequence.copy_into(runtime, 0..first_len, &elements, 0, at)?;
                second_sequence.copy_into(runtime, 0..second_len, &elements, first_len, at)?;
                let predicate = designator(runtime, frame.get(args.start + 3), at)?;
                let key = Key::resolve(runtime, key.map(|register| frame.get(register)), at)?;
                (
                    SortDestination::Merge {
                        result_type,
                        first_len,
                    },
                    elements,
                    total,
                    predicate,
                    key,
                )
            }
        };

        Ok(Self {
            target,
            elements,
            keys: None,
            order: (0..total).collect(),
            total,
            predicate: runtime.root_slot(predicate_function),
            key: key_function,
            stage: SortStep::Start,
            resume,
            at,
        })
    }

    fn initial_merge_state(&self) -> Option<MergeRuns> {
        match &self.target {
            SortDestination::Sort { .. } => (self.total > 1).then(|| MergeRuns {
                run_width: 1,
                run_start: 0,
                right_start: 1,
                run_end: 2.min(self.total),
                left_cursor: 0,
                right_cursor: 1,
                merged_indices: Vec::with_capacity(2.min(self.total)),
            }),
            SortDestination::Merge { first_len, .. } => (*first_len > 0 && *first_len < self.total)
                .then(|| MergeRuns {
                    run_width: self.total,
                    run_start: 0,
                    right_start: *first_len,
                    run_end: self.total,
                    left_cursor: 0,
                    right_cursor: *first_len,
                    merged_indices: Vec::with_capacity(self.total),
                }),
        }
    }

    fn finish(&self, runtime: &Runtime) -> Result<Word, Fault> {
        match &self.target {
            SortDestination::Sort { sequence, cells } => {
                if let Some(cells) = cells {
                    for (pos, cell) in cells.iter().enumerate() {
                        runtime
                            .heap
                            .set_cons_car(cell.get(), self.elements.get(self.order[pos]));
                    }
                    return Ok(sequence.get());
                }
                if let Some(kind) = runtime.heap.treelist_kind(sequence.get()) {
                    return match kind {
                        crate::heap::TreelistKind::Mutable => {
                            let slots = runtime.root_frame(self.total);
                            for (pos, &index) in self.order.iter().enumerate() {
                                slots.set(pos, self.elements.get(index));
                            }
                            runtime
                                .heap
                                .mutable_treelist_reset_from_slots(sequence, &slots, self.total)
                                .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                            Ok(sequence.get())
                        }
                        crate::heap::TreelistKind::Persistent => {
                            let slots = runtime.root_frame(self.total);
                            for (pos, &index) in self.order.iter().enumerate() {
                                slots.set(pos, self.elements.get(index));
                            }
                            runtime
                                .heap
                                .treelist_from_slots(&slots, self.total)
                                .map_err(|error| EvalError::out_of_memory(self.at, error).into())
                        }
                    };
                }
                for (pos, &index) in self.order.iter().enumerate() {
                    arrays::store(
                        runtime,
                        sequence.get(),
                        pos,
                        self.elements.get(index),
                        self.at,
                    )?;
                }
                Ok(sequence.get())
            }
            SortDestination::Merge { result_type, .. } => {
                let merged = runtime.root_frame(self.total);
                for (pos, &idx) in self.order.iter().enumerate() {
                    merged.set(pos, self.elements.get(idx));
                }
                result_type.build(runtime, &merged, 0..self.total, self.at)
            }
        }
    }
}

impl Continuation for SortOperation {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        let mut merge_state = match std::mem::replace(&mut self.stage, SortStep::Start) {
            SortStep::Start => {
                let Some(state) = self.initial_merge_state() else {
                    return self.finish(runtime).map(ContinuationStep::Done);
                };
                if let Some(step) = self.key.apply(runtime, self.elements.get(0), self.at) {
                    self.keys = Some(runtime.root_frame(self.total));
                    self.stage = SortStep::Key { index: 0, state };
                    return Ok(step);
                }
                state
            }
            SortStep::Key { index, state } => {
                let keys = self.keys.as_ref().expect("keying stores the keys");
                keys.set(index, values.get(0));
                let next = index + 1;
                if next < self.total {
                    self.stage = SortStep::Key { index: next, state };
                    return Ok(self
                        .key
                        .apply(runtime, self.elements.get(next), self.at)
                        .expect("non-identity key"));
                }
                state
            }
            SortStep::Compare(mut state) => {
                if values.get(0) != Word::NIL {
                    state.merged_indices.push(self.order[state.right_cursor]);
                    state.right_cursor += 1;
                } else {
                    state.merged_indices.push(self.order[state.left_cursor]);
                    state.left_cursor += 1;
                }
                state
            }
        };

        loop {
            if merge_state.left_cursor < merge_state.right_start
                && merge_state.right_cursor < merge_state.run_end
            {
                let keys = self.keys.as_ref().unwrap_or(&self.elements);
                let right_key = keys.get(self.order[merge_state.right_cursor]);
                let left_key = keys.get(self.order[merge_state.left_cursor]);
                self.stage = SortStep::Compare(merge_state);
                return Ok(call(
                    runtime,
                    self.predicate.get(),
                    [right_key, left_key],
                    self.at,
                ));
            }
            merge_state
                .merged_indices
                .extend_from_slice(&self.order[merge_state.left_cursor..merge_state.right_start]);
            merge_state
                .merged_indices
                .extend_from_slice(&self.order[merge_state.right_cursor..merge_state.run_end]);
            self.order[merge_state.run_start..merge_state.run_end]
                .copy_from_slice(&merge_state.merged_indices);
            merge_state.merged_indices.clear();

            let next_run = merge_state.run_end;
            if next_run + merge_state.run_width < self.total {
                merge_state.run_start = next_run;
                merge_state.right_start = next_run + merge_state.run_width;
                merge_state.run_end = (next_run + 2 * merge_state.run_width).min(self.total);
                merge_state.left_cursor = merge_state.run_start;
                merge_state.right_cursor = merge_state.right_start;
            } else {
                let next_run_width = merge_state.run_width * 2;
                if next_run_width >= self.total {
                    return self.finish(runtime).map(ContinuationStep::Done);
                }
                merge_state.run_width = next_run_width;
                merge_state.run_start = 0;
                merge_state.right_start = next_run_width;
                merge_state.run_end = (2 * next_run_width).min(self.total);
                merge_state.left_cursor = 0;
                merge_state.right_cursor = merge_state.right_start;
            }
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
