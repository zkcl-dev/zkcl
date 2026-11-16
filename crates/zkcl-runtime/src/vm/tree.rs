use std::ops::Range;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::{EvalError, Runtime};

use super::list::list_tail;
use super::sequence::matching::{
    BinaryTest, Comparison, ItemMatcher, Key, check_test_keywords, keyword_values,
};
use super::{Continuation, ContinuationStep, Resume, Values, proper_list};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Subst {
        destructive: bool,
        if_pred: Option<bool>,
    },
    Sublis {
        destructive: bool,
    },
    TreeEqual,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    (
        "SUBST",
        Op::Subst {
            destructive: false,
            if_pred: None,
        },
    ),
    (
        "SUBST-IF",
        Op::Subst {
            destructive: false,
            if_pred: Some(false),
        },
    ),
    (
        "SUBST-IF-NOT",
        Op::Subst {
            destructive: false,
            if_pred: Some(true),
        },
    ),
    (
        "NSUBST",
        Op::Subst {
            destructive: true,
            if_pred: None,
        },
    ),
    (
        "NSUBST-IF",
        Op::Subst {
            destructive: true,
            if_pred: Some(false),
        },
    ),
    (
        "NSUBST-IF-NOT",
        Op::Subst {
            destructive: true,
            if_pred: Some(true),
        },
    ),
    ("SUBLIS", Op::Sublis { destructive: false }),
    ("NSUBLIS", Op::Sublis { destructive: true }),
    ("TREE-EQUAL", Op::TreeEqual),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Subst { .. } => count >= 3,
            Self::Sublis { .. } | Self::TreeEqual => count >= 2,
        }
    }
}

enum Task {
    Visit(RootSlot),
    FinishCons {
        node: RootSlot,
        car_result: Option<RootSlot>,
    },
}

enum Rules {
    Item {
        matcher: ItemMatcher,
        replacement: RootSlot,
    },
    Alist {
        test: BinaryTest,
        entries: Vec<(RootSlot, RootSlot)>,
    },
}

enum Stage {
    Visit,
    AfterKey,
    AfterItemTest,
    TryAlist(usize),
    AfterAlistTest(usize),
    Descend,
}

const NODE: usize = 0;
const KEYED: usize = 1;

struct Substitution {
    destructive: bool,
    key: Key,
    rules: Rules,
    tasks: Vec<Task>,
    state: RootFrame,
    stage: Stage,
    at: usize,
}

struct TreeEquality {
    test: BinaryTest,
    pairs: Vec<(RootSlot, RootSlot)>,
    awaiting_test: bool,
    at: usize,
}

enum TreeTask {
    Substitute(Substitution),
    Equal(TreeEquality),
}

pub(super) struct TreeOperation {
    work: TreeTask,
    resume: Resume,
}

impl TreeOperation {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let work = match op {
            Op::TreeEqual => {
                let [test, test_not] = keyword_values(
                    runtime,
                    frame,
                    args.start + 2..args.end,
                    ["TEST", "TEST-NOT"],
                    at,
                )?;
                check_test_keywords(test, test_not, at)?;
                TreeTask::Equal(TreeEquality {
                    test: BinaryTest::resolve(runtime, test, test_not, at)?,
                    pairs: vec![(
                        runtime.root_slot(frame.get(args.start)),
                        runtime.root_slot(frame.get(args.start + 1)),
                    )],
                    awaiting_test: false,
                    at,
                })
            }
            Op::Subst {
                destructive,
                if_pred,
            } => {
                let arguments = args.start + 3..args.end;
                let [key, test, test_not] = if if_pred.is_some() {
                    let [key] = keyword_values(runtime, frame, arguments, ["KEY"], at)?;
                    [key, None, None]
                } else {
                    keyword_values(runtime, frame, arguments, ["KEY", "TEST", "TEST-NOT"], at)?
                };
                let key = Key::resolve(runtime, key, at)?;
                let target = frame.get(args.start + 1);
                let matcher = if let Some(negate) = if_pred {
                    ItemMatcher::predicate(runtime, target, negate, at)?
                } else {
                    check_test_keywords(test, test_not, at)?;
                    let test = BinaryTest::resolve(runtime, test, test_not, at)?;
                    ItemMatcher::item(runtime, target, test)
                };
                TreeTask::Substitute(Substitution::new(
                    runtime,
                    destructive,
                    key,
                    Rules::Item {
                        matcher,
                        replacement: runtime.root_slot(frame.get(args.start)),
                    },
                    frame.get(args.start + 2),
                    at,
                ))
            }
            Op::Sublis { destructive } => {
                let [key, test, test_not] = keyword_values(
                    runtime,
                    frame,
                    args.start + 2..args.end,
                    ["KEY", "TEST", "TEST-NOT"],
                    at,
                )?;
                let key = Key::resolve(runtime, key, at)?;
                check_test_keywords(test, test_not, at)?;
                let test = BinaryTest::resolve(runtime, test, test_not, at)?;
                let alist = frame.get(args.start);
                let entries = proper_list(runtime, alist, at)
                    .map_err(|error| Fault::type_error(runtime, list_tail(runtime, alist), "LIST", error))?;
                let mut rules = Vec::with_capacity(entries.len());
                for entry in entries {
                    if entry == Word::NIL {
                        continue;
                    }
                    let Some((key, value)) = runtime.heap.cons_fields(entry) else {
                        return Err(Fault::type_error(runtime, 
                            entry,
                            "LIST",
                            EvalError::new(at, "expected an association pair in alist"),
                        ));
                    };
                    rules.push((runtime.root_slot(key), runtime.root_slot(value)));
                }
                TreeTask::Substitute(Substitution::new(
                    runtime,
                    destructive,
                    key,
                    Rules::Alist {
                        test,
                        entries: rules,
                    },
                    frame.get(args.start + 1),
                    at,
                ))
            }
        };
        Ok(Self { work, resume })
    }
}

