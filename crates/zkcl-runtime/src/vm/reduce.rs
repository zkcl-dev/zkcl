use std::ops::Range;

use crate::Runtime;
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};

use super::sequence::matching::{Key, call};
use super::sequence::{Elements, Indices, bounds};
use super::{Continuation, ContinuationStep, Resume, Values, designator, named_keyword_arguments};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Reduce,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[("REDUCE", Op::Reduce)];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Reduce => count >= 2,
        }
    }
}

enum Stage {
    Advance,
    Seed(RootSlot),
    Accumulate,
    AfterKey,
    Empty,
    AfterEmpty,
}

pub(super) struct Reduction {
    elements: Elements,
    indices: Indices,
    key: Key,
    function: RootSlot,
    accumulator: RootFrame,
    stage: Stage,
    resume: Resume,
    at: usize,
}

impl Reduction {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let Op::Reduce = op;
        let keywords = named_keyword_arguments(
            runtime,
            frame,
            args.start + 2..args.end,
            ["KEY", "FROM-END", "START", "END", "INITIAL-VALUE"],
            at,
        )?;
        let elements = Elements::new(runtime, frame.get(args.start + 1), at)?;
        let [key, from_end, start, end, initial] =
            keywords.map(|register| register.map(|register| frame.get(register)));
        let mut indices = Indices::new(bounds(runtime, elements.len(), start, end, at)?, from_end);
        let key = Key::resolve(runtime, key, at)?;
        let function = runtime.root_slot(designator(runtime, frame.get(args.start), at)?);
        let accumulator = runtime.root_frame(1);
        let stage = if let Some(value) = initial {
            accumulator.set(0, value);
            Stage::Advance
        } else if let Some(index) = indices.next() {
            let element = elements.get(runtime, index);
            if key.is_identity() {
                accumulator.set(0, element);
                Stage::Advance
            } else {
                Stage::Seed(runtime.root_slot(element))
            }
        } else {
            Stage::Empty
        };
        Ok(Self {
            elements,
            indices,
            key,
            function,
            accumulator,
            stage,
            resume,
            at,
        })
    }

    fn reduce(&mut self, runtime: &Runtime, element: Word) -> ContinuationStep {
        let accumulator = self.accumulator.get(0);
        let arguments = if self.indices.scans_from_end() {
            [element, accumulator]
        } else {
            [accumulator, element]
        };
        self.stage = Stage::Accumulate;
        call(runtime, self.function.get(), arguments, self.at)
    }
}

impl Continuation for Reduction {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        match std::mem::replace(&mut self.stage, Stage::Advance) {
            Stage::Advance => {}
            Stage::Seed(element) => {
                self.stage = Stage::Accumulate;
                return Ok(self
                    .key
                    .apply(runtime, element.get(), self.at)
                    .expect("seed needs a key"));
            }
            Stage::Accumulate => self.accumulator.set(0, values.get(0)),
            Stage::AfterKey => return Ok(self.reduce(runtime, values.get(0))),
            Stage::Empty => {
                self.stage = Stage::AfterEmpty;
                return Ok(call(runtime, self.function.get(), [], self.at));
            }
            Stage::AfterEmpty => return Ok(ContinuationStep::Done(values.get(0))),
        }

        let Some(index) = self.indices.next() else {
            return Ok(ContinuationStep::Done(self.accumulator.get(0)));
        };
        let element = self.elements.get(runtime, index);
        if let Some(step) = self.key.apply(runtime, element, self.at) {
            self.stage = Stage::AfterKey;
            return Ok(step);
        }
        Ok(self.reduce(runtime, element))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
