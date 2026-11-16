use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, InstBuilder, MemFlags, StackSlotData, StackSlotKind,
};
use cranelift_codegen::isa::TargetIsa;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use rustc_hash::FxHashMap;
use zkcl_opt::BlockId;

use super::{
    BackedgePollState, LoweringConfig, LoweringContext, LoweringError, RootFrameHandle,
    RuntimeCallEmitter, ValueBinding, repr_to_clif_type, word_const,
};
use zkcl_opt::native::{
    JIT_RETURN_TAG_FAULT, NATIVE_ENTRY_PARAMS, RuntimeAbiFunction, WORD_NIL,
    jit_call_context_offsets, jit_return_layout, tail_call,
};

/// Lower one [`SsaFunction`](zkcl_opt::SsaFunction) into a Cranelift [`Function`] using the unified out-pointer C-ABI:
/// `fn(ctx: *mut JitCallContext, target: usize, args_ptr: *const usize, argc: usize, out_ret: *mut JitReturn)`.
pub fn lower_ssa_function<E: RuntimeCallEmitter>(
    config: &LoweringConfig<'_>,
    isa: &dyn TargetIsa,
    fn_builder_ctx: &mut FunctionBuilderContext,
    emitter: &mut E,
) -> Result<Function, LoweringError> {
    let mut func = Function::new();
    lower_ssa_function_into(config, isa, fn_builder_ctx, emitter, &mut func)?;
    Ok(func)
}

/// Lower a prepared function into a cleared Cranelift function.
pub fn lower_ssa_function_into<E: RuntimeCallEmitter>(
    config: &LoweringConfig<'_>,
    isa: &dyn TargetIsa,
    fn_builder_ctx: &mut FunctionBuilderContext,
    emitter: &mut E,
    func: &mut Function,
) -> Result<(), LoweringError> {
    let ssa_function = config.ssa_function;
    let frame_layout = &config.frame_layout;

    let fixnum_bounds = zkcl_target::fixnum_bounds(isa.frontend_config().pointer_width);
    if ssa_function.fixnum_bounds != fixnum_bounds {
        return Err(LoweringError::FixnumBounds {
            expected: fixnum_bounds,
            actual: ssa_function.fixnum_bounds,
        });
    }

    let ptr_ty = isa.pointer_type();
    let ptr_bytes = i32::try_from(ptr_ty.bytes())
        .map_err(|_| LoweringError::IntegerOverflow("pointer width"))?;
    let call_conv = isa.default_call_conv();

    func.name = config.func_name.clone();
    func.signature.call_conv = call_conv;
    func.signature
        .params
        .extend(NATIVE_ENTRY_PARAMS.map(|ty| crate::abi::abi_param(ty, ptr_ty)));
    let mut builder = FunctionBuilder::new(func, fn_builder_ctx);

    let clif_entry = builder.create_block();
    builder.append_block_params_for_function_params(clif_entry);
    let entry_fault_block = builder.create_block();
    builder.set_cold_block(entry_fault_block);
    let fault_block = builder.create_block();
    builder.set_cold_block(fault_block);

    let mut block_map: FxHashMap<BlockId, Block> = FxHashMap::default();
    for &bid in frame_layout.reverse_postorder() {
        let b = &ssa_function.blocks[frame_layout.block_index(bid).unwrap()];
        let cb = builder.create_block();
        for p in &b.params {
            builder.append_block_param(cb, repr_to_clif_type(p.repr, ptr_ty));
        }
        block_map.insert(b.id, cb);
    }

    let mut values = vec![ValueBinding::Undefined; ssa_function.next_value_id as usize];
    for &vid in frame_layout.roots().value_slots.keys() {
        values[vid.0 as usize] = ValueBinding::Rooted(builder.declare_var(ptr_ty));
    }

    let return_layout = jit_return_layout(ptr_bytes);
    let ret_slot = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        return_layout.size,
        3,
    ));

    builder.switch_to_block(clif_entry);
    let &[ctx, target, args_ptr, argc, out_ret] = builder.block_params(clif_entry) else {
        unreachable!("native entry has five ABI parameters");
    };
    let body_setup_block = builder.create_block();
    let unwind_block = builder.create_block();

    let mut lowering = LoweringContext {
        ssa_function,
        classified: &config.classified,
        frame_layout,
        emitter,
        ptr_ty,
        ptr_bytes,
        // Cranelift 0.123 does not lower signed-overflow operations on these ISAs.
        portable_signed_overflow: matches!(isa.name(), "riscv64" | "s390x"),
        ctx,
        target,
        args_ptr,
        argc,
        unrooted_argc: None,
        out_ret,
        values,
        root_frame: None,
        ret_slot,
        return_layout,
        fault_block,
        unwind_block,
        block_map,
        backedge_polls: None,
        deoptimization: Default::default(),
        pack_bits_slot: None,
        word_slice_slot: None,
    };

    lowering.emit_prologue(&mut builder, body_setup_block, entry_fault_block)?;
    lowering.emit_body(&mut builder)?;
    lowering.emit_exits(&mut builder)?;

    builder.seal_all_blocks();
    builder.finalize();
    Ok(())
}

