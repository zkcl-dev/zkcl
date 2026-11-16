use std::cell::OnceCell;

use inkwell::IntPredicate;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::types::BasicType;
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, CallSiteValue, FunctionValue, IntValue,
    PhiValue, PointerValue,
};
use rustc_hash::FxHashMap;
use zkcl_bytecode::VopCall;
use zkcl_opt::native::{
    ClassifiedFunction, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_TAIL_CALL,
    JIT_RETURN_TAG_UNWIND, JIT_RETURN_TAG_VALUES, NATIVE_ENTRY_PARAMS, NativeFrameLayout,
    RuntimeAbiFunction, SliceCharge, TailCall, TailCallee, WORD_NIL, tail_call,
};
use zkcl_opt::{
    BlockId, BranchTarget, FloatFormat, RootSlot, SsaFunction, Terminator, UnwindOp, ValueId,
    ValueRepr,
};

use super::{ModuleCodegen, Result};
use crate::AotError;

mod arguments;
mod deopt;
mod numeric;
mod ops;
mod scratch;

use scratch::CallScratch;

struct Block<'ctx> {
    block: BasicBlock<'ctx>,
    params: Vec<PhiValue<'ctx>>,
}

#[derive(Clone, Copy)]
struct RootFrame<'ctx> {
    slots: PointerValue<'ctx>,
    index: IntValue<'ctx>,
}

