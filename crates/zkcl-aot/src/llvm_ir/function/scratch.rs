//! Temporary arrays borrowed by runtime calls during this native activation.

use std::cell::Cell;

use inkwell::types::IntType;
use inkwell::values::{BasicValue, IntValue, PointerValue};

use super::{Function, Result};

pub(super) struct CallScratch<'ctx> {
    pub words: ScratchArray<'ctx>,
    /// Deoptimization passes register indices alongside restored words.
    pub registers: ScratchArray<'ctx>,
    /// Handler registration passes condition slots alongside the state numbers.
    pub handler_slots: ScratchArray<'ctx>,
}

impl<'ctx> CallScratch<'ctx> {
    pub(super) fn new(word: IntType<'ctx>, register: IntType<'ctx>) -> Self {
        Self {
            words: ScratchArray::new(word, "call_words"),
            registers: ScratchArray::new(register, "restore_registers"),
            handler_slots: ScratchArray::new(word, "handler_slots"),
        }
    }
}

pub(super) struct ScratchArray<'ctx> {
    element: IntType<'ctx>,
    name: &'static str,
    allocation: Cell<Option<(PointerValue<'ctx>, usize)>>,
}

impl<'ctx> ScratchArray<'ctx> {
    fn new(element: IntType<'ctx>, name: &'static str) -> Self {
        Self {
            element,
            name,
            allocation: Cell::new(None),
        }
    }

    /// Stage one borrowed slice. The entry allocation grows to its largest use.
    pub(super) fn write(
        &self,
        function: &Function<'_, 'ctx>,
        values: &[IntValue<'ctx>],
    ) -> Result<PointerValue<'ctx>> {
        if values.is_empty() {
            return Ok(function.ir.ptr.const_null());
        }
        let len = values.len();
        let pointer = match self.allocation.get() {
            Some((pointer, capacity)) => {
                if len > capacity {
                    let alloca = pointer
                        .as_instruction_value()
                        .expect("scratch array allocation");
                    // The array length is the only operand of an LLVM alloca.
                    assert!(alloca.set_operand(0, function.word(len)));
                    self.allocation.set(Some((pointer, len)));
                }
                pointer
            }
            None => {
                let pointer = function.allocas.build_array_alloca(
                    self.element,
                    function.word(len),
                    self.name,
                )?;
                self.allocation.set(Some((pointer, len)));
                pointer
            }
        };
        for (index, &value) in values.iter().enumerate() {
            let slot = function.element(self.element, pointer, index)?;
            function.builder.build_store(slot, value)?;
        }
        Ok(pointer)
    }
}
