use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{ListSpine, RootFrame, RootSlot, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::list::ListBuilder;
use super::sequence::matching::{BinaryTest, Comparison, Key, keyword_values};
use super::{Continuation, ContinuationStep, Resume, Values};

const ELEMENT: usize = 0;
const KEYED_ELEMENT: usize = 1;
const DUPLICATE_CURSOR: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Adjoin,
    Union,
    Intersection,
    Difference,
    ExclusiveOr,
    Subsetp,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("ADJOIN", Op::Adjoin),
    ("UNION", Op::Union),
    ("NUNION", Op::Union),
    ("INTERSECTION", Op::Intersection),
    ("NINTERSECTION", Op::Intersection),
    ("SET-DIFFERENCE", Op::Difference),
    ("NSET-DIFFERENCE", Op::Difference),
    ("SET-EXCLUSIVE-OR", Op::ExclusiveOr),
    ("NSET-EXCLUSIVE-OR", Op::ExclusiveOr),
    ("SUBSETP", Op::Subsetp),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        count >= 2
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    First,
    Second,
}

#[derive(Clone, Copy)]
enum Scan {
    Target(usize),
    Duplicate,
    Kept(usize),
}

#[derive(Clone, Copy)]
enum Stage {
    Next,
    AfterKeyDriver,
    Scan(Scan),
    AfterKey(Scan),
    AfterTest(Scan),
}

/// The list set operations share one traversal: take an element from the first
/// list, apply the key function, then compare it against every element of the
/// second list. The test function always receives the first list's element as
/// its first argument, which is what the standard means by `(test item element)`;
/// SET-EXCLUSIVE-OR therefore swaps the arguments while it traverses its second
/// list. Both arguments must be proper lists, so vectors are rejected even
/// though they are sequences.
pub(super) struct SetOperation {
    op: Op,
    key: Key,
    test: BinaryTest,
    list: RootSlot,
    result: ListBuilder,
    driver: Vec<RootSlot>,
    driver_at: usize,
    target: Vec<RootSlot>,
    state: RootFrame,
    /// Keyed elements of the first list that UNION has already appended.
    kept: Vec<RootSlot>,
    phase: Phase,
    stage: Stage,
    resume: Resume,
    at: usize,
}

impl SetOperation {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let [key, test, test_not] = keyword_values(
            runtime,
            frame,
            args.start + 2..args.end,
            ["KEY", "TEST", "TEST-NOT"],
            at,
        )?;
        let key = Key::resolve(runtime, key, at)?;
        let test = BinaryTest::resolve(runtime, test, test_not, at)?;

