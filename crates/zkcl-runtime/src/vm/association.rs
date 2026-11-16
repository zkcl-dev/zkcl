use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlots, SpineCycle, Word};
use crate::{EvalError, Runtime};

use super::sequence::matching::{Comparison, ItemMatcher, Key, MatchCriterion, keyword_values};
use super::{Continuation, ContinuationStep, Resume, Values};

#[derive(Clone, Copy)]
enum Source {
    Member,
    Assoc,
    Rassoc,
}

#[derive(Clone, Copy)]
pub(crate) struct Op(Source, MatchCriterion);

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MEMBER", Op(Source::Member, MatchCriterion::Item)),
    (
        "MEMBER-IF",
        Op(Source::Member, MatchCriterion::Predicate { negate: false }),
    ),
    (
        "MEMBER-IF-NOT",
        Op(Source::Member, MatchCriterion::Predicate { negate: true }),
    ),
    ("ASSOC", Op(Source::Assoc, MatchCriterion::Item)),
    (
        "ASSOC-IF",
        Op(Source::Assoc, MatchCriterion::Predicate { negate: false }),
    ),
    (
        "ASSOC-IF-NOT",
        Op(Source::Assoc, MatchCriterion::Predicate { negate: true }),
    ),
    ("RASSOC", Op(Source::Rassoc, MatchCriterion::Item)),
    (
        "RASSOC-IF",
        Op(Source::Rassoc, MatchCriterion::Predicate { negate: false }),
    ),
    (
        "RASSOC-IF-NOT",
        Op(Source::Rassoc, MatchCriterion::Predicate { negate: true }),
    ),
];

#[derive(Clone, Copy)]
enum Stage {
    Search,
    AfterKey,
    AfterTest,
}

const CURSOR: usize = 0;
const CHECKPOINT: usize = 1;
const NEXT: usize = 2;
const RESULT: usize = 3;

pub(super) struct Association {
    source: Source,
    key: Key,
    matcher: ItemMatcher,
    stage: Stage,
    roots: RootFrame,
    cycle: SpineCycle,
    resume: Resume,
    at: usize,
}

impl Association {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let arguments = args.start + 2..args.end;
        let [key, test, test_not] = match op.1 {
            MatchCriterion::Item => {
                keyword_values(runtime, frame, arguments, ["KEY", "TEST", "TEST-NOT"], at)?
            }
            MatchCriterion::Predicate { .. } => {
                let [key] = keyword_values(runtime, frame, arguments, ["KEY"], at)?;
                [key, None, None]
            }
        };

        let roots = runtime.root_frame(RESULT + 1);
        let item = frame.get(args.start);
        let list = frame.get(args.start + 1);
        roots.set(CURSOR, list);
        roots.set(CHECKPOINT, list);
        let key = Key::resolve(runtime, key, at)?;
        let matcher = op.1.resolve(runtime, item, test, test_not, at)?;
        Ok(Self {
            source: op.0,
            key,
            matcher,
            stage: Stage::Search,
            roots,
            cycle: SpineCycle::new(),
            resume,
            at,
        })
    }

    fn advance(&mut self, runtime: &Runtime) -> Result<(), Fault> {
        let next = self.roots.get(NEXT);
        let checkpoint = self.roots.get(CHECKPOINT);
        if self.cycle.step(next, checkpoint).map_err(|_| {
            Fault::type_error(
                runtime,
                next,
                "LIST",
                EvalError::new(self.at, "expected a finite list"),
            )
        })? {
            self.roots.set(CHECKPOINT, next);
        }
        self.roots.set(CURSOR, next);
        Ok(())
    }
}

impl Continuation for Association {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            let object = match self.stage {
                Stage::AfterKey => values.get(0),
                Stage::AfterTest => {
                    if self.matcher.returned_match(values) {
                        return Ok(ContinuationStep::Done(self.roots.get(RESULT)));
                    }
                    self.advance(runtime)?;
                    self.stage = Stage::Search;
                    continue;
                }
                Stage::Search => {
                    let cursor = self.roots.get(CURSOR);
                    if cursor == Word::NIL {
                        return Ok(ContinuationStep::Done(Word::NIL));
                    }
                    let (element, rest) = runtime.heap.cons_fields(cursor).ok_or_else(|| {
                        Fault::type_error(runtime, 
                            cursor,
                            "LIST",
                            EvalError::new(self.at, "expected a proper list"),
                        )
                    })?;
                    self.roots.set(NEXT, rest);
                    let (object, result) = match self.source {
                        Source::Member => (element, cursor),
                        Source::Assoc | Source::Rassoc => {
                            if element == Word::NIL {
                                self.advance(runtime)?;
                                continue;
                            }
                            let (car, cdr) =
                                runtime.heap.cons_fields(element).ok_or_else(|| {
                                    Fault::type_error(runtime, 
                                        element,
                                        "CONS",
                                        EvalError::new(
                                            self.at,
                                            "expected an association list entry",
                                        ),
                                    )
                                })?;
                            let field = if matches!(self.source, Source::Assoc) {
                                car
                            } else {
                                cdr
                            };
                            (field, element)
                        }
                    };
                    self.roots.set(RESULT, result);
                    if let Some(step) = self.key.apply(runtime, object, self.at) {
                        self.stage = Stage::AfterKey;
                        return Ok(step);
                    }
                    object
                }
            };

            match self.matcher.compare(runtime, object, self.at) {
                Comparison::Known(true) => {
                    return Ok(ContinuationStep::Done(self.roots.get(RESULT)));
                }
                Comparison::Known(false) => {
                    self.advance(runtime)?;
                    self.stage = Stage::Search;
                }
                Comparison::Call(step) => {
                    self.stage = Stage::AfterTest;
                    return Ok(step);
                }
            }
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
