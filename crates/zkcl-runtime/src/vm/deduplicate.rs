use std::collections::HashMap;
use std::ops::Range;

use crate::Runtime;
use crate::condition::Fault;
use crate::heap::{HashTest, RootFrame, RootSlots, Word};

use super::sequence::matching::{BinaryTest, Comparison, Key};
use super::sequence::{Elements, Selection, bounds};
use super::{Continuation, ContinuationStep, Resume, Values, equality, named_keyword_arguments};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Remove,
    Delete,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("REMOVE-DUPLICATES", Op::Remove),
    ("DELETE-DUPLICATES", Op::Delete),
];

#[derive(Clone, Copy)]
enum Stage {
    Candidate,
    CandidateKey,
    Peer,
    PeerKey,
    Test,
}

pub(super) struct Deduplicate {
    selection: Selection,
    op: Op,
    range: Range<usize>,
    from_end: bool,
    key: Key,
    test: BinaryTest,
    keyed_candidate: RootFrame,
    candidate: usize,
    peer: Range<usize>,
    stage: Stage,
    resume: Resume,
    at: usize,
}

impl Deduplicate {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let keywords = named_keyword_arguments(
            runtime,
            frame,
            args.start + 1..args.end,
            ["FROM-END", "START", "END", "KEY", "TEST", "TEST-NOT"],
            at,
        )?;
        let elements = Elements::new(runtime, frame.get(args.start), at)?;
        let [from_end, start, end, key, test, test_not] =
            keywords.map(|register| register.map(|register| frame.get(register)));
        let range = bounds(runtime, elements.len(), start, end, at)?;
        let start = range.start;
        let key = Key::resolve(runtime, key, at)?;
        let test = BinaryTest::resolve(runtime, test, test_not, at)?;
        Ok(Self {
            selection: Selection::new(elements),
            op,
            range,
            from_end: from_end.is_some_and(|word| word != Word::NIL),
            key,
            test,
            keyed_candidate: runtime.root_frame(1),
            candidate: start,
            peer: 0..0,
            stage: Stage::Candidate,
            resume,
            at,
        })
    }

    fn mark_default(&mut self, runtime: &Runtime, indices: impl Iterator<Item = usize>) {
        let mut seen: HashMap<usize, Vec<usize>> = HashMap::new();
        for index in indices {
            let element = self.selection.elements().get(runtime, index);
            let hash = equality::hash(&runtime.heap, element, HashTest::Eql);
            let peers = seen.entry(hash).or_default();
            if peers.iter().any(|&peer| {
                equality::matches(
                    &runtime.heap,
                    element,
                    self.selection.elements().get(runtime, peer),
                    HashTest::Eql,
                )
            }) {
                self.selection.discard(index);
            } else {
                peers.push(index);
            }
        }
    }

    fn next_candidate(&mut self, matched: bool) {
        if matched {
            self.selection.discard(self.candidate);
            self.candidate += 1;
            self.stage = Stage::Candidate;
        } else {
            self.stage = Stage::Peer;
        }
    }

    fn compare(&mut self, runtime: &Runtime, peer: Word) -> Option<ContinuationStep> {
        match self
            .test
            .compare(runtime, self.keyed_candidate.get(0), peer, self.at)
        {
            Comparison::Known(matched) => {
                self.next_candidate(matched);
                None
            }
            Comparison::Call(step) => {
                self.stage = Stage::Test;
                Some(step)
            }
        }
    }

    fn finish(&self, runtime: &Runtime) -> Result<ContinuationStep, Fault> {
        self.selection
            .result(runtime, matches!(self.op, Op::Delete), None, self.at)
            .map(ContinuationStep::Done)
    }
}

impl Continuation for Deduplicate {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if self.key.is_identity() && matches!(self.test, BinaryTest::Eql) {
            if self.from_end {
                self.mark_default(runtime, self.range.clone());
            } else {
                self.mark_default(runtime, self.range.clone().rev());
            }
            return self.finish(runtime);
        }
        loop {
            let step = match self.stage {
                Stage::Candidate => {
                    if self.candidate == self.range.end {
                        return self.finish(runtime);
                    }
                    self.peer = if self.from_end {
                        self.range.start..self.candidate
                    } else {
                        self.candidate + 1..self.range.end
                    };
                    if self.peer.is_empty() {
                        self.candidate += 1;
                        continue;
                    }
                    let element = self.selection.elements().get(runtime, self.candidate);
                    if let Some(step) = self.key.apply(runtime, element, self.at) {
                        self.stage = Stage::CandidateKey;
                        return Ok(step);
                    }
                    self.keyed_candidate.set(0, element);
                    self.stage = Stage::Peer;
                    None
                }
                Stage::CandidateKey => {
                    self.keyed_candidate.set(0, values.get(0));
                    self.stage = Stage::Peer;
                    None
                }
                Stage::Peer => {
                    let peer = self
                        .peer
                        .find(|&index| !self.from_end || self.selection.is_kept(index));
                    let Some(peer) = peer else {
                        self.candidate += 1;
                        self.stage = Stage::Candidate;
                        continue;
                    };
                    let element = self.selection.elements().get(runtime, peer);
                    if let Some(step) = self.key.apply(runtime, element, self.at) {
                        self.stage = Stage::PeerKey;
                        return Ok(step);
                    }
                    self.compare(runtime, element)
                }
                Stage::PeerKey => self.compare(runtime, values.get(0)),
                Stage::Test => {
                    self.next_candidate(self.test.returned_match(values));
                    None
                }
            };
            if let Some(step) = step {
                return Ok(step);
            }
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