        let first = frame.get(args.start);
        let second = frame.get(args.start + 1);
        let driver = match op {
            Op::Adjoin => vec![runtime.root_slot(first)],
            _ => snapshot(runtime, first, at)?,
        };
        let target = snapshot(runtime, second, at)?;
        let result = ListBuilder::new(runtime);
        if op == Op::Union {
            for element in &target {
                result.push(runtime, || element.get(), at)?;
            }
        }
        Ok(Self {
            op,
            key,
            test,
            list: runtime.root_slot(if op == Op::Adjoin { second } else { Word::NIL }),
            result,
            driver,
            driver_at: 0,
            target,
            state: runtime.root_frame(DUPLICATE_CURSOR + 1),
            kept: Vec::new(),
            phase: Phase::First,
            stage: Stage::Next,
            resume,
            at,
        })
    }

    fn finish(&self) -> Word {
        match self.op {
            Op::Subsetp => Word::T,
            _ => self.result.head(),
        }
    }

    fn scan_driver(&mut self, keyed: Word) {
        self.state.set(KEYED_ELEMENT, keyed);
        self.stage = Stage::Scan(Scan::Target(0));
    }

    fn scan_duplicates(&mut self) {
        self.state.set(DUPLICATE_CURSOR, self.result.head());
        self.stage = Stage::Scan(Scan::Duplicate);
    }

    fn next_probe(&self, runtime: &Runtime, scan: Scan) -> Option<(Scan, Word)> {
        match scan {
            Scan::Target(index) => self
                .target
                .get(index)
                .map(|element| (Scan::Target(index + 1), element.get())),
            Scan::Duplicate => {
                // The cursor only visits conses created by the result builder.
                let cursor = self.state.get(DUPLICATE_CURSOR);
                if cursor == Word::NIL {
                    return None;
                }
                let (car, cdr) = runtime.heap.cons_fields(cursor).expect("result list cell");
                self.state.set(DUPLICATE_CURSOR, cdr);
                Some((Scan::Duplicate, car))
            }
            Scan::Kept(index) => self
                .kept
                .get(index)
                .map(|element| (Scan::Kept(index + 1), element.get())),
        }
    }

    fn compare(&mut self, runtime: &Runtime, scan: Scan, probe: Word) -> Option<ContinuationStep> {
        let driver = self.state.get(KEYED_ELEMENT);
        let (left, right) = if matches!(scan, Scan::Kept(_)) || self.phase == Phase::Second {
            (probe, driver)
        } else {
            (driver, probe)
        };
        match self.test.compare(runtime, left, right, self.at) {
            Comparison::Known(matched) => self.after_test(scan, matched),
            Comparison::Call(step) => {
                self.stage = Stage::AfterTest(scan);
                Some(step)
            }
        }
    }

    fn after_test(&mut self, scan: Scan, matched: bool) -> Option<ContinuationStep> {
        self.stage = Stage::Scan(scan);
        match scan {
            Scan::Target(_) if matched => match self.op {
                Op::Adjoin => return Some(ContinuationStep::Done(self.list.get())),
                Op::Intersection => self.scan_duplicates(),
                _ => self.stage = Stage::Next,
            },
            Scan::Target(_)
                if self.op == Op::Union
                    && self.key.is_identity()
                    && matches!(self.test, BinaryTest::Eql) =>
            {
                self.scan_duplicates();
            }
            Scan::Duplicate | Scan::Kept(_) if matched => self.stage = Stage::Next,
            _ => {}
        }
        None
    }

    fn append_element(&self, runtime: &Runtime) -> Result<(), Fault> {
        self.result
            .push(runtime, || self.state.get(ELEMENT), self.at)?;
        Ok(())
    }

    fn after_scan(
        &mut self,
        runtime: &Runtime,
        scan: Scan,
    ) -> Result<Option<ContinuationStep>, Fault> {
        self.stage = Stage::Next;
        match scan {
            Scan::Target(_) => match self.op {
                Op::Adjoin => {
                    let cell = runtime
                        .heap
                        .cons(|| (self.state.get(ELEMENT), self.list.get()))
                        .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                    return Ok(Some(ContinuationStep::Done(cell)));
                }
                Op::Subsetp => return Ok(Some(ContinuationStep::Done(Word::NIL))),
                Op::Union => self.stage = Stage::Scan(Scan::Kept(0)),
                Op::Difference | Op::ExclusiveOr => self.append_element(runtime)?,
                Op::Intersection => {}
            },
            Scan::Duplicate => self.append_element(runtime)?,
            Scan::Kept(_) => {
                self.append_element(runtime)?;
                self.kept.push(runtime.root_slot(self.state.get(KEYED_ELEMENT)));
            }
        }
        Ok(None)
    }
}

impl Continuation for SetOperation {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            let step = match self.stage {
                Stage::Next => {
                    if self.driver_at == self.driver.len() {
                        if self.op == Op::ExclusiveOr && self.phase == Phase::First {
                            self.phase = Phase::Second;
                            std::mem::swap(&mut self.driver, &mut self.target);
                            self.driver_at = 0;
                            continue;
                        }
                        return Ok(ContinuationStep::Done(self.finish()));
                    }
                    let element = self.driver[self.driver_at].get();
                    self.driver_at += 1;
                    self.state.set(ELEMENT, element);
                    if let Some(step) = self.key.apply(runtime, element, self.at) {
                        self.stage = Stage::AfterKeyDriver;
                        return Ok(step);
                    }
                    self.scan_driver(element);
                    None
                }
                Stage::AfterKeyDriver => {
                    self.scan_driver(values.get(0));
                    None
                }
                Stage::Scan(scan) => match self.next_probe(runtime, scan) {
                    Some((next, element)) => {
                        // UNION remembers the keys of retained driver elements.
                        if !matches!(scan, Scan::Kept(_))
                            && let Some(step) = self.key.apply(runtime, element, self.at)
                        {
                            self.stage = Stage::AfterKey(next);
                            return Ok(step);
                        }
                        self.compare(runtime, next, element)
                    }
                    None => self.after_scan(runtime, scan)?,
                },
                Stage::AfterKey(next) => self.compare(runtime, next, values.get(0)),
                Stage::AfterTest(next) => self.after_test(next, self.test.returned_match(values)),
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

fn snapshot(runtime: &Runtime, word: Word, at: usize) -> Result<Vec<RootSlot>, Fault> {
    let invalid = || Fault::type_error(runtime, word, "LIST", EvalError::new(at, "expected a proper list"));
    let mut result = Vec::new();
    let mut spine = ListSpine::new(word);
    loop {
        match spine.next(&runtime.heap) {
            Ok(Some((car, _))) => result.push(runtime.root_slot(car)),
            Ok(None) => break,
            Err(_) => return Err(invalid()),
        }
    }
    if spine.tail() != Word::NIL {
        return Err(Fault::type_error(runtime, 
            spine.tail(),
            "LIST",
            EvalError::new(at, "expected a proper list"),
        ));
    }
    Ok(result)
}
