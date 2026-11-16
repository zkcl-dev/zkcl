use zkcl_bytecode::KeywordTable;

use super::keywords::keyword_arguments;
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, RootStackFrame, Word};
use crate::{EvalError, Runtime};

/// Rooted arguments after the required parameters, plus parsed keyword positions.
#[derive(Default)]
pub(crate) struct ArgumentTail<'rt> {
    slots: Option<RootStackFrame<'rt>>,
    keywords: Vec<Option<usize>>,
}

impl<'rt> ArgumentTail<'rt> {
    pub(crate) fn new(
        runtime: &'rt Runtime,
        count: usize,
        argument: impl Fn(usize) -> Word,
    ) -> Self {
        let slots = (count != 0).then(|| {
            let slots = runtime.alloc_vm_slots(count);
            for index in 0..count {
                slots.set(index, argument(index));
            }
            slots
        });
        Self {
            slots,
            keywords: Vec::new(),
        }
    }

    fn slots(&self) -> &RootSlots {
        match &self.slots {
            Some(slots) => slots,
            None => RootSlots::empty(),
        }
    }

    pub(crate) fn get(&self, index: usize) -> Word {
        let slots = self.slots();
        if index < slots.len() {
            slots.get(index)
        } else {
            Word::UNBOUND
        }
    }

    pub(crate) fn keyword(&self, index: usize, at: usize) -> Result<Word, Fault> {
        let argument = self.keywords.get(index).ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "keyword argument is out of bounds"),
            )
        })?;
        Ok(argument.map_or(Word::UNBOUND, |index| self.slots().get(index)))
    }

    pub(crate) fn check_keywords(
        &mut self,
        runtime: &Runtime,
        table: &KeywordTable,
        constant: impl Fn(usize) -> Word,
        start: usize,
        at: usize,
    ) -> Result<(), Fault> {
        let names: Vec<_> = table.names.iter().map(|&index| constant(index)).collect();
        let slots = self.slots();
        self.keywords = keyword_arguments(
            runtime,
            &names,
            start..slots.len(),
            |index| slots.get(index),
            table.allow_other_keys,
            constant(table.override_key),
        )
        .map_err(|error| error.program_error(at))?;
        Ok(())
    }

    pub(crate) fn rest(
        &self,
        runtime: &Runtime,
        start: usize,
        output: &RootSlots,
        out: usize,
        at: usize,
    ) -> Result<(), EvalError> {
        output.set(out, Word::NIL);
        let arguments = self.slots();
        for index in (start..arguments.len()).rev() {
            let pair = runtime
                .heap
                .cons(|| (arguments.get(index), output.get(out)))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            output.set(out, pair);
        }
        Ok(())
    }
}