impl Continuation for TreeOperation {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        match &mut self.work {
            TreeTask::Substitute(substitution) => substitution.step(runtime, values),
            TreeTask::Equal(equality) => Ok(equality.step(runtime, values)),
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

impl TreeEquality {
    fn step(&mut self, runtime: &Runtime, values: &Values) -> ContinuationStep {
        if self.awaiting_test && !self.test.returned_match(values) {
            return ContinuationStep::Done(Word::NIL);
        }
        while let Some((left, right)) = self.pairs.pop() {
            let left = left.get();
            let right = right.get();
            match (
                runtime.heap.cons_fields(left),
                runtime.heap.cons_fields(right),
            ) {
                (Some((left_car, left_cdr)), Some((right_car, right_cdr))) => {
                    self.pairs
                        .push((runtime.root_slot(left_cdr), runtime.root_slot(right_cdr)));
                    self.pairs
                        .push((runtime.root_slot(left_car), runtime.root_slot(right_car)));
                }
                (Some(_), None) | (None, Some(_)) => return ContinuationStep::Done(Word::NIL),
                (None, None) => match self.test.compare(runtime, left, right, self.at) {
                    Comparison::Known(true) => {}
                    Comparison::Known(false) => return ContinuationStep::Done(Word::NIL),
                    Comparison::Call(step) => {
                        self.awaiting_test = true;
                        return step;
                    }
                },
            }
        }
        ContinuationStep::Done(Word::T)
    }
}

impl Substitution {
    fn new(runtime: &Runtime, destructive: bool, key: Key, rules: Rules, tree: Word, at: usize) -> Self {
        Self {
            destructive,
            key,
            rules,
            tasks: vec![Task::Visit(runtime.root_slot(tree))],
            state: runtime.root_frame(KEYED + 1),
            stage: Stage::Visit,
            at,
        }
    }

    fn finish_node(
        &mut self,
        runtime: &Runtime,
        value: Word,
    ) -> Result<Option<ContinuationStep>, Fault> {
        self.stage = Stage::Visit;
        let mut produced = runtime.root_slot(value);
        while let Some(top) = self.tasks.pop() {
            match top {
                Task::FinishCons {
                    node,
                    car_result: None,
                } => {
                    let cdr = runtime.heap.cons_fields(node.get()).expect("cons frame").1;
                    self.tasks.push(Task::FinishCons {
                        node,
                        car_result: Some(produced),
                    });
                    self.tasks.push(Task::Visit(runtime.root_slot(cdr)));
                    return Ok(None);
                }
                Task::FinishCons {
                    node,
                    car_result: Some(car_result),
                } => {
                    let (old_car, old_cdr) =
                        runtime.heap.cons_fields(node.get()).expect("cons frame");
                    if self.destructive {
                        runtime.heap.set_cons_car(node.get(), car_result.get());
                        runtime.heap.set_cons_cdr(node.get(), produced.get());
                        produced = node;
                    } else if car_result.get() == old_car && produced.get() == old_cdr {
                        produced = node;
                    } else {
                        let cell = runtime
                            .heap
                            .cons(|| (car_result.get(), produced.get()))
                            .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                        produced = runtime.root_slot(cell);
                    }
                }
                Task::Visit(_) => unreachable!("visit frames are popped before result reduction"),
            }
        }
        Ok(Some(ContinuationStep::Done(produced.get())))
    }

    fn after_test(
        &mut self,
        runtime: &Runtime,
        replacement: Option<Word>,
        on_miss: Stage,
    ) -> Result<Option<ContinuationStep>, Fault> {
        if let Some(replacement) = replacement {
            self.finish_node(runtime, replacement)
        } else {
            self.stage = on_miss;
            Ok(None)
        }
    }

