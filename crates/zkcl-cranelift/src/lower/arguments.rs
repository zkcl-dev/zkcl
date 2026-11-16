//! Save the optional arguments or argument tail before entering the Lisp body.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{BlockArg, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use zkcl_opt::native::{NativeArguments, RuntimeAbiFunction, WORD_UNBOUND};

use super::{LoweringContext, LoweringError, RuntimeCallEmitter, word_const};

impl<E: RuntimeCallEmitter> LoweringContext<'_, E> {
    pub(super) fn prepare_argument_tail(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<(), LoweringError> {
        match self.frame_layout.arguments() {
            NativeArguments::Unused => {}
            NativeArguments::UnrootedOptionals => {
                let argc = builder.declare_var(self.ptr_ty);
                builder.def_var(argc, self.argc);
                self.unrooted_argc = Some(argc);
            }
            NativeArguments::RootedOptionals(slots) => {
                for (index, slot) in slots.iter() {
                    let argument = self.load_optional_argument(builder, self.argc, index)?;
                    self.store_argument_root(builder, slot, argument)?;
                }
            }
            NativeArguments::RuntimeTail => {
                let required = self.ssa_function.arity.required;
                let offset = self.word_offset(required, "argument offset")?;
                let tail = builder.ins().iadd_imm(self.args_ptr, i64::from(offset));
                let count = builder.ins().iadd_imm(self.argc, -(required as i64));
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::StageArguments,
                    &[self.ctx, self.argument_frame(), tail, count],
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn local_optional_argument(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        index: usize,
    ) -> Result<Option<Value>, LoweringError> {
        match self.frame_layout.arguments() {
            NativeArguments::Unused => unreachable!("no optional argument instructions"),
            NativeArguments::UnrootedOptionals => {
                let argc = builder.use_var(self.unrooted_argc.expect("live argument count"));
                self.load_optional_argument(builder, argc, index).map(Some)
            }
            NativeArguments::RootedOptionals(slots) => {
                let frame = self.root_frame.expect("rooted optional arguments");
                let offset = self.word_offset(slots.slot_for(index), "argument offset")?;
                Ok(Some(builder.ins().load(
                    self.ptr_ty,
                    MemFlags::trusted(),
                    frame.slots_ptr,
                    offset,
                )))
            }
            NativeArguments::RuntimeTail => Ok(None),
        }
    }

    /// Returns false when the shared runtime release call must be emitted.
    pub(super) fn release_local_arguments(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<bool, LoweringError> {
        match self.frame_layout.arguments() {
            NativeArguments::Unused => {}
            NativeArguments::UnrootedOptionals => {
                let zero = word_const(builder, self.ptr_ty, 0);
                builder.def_var(self.unrooted_argc.expect("live argument count"), zero);
            }
            NativeArguments::RootedOptionals(slots) => {
                let unbound = word_const(builder, self.ptr_ty, WORD_UNBOUND);
                for slot in slots.slots() {
                    self.store_argument_root(builder, slot, unbound)?;
                }
            }
            NativeArguments::RuntimeTail => return Ok(false),
        }
        Ok(true)
    }

    fn argument_frame(&self) -> Value {
        self.root_frame.expect("runtime argument tail").frame_index
    }

    fn store_argument_root(
        &self,
        builder: &mut FunctionBuilder<'_>,
        slot: usize,
        value: Value,
    ) -> Result<(), LoweringError> {
        let frame = self.root_frame.expect("rooted optional arguments");
        let offset = self.word_offset(slot, "argument offset")?;
        builder
            .ins()
            .store(MemFlags::trusted(), value, frame.slots_ptr, offset);
        Ok(())
    }

    fn load_optional_argument(
        &self,
        builder: &mut FunctionBuilder<'_>,
        argc: Value,
        index: usize,
    ) -> Result<Value, LoweringError> {
        let position = self.ssa_function.arity.required + index;
        let offset = self.word_offset(position, "argument offset")?;
        let supplied = builder
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, argc, position as i64);
        let present = builder.create_block();
        let absent = builder.create_block();
        let join = builder.create_block();
        let argument = builder.append_block_param(join, self.ptr_ty);
        builder.ins().brif(supplied, present, &[], absent, &[]);

        builder.switch_to_block(present);
        let word = builder
            .ins()
            .load(self.ptr_ty, MemFlags::trusted(), self.args_ptr, offset);
        builder.ins().jump(join, &[BlockArg::Value(word)]);

        builder.switch_to_block(absent);
        let unbound = word_const(builder, self.ptr_ty, WORD_UNBOUND);
        builder.ins().jump(join, &[BlockArg::Value(unbound)]);

        builder.switch_to_block(join);
        Ok(argument)
    }
}
