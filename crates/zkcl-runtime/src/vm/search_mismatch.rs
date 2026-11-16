use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::sequence::matching::{BinaryTest, Comparison, Key, check_test_keywords, keyword_values};
use super::sequence::{Sequence, bounds};
use super::{Continuation, ContinuationStep, Resume, Values};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Search,
    Mismatch,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[("SEARCH", Op::Search), ("MISMATCH", Op::Mismatch)];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        count >= 2
    }
}

enum Stage {
    ApplyKeys { sequence: usize, next: usize },
    StartComparison,
    Compare,
    AfterTest,
}

pub(super) struct SubsequenceComparison {
    op: Op,
    elements: [RootFrame; 2],
    start1: usize,
    start2: usize,
    len1: usize,
    len2: usize,
    from_end: bool,
    key: Key,
    test: BinaryTest,
    // For SEARCH: outer position in 0..=(len2 - len1) and inner offset 0..len1.
    // For MISMATCH: step k in 0..min(len1, len2).
    candidate_start: usize,
    offset: usize,
    stage: Stage,
    resume: Resume,
    at: usize,
}

impl SubsequenceComparison {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let [from_end, test, test_not, key, start1, start2, end1, end2] = keyword_values(
            runtime,
            frame,
            args.start + 2..args.end,
            [
                "FROM-END", "TEST", "TEST-NOT", "KEY", "START1", "START2", "END1", "END2",
            ],
            at,
        )?;
        check_test_keywords(test, test_not, at)?;

        let seq1 = Sequence::new(runtime, frame, args.start, at)?;
        let seq2 = Sequence::new(runtime, frame, args.start + 1, at)?;
        let range1 = bounds(runtime, seq1.len, start1, end1, at)?;
        let range2 = bounds(runtime, seq2.len, start2, end2, at)?;

        let key = Key::resolve(runtime, key, at)?;
        let test = BinaryTest::resolve(runtime, test, test_not, at)?;

        let len1 = range1.len();
        let len2 = range2.len();
        let start1_idx = range1.start;
        let start2_idx = range2.start;
        let first_elements = seq1.snapshot(runtime, range1, at)?;
        let second_elements = seq2.snapshot(runtime, range2, at)?;
        let from_end = from_end.is_some_and(|w| w != Word::NIL);

        let stage = if key.is_identity() {
            Stage::StartComparison
        } else {
            Stage::ApplyKeys {
                sequence: 0,
                next: 0,
            }
        };

        let candidate_start = if matches!(op, Op::Search) && from_end && len1 <= len2 {
            len2 - len1
        } else {
            0
        };

        Ok(Self {
            op,
            elements: [first_elements, second_elements],
            start1: start1_idx,
            start2: start2_idx,
            len1,
            len2,
            from_end,
            key,
            test,
            candidate_start,
            offset: 0,
            stage,
            resume,
            at,
        })
    }

    fn index_result(&self, idx: usize) -> Result<ContinuationStep, Fault> {
        let word = Word::fixnum(idx as isize)
            .ok_or_else(|| EvalError::new(self.at, "sequence index out of fixnum range"))?;
        Ok(ContinuationStep::Done(word))
    }

    fn current_pair(&self) -> (Word, Word) {
        match self.op {
            Op::Search => (
                self.elements[0].get(self.offset),
                self.elements[1].get(self.candidate_start + self.offset),
            ),
            Op::Mismatch => {
                if self.from_end {
                    (
                        self.elements[0].get(self.len1 - 1 - self.offset),
                        self.elements[1].get(self.len2 - 1 - self.offset),
                    )
                } else {
                    (
                        self.elements[0].get(self.offset),
                        self.elements[1].get(self.offset),
                    )
                }
            }
        }
    }

    fn mismatch_position(&self, offset: usize) -> usize {
        self.start1
            + if self.from_end {
                self.len1 - offset
            } else {
                offset
            }
    }

    fn start_comparison(&self) -> Result<Option<ContinuationStep>, Fault> {
        match self.op {
            Op::Search if self.len1 == 0 => self
                .index_result(self.start2 + self.candidate_start)
                .map(Some),
            Op::Search if self.len1 > self.len2 => Ok(Some(ContinuationStep::Done(Word::NIL))),
            Op::Mismatch if self.len1.min(self.len2) == 0 => {
                if self.len1 == self.len2 {
                    Ok(Some(ContinuationStep::Done(Word::NIL)))
                } else {
                    self.index_result(self.mismatch_position(0)).map(Some)
                }
            }
            _ => Ok(None),
        }
    }

    fn advance(&mut self, matched: bool) -> Result<Option<ContinuationStep>, Fault> {
        match (self.op, matched) {
            (Op::Search, true) => {
                self.offset += 1;
                if self.offset == self.len1 {
                    return self
                        .index_result(self.start2 + self.candidate_start)
                        .map(Some);
                }
            }
            (Op::Search, false) => {
                if self.from_end {
                    if self.candidate_start == 0 {
                        return Ok(Some(ContinuationStep::Done(Word::NIL)));
                    }
                    self.candidate_start -= 1;
                } else {
                    if self.candidate_start + self.len1 == self.len2 {
                        return Ok(Some(ContinuationStep::Done(Word::NIL)));
                    }
                    self.candidate_start += 1;
                }
                self.offset = 0;
            }
            (Op::Mismatch, true) => {
                self.offset += 1;
                if self.offset == self.len1.min(self.len2) {
                    return if self.len1 == self.len2 {
                        Ok(Some(ContinuationStep::Done(Word::NIL)))
                    } else {
                        self.index_result(self.mismatch_position(self.offset))
                            .map(Some)
                    };
                }
            }
            (Op::Mismatch, false) => {
                return self
                    .index_result(self.mismatch_position(self.offset))
                    .map(Some);
            }
        }
        Ok(None)
    }
}

impl Continuation for SubsequenceComparison {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            match self.stage {
                Stage::ApplyKeys { sequence, next } => {
                    let elements = &self.elements[sequence];
                    if next > 0 {
                        elements.set(next - 1, values.get(0));
                    }
                    if next < elements.len() {
                        self.stage = Stage::ApplyKeys {
                            sequence,
                            next: next + 1,
                        };
                        return Ok(self
                            .key
                            .apply(runtime, elements.get(next), self.at)
                            .expect("non-identity key"));
                    }
                    self.stage = if sequence == 0 {
                        Stage::ApplyKeys {
                            sequence: 1,
                            next: 0,
                        }
                    } else {
                        Stage::StartComparison
                    };
                }
                Stage::StartComparison => {
                    if let Some(step) = self.start_comparison()? {
                        return Ok(step);
                    }
                    self.stage = Stage::Compare;
                }
                Stage::Compare => {
                    let (left, right) = self.current_pair();
                    match self.test.compare(runtime, left, right, self.at) {
                        Comparison::Known(matched) => {
                            if let Some(step) = self.advance(matched)? {
                                return Ok(step);
                            }
                        }
                        Comparison::Call(step) => {
                            self.stage = Stage::AfterTest;
                            return Ok(step);
                        }
                    }
                }
                Stage::AfterTest => {
                    if let Some(step) = self.advance(self.test.returned_match(values))? {
                        return Ok(step);
                    }
                    self.stage = Stage::Compare;
                }
            }
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