impl<E: RuntimeCallEmitter> LoweringContext<'_, E> {
    fn emit_prologue(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        setup: Block,
        entry_fault: Block,
    ) -> Result<(), LoweringError> {
        self.emit_entry_checks(builder, setup, entry_fault);

        // Entry checks report failure before this invocation registers any roots.
        builder.switch_to_block(entry_fault);
        self.emit_return(builder, JIT_RETURN_TAG_FAULT, None);

        builder.switch_to_block(setup);
        self.root_frame = self.emit_root_frame(builder)?;
        let args = self.emit_entry_arguments(builder)?;
        self.prepare_argument_tail(builder)?;
        if self
            .ssa_function
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .any(|inst| matches!(inst.op, zkcl_opt::SsaOp::DebugPoint { .. }))
        {
            self.emitter.emit_runtime_call(
                builder,
                RuntimeAbiFunction::StageDebugArguments,
                &[self.ctx, self.args_ptr, self.argc],
            )?;
            for (register, slot) in self.frame_layout.debug_unwind_registers() {
                let register = builder.ins().iconst(self.ptr_ty, register as i64);
                let slot = builder.ins().iconst(self.ptr_ty, slot as i64);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::MapDebugUnwindRegister,
                    &[self.ctx, register, slot],
                )?;
            }
        }
        self.backedge_polls = self.emit_backedge_poll_inputs(builder);
        builder
            .ins()
            .jump(self.block_map[&self.ssa_function.entry], &args);
        Ok(())
    }

    fn emit_entry_checks(
        &self,
        builder: &mut FunctionBuilder<'_>,
        setup: Block,
        entry_fault: Block,
    ) {
        let arity = self.ssa_function.arity;
        let required = arity.required;
        let argc = self.argc;
        let arity_ok = match arity.maximum {
            Some(max) if max == required => {
                Some(builder.ins().icmp_imm(IntCC::Equal, argc, required as i64))
            }
            maximum => {
                let min_ok = (required > 0).then(|| {
                    builder
                        .ins()
                        .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, argc, required as i64)
                });
                let max_ok = maximum.map(|max| {
                    builder
                        .ins()
                        .icmp_imm(IntCC::UnsignedLessThanOrEqual, argc, max as i64)
                });
                match (min_ok, max_ok) {
                    (Some(min), Some(max)) => Some(builder.ins().band(min, max)),
                    (min, max) => min.or(max),
                }
            }
        };
        // Standalone machine entries can receive a detached context. Any
        // function with a runtime root frame must reject it before allocation.
        let entry_ok = if self.frame_layout.root_slots() != 0 {
            let runtime = builder.ins().load(
                self.ptr_ty,
                MemFlags::trusted(),
                self.ctx,
                jit_call_context_offsets(self.ptr_bytes).runtime,
            );
            let attached = builder.ins().icmp_imm(IntCC::NotEqual, runtime, 0);
            Some(arity_ok.map_or(attached, |arity_ok| builder.ins().band(arity_ok, attached)))
        } else {
            arity_ok
        };
        if let Some(entry_ok) = entry_ok {
            builder.ins().brif(entry_ok, setup, &[], entry_fault, &[]);
        } else {
            builder.ins().jump(setup, &[]);
        }
    }

    fn emit_root_frame(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<Option<RootFrameHandle>, LoweringError> {
        let root_slots = self.frame_layout.root_slots();
        if root_slots == 0 {
            return Ok(None);
        }

        let ptr_ty = self.ptr_ty;
        let mut out_parameter = || {
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                ptr_ty.bytes(),
                3,
            ));
            let address = builder.ins().stack_addr(ptr_ty, slot, 0);
            (slot, address)
        };
        let (slots_slot, slots_out) = out_parameter();
        let (frame_slot, frame_out) = out_parameter();
        let count = builder.ins().iconst(ptr_ty, root_slots as i64);
        self.emitter.emit_runtime_call(
            builder,
            RuntimeAbiFunction::AllocRootFrame,
            &[self.ctx, count, slots_out, frame_out],
        )?;
        let slots_ptr = builder.ins().stack_load(ptr_ty, slots_slot, 0);
        let frame_index = builder.ins().stack_load(ptr_ty, frame_slot, 0);

        builder
            .ins()
            .store(MemFlags::trusted(), self.target, slots_ptr, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), self.target, slots_ptr, self.ptr_bytes);

        let nil = word_const(builder, ptr_ty, WORD_NIL);
        for &vid in self.frame_layout.roots().value_slots.keys() {
            if let ValueBinding::Rooted(var) = self.values[vid.0 as usize] {
                builder.def_var(var, nil);
            }
        }

        Ok(Some(RootFrameHandle {
            slots_ptr,
            frame_index,
        }))
    }

    fn emit_entry_arguments(
        &self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<Vec<BlockArg>, LoweringError> {
        let required = self.ssa_function.arity.required;
        let mut arguments = Vec::with_capacity(required);
        for i in 0..required {
            let offset = i32::try_from(i * self.ptr_bytes_usize())
                .map_err(|_| LoweringError::IntegerOverflow("entry arg offset"))?;
            let argument =
                builder
                    .ins()
                    .load(self.ptr_ty, MemFlags::trusted(), self.args_ptr, offset);
            if let Some(root_frame) = self.root_frame
                && let Some(vid) = self.ssa_function.entry_params.get(i)
                && let Some(&slot) = self.frame_layout.roots().value_slots.get(vid)
            {
                let ValueBinding::Rooted(var) = self.values[vid.0 as usize] else {
                    unreachable!("root layout entries are declared as variables");
                };
                builder.def_var(var, argument);
                let offset = i32::try_from(slot.0 * self.ptr_bytes_usize())
                    .map_err(|_| LoweringError::IntegerOverflow("entry arg root slot"))?;
                builder
                    .ins()
                    .store(MemFlags::trusted(), argument, root_frame.slots_ptr, offset);
            }
            arguments.push(BlockArg::Value(argument));
        }
        Ok(arguments)
    }

    fn emit_backedge_poll_inputs(
        &self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Option<BackedgePollState> {
        if !self.frame_layout.needs_execution_polls()
            && !self.ssa_function.blocks.iter().any(|block| {
                tail_call(self.ssa_function, self.frame_layout, block)
                    .and_then(|call| call.fixed_arguments(self.ssa_function.arity))
                    .is_some()
            })
        {
            return None;
        }

        // Each loop check reloads the flag and budget through these invocation-local pointers.
        let context = jit_call_context_offsets(self.ptr_bytes);
        let cancel_flag = builder.ins().load(
            self.ptr_ty,
            MemFlags::trusted(),
            self.ctx,
            context.cancel_flag,
        );
        let remaining_steps = builder.ins().load(
            self.ptr_ty,
            MemFlags::trusted(),
            self.ctx,
            context.remaining_steps,
        );
        let slice_steps = builder.ins().load(
            self.ptr_ty,
            MemFlags::trusted(),
            self.ctx,
            context.slice_steps,
        );
        let budgets = builder.ins().bor(remaining_steps, slice_steps);
        let has_budget = builder.ins().icmp_imm(IntCC::NotEqual, budgets, 0);
        Some(BackedgePollState {
            cancel_flag,
            remaining_steps,
            slice_steps,
            has_budget,
        })
    }

    fn emit_body(&mut self, builder: &mut FunctionBuilder<'_>) -> Result<(), LoweringError> {
        let frame_layout = self.frame_layout;
        for &bid in frame_layout.reverse_postorder() {
            let block_index = frame_layout.block_index(bid).unwrap();
            let block = &self.ssa_function.blocks[block_index];
            let clif_block = self.block_map[&bid];
            builder.switch_to_block(clif_block);
            let params = builder.block_params(clif_block).to_vec();
            for (param, value) in block.params.iter().zip(params) {
                self.def_val(builder, Some(param.value), value);
            }
            let tail = tail_call(self.ssa_function, frame_layout, block);
            let body_len = block.insts.len() - usize::from(tail.is_some());
            for (index, inst, live) in frame_layout.instructions(block).take(body_len) {
                for &(id, slot) in live {
                    self.spill_root(builder, id, slot);
                }
                self.emit_inst(builder, block_index, bid, index, inst, live)?;
                // Exceptional returns branch to the separate unwind/fault exits.
                for &(id, slot) in live {
                    self.reload_root(builder, id, slot);
                }
            }
            if let Some(tail) = tail {
                self.emit_tail_call(builder, tail)?;
            } else {
                if let Some(pc) = frame_layout.terminal_poll_pc(bid) {
                    let polls = self.backedge_polls.expect("terminal execution poll");
                    self.emit_backedge_safepoint_poll(builder, pc, polls, None)?;
                }
                self.emit_terminator(builder, &block.terminator)?;
            }
        }
        Ok(())
    }

    fn emit_exits(&mut self, builder: &mut FunctionBuilder<'_>) -> Result<(), LoweringError> {
        builder.switch_to_block(self.fault_block);
        self.emit_drop_root_frame(builder)?;
        self.emit_return(builder, JIT_RETURN_TAG_FAULT, None);

        builder.switch_to_block(self.unwind_block);
        if let Some(root_frame) = self.root_frame {
            // A debugger return unwinds this activation's cleanup scopes first.
            // Only the target activation converts its sentinel into normal values.
            let (target_frame, state) = self.load_unwind_target(builder);
            let own = builder
                .ins()
                .icmp(IntCC::Equal, target_frame, root_frame.frame_index);
            let returned = builder.ins().icmp_imm(IntCC::Equal, state, -1);
            let returned = builder.ins().band(own, returned);
            let finish = builder.create_block();
            let unwind = builder.create_block();
            builder.ins().brif(returned, finish, &[], unwind, &[]);
            builder.switch_to_block(finish);
            self.emit_drop_root_frame(builder)?;
            self.emit_return(
                builder,
                zkcl_bytecode::native_abi::JIT_RETURN_TAG_VALUES,
                None,
            );
            builder.switch_to_block(unwind);
        }
        if let Some(root_frame) = self.root_frame
            && !self.frame_layout.unwind_table().targets().is_empty()
        {
            self.emit_local_unwind_dispatch(builder, root_frame)?;
        }

        // An unwind to an outer frame exits with its original return record.
        self.emit_drop_root_frame(builder)?;
        self.emit_copy_ret_slot_to_out(builder);
        builder.ins().return_(&[]);
        Ok(())
    }

    fn emit_local_unwind_dispatch(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        root_frame: RootFrameHandle,
    ) -> Result<(), LoweringError> {
        let (target_frame, _) = self.load_unwind_target(builder);
        let is_local = builder
            .ins()
            .icmp(IntCC::Equal, target_frame, root_frame.frame_index);
        let local = builder.create_block();
        let outer = builder.create_block();
        builder.ins().brif(is_local, local, &[], outer, &[]);

        builder.switch_to_block(local);
        // A bytecode transfer can name this frame without a native landing.
        let pending = self.emitter.emit_runtime_call(
            builder,
            RuntimeAbiFunction::PendingUnwindState,
            &[self.ctx, root_frame.frame_index],
        )?;
        let state = builder.inst_results(pending)[0];
        let has_landing = builder.ins().icmp_imm(IntCC::NotEqual, state, 0);
        let dispatch = builder.create_block();
        builder.ins().brif(has_landing, dispatch, &[], outer, &[]);
        builder.switch_to_block(dispatch);
        // A cleanup can have moved any rooted object before reaching this dispatch.
        for (&vid, &slot) in &self.frame_layout.roots().value_slots {
            self.reload_root(builder, vid, slot);
        }
        for (index, target) in self
            .frame_layout
            .unwind_table()
            .targets()
            .iter()
            .enumerate()
        {
            let matched = builder.create_block();
            let next = builder.create_block();
            let is_match = builder
                .ins()
                .icmp_imm(IntCC::Equal, state, index as i64 + 1);
            builder.ins().brif(is_match, matched, &[], next, &[]);

            builder.switch_to_block(matched);
            let block = &self.ssa_function.blocks[self.frame_layout.block_index(*target).unwrap()];
            let parameters = self.frame_layout.unwind_parameters(block);
            let mut args = Vec::with_capacity(parameters.len());
            for (_, slot) in parameters {
                let offset = i32::try_from(slot * self.ptr_bytes_usize())
                    .map_err(|_| LoweringError::IntegerOverflow("unwind register offset"))?;
                let value = builder.ins().load(
                    self.ptr_ty,
                    MemFlags::trusted(),
                    root_frame.slots_ptr,
                    offset,
                );
                args.push(BlockArg::Value(value));
            }
            builder.ins().jump(self.block_map[target], &args);

            builder.switch_to_block(next);
        }
        builder.ins().jump(outer, &[]);
        builder.switch_to_block(outer);
        Ok(())
    }
}
