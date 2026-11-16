use std::ops::Range;

use crate::Runtime;
use crate::condition::Fault;
use crate::heap::{RootSlot, RootSlots};

use super::sequence::Selection;
use super::sequence::matching::{
    ElementMatches, MatchArguments, MatchCriterion, MatchOperation, MatchStep,
};
use super::{Continuation, ContinuationStep, Resume, Values};

#[derive(Clone, Copy)]
enum Edit {
    Remove,
    Delete,
    Substitute,
    NSubstitute,
}

#[derive(Clone, Copy)]
pub(crate) struct Op(Edit, MatchCriterion);

pub(super) const PRIMITIVES: &[(&str, Op)] = {
    use Edit::*;
    use MatchCriterion::{Item, Predicate};
    &[
        ("REMOVE", Op(Remove, Item)),
        ("REMOVE-IF", Op(Remove, Predicate { negate: false })),
        ("REMOVE-IF-NOT", Op(Remove, Predicate { negate: true })),
        ("DELETE", Op(Delete, Item)),
        ("DELETE-IF", Op(Delete, Predicate { negate: false })),
        ("DELETE-IF-NOT", Op(Delete, Predicate { negate: true })),
        ("SUBSTITUTE", Op(Substitute, Item)),
        ("SUBSTITUTE-IF", Op(Substitute, Predicate { negate: false })),
        (
            "SUBSTITUTE-IF-NOT",
            Op(Substitute, Predicate { negate: true }),
        ),
        ("NSUBSTITUTE", Op(NSubstitute, Item)),
        (
            "NSUBSTITUTE-IF",
            Op(NSubstitute, Predicate { negate: false }),
        ),
        (
            "NSUBSTITUTE-IF-NOT",
            Op(NSubstitute, Predicate { negate: true }),
        ),
    ]
};

impl Edit {
    fn substitutes(self) -> bool {
        matches!(self, Self::Substitute | Self::NSubstitute)
    }
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        count >= 2 + usize::from(self.0.substitutes())
    }
}

pub(super) struct PredicateRemove {
    selection: Selection,
    matches: ElementMatches,
    destructive: bool,
    replacement: Option<RootSlot>,
    resume: Resume,
    at: usize,
}

impl PredicateRemove {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let Op(edit, criterion) = op;
        let start = args.start;
        let matcher_args = start + usize::from(edit.substitutes())..args.end;
        let (matches, elements) = MatchArguments::new(
            runtime,
            frame,
            matcher_args,
            criterion,
            MatchOperation::Edit,
            at,
        )?
        .build(runtime)?;
        let replacement = edit.substitutes().then(|| runtime.root_slot(frame.get(start)));
        Ok(Self {
            selection: Selection::new(elements),
            matches,
            destructive: matches!(edit, Edit::Delete | Edit::NSubstitute),
            replacement,
            resume,
            at,
        })
    }

    fn mark_hit(&mut self, index: usize) {
        if self.replacement.is_some() {
            self.selection.replace(index);
        } else {
            self.selection.discard(index);
        }
    }
}

impl Continuation for PredicateRemove {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            match self
                .matches
                .next(runtime, self.selection.elements(), values)
            {
                MatchStep::Call(step) => return Ok(step),
                MatchStep::Found { index, .. } => self.mark_hit(index),
                MatchStep::Exhausted => break,
            }
        }

        self.selection
            .result(
                runtime,
                self.destructive,
                self.replacement.as_ref().map(RootSlot::get),
                self.at,
            )
            .map(ContinuationStep::Done)
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
