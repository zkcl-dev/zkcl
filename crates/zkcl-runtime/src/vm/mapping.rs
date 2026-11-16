use std::ops::Range;

use super::{Continuation, ContinuationStep, Resume, Values, designator, list};
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, SpineCycle, Word};
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Mapcar,
    Mapc,
    Maplist,
    Mapl,
    Mapcan,
    Mapcon,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAPCAR", Op::Mapcar),
    ("MAPC", Op::Mapc),
    ("MAPLIST", Op::Maplist),
    ("MAPL", Op::Mapl),
    ("MAPCAN", Op::Mapcan),
    ("MAPCON", Op::Mapcon),
];

enum Output {
    Original(RootSlot),
    List(list::ListBuilder),
    Concatenate(Vec<RootSlot>),
}

pub(super) struct Mapping {
    function: RootSlot,
    cursors: RootFrame,
    checkpoints: RootFrame,
    cycles: Vec<Option<SpineCycle>>,
    count: usize,
    use_tails: bool,
    output: Output,
    resume: Resume,
    at: usize,
}

impl Mapping {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        op: Op,
        mut args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let function = designator(runtime, frame.get(args.next().unwrap()), at)?;
        let count = args.len();
        let cursors = runtime.root_frame(count);
        let checkpoints = runtime.root_frame(count);
        let mut cycles = Vec::with_capacity(count);
        for (index, reg) in args.enumerate() {
            let list = frame.get(reg);
            if list != Word::NIL && runtime.heap.cons_fields(list).is_none() {
                return Err(list_fault(runtime, list, "expected a proper list", at));
            }
            cursors.set(index, list);
            checkpoints.set(index, list);
            cycles.push(Some(SpineCycle::new()));
        }
        let output = match op {
            Op::Mapc | Op::Mapl => Output::Original(runtime.root_slot(cursors.get(0))),
            Op::Mapcar | Op::Maplist => Output::List(list::ListBuilder::new(runtime)),
            Op::Mapcan | Op::Mapcon => Output::Concatenate(Vec::new()),
        };
        Ok(Self {
            function: runtime.root_slot(function),
            cursors,
            checkpoints,
            cycles,
            count,
            use_tails: matches!(op, Op::Maplist | Op::Mapl | Op::Mapcon),
            output,
            resume,
            at,
        })
    }

    fn next_arguments(&mut self, runtime: &Runtime) -> Result<Option<RootFrame>, Fault> {
        if (0..self.count).any(|index| self.cursors.get(index) == Word::NIL) {
            return Ok(None);
        }

        let arguments = runtime.root_frame(self.count);
        let mut detected_cycle = false;
        for (index, cycle) in self.cycles.iter_mut().enumerate() {
            let cursor = self.cursors.get(index);
            let (car, cdr) = runtime
                .heap
                .cons_fields(cursor)
                .ok_or_else(|| list_fault(runtime, cursor, "expected a proper list", self.at))?;
            arguments.set(index, if self.use_tails { cursor } else { car });
            self.cursors.set(index, cdr);

            // Each checkpoint remains rooted across calls to the mapper.
            if let Some(detector) = cycle {
                match detector.step(cdr, self.checkpoints.get(index)) {
                    Ok(true) => self.checkpoints.set(index, cdr),
                    Ok(false) => {}
                    Err(_) => {
                        *cycle = None;
                        self.checkpoints.set(index, Word::NIL);
                        detected_cycle = true;
                    }
                }
            }
        }
        // A finite input can bound mapping over a circular input.
        if detected_cycle && self.cycles.iter().all(Option::is_none) {
            return Err(Fault::type_error(runtime, 
                self.cursors.get(0),
                "LIST",
                EvalError::new(self.at, "list mapping requires at least one finite input"),
            ));
        }
        Ok(Some(arguments))
    }

    fn collect(&mut self, runtime: &Runtime, values: &Values) -> Result<(), EvalError> {
        match &mut self.output {
            Output::Original(_) => {}
            Output::List(list) => list.push(runtime, || values.get(0), self.at)?,
            // Defer NCONC until every callback returns. Subsequent callbacks
            // can still observe the lists returned by the earlier callbacks.
            Output::Concatenate(lists) => lists.push(runtime.root_slot(values.get(0))),
        }
        Ok(())
    }

    fn result(&self, runtime: &Runtime) -> Result<Word, Fault> {
        match &self.output {
            Output::Original(list) => Ok(list.get()),
            Output::List(list) => Ok(list.head()),
            Output::Concatenate(lists) => {
                list::nconc(runtime, lists.iter().map(RootSlot::get), self.at)
            }
        }
    }
}

fn list_fault(runtime: &Runtime, datum: Word, message: &str, at: usize) -> Fault {
    Fault::type_error(runtime, 
        list::list_tail(runtime, datum),
        "LIST",
        EvalError::new(at, message),
    )
}

impl Continuation for Mapping {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if returned {
            self.collect(runtime, values)?;
        }
        if let Some(args) = self.next_arguments(runtime)? {
            Ok(ContinuationStep::Call {
                target: self.function.get(),
                args,
                at: self.at,
            })
        } else {
            self.result(runtime).map(ContinuationStep::Done)
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
