//! Decode the native return record after a runtime call.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, Value, types};
use cranelift_frontend::FunctionBuilder;
use zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_RESUME;
use zkcl_opt::native::{
    JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_UNWIND, JIT_RETURN_TAG_VALUES,
    RuntimeCall,
};
use zkcl_opt::{DeoptState, RootSlot, ValueId};

use super::{LoweringContext, LoweringError, RuntimeCallEmitter};

impl<E: RuntimeCallEmitter> LoweringContext<'_, E> {
    /// Continue a runtime function call, extracting the primary value when requested.
    /// The common single-value result takes one conditional branch.
    pub(super) fn finish_runtime_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        has_result: bool,
    ) -> Result<Option<Value>, LoweringError> {
        self.finish_mapped_call(builder, has_result, None, &[])
    }

    pub(super) fn finish_mapped_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        has_result: bool,
        recovery: Option<&DeoptState>,
        live: &[(ValueId, RootSlot)],
    ) -> Result<Option<Value>, LoweringError> {
        let tag = self.load_return_tag(builder);
        let continuation = builder.create_block();
        let exceptional = builder.create_block();
        let result = if has_result {
            let one = builder.create_block();
            let other = builder.create_block();
            let values = builder.create_block();
            let result = builder.append_block_param(continuation, self.ptr_ty);
            let single = builder
                .ins()
                .icmp_imm(IntCC::Equal, tag, i64::from(JIT_RETURN_TAG_ONE));
            builder.ins().brif(single, one, &[], other, &[]);

            builder.switch_to_block(one);
            let single_value = self.load_return_single_value(builder);
            builder
                .ins()
                .jump(continuation, &[BlockArg::Value(single_value)]);

            builder.switch_to_block(other);
            let multiple =
                builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, i64::from(JIT_RETURN_TAG_VALUES));
            builder.ins().brif(multiple, values, &[], exceptional, &[]);

            builder.switch_to_block(values);
            let call = self.emit_call(builder, &RuntimeCall::read_value(0), None)?;
            let primary = builder.inst_results(call)[0];
            builder
                .ins()
                .jump(continuation, &[BlockArg::Value(primary)]);
            Some(result)
        } else {
            let returned = builder.ins().icmp_imm(
                IntCC::UnsignedLessThan,
                tag,
                i64::from(JIT_RETURN_TAG_UNWIND),
            );
            builder
                .ins()
                .brif(returned, continuation, &[], exceptional, &[]);
            None
        };

        builder.set_cold_block(exceptional);
        builder.switch_to_block(exceptional);
        self.call_handoff(builder, tag, recovery, live)?;
        self.branch_on_return_fault(builder, tag, self.unwind_block);

        builder.switch_to_block(continuation);
        Ok(result)
    }

    pub(super) fn finish_call_preserving_values(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        has_result: bool,
        recovery: Option<&DeoptState>,
        live: &[(ValueId, RootSlot)],
    ) -> Result<Option<Value>, LoweringError> {
        let res = self.finish_mapped_call(builder, has_result, recovery, live)?;
        if res.is_none() {
            self.preserve_single_value(builder)?;
        }
        Ok(res)
    }

    pub(super) fn finish_debug_point(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<(), LoweringError> {
        let tag = self.load_return_tag(builder);
        let next = builder.create_block();
        let exceptional = builder.create_block();
        let returned = builder.ins().icmp_imm(
            IntCC::UnsignedLessThan,
            tag,
            i64::from(JIT_RETURN_TAG_UNWIND),
        );
        builder.ins().brif(returned, next, &[], exceptional, &[]);

        builder.set_cold_block(exceptional);
        builder.switch_to_block(exceptional);
        let resume_vm = builder.create_block();
        let unwind_or_fault = builder.create_block();
        let handed_off = builder
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(JIT_RETURN_TAG_VM_RESUME));
        builder
            .ins()
            .brif(handed_off, resume_vm, &[], unwind_or_fault, &[]);

        builder.switch_to_block(resume_vm);
        // The runtime has already reconstructed the bytecode frame. Forward
        // the handoff directly so its state is never read as an unwind target.
        self.emit_drop_root_frame(builder)?;
        self.emit_copy_ret_slot_to_out(builder);
        builder.ins().return_(&[]);

        builder.switch_to_block(unwind_or_fault);
        self.branch_on_return_fault(builder, tag, self.unwind_block);
        builder.switch_to_block(next);
        Ok(())
    }

    pub(super) fn bind_runtime_call_result(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        result: Option<ValueId>,
    ) -> Result<(), LoweringError> {
        if let Some(value) = self.finish_runtime_call(builder, result.is_some())? {
            self.def_val(builder, result, value);
        }
        Ok(())
    }

    /// Mirror a single-value call result into ExecState values so a following
    /// ReturnDynamicValues observes the callee's values like the bytecode VM does.
    fn preserve_single_value(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<(), LoweringError> {
        let tag = self.load_return_tag(builder);
        let is_values = builder
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(JIT_RETURN_TAG_VALUES));
        let one_block = builder.create_block();
        let done_block = builder.create_block();
        builder
            .ins()
            .brif(is_values, done_block, &[], one_block, &[]);
        builder.switch_to_block(one_block);
        self.emit_call(builder, &RuntimeCall::preserve_single_value(), None)?;
        builder.ins().jump(done_block, &[]);
        builder.switch_to_block(done_block);
        Ok(())
    }

    pub(super) fn load_return_tag(&self, builder: &mut FunctionBuilder<'_>) -> Value {
        builder
            .ins()
            .stack_load(types::I8, self.ret_slot, self.return_layout.tag)
    }

    pub(super) fn load_return_single_value(&self, builder: &mut FunctionBuilder<'_>) -> Value {
        builder
            .ins()
            .stack_load(self.ptr_ty, self.ret_slot, self.return_layout.single_value)
    }

    pub(super) fn load_unwind_target(&self, builder: &mut FunctionBuilder<'_>) -> (Value, Value) {
        let target_frame =
            builder
                .ins()
                .stack_load(self.ptr_ty, self.ret_slot, self.return_layout.target_frame);
        let state = builder
            .ins()
            .stack_load(self.ptr_ty, self.ret_slot, self.return_layout.state);
        (target_frame, state)
    }

    pub(super) fn branch_on_return_fault(
        &self,
        builder: &mut FunctionBuilder<'_>,
        tag: Value,
        otherwise: Block,
    ) {
        let failed = builder
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(JIT_RETURN_TAG_FAULT));
        builder
            .ins()
            .brif(failed, self.fault_block, &[], otherwise, &[]);
    }
}
