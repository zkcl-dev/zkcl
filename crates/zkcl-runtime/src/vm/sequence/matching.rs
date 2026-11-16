use num_bigint::Sign;
use num_traits::ToPrimitive;

use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, RootFrame, RootSlot, RootSlots, Word};
use crate::vm::keywords::named_keyword_arguments_into;
use crate::vm::{ContinuationStep, Values, designator, equality, named_keyword_arguments};
use crate::{EvalError, Runtime};

use std::ops::Range;

use super::{Indices, bounds, selection::Elements};

pub(in crate::vm) fn keyword_values<const N: usize>(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    names: [&str; N],
    at: usize,
) -> Result<[Option<Word>; N], Fault> {
    Ok(named_keyword_arguments(runtime, frame, args, names, at)?
        .map(|index| index.map(|index| frame.get(index))))
}

pub(in crate::vm) fn call<const N: usize>(
    runtime: &Runtime,
    target: Word,
    arguments: [Word; N],
    at: usize,
) -> ContinuationStep {
    let args = runtime.root_frame(N);
    for (index, value) in arguments.into_iter().enumerate() {
        args.set(index, value);
    }
    ContinuationStep::Call { target, args, at }
}

pub(in crate::vm) struct Key(Option<RootSlot>);

impl Key {
    pub(in crate::vm) fn resolve(
        runtime: &Runtime,
        key: Option<Word>,
        at: usize,
    ) -> Result<Self, Fault> {
        match key {
            None | Some(Word::NIL) => Ok(Self(None)),
            Some(word) => Ok(Self(Some(runtime.root_slot(designator(runtime, word, at)?)))),
        }
    }

    pub(in crate::vm) fn is_identity(&self) -> bool {
        self.0.is_none()
    }

    pub(in crate::vm) fn apply(
        &self,
        runtime: &Runtime,
        value: Word,
        at: usize,
    ) -> Option<ContinuationStep> {
        self.0
            .as_ref()
            .map(|key| call(runtime, key.get(), [value], at))
    }
}

pub(in crate::vm) enum Comparison {
    Known(bool),
    Call(ContinuationStep),
}

pub(in crate::vm) enum BinaryTest {
    Eql,
    Function { target: RootSlot, negate: bool },
}

impl BinaryTest {
    pub(in crate::vm) fn resolve(
        runtime: &Runtime,
        test: Option<Word>,
        test_not: Option<Word>,
        at: usize,
    ) -> Result<Self, Fault> {
        match test.or(test_not) {
            None => Ok(Self::Eql),
            Some(word) => Ok(Self::Function {
                target: runtime.root_slot(designator(runtime, word, at)?),
                negate: test.is_none(),
            }),
        }
    }

    pub(in crate::vm) fn compare(
        &self,
        runtime: &Runtime,
        first: Word,
        second: Word,
        at: usize,
    ) -> Comparison {
        match self {
            Self::Eql => Comparison::Known(equality::eql(&runtime.heap, first, second)),
            Self::Function { target, .. } => {
                Comparison::Call(call(runtime, target.get(), [first, second], at))
            }
        }
    }

    pub(in crate::vm) fn returned_match(&self, values: &Values) -> bool {
        let Self::Function { negate, .. } = self else {
            unreachable!("EQL does not invoke a callback");
        };
        truth(values, *negate)
    }
}

