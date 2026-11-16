//! Temporary arrays borrowed by runtime calls during this native activation.

use std::cell::Cell;

use inkwell::types::{BasicType, BasicTypeEnum};
use inkwell::values::{BasicValue, PointerValue};

use super::{Function, ModuleCodegen, Result};

pub(super) struct CallScratch<'ctx> {
    pub words: ScratchArray<'ctx>,
    /// Deoptimization passes register indices alongside restored words.
    pub registers: ScratchArray<'ctx>,
    pub restore_scalars: ScratchArray<'ctx>,
    pub restore_formats: ScratchArray<'ctx>,
    pub packs: ScratchArray<'ctx>,
    /// Handler registration passes condition slots alongside the state numbers.
    pub handler_slots: ScratchArray<'ctx>,
}

impl<'ctx> CallScratch<'ctx> {
    pub(super) fn new(ir: &ModuleCodegen<'ctx>) -> Self {
        Self {
            words: ScratchArray::new(ir.word, "call_words"),
            registers: ScratchArray::new(ir.context.i32_type(), "restore_registers"),
            restore_scalars: ScratchArray::new(ir.context.i64_type(), "restore_scalars"),
            restore_formats: ScratchArray::new(ir.context.i8_type(), "restore_formats"),
            packs: ScratchArray::new(ir.pack_type(), "packs"),
            handler_slots: ScratchArray::new(ir.word, "handler_slots"),
        }
    }
}

pub(super) struct ScratchArray<'ctx> {
    element: BasicTypeEnum<'ctx>,
    name: &'static str,
    allocation: Cell<Option<(PointerValue<'ctx>, usize)>>,
}

impl<'ctx> ScratchArray<'ctx> {
    fn new(element: impl BasicType<'ctx>, name: &'static str) -> Self {
        Self {
            element: element.as_basic_type_enum(),
            name,
            allocation: Cell::new(None),
        }
    }

    /// The entry allocation grows to its largest use.
    pub(super) fn reserve(
        &self,
        function: &Function<'_, 'ctx>,
        len: usize,
    ) -> Result<PointerValue<'ctx>> {
        if len == 0 {
            return Ok(function.ir.ptr.const_null());
        }
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
        Ok(pointer)
    }

    /// Stage one borrowed slice in the entry allocation.
    pub(super) fn write(
        &self,
        function: &Function<'_, 'ctx>,
        values: &[impl BasicValue<'ctx> + Copy],
    ) -> Result<PointerValue<'ctx>> {
        let pointer = self.reserve(function, values.len())?;
        for (index, &value) in values.iter().enumerate() {
            let slot = function.element(self.element, pointer, index)?;
            function.builder.build_store(slot, value)?;
        }
        Ok(pointer)
    }
}
