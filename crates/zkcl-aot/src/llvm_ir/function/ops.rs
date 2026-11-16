use inkwell::IntPredicate;
use inkwell::intrinsics::Intrinsic;
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, CallSiteValue, IntValue};
use zkcl_bytecode::native_abi::JIT_CALL_PRIMARY_SLOW;
use zkcl_bytecode::{Aarch64Popcount, FixnumArithmetic, Immediate, WordCountOp};
use zkcl_opt::native::{
    CallOperand, CallReturn, CallWhen, FIXNUM_SHIFT, NativeOp, RuntimeCall, SliceCharge,
    TAG_FIXNUM, TAG_MASK, WordOp,
};
use zkcl_opt::{CellTarget, CmpOp, DeoptState, RootSlot, SsaInst, SsaOp};

use super::{
    AotError, BlockId, Function, JIT_RETURN_TAG_ONE, Result, RuntimeAbiFunction, ValueId, WORD_NIL,
};

impl<'ctx> Function<'_, 'ctx> {
    pub(super) fn immediate(&self, imm: Immediate) -> Result<IntValue<'ctx>> {
        let raw = imm
            .encode_word(self.ir.word.get_bit_width() as u8)
            .ok_or_else(|| match imm {
                Immediate::Fixnum(n) => AotError::InvalidFixnum(n),
                _ => unreachable!("the target word width was validated by the module emitter"),
            })?;
        Ok(self.ir.word.const_int(raw, false))
    }

    fn is_fixnum(&self, value: IntValue<'ctx>) -> Result<IntValue<'ctx>> {
        let tag = self
            .builder
            .build_and(value, self.ir.word.const_int(TAG_MASK, false), "")?;
        Ok(self.builder.build_int_compare(
            IntPredicate::EQ,
            tag,
            self.ir.word.const_int(TAG_FIXNUM, false),
            "",
        )?)
    }

    pub(super) fn unbox_fixnum(&self, value: IntValue<'ctx>) -> Result<IntValue<'ctx>> {
        let ty = self.ir.context.i64_type();
        let value = self.builder.build_int_s_extend_or_bit_cast(value, ty, "")?;
        Ok(self.builder.build_right_shift(
            value,
            ty.const_int(FIXNUM_SHIFT.into(), false),
            true,
            "",
        )?)
    }

    pub(super) fn box_fixnum(&self, value: IntValue<'ctx>) -> Result<IntValue<'ctx>> {
        let value = self
            .builder
            .build_int_truncate_or_bit_cast(value, self.ir.word, "")?;
        let shifted = self
            .builder
            .build_left_shift(value, self.word(FIXNUM_SHIFT as usize), "")?;
        Ok(self
            .builder
            .build_or(shifted, self.ir.word.const_int(TAG_FIXNUM, false), "")?)
    }

    fn checked_arithmetic(
        &self,
        op: &SsaOp,
        lhs: ValueId,
        rhs: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<IntValue<'ctx>> {
        let left = self.value(lhs)?;
        let right = self.value(rhs)?;
        let wide = self.ir.context.i64_type();
        let (name, left, right) = match op {
            SsaOp::MulI64Checked(..) => ("llvm.smul.with.overflow", left, right),
            _ => {
                // Overflow of the shifted sum/difference is precisely overflow of a tagged fixnum.
                let shift = self.word(FIXNUM_SHIFT as usize);
                let left = self
                    .builder
                    .build_int_truncate_or_bit_cast(left, self.ir.word, "")?;
                let right = self
                    .builder
                    .build_int_truncate_or_bit_cast(right, self.ir.word, "")?;
                let left = self.builder.build_left_shift(left, shift, "")?;
                let right = self.builder.build_left_shift(right, shift, "")?;
                let name = if matches!(op, SsaOp::AddI64Checked(..)) {
                    "llvm.sadd.with.overflow"
                } else {
                    "llvm.ssub.with.overflow"
                };
                (name, left, right)
            }
        };
        let (result, mut overflow) = self.signed_overflow(name, left, right)?;
        if matches!(op, SsaOp::MulI64Checked(..)) {
            let (min, max) = self.ssa.fixnum_bounds;
            let low = self.builder.build_int_compare(
                IntPredicate::SLT,
                result,
                wide.const_int(min as u64, false),
                "",
            )?;
            let high = self.builder.build_int_compare(
                IntPredicate::SGT,
                result,
                wide.const_int(max as u64, false),
                "",
            )?;
            overflow =
                self.builder
                    .build_or(overflow, self.builder.build_or(low, high, "")?, "")?;
        }
        self.guard(self.builder.build_not(overflow, "")?, deopt)?;
        if matches!(op, SsaOp::MulI64Checked(..)) {
            Ok(result)
        } else {
            self.unbox_fixnum(result)
        }
    }

    fn signed_overflow(
        &self,
        name: &str,
        left: IntValue<'ctx>,
        right: IntValue<'ctx>,
    ) -> Result<(IntValue<'ctx>, IntValue<'ctx>)> {
        let function = Intrinsic::find(name)
            .and_then(|intrinsic| {
                intrinsic.get_declaration(&self.ir.module, &[left.get_type().into()])
            })
            .ok_or_else(|| self.error(format!("missing LLVM intrinsic {name}")))?;
        let pair = self
            .builder
            .build_call(function, &[left.into(), right.into()], "")?
            .try_as_basic_value()
            .unwrap_basic()
            .into_struct_value();
        let result = self
            .builder
            .build_extract_value(pair, 0, "")?
            .into_int_value();
        let overflow = self
            .builder
            .build_extract_value(pair, 1, "")?
            .into_int_value();
        Ok((result, overflow))
    }

    fn try_fixnum_arithmetic(
        &self,
        op: FixnumArithmetic,
        symbol: usize,
        lhs: IntValue<'ctx>,
        rhs: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        use FixnumArithmetic::*;

        let bound = self.is_primitive_binding(symbol, op.binding_index())?;
        let fixnums = self
            .builder
            .build_and(self.is_fixnum(lhs)?, self.is_fixnum(rhs)?, "")?;
        // Compute the scaled result in the target word, so the intrinsic's
        // overflow bit also covers values outside the target fixnum range.
        let mask = self.ir.word.const_int(!TAG_MASK, false);
        let left = self.builder.build_and(lhs, mask, "")?;
        let (name, right) = match op {
            Add | Inc => (
                "llvm.sadd.with.overflow",
                self.builder.build_and(rhs, mask, "")?,
            ),
            Sub | Dec => (
                "llvm.ssub.with.overflow",
                self.builder.build_and(rhs, mask, "")?,
            ),
            Mul => (
                "llvm.smul.with.overflow",
                self.builder
                    .build_right_shift(rhs, self.word(FIXNUM_SHIFT as usize), true, "")?,
            ),
        };
        let (result, overflow) = self.signed_overflow(name, left, right)?;
        let tagged =
            self.builder
                .build_or(result, self.ir.word.const_int(TAG_FIXNUM, false), "")?;
        let valid = self.builder.build_and(bound, fixnums, "")?;
        let valid = self
            .builder
            .build_and(valid, self.builder.build_not(overflow, "")?, "")?;
        Ok(self
            .builder
            .build_select(valid, tagged, self.ir.word.const_int(WORD_NIL, false), "")?
            .into_int_value())
    }

    fn guard_transform_binding(
        &self,
        symbol: usize,
        binding: usize,
        deopt: Option<&DeoptState>,
    ) -> Result<()> {
        let bound = self.call_word(
            RuntimeAbiFunction::CheckTransformBinding,
            &[
                self.ctx.into(),
                self.target_word(0)?.into(),
                self.word(symbol).into(),
                self.word(binding).into(),
            ],
        )?;
        self.guard(bound, deopt)
    }

    fn unbox_u64_guard(
        &self,
        input: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<IntValue<'ctx>> {
        let input = self.value(input)?;
        let word = self.ir.word;
        let sign_and_tag = (1_u64 << (word.get_bit_width() - 1)) | TAG_MASK;
        let masked = self
            .builder
            .build_and(input, word.const_int(sign_and_tag, false), "")?;
        let is_nonnegative_fixnum = self.builder.build_int_compare(
            IntPredicate::EQ,
            masked,
            word.const_int(TAG_FIXNUM, false),
            "",
        )?;
        self.choose_int(
            is_nonnegative_fixnum,
            || self.unbox_fixnum(input),
            || {
                let ty = self.ir.context.i64_type();
                let storage = self.allocas.build_alloca(ty, "u64_slot")?;
                let ready = self.call_word(
                    RuntimeAbiFunction::UnboxU64,
                    &[self.ctx.into(), input.into(), storage.into()],
                )?;
                self.guard(ready, deopt)?;
                Ok(self
                    .builder
                    .build_load(ty, storage, "u64")?
                    .into_int_value())
            },
        )
    }

    fn word_count(
        &self,
        op: WordCountOp,
        input: ValueId,
        pc: Option<usize>,
    ) -> Result<IntValue<'ctx>> {
        let bits = self.value(input)?;
        if op == WordCountOp::Popcount
            && self.ir.aarch64_asimd
            && let Some(pc) = pc
            && let Ok(index) = self.vop_calls.binary_search_by_key(&pc, |call| call.pc)
            && let call = &self.vop_calls[index]
            && call.operation == op
            && let Some(assembly) = call.aarch64_popcount
        {
            return self.aarch64_popcount(assembly, bits);
        }
        let intrinsic = match op {
            WordCountOp::Popcount => "llvm.ctpop",
            WordCountOp::LeadingZeros => "llvm.ctlz",
            WordCountOp::TrailingZeros => "llvm.cttz",
        };
        let function = Intrinsic::find(intrinsic)
            .and_then(|intrinsic| {
                intrinsic.get_declaration(&self.ir.module, &[bits.get_type().into()])
            })
            .ok_or_else(|| self.error(format!("missing LLVM {intrinsic} intrinsic")))?;
        // A false `is_zero_poison` operand preserves the defined width on zero.
        let defined_on_zero = self.ir.context.bool_type().const_zero().into();
        let args = [bits.into(), defined_on_zero];
        let arity = if op == WordCountOp::Popcount { 1 } else { 2 };
        Ok(self
            .builder
            .build_call(function, &args[..arity], "word_count")?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value())
    }

    fn aarch64_popcount(
        &self,
        assembly: Aarch64Popcount,
        bits: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        let reduce = match assembly {
            Aarch64Popcount::AddBytes => "addv ${1:b}, $1.8b",
            Aarch64Popcount::WidenAndAddBytes => "uaddlv ${1:h}, $1.8b",
        };
        let word = self.ir.context.i64_type();
        let temporary = self.ir.context.i8_type().vec_type(8);
        // The second output gives LLVM a vector register to allocate. The
        // early-clobber constraint covers its first write in the template.
        let outputs = self
            .ir
            .context
            .struct_type(&[word.into(), temporary.into()], false);
        let signature = outputs.fn_type(&[word.into()], false);
        let asm = self.ir.context.create_inline_asm(
            signature,
            format!("fmov ${{1:d}}, $2\n\tcnt $1.8b, $1.8b\n\t{reduce}\n\tfmov $0, ${{1:d}}"),
            "=r,=&w,r".into(),
            false,
            false,
            None,
            false,
        );
        let outputs = self
            .builder
            .build_indirect_call(signature, asm, &[bits.into()], "popcount_asm")?
            .try_as_basic_value()
            .unwrap_basic()
            .into_struct_value();
        Ok(self
            .builder
            .build_extract_value(outputs, 0, "popcount")?
            .into_int_value())
    }

    pub(super) fn is_primitive_binding(
        &self,
        symbol: usize,
        binding: u8,
    ) -> Result<IntValue<'ctx>> {
        self.call_word(
            RuntimeAbiFunction::IsPrimitiveBinding,
            &[
                self.ctx.into(),
                self.target_word(0)?.into(),
                self.word(symbol).into(),
                self.word(usize::from(binding)).into(),
            ],
        )
    }

    pub(super) fn guard(&self, valid: IntValue<'ctx>, deopt: Option<&DeoptState>) -> Result<()> {
        self.guard_with_roots(valid, deopt, &[], None)
    }

    fn guard_with_roots(
        &self,
        valid: IntValue<'ctx>,
        deopt: Option<&DeoptState>,
        live: &[(ValueId, RootSlot)],
        static_values: Option<&[ValueId]>,
    ) -> Result<()> {
        let next = self.new_block("guard_ok");
        if let Some(state) = deopt {
            let exit = self.new_block("deopt");
            self.builder.build_conditional_branch(valid, next, exit)?;
            self.builder.position_at_end(exit);
            if let Some(values) = static_values {
                self.set_values(values)?;
            }
            self.emit_deopt(state, live)?;
        } else {
            let fault = *self.fault.get_or_init(|| self.new_block("fault"));
            self.builder.build_conditional_branch(valid, next, fault)?;
        }
        self.builder.position_at_end(next);
        Ok(())
    }

    fn call_handoff(&self, deopt: Option<&DeoptState>, live: &[(ValueId, RootSlot)]) -> Result<()> {
        if deopt.is_some() {
            let valid = self.builder.build_int_compare(
                IntPredicate::ULT,
                self.result_tag()?,
                self.tag(zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_CALL),
                "",
            )?;
            self.guard_with_roots(valid, deopt, live, None)?;
        }
        Ok(())
    }

    fn checkpoint(
        &self,
        inst: &SsaInst,
        values: Option<&[ValueId]>,
        protocol: SliceCharge,
    ) -> Result<()> {
        self.poll_backedge(inst.pc.expect("checkpoint has a bytecode location"))?;
        let offset = zkcl_bytecode::native_abi::jit_call_context_offsets(
            (self.ir.word.get_bit_width() / 8) as i32,
        )
        .slice_steps;
        let pointer = self.element(self.ir.context.i8_type(), self.ctx, offset as usize)?;
        let slice = self
            .builder
            .build_load(self.ir.ptr, pointer, "slice")?
            .into_pointer_value();
        let charge = self.new_block("charge_slice");
        let consume = self.new_block("consume_slice");
        let refill = self.new_block("refill_slice");
        let next = self.new_block("slice_ready");
        self.builder.build_conditional_branch(
            self.builder.build_is_null(slice, "")?,
            next,
            charge,
        )?;
        self.builder.position_at_end(charge);
        let ty = self.ir.context.i16_type();
        let remaining = self.builder.build_load(ty, slice, "")?.into_int_value();
        let exhausted =
            self.builder
                .build_int_compare(IntPredicate::EQ, remaining, ty.const_zero(), "")?;
        self.builder
            .build_conditional_branch(exhausted, refill, consume)?;
        self.builder.position_at_end(consume);
        let remaining = self.builder.build_int_sub(
            remaining,
            ty.const_int(protocol.cost().into(), false),
            "",
        )?;
        self.builder.build_store(slice, remaining)?;
        self.builder.build_unconditional_branch(next)?;
        self.builder.position_at_end(refill);
        let refilled = self.emit_call(&protocol.refill(), None)?;
        let refilled = refilled
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        self.guard_with_roots(refilled, inst.deopt.as_ref(), &[], values)?;
        self.builder.build_unconditional_branch(next)?;
        self.builder.position_at_end(next);
        Ok(())
    }

    pub(super) fn load_constant(&self, index: usize) -> Result<IntValue<'ctx>> {
        let call = self.emit_call(&RuntimeCall::constant(index), None)?;
        Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
    }

    fn capture(&self, index: usize) -> Result<IntValue<'ctx>> {
        self.call_one(
            RuntimeAbiFunction::ReadCapture,
            &[
                self.ctx.into(),
                self.target_word(1)?.into(),
                self.word(index).into(),
                self.word(0).into(),
                self.result.into(),
            ],
        )
    }

    fn cell(&self, target: &CellTarget) -> Result<IntValue<'ctx>> {
        match target {
            CellTarget::Local(value) => self.value(*value),
            CellTarget::Capture(index) => self.capture(*index),
        }
    }

    pub(super) fn set_values(&self, values: &[ValueId]) -> Result<()> {
        self.emit_call(&RuntimeCall::set_values(values), None)?;
        Ok(())
    }

    fn receive_call(&self, primary: bool) -> Result<Option<IntValue<'ctx>>> {
        let tag = self.successful_result()?;
        let one = self.builder.build_int_compare(
            IntPredicate::EQ,
            tag,
            self.tag(JIT_RETURN_TAG_ONE),
            "",
        )?;
        if primary {
            return self
                .choose_int(
                    one,
                    || self.single_value(),
                    || {
                        let call = self.emit_call(&RuntimeCall::read_value(0), None)?;
                        Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                    },
                )
                .map(Some);
        }
        let set_one = self.new_block("set_one");
        let done = self.new_block("values_ready");
        self.builder.build_conditional_branch(one, set_one, done)?;
        self.builder.position_at_end(set_one);
        self.emit_call(&RuntimeCall::preserve_single_value(), None)?;
        self.builder.build_unconditional_branch(done)?;
        self.builder.position_at_end(done);
        Ok(None)
    }

    pub(super) fn emit_call(
        &self,
        call: &RuntimeCall<'_>,
        site: Option<(BlockId, usize)>,
    ) -> Result<CallSiteValue<'ctx>> {
        use CallOperand::*;

        let mut args = [BasicMetadataValueEnum::from(self.ctx); RuntimeCall::MAX_OPERANDS];
        for (arg, &operand) in args.iter_mut().zip(call.operands()) {
            *arg = match operand {
                Context => self.ctx.into(),
                Frame => self.frame_index().into(),
                Code => self.target_word(self.layout.roots().code_slot.0)?.into(),
                Word(word) => self.word(word).into(),
                Bool(value) => self.boolean(value).into(),
                Value(value) => self.value(value)?.into(),
                SingleValue => self.single_value()?.into(),
                Constant(index) => self.load_constant(index)?.into(),
                Cell(cell) => self.cell(&cell)?.into(),
                BoxLocation => self.word(0).into(),
                Words(values) => self
                    .scratch
                    .words
                    .write(self, &self.operands(values)?)?
                    .into(),
                Length(values) => self.word(values.len()).into(),
                CatchTag(value) => {
                    let (block, index) = site.expect("catch call belongs to a block");
                    let slot = self.layout.catch_tag_slot(block, index);
                    self.builder
                        .build_store(self.root(slot)?, self.value(value)?)?;
                    self.word(slot).into()
                }
                UnwindState(target) => self
                    .word(self.layout.unwind_table().state_for(target))
                    .into(),
                TagbodyBase => {
                    let (block, index) = site.expect("tagbody call belongs to a block");
                    self.word(self.layout.unwind_table().tagbody_base(block, index))
                        .into()
                }
                HandlerStates(targets) => {
                    let states: Vec<_> = targets
                        .iter()
                        .map(|&target| self.word(self.layout.unwind_table().state_for(target)))
                        .collect();
                    self.scratch.words.write(self, &states)?.into()
                }
                HandlerSlots(targets) => {
                    let slots: Vec<_> = targets
                        .iter()
                        .map(|&target| self.word(self.layout.handler_condition_slot(target)))
                        .collect();
                    self.scratch.handler_slots.write(self, &slots)?.into()
                }
                RootCount => self.word(self.layout.root_slots()).into(),
                ResultOut => self.result.into(),
            };
        }
        self.call(call.function, &args[..call.operands().len()])
    }

    fn emit_ssa_call(
        &self,
        bid: BlockId,
        index: usize,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
        call: &RuntimeCall<'_>,
    ) -> Result<Option<IntValue<'ctx>>> {
        match call.when {
            CallWhen::ReleaseArguments if self.release_local_arguments()? => return Ok(None),
            CallWhen::OptionalArgument(index) => {
                if let Some(value) = self.local_optional_argument(index)? {
                    return Ok(Some(value));
                }
            }
            _ => {}
        }
        let site = Some((bid, index));
        if let Some(register_call) = RuntimeCall::primary_register_call(&inst.op) {
            let word = self
                .emit_call(&register_call, site)?
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value();
            let complete = self.builder.build_int_compare(
                IntPredicate::NE,
                word,
                self.ir.word.const_int(JIT_CALL_PRIMARY_SLOW, false),
                "",
            )?;
            return self
                .choose_int(
                    complete,
                    || Ok(word),
                    || {
                        self.call_handoff(inst.deopt.as_ref(), live)?;
                        Ok(self
                            .receive_call(true)?
                            .expect("a scalar call has a primary result"))
                    },
                )
                .map(Some);
        }
        let emitted = self.emit_call(call, site)?;
        let result = match call.result {
            CallReturn::Void => None,
            CallReturn::Word => Some(emitted.try_as_basic_value().unwrap_basic().into_int_value()),
            CallReturn::Effect | CallReturn::DebugPoint => {
                self.successful_result()?;
                None
            }
            CallReturn::ReadCell | CallReturn::One => {
                if matches!(call.result, CallReturn::ReadCell) {
                    self.call_handoff(inst.deopt.as_ref(), live)?;
                }
                self.successful_result()?;
                Some(self.single_value()?)
            }
            CallReturn::LispCall { primary } => {
                self.call_handoff(inst.deopt.as_ref(), live)?;
                self.receive_call(primary)?
            }
            CallReturn::Cleanup => self.receive_call(false)?,
        };
        if let Some(pc) = inst.pc.filter(|_| call.mark_scope) {
            self.emit_call(&RuntimeCall::mark_scope(pc), None)?;
        }
        Ok(result)
    }

    fn emit_word_op(&self, op: WordOp) -> Result<IntValue<'ctx>> {
        use zkcl_opt::native::{I64Arithmetic, ScalarConstant};

        Ok(match op {
            WordOp::Constant(ScalarConstant::Word(imm)) => self.immediate(imm)?,
            WordOp::Constant(ScalarConstant::I64(n)) => {
                self.ir.context.i64_type().const_int(n as u64, false)
            }
            WordOp::Constant(ScalarConstant::U64(n)) => {
                self.ir.context.i64_type().const_int(n, false)
            }
            WordOp::Constant(ScalarConstant::Bool(b)) => self.boolean(b),
            WordOp::Copy(value) => self.value(value)?,
            WordOp::Select(cond, yes, no) => self
                .builder
                .build_select(
                    self.value(cond)?,
                    self.immediate(yes)?,
                    self.immediate(no)?,
                    "",
                )?
                .into_int_value(),
            WordOp::BoxFixnum(value) => self.box_fixnum(self.value(value)?)?,
            WordOp::Test(value, test) => {
                let mut value = self.value(value)?;
                if let Some(mask) = test.mask {
                    value =
                        self.builder
                            .build_and(value, self.ir.word.const_int(mask, false), "")?;
                }
                let cmp = if test.equal {
                    IntPredicate::EQ
                } else {
                    IntPredicate::NE
                };
                self.builder.build_int_compare(
                    cmp,
                    value,
                    self.ir.word.const_int(test.bits, false),
                    "",
                )?
            }
            WordOp::FixnumRange { value, low, span } => {
                let word = self.value(value)?;
                let is_fixnum = self.is_fixnum(word)?;
                let unboxed = self.unbox_fixnum(word)?;
                let ty = unboxed.get_type();
                let offset =
                    self.builder
                        .build_int_sub(unboxed, ty.const_int(low as u64, true), "")?;
                let in_range = self.builder.build_int_compare(
                    IntPredicate::ULE,
                    offset,
                    ty.const_int(span as u64, false),
                    "",
                )?;
                self.builder.build_and(is_fixnum, in_range, "")?
            }
            WordOp::I64(op, lhs, rhs) => {
                let lhs = self.value(lhs)?;
                let rhs = self.value(rhs)?;
                match op {
                    I64Arithmetic::Add => self.builder.build_int_nsw_add(lhs, rhs, "")?,
                    I64Arithmetic::Sub => self.builder.build_int_nsw_sub(lhs, rhs, "")?,
                    I64Arithmetic::Mul => self.builder.build_int_mul(lhs, rhs, "")?,
                }
            }
            WordOp::BitwiseI64(op, lhs, rhs) => {
                let lhs = self.value(lhs)?;
                let rhs = self.value(rhs)?;
                match op {
                    zkcl_opt::BitwiseOp::And => self.builder.build_and(lhs, rhs, "")?,
                    zkcl_opt::BitwiseOp::Or => self.builder.build_or(lhs, rhs, "")?,
                    zkcl_opt::BitwiseOp::Xor => self.builder.build_xor(lhs, rhs, "")?,
                }
            }
            WordOp::CompareI64(op, lhs, rhs) => {
                let pred = match op {
                    CmpOp::Eq => IntPredicate::EQ,
                    CmpOp::Ne => IntPredicate::NE,
                    CmpOp::Lt => IntPredicate::SLT,
                    CmpOp::Le => IntPredicate::SLE,
                    CmpOp::Gt => IntPredicate::SGT,
                    CmpOp::Ge => IntPredicate::SGE,
                };
                self.builder
                    .build_int_compare(pred, self.value(lhs)?, self.value(rhs)?, "")?
            }
            WordOp::Not(value) => self.builder.build_not(self.value(value)?, "")?,
        })
    }

    pub(super) fn emit_inst(
        &self,
        block_index: usize,
        bid: BlockId,
        index: usize,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
    ) -> Result<Option<BasicValueEnum<'ctx>>> {
        match self.classified.operation(block_index, index) {
            NativeOp::Call(call) => {
                return self
                    .emit_ssa_call(bid, index, inst, live, &call)
                    .map(|value| value.map(Into::into));
            }
            NativeOp::Word(op) => return self.emit_word_op(op).map(|value| Some(value.into())),
            NativeOp::LoadHandlerConditionRoot => {
                return self
                    .load_word(self.root(self.layout.handler_condition_slot(bid))?)
                    .map(|value| Some(value.into()));
            }
            NativeOp::StoreUnwindRoot { reg, value } => {
                if let Some(slot) = self.layout.unwind_register_slot(reg) {
                    self.builder
                        .build_store(self.root(slot)?, self.value(value)?)?;
                }
                return Ok(None);
            }
            NativeOp::Checkpoint { values, charge } => {
                self.checkpoint(inst, values, charge)?;
                return Ok(None);
            }
            NativeOp::Target { .. } | NativeOp::Unsupported => {}
        }
        let result = match &inst.op {
            SsaOp::GuardPrimitiveBinding { symbol, primitive } => {
                let valid = self.is_primitive_binding(*symbol, primitive.abi_index())?;
                self.guard(valid, inst.deopt.as_ref())?;
                None
            }
            SsaOp::UnboxFixnumGuard(src) => {
                let tagged = self.value(*src)?;
                self.guard(self.is_fixnum(tagged)?, inst.deopt.as_ref())?;
                Some(self.unbox_fixnum(tagged)?)
            }
            SsaOp::TryFixnumArithmetic {
                op,
                symbol,
                lhs,
                rhs,
            } => Some(self.try_fixnum_arithmetic(
                *op,
                *symbol,
                self.value(*lhs)?,
                self.value(*rhs)?,
            )?),
            SsaOp::GuardTransformBinding { symbol, binding } => {
                self.guard_transform_binding(*symbol, *binding, inst.deopt.as_ref())?;
                None
            }
            SsaOp::UnboxU64Guard(input) => Some(self.unbox_u64_guard(*input, inst.deopt.as_ref())?),
            SsaOp::WordCount { op, input } => Some(self.word_count(*op, *input, inst.pc)?),
            SsaOp::AddI64Checked(a, b)
            | SsaOp::SubI64Checked(a, b)
            | SsaOp::MulI64Checked(a, b) => {
                Some(self.checked_arithmetic(&inst.op, *a, *b, inst.deopt.as_ref())?)
            }
            SsaOp::ReadCapture(index) => Some(self.capture(*index)?),
            other => {
                return self.emit_numeric(inst, other);
            }
        };
        Ok(result.map(Into::into))
    }
}