    fn test_node(
        &mut self,
        runtime: &Runtime,
        keyed: Word,
    ) -> Result<Option<ContinuationStep>, Fault> {
        match &self.rules {
            Rules::Item {
                matcher,
                replacement,
            } => match matcher.compare(runtime, keyed, self.at) {
                Comparison::Known(matched) => {
                    self.after_test(runtime, matched.then(|| replacement.get()), Stage::Descend)
                }
                Comparison::Call(step) => {
                    self.stage = Stage::AfterItemTest;
                    Ok(Some(step))
                }
            },
            Rules::Alist { .. } => {
                self.state.set(KEYED, keyed);
                self.stage = Stage::TryAlist(0);
                Ok(None)
            }
        }
    }

    fn step(&mut self, runtime: &Runtime, values: &Values) -> Result<ContinuationStep, Fault> {
        loop {
            let step = match self.stage {
                Stage::Visit => {
                    let Some(Task::Visit(node)) = self.tasks.pop() else {
                        unreachable!("visit stage expects a node");
                    };
                    let word = node.get();
                    self.state.set(NODE, word);
                    if let Some(step) = self.key.apply(runtime, word, self.at) {
                        self.stage = Stage::AfterKey;
                        return Ok(step);
                    }
                    self.test_node(runtime, word)?
                }
                Stage::AfterKey => self.test_node(runtime, values.get(0))?,
                Stage::AfterItemTest => {
                    let Rules::Item {
                        matcher,
                        replacement,
                    } = &self.rules
                    else {
                        unreachable!("item test uses the item rule");
                    };
                    let matched = matcher.returned_match(values).then(|| replacement.get());
                    self.after_test(runtime, matched, Stage::Descend)?
                }
                Stage::TryAlist(index) => {
                    let Rules::Alist { test, entries } = &self.rules else {
                        unreachable!("association scan uses the alist rule");
                    };
                    let Some((key, replacement)) = entries.get(index) else {
                        self.stage = Stage::Descend;
                        continue;
                    };
                    match test.compare(runtime, self.state.get(KEYED), key.get(), self.at) {
                        Comparison::Known(matched) => self.after_test(
                            runtime,
                            matched.then(|| replacement.get()),
                            Stage::TryAlist(index + 1),
                        )?,
                        Comparison::Call(step) => {
                            self.stage = Stage::AfterAlistTest(index);
                            return Ok(step);
                        }
                    }
                }
                Stage::AfterAlistTest(index) => {
                    let Rules::Alist { test, entries } = &self.rules else {
                        unreachable!("association test uses the alist rule");
                    };
                    let matched = test.returned_match(values).then(|| entries[index].1.get());
                    self.after_test(runtime, matched, Stage::TryAlist(index + 1))?
                }
                Stage::Descend => {
                    let node = self.state.get(NODE);
                    if let Some((car, _)) = runtime.heap.cons_fields(node) {
                        self.tasks.push(Task::FinishCons {
                            node: runtime.root_slot(node),
                            car_result: None,
                        });
                        self.tasks.push(Task::Visit(runtime.root_slot(car)));
                        self.stage = Stage::Visit;
                        None
                    } else {
                        self.finish_node(runtime, node)?
                    }
                }
            };
            if let Some(step) = step {
                return Ok(step);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn substitution_reloads_cons_fields_after_moving_collection() {
        let mut config = Heap::builder();
        config.stress_bytes = Some(1);
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let original = runtime.root(runtime.heap.string("original car").unwrap());
        let replacement = runtime.root(runtime.heap.string("replacement car").unwrap());
        let tail = runtime.root(runtime.heap.string("unchanged cdr").unwrap());
        let tree = runtime.cons(&original, &tail).unwrap();

        let args = runtime.root_frame(3);
        args.set(0, replacement.slot.get());
        args.set(1, original.slot.get());
        args.set(2, tree.slot.get());
        let mut operation = TreeOperation::new(
            &runtime,
            &args,
            Op::Subst {
                destructive: false,
                if_pred: None,
            },
            0..3,
            Resume::Continuation,
            0,
        )
        .expect("SUBST construction failed");
        let ContinuationStep::Done(result) = operation
            .step(&runtime, &Values::new(&runtime), false)
            .expect("SUBST failed under collection stress")
        else {
            panic!("default SUBST should not need a Lisp callback");
        };
        assert_eq!(
            runtime.heap.cons_fields(result),
            Some((replacement.slot.get(), tail.slot.get()))
        );
        assert_eq!(
            runtime.heap.cons_fields(tree.slot.get()),
            Some((original.slot.get(), tail.slot.get()))
        );
    }
}
