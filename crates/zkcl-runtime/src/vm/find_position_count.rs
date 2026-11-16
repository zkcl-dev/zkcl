use std::ops::Range;

use crate::Runtime;
use crate::condition::Fault;
use crate::heap::{RootSlots, Word};

use super::sequence::Elements;
use super::sequence::matching::{
    ElementMatches, MatchArguments, MatchCriterion, MatchOperation, MatchStep,
};
use super::{Continuation, ContinuationStep, Invocation, Resume, Values};

#[derive(Clone, Copy)]
enum SearchResultKind {
    Element,
    Index,
    Count,
}

#[derive(Clone, Copy)]
pub(crate) struct Op {
    result_kind: SearchResultKind,
    criterion: MatchCriterion,
}

const fn item(result_kind: SearchResultKind) -> Op {
    Op {
        result_kind,
        criterion: MatchCriterion::Item,
    }
}

const fn predicate(result_kind: SearchResultKind, negate: bool) -> Op {
    Op {
        result_kind,
        criterion: MatchCriterion::Predicate { negate },
    }
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("FIND", item(SearchResultKind::Element)),
    ("FIND-IF", predicate(SearchResultKind::Element, false)),
    ("FIND-IF-NOT", predicate(SearchResultKind::Element, true)),
    ("POSITION", item(SearchResultKind::Index)),
    ("POSITION-IF", predicate(SearchResultKind::Index, false)),
    ("POSITION-IF-NOT", predicate(SearchResultKind::Index, true)),
    ("COUNT", item(SearchResultKind::Count)),
    ("COUNT-IF", predicate(SearchResultKind::Count, false)),
    ("COUNT-IF-NOT", predicate(SearchResultKind::Count, true)),
];

pub(super) struct ElementSearch {
    kind: SearchResultKind,
    elements: Elements,
    matches: ElementMatches,
    match_count: usize,
    resume: Resume,
}

pub(super) fn invoke<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let arguments = MatchArguments::new(
        runtime,
        frame,
        args,
        op.criterion,
        MatchOperation::Search,
        at,
    )?;
    let kind = op.result_kind;
    if let Some(range) = arguments.default_string_search_range(runtime)?
        && let Some(result) = runtime.heap.with_array_character_slice(
            arguments.sequence(),
            range.start,
            range.len(),
            |text| kind.search_string(text, arguments.item(), range, arguments.scans_from_end()),
        )
    {
        values.one(result);
        return Ok(Invocation::Done(resume));
    }
    let (matches, elements) = arguments.build(runtime)?;
    Ok(Invocation::Continuation(Box::new(ElementSearch {
        kind,
        elements,
        matches,
        match_count: 0,
        resume,
    })))
}

impl SearchResultKind {
    // Default EQL on characters cannot enter Lisp. Search the borrowed UTF-8
    // directly and return an immediate value before releasing the borrow.
    fn search_string(self, text: &str, item: Word, range: Range<usize>, from_end: bool) -> Word {
        let Some(character) = item.as_character() else {
            return match self {
                Self::Element | Self::Index => Word::NIL,
                Self::Count => Word::fixnum(0).unwrap(),
            };
        };
        match self {
            Self::Element => {
                if text.contains(character) {
                    item
                } else {
                    Word::NIL
                }
            }
            Self::Index => {
                let byte = if from_end {
                    text.rfind(character)
                } else {
                    text.find(character)
                };
                let Some(byte) = byte else {
                    return Word::NIL;
                };
                let offset = if text.len() == range.len() {
                    byte
                } else {
                    text[..byte].chars().count()
                };
                Word::fixnum((range.start + offset) as isize).expect("index fits a fixnum")
            }
            Self::Count => {
                let count = text.matches(character).count();
                Word::fixnum(count as isize).expect("count fits a fixnum")
            }
        }
    }
}

impl Continuation for ElementSearch {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            let result = match self.matches.next(runtime, &self.elements, values) {
                MatchStep::Call(step) => return Ok(step),
                MatchStep::Found { index, element } => match self.kind {
                    SearchResultKind::Element => element,
                    SearchResultKind::Index => {
                        Word::fixnum(index as isize).expect("index fits a fixnum")
                    }
                    SearchResultKind::Count => {
                        self.match_count += 1;
                        continue;
                    }
                },
                MatchStep::Exhausted => match self.kind {
                    SearchResultKind::Element | SearchResultKind::Index => Word::NIL,
                    SearchResultKind::Count => {
                        Word::fixnum(self.match_count as isize).expect("count fits a fixnum")
                    }
                },
            };
            return Ok(ContinuationStep::Done(result));
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