#[derive(Clone, Copy)]
enum ValueBinding<'ctx> {
    Undefined,
    Direct(BasicValueEnum<'ctx>),
    Rooted(PointerValue<'ctx>),
}

struct Function<'a, 'ctx> {
    ir: &'a ModuleCodegen<'ctx>,
    ssa: &'a SsaFunction,
    classified: &'a ClassifiedFunction<'a>,
    layout: &'a NativeFrameLayout,
    vop_calls: &'a [VopCall],
    symbol: &'a str,
    function: FunctionValue<'ctx>,
    builder: Builder<'ctx>,
    allocas: Builder<'ctx>,
    ctx: PointerValue<'ctx>,
    target: IntValue<'ctx>,
    args: PointerValue<'ctx>,
    argc: IntValue<'ctx>,
    unrooted_argc: Option<PointerValue<'ctx>>,
    output: PointerValue<'ctx>,
    result: PointerValue<'ctx>,
    scratch: CallScratch<'ctx>,
    frame: Option<RootFrame<'ctx>>,
    blocks: FxHashMap<BlockId, Block<'ctx>>,
    values: Vec<ValueBinding<'ctx>>,
    fault: OnceCell<BasicBlock<'ctx>>,
    forward: OnceCell<BasicBlock<'ctx>>,
    check_unwind: Option<BasicBlock<'ctx>>,
}

pub(super) fn emit<'ctx>(
    ir: &ModuleCodegen<'ctx>,
    ssa: &SsaFunction,
    classified: &ClassifiedFunction<'_>,
    layout: &NativeFrameLayout,
    symbol: &str,
    vop_calls: &[VopCall],
) -> Result<()> {
    let signature = ir
        .context
        .void_type()
        .fn_type(&NATIVE_ENTRY_PARAMS.map(|ty| ir.abi_type(ty).into()), false);
    let function = ir.module.add_function(symbol, signature, None);
    let mut params = function.get_param_iter();
    let param_names: [&str; NATIVE_ENTRY_PARAMS.len()] =
        ["ctx", "target", "args_ptr", "argc", "sret"];
    let [ctx, target, args, argc, output] = param_names.map(|name| {
        let param = params.next().expect("native entry has five ABI parameters");
        param.set_name(name);
        param
    });
    let entry = ir.context.append_basic_block(function, "entry");
    let builder = ir.context.create_builder();
    builder.position_at_end(entry);
    let result = builder.build_alloca(ir.result, "jit_ret")?;
    // Scratch arrays have fixed sizes and must be allocated once even when used in a loop.
    let allocas = ir.context.create_builder();
    allocas.position_before(&result.as_instruction_value().unwrap());
    let check_unwind =
        (layout.root_slots() != 0).then(|| ir.context.append_basic_block(function, "check_unwind"));
    let mut emitter = Function {
        ir,
        ssa,
        classified,
        layout,
        vop_calls,
        symbol,
        function,
        builder,
        allocas,
        ctx: ctx.into_pointer_value(),
        target: target.into_int_value(),
        args: args.into_pointer_value(),
        argc: argc.into_int_value(),
        unrooted_argc: None,
        output: output.into_pointer_value(),
        result,
        scratch: CallScratch::new(ir),
        fault: OnceCell::new(),
        forward: OnceCell::new(),
        check_unwind,
        frame: None,
        blocks: FxHashMap::default(),
        values: vec![ValueBinding::Undefined; ssa.next_value_id as usize],
    };
    let rpo = layout.reverse_postorder();
    for &id in rpo {
        let ssa_block = &ssa.blocks[layout.block_index(id).unwrap()];
        let block = emitter.new_block(&format!("bb_{}", id.0));
        emitter.builder.position_at_end(block);
        let mut params = Vec::with_capacity(ssa_block.params.len());
        for param in &ssa_block.params {
            let ty = match param.repr {
                ValueRepr::TaggedWord => ir.word.into(),
                ValueRepr::UnboxedI64 | ValueRepr::UnboxedU64 => ir.context.i64_type().into(),
                ValueRepr::Bool => ir.context.bool_type().into(),
                ValueRepr::UnboxedF32 => ir.float_type(FloatFormat::F32).into(),
                ValueRepr::UnboxedF64 => ir.float_type(FloatFormat::F64).into(),
                ValueRepr::UnboxedI32x4 => ir.pack_type().as_basic_type_enum(),
            };
            params.push(
                emitter
                    .builder
                    .build_phi(ty, &format!("p_{}", param.value.0))?,
            );
        }
        emitter.blocks.insert(id, Block { block, params });
    }
    for &id in layout.roots().value_slots.keys() {
        let local = emitter
            .allocas
            .build_alloca(ir.word, &format!("root_var_{}", id.0))?;
        emitter.values[id.0 as usize] = ValueBinding::Rooted(local);
    }
    emitter.builder.position_at_end(entry);
    emitter.emit_entry()?;
    for &id in rpo {
        let block_index = layout.block_index(id).unwrap();
        let block = &ssa.blocks[block_index];
        emitter.builder.position_at_end(emitter.blocks[&id].block);
        for (i, param) in block.params.iter().enumerate() {
            let value = emitter.blocks[&id].params[i].as_basic_value();
            emitter.define(param.value, value)?;
        }
        let tail = tail_call(ssa, layout, block);
        let body_len = block.insts.len() - usize::from(tail.is_some());
        for (i, inst, live) in layout.instructions(block).take(body_len) {
            emitter.spill_roots(live)?;
            let result = emitter.emit_inst(block_index, id, i, inst, live)?;
            emitter.reload_roots(live)?;
            if let (Some(id), Some(value)) = (inst.id, result) {
                emitter.define(id, value)?;
            }
        }
        if let Some(pc) = layout.terminal_poll_pc(id) {
            emitter.poll_backedge(pc)?;
        }
        if let Some(tail) = tail {
            emitter.emit_tail_call(tail)?;
        } else {
            emitter.terminate(&block.terminator)?;
        }
    }
    if let Some(check) = check_unwind {
        emitter.builder.position_at_end(check);
        let unwind = emitter.builder.build_int_compare(
            IntPredicate::EQ,
            emitter.result_tag()?,
            emitter.tag(JIT_RETURN_TAG_UNWIND),
            "",
        )?;
        let dispatch = emitter.new_block("unwind_dispatch");
        emitter
            .builder
            .build_conditional_branch(unwind, dispatch, emitter.forward_block())?;
        emitter.builder.position_at_end(dispatch);
        emitter.dispatch_unwind()?;
    }
    if let Some(&forward) = emitter.forward.get() {
        emitter.builder.position_at_end(forward);
        emitter.forward_result()?;
    }
    if let Some(&fault) = emitter.fault.get() {
        emitter.builder.position_at_end(fault);
        emitter.return_result(JIT_RETURN_TAG_FAULT, emitter.word(0))?;
    }
    Ok(())
}