pub(in crate::vm) fn check_test_keywords(
    test: Option<Word>,
    test_not: Option<Word>,
    at: usize,
) -> Result<(), Fault> {
    if test.is_some() && test_not.is_some() {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "cannot specify both :TEST and :TEST-NOT"),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(in crate::vm) enum MatchCriterion {
    Item,
    Predicate { negate: bool },
}

impl MatchCriterion {
    pub(in crate::vm) fn resolve(
        self,
        runtime: &Runtime,
        target: Word,
        test: Option<Word>,
        test_not: Option<Word>,
        at: usize,
    ) -> Result<ItemMatcher, Fault> {
        match self {
            Self::Item => Ok(ItemMatcher::item(
                runtime,
                target,
                BinaryTest::resolve(runtime, test, test_not, at)?,
            )),
            Self::Predicate { negate } => ItemMatcher::predicate(runtime, target, negate, at),
        }
    }
}

pub(in crate::vm) enum ItemMatcher {
    Item { item: RootSlot, test: BinaryTest },
    Predicate { target: RootSlot, negate: bool },
}

impl ItemMatcher {
    pub(in crate::vm) fn item(runtime: &Runtime, item: Word, test: BinaryTest) -> Self {
        Self::Item {
            item: runtime.root_slot(item),
            test,
        }
    }

    pub(in crate::vm) fn predicate(
        runtime: &Runtime,
        predicate: Word,
        negate: bool,
        at: usize,
    ) -> Result<Self, Fault> {
        Ok(Self::Predicate {
            target: runtime.root_slot(designator(runtime, predicate, at)?),
            negate,
        })
    }

    pub(in crate::vm) fn compare(&self, runtime: &Runtime, element: Word, at: usize) -> Comparison {
        match self {
            Self::Item { item, test } => test.compare(runtime, item.get(), element, at),
            Self::Predicate { target, .. } => {
                Comparison::Call(call(runtime, target.get(), [element], at))
            }
        }
    }

    pub(in crate::vm) fn returned_match(&self, values: &Values) -> bool {
        match self {
            Self::Item { test, .. } => test.returned_match(values),
            Self::Predicate { negate, .. } => truth(values, *negate),
        }
    }
}

fn truth(values: &Values, negate: bool) -> bool {
    (values.get(0) != Word::NIL) != negate
}

enum Stage {
    Advance,
    Key(usize),
    Test(usize),
}

#[derive(Clone, Copy)]
pub(in crate::vm) enum MatchOperation {
    Search,
    Edit,
}

pub(in crate::vm) enum MatchStep {
    Found { index: usize, element: Word },
    Exhausted,
    Call(ContinuationStep),
}

/// Tests each element once in the requested order, suspending for KEY and TEST
/// callbacks. FIND can return the original element even if a callback changed
/// the car of its cons in the meantime.
pub(in crate::vm) struct ElementMatches {
    indices: Indices,
    remaining: usize,
    key: Key,
    matcher: ItemMatcher,
    element: RootFrame,
    stage: Stage,
    at: usize,
}

pub(in crate::vm) struct MatchArguments<'args> {
    frame: &'args RootSlots,
    item: usize,
    criterion: MatchCriterion,
    operation: MatchOperation,
    at: usize,
    count: Option<usize>,
    start: Option<usize>,
    end: Option<usize>,
    from_end: Option<usize>,
    key: Option<usize>,
    test: Option<usize>,
    test_not: Option<usize>,
}

impl<'args> MatchArguments<'args> {
    /// ARGS starts with the item or predicate, followed by the sequence.
    pub(in crate::vm) fn new(
        runtime: &Runtime,
        frame: &'args RootSlots,
        args: Range<usize>,
        criterion: MatchCriterion,
        operation: MatchOperation,
        at: usize,
    ) -> Result<Self, Fault> {
        const KEYWORDS: [&str; 7] = [
            "COUNT", "START", "END", "FROM-END", "KEY", "TEST", "TEST-NOT",
        ];
        let skip_count = usize::from(matches!(operation, MatchOperation::Search));
        let skip_tests = match criterion {
            MatchCriterion::Item => 0,
            MatchCriterion::Predicate { .. } => 2,
        };
        let accepted = skip_count..KEYWORDS.len() - skip_tests;
        let mut slots = [None; KEYWORDS.len()];
        named_keyword_arguments_into(
            runtime,
            frame,
            args.start + 2..args.end,
            &KEYWORDS[accepted.clone()],
            &mut slots[accepted],
            at,
        )?;
        let [count, start, end, from_end, key, test, test_not] = slots;
        Ok(Self {
            frame,
            item: args.start,
            criterion,
            operation,
            at,
            count,
            start,
            end,
            from_end,
            key,
            test,
            test_not,
        })
    }

    fn value(&self, slot: Option<usize>) -> Option<Word> {
        slot.map(|index| self.frame.get(index))
    }

    pub(in crate::vm) fn item(&self) -> Word {
        self.frame.get(self.item)
    }

    pub(in crate::vm) fn sequence(&self) -> Word {
        self.frame.get(self.item + 1)
    }

    pub(in crate::vm) fn scans_from_end(&self) -> bool {
        self.value(self.from_end)
            .is_some_and(|word| word != Word::NIL)
    }

    fn range(&self, runtime: &Runtime, length: usize) -> Result<Range<usize>, Fault> {
        bounds(
            runtime,
            length,
            self.value(self.start),
            self.value(self.end),
            self.at,
        )
    }

    pub(in crate::vm) fn default_string_search_range(
        &self,
        runtime: &Runtime,
    ) -> Result<Option<Range<usize>>, Fault> {
        if !matches!(self.criterion, MatchCriterion::Item)
            || self.value(self.key).is_some_and(|key| key != Word::NIL)
            || self.test.is_some()
            || self.test_not.is_some()
        {
            return Ok(None);
        }
        let Some(info) = runtime
            .heap
            .array_info(self.sequence())
            .filter(|info| info.rank == 1 && info.element == ArrayElement::Character)
        else {
            return Ok(None);
        };
        self.range(runtime, info.fill_pointer.unwrap_or(info.size))
            .map(Some)
    }

    pub(in crate::vm) fn build(
        self,
        runtime: &Runtime,
    ) -> Result<(ElementMatches, Elements), Fault> {
        // A specialized vector may allocate when its elements are boxed. Keep
        // keyword locations in the caller's frame until the snapshot is rooted.
        let at = self.at;
        let elements = Elements::new(runtime, self.sequence(), at)?;
        let range = self.range(runtime, elements.len())?;
        let remaining = match self.value(self.count) {
            None | Some(Word::NIL) => range.len(),
            Some(word) => {
                let number = runtime.heap.integer(word).ok_or_else(|| {
                    Fault::type_error(runtime, 
                        word,
                        "INTEGER",
                        EvalError::new(at, "expected an integer or NIL for COUNT"),
                    )
                })?;
                if number.sign() == Sign::Minus {
                    0
                } else {
                    number.to_usize().unwrap_or(usize::MAX)
                }
            }
        };
        let resolve_key = || Key::resolve(runtime, self.value(self.key), at);
        let resolve_matcher = || {
            self.criterion.resolve(
                runtime,
                self.item(),
                self.value(self.test),
                self.value(self.test_not),
                at,
            )
        };
        // If both designators are invalid, searches report KEY first and
        // edits report the predicate or TEST first.
        let (key, matcher) = match self.operation {
            MatchOperation::Search => (resolve_key()?, resolve_matcher()?),
            MatchOperation::Edit => {
                let matcher = resolve_matcher()?;
                (resolve_key()?, matcher)
            }
        };
        Ok((
            ElementMatches {
                indices: Indices::new(range, self.value(self.from_end)),
                remaining,
                key,
                matcher,
                element: runtime.root_frame(1),
                stage: Stage::Advance,
                at,
            },
            elements,
        ))
    }
}

impl ElementMatches {
    pub(in crate::vm) fn next(
        &mut self,
        runtime: &Runtime,
        elements: &Elements,
        values: &Values,
    ) -> MatchStep {
        if self.remaining == 0 {
            return MatchStep::Exhausted;
        }
        match std::mem::replace(&mut self.stage, Stage::Advance) {
            Stage::Advance => {}
            Stage::Key(index) => {
                if let Some(step) = self.compare(runtime, index, values.get(0)) {
                    return step;
                }
            }
            Stage::Test(index) => {
                if self.matcher.returned_match(values) {
                    return self.found(index);
                }
            }
        }

        while let Some(index) = self.indices.next() {
            let element = elements.get(runtime, index);
            self.element.set(0, element);
            if let Some(step) = self.key.apply(runtime, element, self.at) {
                self.stage = Stage::Key(index);
                return MatchStep::Call(step);
            }
            if let Some(step) = self.compare(runtime, index, element) {
                return step;
            }
        }

        MatchStep::Exhausted
    }

    fn compare(&mut self, runtime: &Runtime, index: usize, value: Word) -> Option<MatchStep> {
        match self.matcher.compare(runtime, value, self.at) {
            Comparison::Known(true) => Some(self.found(index)),
            Comparison::Known(false) => None,
            Comparison::Call(step) => {
                self.stage = Stage::Test(index);
                Some(MatchStep::Call(step))
            }
        }
    }

    fn found(&mut self, index: usize) -> MatchStep {
        self.remaining -= 1;
        MatchStep::Found {
            index,
            element: self.element.get(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ConditionClass, EvalErrorKind, Runtime};

    #[test]
    fn from_end_substitution_stops_calling_the_key_at_the_requested_count() {
        let runtime = Runtime::start().unwrap();
        let values = runtime
            .eval(
                "(let ((seen nil))
               (values (substitute :new 2 '((0) (2) (2) (3) (2) (4))
                                   :start 1 :end 5 :from-end t :count 2
                                   :key (lambda (entry) (push (car entry) seen) (car entry))
                                   :test #'=)
                       seen))",
            )
            .unwrap();
        assert_eq!(
            runtime.repr(&values[0]).unwrap(),
            "((0) (2) :NEW (3) :NEW (4))"
        );
        assert_eq!(runtime.repr(&values[1]).unwrap(), "(2 3 2)");
    }

    #[test]
    fn sequence_functions_reject_keywords_outside_their_lambda_list() {
        let runtime = Runtime::start().unwrap();
        for form in [
            "(find 2 '(1 2) :count 1)",
            "(substitute-if 0 #'oddp '(1) :test #'eql)",
        ] {
            let error = runtime.eval(form).err().expect(form);
            assert_eq!(
                error.kind(),
                EvalErrorKind::LispCondition(ConditionClass::ProgramError)
            );
        }
    }
    #[test]
    fn search_checks_the_key_first_while_edits_check_the_predicate_first() {
        let runtime = Runtime::start().unwrap();
        for (form, expected) in [
            ("(find-if 11 '(1) :key 22)", 22),
            ("(find 1 '(1) :test 11 :key 22)", 22),
            ("(remove-if 11 '(1) :key 22)", 11),
            ("(substitute 0 1 '(1) :test 11 :key 22)", 11),
        ] {
            let form = format!("(handler-case {form} (type-error (c) (type-error-datum c)))");
            assert_eq!(
                runtime.eval(&form).unwrap()[0].as_fixnum(),
                Some(expected),
                "{form}"
            );
        }
    }
}