impl<'ctx> Function<'_, 'ctx> {
    fn error(&self, reason: impl Into<String>) -> AotError {
        AotError::LlvmLowering {
            symbol: self.symbol.to_owned(),
            reason: reason.into(),
        }
    }

    fn new_block(&self, name: &str) -> BasicBlock<'ctx> {
        self.ir.context.append_basic_block(self.function, name)
    }

    fn forward_block(&self) -> BasicBlock<'ctx> {
        *self
            .forward
            .get_or_init(|| self.new_block("forward_result"))
    }

    fn exceptional_block(&self) -> BasicBlock<'ctx> {
        self.check_unwind.unwrap_or_else(|| self.forward_block())
    }

    fn current_block(&self) -> BasicBlock<'ctx> {
        self.builder
            .get_insert_block()
            .expect("emitting a function block")
    }

    fn word(&self, value: usize) -> IntValue<'ctx> {
        self.ir.word.const_int(value as u64, false)
    }

    fn boolean(&self, value: bool) -> IntValue<'ctx> {
        self.ir.context.bool_type().const_int(value as u64, false)
    }

    fn tag(&self, value: u8) -> IntValue<'ctx> {
        self.ir.context.i8_type().const_int(value.into(), false)
    }

    fn call(
        &self,
        runtime_function: RuntimeAbiFunction,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Result<CallSiteValue<'ctx>> {
        let function = self.ir.runtime_functions[runtime_function as usize];
        let call = self.builder.build_call(function, args, "")?;
        self.ir
            .add_bool_attributes(runtime_function, |location, attribute| {
                call.add_attribute(location, attribute);
            });
        Ok(call)
    }

    fn call_word(
        &self,
        runtime_function: RuntimeAbiFunction,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Result<IntValue<'ctx>> {
        Ok(self
            .call(runtime_function, args)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value())
    }

    fn call_one(
        &self,
        runtime_function: RuntimeAbiFunction,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Result<IntValue<'ctx>> {
        self.call(runtime_function, args)?;
        self.successful_result()?;
        self.single_value()
    }

    fn choose_int(
        &self,
        cond: IntValue<'ctx>,
        yes: impl FnOnce() -> Result<IntValue<'ctx>>,
        no: impl FnOnce() -> Result<IntValue<'ctx>>,
    ) -> Result<IntValue<'ctx>> {
        let yes_block = self.new_block("then");
        let no_block = self.new_block("else");
        let merge = self.new_block("merge");
        self.builder
            .build_conditional_branch(cond, yes_block, no_block)?;
        self.builder.position_at_end(yes_block);
        let left = yes()?;
        let left_pred = self.current_block();
        self.builder.build_unconditional_branch(merge)?;
        self.builder.position_at_end(no_block);
        let right = no()?;
        let right_pred = self.current_block();
        self.builder.build_unconditional_branch(merge)?;
        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(left.get_type(), "")?;
        phi.add_incoming(&[(&left, left_pred), (&right, right_pred)]);
        Ok(phi.as_basic_value().into_int_value())
    }

    fn element(
        &self,
        ty: impl BasicType<'ctx>,
        base: PointerValue<'ctx>,
        index: usize,
    ) -> Result<PointerValue<'ctx>> {
        // The caller owns the array or has checked this argument index against argc.
        Ok(unsafe {
            self.builder
                .build_in_bounds_gep(ty, base, &[self.word(index)], "")?
        })
    }

    fn load_word(&self, ptr: PointerValue<'ctx>) -> Result<IntValue<'ctx>> {
        Ok(self
            .builder
            .build_load(self.ir.word, ptr, "")?
            .into_int_value())
    }

    fn basic_value(&self, id: ValueId) -> Result<BasicValueEnum<'ctx>> {
        match self.values[id.0 as usize] {
            ValueBinding::Direct(value) => Ok(value),
            ValueBinding::Rooted(local) => self.load_word(local).map(Into::into),
            ValueBinding::Undefined => Err(self.error(format!("undefined SSA value {}", id.0))),
        }
    }

    fn value(&self, id: ValueId) -> Result<IntValue<'ctx>> {
        Ok(self.basic_value(id)?.into_int_value())
    }

    fn operands(&self, ids: &[ValueId]) -> Result<Vec<IntValue<'ctx>>> {
        ids.iter().map(|&id| self.value(id)).collect()
    }

    fn edge_values(&self, ids: &[ValueId]) -> Result<Vec<BasicValueEnum<'ctx>>> {
        ids.iter().map(|&id| self.basic_value(id)).collect()
    }

    fn define(&mut self, id: ValueId, value: BasicValueEnum<'ctx>) -> Result<()> {
        let binding = &mut self.values[id.0 as usize];
        match *binding {
            ValueBinding::Rooted(local) => {
                self.builder.build_store(local, value)?;
            }
            _ => *binding = ValueBinding::Direct(value),
        }
        Ok(())
    }

    fn root(&self, slot: usize) -> Result<PointerValue<'ctx>> {
        self.element(
            self.ir.word,
            self.frame.expect("root slot in native frame").slots,
            slot,
        )
    }

    fn spill_roots(&self, live: &[(ValueId, RootSlot)]) -> Result<()> {
        for &(id, slot) in live {
            self.builder
                .build_store(self.root(slot.0)?, self.value(id)?)?;
        }
        Ok(())
    }

    fn reload_roots(&mut self, live: &[(ValueId, RootSlot)]) -> Result<()> {
        for &(id, slot) in live {
            self.define(id, self.load_word(self.root(slot.0)?)?.into())?;
        }
        Ok(())
    }

    fn target_word(&self, slot: usize) -> Result<IntValue<'ctx>> {
        match self.frame {
            Some(_) => self.load_word(self.root(slot)?),
            None => Ok(self.target),
        }
    }

    fn frame_index(&self) -> IntValue<'ctx> {
        self.frame.map_or_else(|| self.word(0), |frame| frame.index)
    }

    fn drop_frame(&self) -> Result<()> {
        if let Some(frame) = self.frame {
            self.call(
                RuntimeAbiFunction::DropRootFrame,
                &[self.ctx.into(), frame.index.into()],
            )?;
        }
        Ok(())
    }

    fn emit_entry(&mut self) -> Result<()> {
        let arity = self.ssa.arity;
        if arity.required != 0 || arity.maximum.is_some() {
            let mut valid = self.builder.build_int_compare(
                IntPredicate::UGE,
                self.argc,
                self.word(arity.required),
                "",
            )?;
            if let Some(max) = arity.maximum {
                let bounded = self.builder.build_int_compare(
                    IntPredicate::ULE,
                    self.argc,
                    self.word(max),
                    "",
                )?;
                valid = self.builder.build_and(valid, bounded, "")?;
            }
            let ok = self.new_block("args_ok");
            let bad = self.new_block("arity_fault");
            self.builder.build_conditional_branch(valid, ok, bad)?;
            self.builder.position_at_end(bad);
            self.return_result(JIT_RETURN_TAG_FAULT, self.word(0))?;
            self.builder.position_at_end(ok);
        }
        if self.layout.root_slots() != 0 {
            let slots = self.allocas.build_alloca(self.ir.ptr, "root_slots")?;
            let index = self.allocas.build_alloca(self.ir.word, "frame_index")?;
            self.call(
                RuntimeAbiFunction::AllocRootFrame,
                &[
                    self.ctx.into(),
                    self.word(self.layout.root_slots()).into(),
                    slots.into(),
                    index.into(),
                ],
            )?;
            self.frame = Some(RootFrame {
                slots: self
                    .builder
                    .build_load(self.ir.ptr, slots, "")?
                    .into_pointer_value(),
                index: self.load_word(index)?,
            });
            self.builder
                .build_store(self.root(self.layout.roots().code_slot.0)?, self.target)?;
            self.builder
                .build_store(self.root(self.layout.roots().closure_slot.0)?, self.target)?;
        }
        let entry = self.ssa.entry;
        let params = &self.ssa.blocks[self.layout.block_index(entry).unwrap()].params;
        let mut args = Vec::with_capacity(params.len());
        for (index, param) in params.iter().enumerate() {
            let arg = self.load_word(self.element(self.ir.word, self.args, index)?)?;
            if self.frame.is_some()
                && let Some(slot) = self.layout.roots().value_slots.get(&param.value)
            {
                self.builder.build_store(self.root(slot.0)?, arg)?;
            }
            args.push(match param.repr {
                ValueRepr::TaggedWord => arg.into(),
                ValueRepr::UnboxedI64 => self.unbox_fixnum(arg)?.into(),
                _ => return Err(self.error("unsupported native entry argument representation")),
            });
        }
        self.prepare_argument_tail()?;
        if self
            .ssa
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .any(|inst| matches!(inst.op, zkcl_opt::SsaOp::DebugPoint { .. }))
        {
            self.call(
                RuntimeAbiFunction::StageDebugArguments,
                &[self.ctx.into(), self.args.into(), self.argc.into()],
            )?;
            for (register, slot) in self.layout.debug_unwind_registers() {
                self.call(
                    RuntimeAbiFunction::MapDebugUnwindRegister,
                    &[
                        self.ctx.into(),
                        self.word(register).into(),
                        self.word(slot).into(),
                    ],
                )?;
            }
        }
        self.jump(entry, &args)
    }

    fn incoming(&self, target: BlockId, args: &[BasicValueEnum<'ctx>]) -> Result<BasicBlock<'ctx>> {
        let block = &self.blocks[&target];
        if args.len() != block.params.len() {
            return Err(self.error(format!(
                "block {} expects {} arguments, got {}",
                target.0,
                block.params.len(),
                args.len()
            )));
        }
        let pred = self.current_block();
        for (phi, value) in block.params.iter().zip(args) {
            phi.add_incoming(&[(value, pred)]);
        }
        Ok(block.block)
    }

    fn jump(&self, target: BlockId, args: &[BasicValueEnum<'ctx>]) -> Result<()> {
        self.builder
            .build_unconditional_branch(self.incoming(target, args)?)?;
        Ok(())
    }

    fn branch(
        &self,
        condition: IntValue<'ctx>,
        yes: &BranchTarget,
        no: &BranchTarget,
    ) -> Result<()> {
        if yes == no {
            return self.jump(yes.block, &self.edge_values(&yes.args)?);
        }
        if yes.block == no.block {
            // LLVM phi operands are keyed by predecessor block, not by branch arm.
            let left = self.new_block("then_edge");
            let right = self.new_block("else_edge");
            self.builder
                .build_conditional_branch(condition, left, right)?;
            for (block, edge) in [(left, yes), (right, no)] {
                self.builder.position_at_end(block);
                self.jump(edge.block, &self.edge_values(&edge.args)?)?;
            }
        } else {
            let left = self.incoming(yes.block, &self.edge_values(&yes.args)?)?;
            let right = self.incoming(no.block, &self.edge_values(&no.args)?)?;
            self.builder
                .build_conditional_branch(condition, left, right)?;
        }
        Ok(())
    }

    fn dispatch_unwind(&self) -> Result<()> {
        let target = self.load_word(self.field(self.result, 2)?)?;
        let state = self.load_word(self.field(self.result, 3)?)?;
        let own =
            self.builder
                .build_int_compare(IntPredicate::EQ, target, self.frame_index(), "")?;
        let sentinel = self.builder.build_int_compare(
            IntPredicate::EQ,
            state,
            self.ir.word.const_all_ones(),
            "",
        )?;
        let returned = self.builder.build_and(own, sentinel, "")?;
        let finish = self.new_block("debug_frame_return");
        let normal = self.new_block("normal_unwind");
        self.builder
            .build_conditional_branch(returned, finish, normal)?;
        self.builder.position_at_end(finish);
        self.return_result(JIT_RETURN_TAG_VALUES, self.word(0))?;
        self.builder.position_at_end(normal);
        let state = self.call_word(
            RuntimeAbiFunction::PendingUnwindState,
            &[self.ctx.into(), self.frame_index().into()],
        )?;
        let start = self.current_block();
        let targets = self.layout.unwind_table().targets();
        let mut cases = Vec::with_capacity(targets.len());
        for (i, &target) in targets.iter().enumerate() {
            let resume = self.new_block("unwind_resume");
            cases.push((self.word(i + 1), resume));
            self.builder.position_at_end(resume);
            let block = &self.ssa.blocks[self.layout.block_index(target).unwrap()];
            let parameters = self.layout.unwind_parameters(block);
            let mut args = Vec::with_capacity(parameters.len());
            for (_, slot) in parameters {
                args.push(self.load_word(self.root(slot)?)?.into());
            }
            self.jump(target, &args)?;
        }
        self.builder.position_at_end(start);
        self.builder
            .build_switch(state, self.forward_block(), &cases)?;
        Ok(())
    }

    fn field(&self, base: PointerValue<'ctx>, index: u32) -> Result<PointerValue<'ctx>> {
        Ok(self
            .builder
            .build_struct_gep(self.ir.result, base, index, "")?)
    }

    fn result_tag(&self) -> Result<IntValue<'ctx>> {
        Ok(self
            .builder
            .build_load(
                self.ir.context.i8_type(),
                self.field(self.result, 0)?,
                "tag",
            )?
            .into_int_value())
    }

    fn single_value(&self) -> Result<IntValue<'ctx>> {
        self.load_word(self.field(self.result, 1)?)
    }

    fn return_result(&self, tag: u8, single_value: IntValue<'ctx>) -> Result<()> {
        self.drop_frame()?;
        self.builder
            .build_store(self.field(self.output, 0)?, self.tag(tag))?;
        for (i, value) in [single_value, self.word(0), self.word(0)]
            .into_iter()
            .enumerate()
        {
            self.builder
                .build_store(self.field(self.output, i as u32 + 1)?, value)?;
        }
        self.builder.build_return(None)?;
        Ok(())
    }

    fn forward_result(&self) -> Result<()> {
        let result = self
            .builder
            .build_load(self.ir.result, self.result, "forwarded")?;
        self.drop_frame()?;
        self.builder.build_store(self.output, result)?;
        self.builder.build_return(None)?;
        Ok(())
    }

    fn successful_result(&self) -> Result<IntValue<'ctx>> {
        let tag = self.result_tag()?;
        let success = self.builder.build_int_compare(
            IntPredicate::ULT,
            tag,
            self.tag(JIT_RETURN_TAG_UNWIND),
            "",
        )?;
        let ok = self.new_block("ok");
        self.builder
            .build_conditional_branch(success, ok, self.exceptional_block())?;
        self.builder.position_at_end(ok);
        Ok(tag)
    }

    fn poll_backedge(&self, at: usize) -> Result<()> {
        self.emit_call(&SliceCharge::poll(at), None)?;
        self.successful_result()?;
        Ok(())
    }

    fn terminate(&self, term: &Terminator) -> Result<()> {
        match term {
            Terminator::Jump(target) => self.jump(target.block, &self.edge_values(&target.args)?),
            Terminator::BranchTruthy {
                cond,
                truthy,
                falsy,
            } => {
                let truth = self.builder.build_int_compare(
                    IntPredicate::NE,
                    self.value(*cond)?,
                    self.ir.word.const_int(WORD_NIL, false),
                    "",
                )?;
                self.branch(truth, truthy, falsy)
            }
            Terminator::BranchBool {
                cond,
                then_target,
                else_target,
            } => self.branch(self.value(*cond)?, then_target, else_target),
            Terminator::ReturnOne(value) => {
                self.return_result(JIT_RETURN_TAG_ONE, self.value(*value)?)
            }
            Terminator::ReturnValues(values) => {
                if let [one] = values.as_slice() {
                    return self.return_result(JIT_RETURN_TAG_ONE, self.value(*one)?);
                }
                self.set_values(values)?;
                self.return_result(JIT_RETURN_TAG_VALUES, self.word(0))
            }
            Terminator::ReturnDynamicValues => {
                self.return_result(JIT_RETURN_TAG_VALUES, self.word(0))
            }
            Terminator::Unwind(op) => {
                match op {
                    UnwindOp::ReturnFrom { token, at } => self.call(
                        RuntimeAbiFunction::ReturnFrom,
                        &[
                            self.ctx.into(),
                            self.value(*token)?.into(),
                            self.word(*at).into(),
                            self.result.into(),
                        ],
                    )?,
                    UnwindOp::Throw { tag, at } => self.call(
                        RuntimeAbiFunction::Throw,
                        &[
                            self.ctx.into(),
                            self.value(*tag)?.into(),
                            self.word(*at).into(),
                            self.result.into(),
                        ],
                    )?,
                    UnwindOp::Go { token, label, at } => self.call(
                        RuntimeAbiFunction::Go,
                        &[
                            self.ctx.into(),
                            self.value(*token)?.into(),
                            self.word(*label).into(),
                            self.word(*at).into(),
                            self.result.into(),
                        ],
                    )?,
                };
                self.builder
                    .build_unconditional_branch(self.exceptional_block())?;
                Ok(())
            }
        }
    }

    fn emit_tail_call(&self, tail: TailCall<'_>) -> Result<()> {
        let callee = match tail.callee {
            TailCallee::Named(symbol) => self.load_constant(symbol)?,
            TailCallee::Value(value) => self.value(value)?,
        };
        let args = tail.args.unwrap_or_default();
        let use_values = self
            .ir
            .context
            .bool_type()
            .const_int(u64::from(tail.args.is_none()), false);
        self.call(
            RuntimeAbiFunction::StageTailCall,
            &[
                self.ctx.into(),
                callee.into(),
                self.scratch
                    .words
                    .write(self, &self.operands(args)?)?
                    .into(),
                self.word(args.len()).into(),
                self.word(tail.at).into(),
                use_values.into(),
            ],
        )?;
        self.return_result(JIT_RETURN_TAG_TAIL_CALL, self.word(0))
    }
}
