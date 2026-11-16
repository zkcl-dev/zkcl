use zkcl_bytecode::{FIXNUM_SHIFT, FixnumArithmetic, Immediate};
use zkcl_opt::native::{
    CallOperand, CallReturn, CallWhen, I64Arithmetic, NativeOp, RuntimeCall, SliceCharge, WordOp,
};
use zkcl_opt::{
    ArithOp, CellTarget, CmpOp, DeoptRegisterValue, DeoptState, FloatArithOp, FloatFormat,
    GuardedPrimitive, RootSlot, SsaInst, SsaOp, ValueId,
};

use super::{
    BlockId, BlockType, CONTEXT, Emitter, Ins, JIT_RETURN_TAG_ONE, RESULT, RuntimeAbiFunction,
    SCRATCH, UnsupportedOp, ValType, WORD_BYTES,
};

pub(super) enum WasmTargetOp {
    TryFixnumArithmetic(FixnumArithmetic, usize, ValueId, ValueId),
    UnboxFixnumGuard(ValueId),
    CheckedFixnumArithmetic(ArithOp, ValueId, ValueId),
    GuardPrimitiveBinding {
        symbol: usize,
        primitive: GuardedPrimitive,
    },
    UnboxFloatGuard {
        format: FloatFormat,
        value: ValueId,
    },
    FloatArithChecked {
        format: FloatFormat,
        op: FloatArithOp,
        lhs: ValueId,
        rhs: ValueId,
    },
    BoxFloat {
        format: FloatFormat,
        value: ValueId,
    },
    ReadFloatVector {
        symbol: usize,
        format: FloatFormat,
        array: ValueId,
        index: ValueId,
    },
    WriteFloatVector {
        symbol: usize,
        format: FloatFormat,
        setter: ValueId,
        array: ValueId,
        index: ValueId,
        value: ValueId,
    },
    PopcountU64 {
        input: ValueId,
        symbol: usize,
        binding: usize,
    },
    ReadCapture(usize),
}

impl WasmTargetOp {
    pub(super) fn for_op(op: &SsaOp) -> Option<Self> {
        match *op {
            SsaOp::TryFixnumArithmetic {
                op,
                symbol,
                lhs,
                rhs,
            } => Some(Self::TryFixnumArithmetic(op, symbol, lhs, rhs)),
            SsaOp::UnboxFixnumGuard(src) => Some(Self::UnboxFixnumGuard(src)),
            SsaOp::AddI64Checked(lhs, rhs) => {
                Some(Self::CheckedFixnumArithmetic(ArithOp::Add, lhs, rhs))
            }
            SsaOp::SubI64Checked(lhs, rhs) => {
                Some(Self::CheckedFixnumArithmetic(ArithOp::Sub, lhs, rhs))
            }
            SsaOp::MulI64Checked(lhs, rhs) => {
                Some(Self::CheckedFixnumArithmetic(ArithOp::Mul, lhs, rhs))
            }
            SsaOp::GuardPrimitiveBinding { symbol, primitive } => {
                Some(Self::GuardPrimitiveBinding { symbol, primitive })
            }
            SsaOp::UnboxFloatGuard { format, value } => {
                Some(Self::UnboxFloatGuard { format, value })
            }
            SsaOp::FloatArithChecked {
                format,
                op,
                lhs,
                rhs,
            } => Some(Self::FloatArithChecked {
                format,
                op,
                lhs,
                rhs,
            }),
            SsaOp::BoxFloat { format, value } => Some(Self::BoxFloat { format, value }),
            SsaOp::ReadFloatVector {
                symbol,
                format,
                array,
                index,
            } => Some(Self::ReadFloatVector {
                symbol,
                format,
                array,
                index,
            }),
            SsaOp::WriteFloatVector {
                symbol,
                format,
                setter,
                array,
                index,
                value,
            } => Some(Self::WriteFloatVector {
                symbol,
                format,
                setter,
                array,
                index,
                value,
            }),
            SsaOp::PopcountU64 {
                input,
                symbol,
                binding,
                ..
            } => Some(Self::PopcountU64 {
                input,
                symbol,
                binding,
            }),
            SsaOp::ReadCapture(index) => Some(Self::ReadCapture(index)),
            _ => None,
        }
    }
}

impl<F: Fn(RuntimeAbiFunction) -> Option<u32>> Emitter<'_, F> {
    pub(super) fn operation(
        &mut self,
        block: BlockId,
        index: usize,
        operation: NativeOp<'_>,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
    ) -> Result<(), UnsupportedOp> {
        match operation {
            NativeOp::Call(call) => self.emit_ssa_call(block, index, inst, live, &call),
            // The local map substitutes literals at their use sites.
            NativeOp::Word(WordOp::Constant(_)) => return Ok(()),
            NativeOp::Word(op) => self.emit_word_op(op),
            NativeOp::Checkpoint { values, charge } => self.checkpoint(
                inst.deopt.as_ref().expect("checkpoint recovery map"),
                values,
                charge,
            ),
            NativeOp::StoreUnwindRoot { reg, value } => {
                if let Some(slot) = self.frame.unwind_register_slot(reg) {
                    self.root_slot(slot);
                    self.get(value);
                    self.store(0);
                }
            }
            NativeOp::LoadHandlerConditionRoot => {
                self.read_root(self.frame.handler_condition_slot(block))
            }
            NativeOp::Target { .. } => match WasmTargetOp::for_op(&inst.op).ok_or(UnsupportedOp)? {
                WasmTargetOp::TryFixnumArithmetic(op, symbol, lhs, rhs) => {
                    self.try_arithmetic(op, symbol, lhs, rhs);
                }
                WasmTargetOp::UnboxFixnumGuard(src) => {
                    self.unbox_fixnum_guard(src, inst.deopt.as_ref())?;
                }
                WasmTargetOp::CheckedFixnumArithmetic(op, lhs, rhs) => {
                    self.checked_fixnum_arithmetic(op, lhs, rhs, inst.deopt.as_ref())?;
                }
                WasmTargetOp::GuardPrimitiveBinding { symbol, primitive } => {
                    self.guard_primitive_binding(symbol, primitive, inst.deopt.as_ref())?;
                    return Ok(());
                }
                WasmTargetOp::UnboxFloatGuard { format, value } => {
                    self.unbox_float_guard(format, value, inst.deopt.as_ref())?;
                }
                WasmTargetOp::FloatArithChecked {
                    format,
                    op,
                    lhs,
                    rhs,
                } => {
                    self.float_arith_checked(format, op, lhs, rhs, inst.deopt.as_ref())?;
                }
                WasmTargetOp::BoxFloat { format, value } => {
                    self.box_float(format, value, inst.pc.unwrap_or(0));
                }
                WasmTargetOp::ReadFloatVector {
                    symbol,
                    format,
                    array,
                    index,
                } => {
                    self.read_float_vector(symbol, format, array, index, inst.deopt.as_ref())?;
                }
                WasmTargetOp::WriteFloatVector {
                    symbol,
                    format,
                    setter,
                    array,
                    index,
                    value,
                } => {
                    self.write_float_vector(
                        symbol,
                        format,
                        setter,
                        array,
                        index,
                        value,
                        inst.deopt.as_ref(),
                    )?;
                    return Ok(());
                }
                WasmTargetOp::PopcountU64 {
                    input,
                    symbol,
                    binding,
                } => {
                    self.popcount_u64(input, symbol, binding, inst.deopt.as_ref())?;
                }
                WasmTargetOp::ReadCapture(index) => self.capture(index),
            },
            NativeOp::Unsupported => return Err(UnsupportedOp),
        }
        if let Some(id) = inst.id {
            self.set(id);
        } else if matches!(
            WasmTargetOp::for_op(&inst.op),
            Some(
                WasmTargetOp::UnboxFixnumGuard(_)
                    | WasmTargetOp::CheckedFixnumArithmetic(..)
                    | WasmTargetOp::UnboxFloatGuard { .. }
                    | WasmTargetOp::FloatArithChecked { .. }
                    | WasmTargetOp::BoxFloat { .. }
                    | WasmTargetOp::ReadFloatVector { .. }
                    | WasmTargetOp::PopcountU64 { .. }
            )
        ) {
            self.ins(Ins::Drop);
        }
        Ok(())
    }

    fn guard_unless_nz(&mut self, deopt: Option<&DeoptState>) -> Result<(), UnsupportedOp> {
        let deopt = deopt.ok_or(UnsupportedOp)?;
        self.ins(Ins::I32Eqz);
        self.scope(Ins::If(BlockType::Empty));
        self.restore_bytecode(deopt);
        self.end();
        Ok(())
    }

    fn emit_float_bits(&mut self, format: FloatFormat, value: ValueId) {
        self.get(value);
        match format {
            FloatFormat::F32 => {
                self.ins(Ins::I32ReinterpretF32);
                self.ins(Ins::I64ExtendI32U);
            }
            FloatFormat::F64 => {
                self.ins(Ins::I64ReinterpretF64);
            }
        }
    }

    fn emit_float_from_wide(&mut self, format: FloatFormat, wide: u32) {
        self.ins(Ins::LocalGet(wide));
        match format {
            FloatFormat::F32 => {
                self.ins(Ins::I32WrapI64);
                self.ins(Ins::F32ReinterpretI32);
            }
            FloatFormat::F64 => {
                self.ins(Ins::F64ReinterpretI64);
            }
        }
    }

    fn unbox_fixnum_guard(
        &mut self,
        src: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        self.is_fixnum(src);
        self.guard_unless_nz(deopt)?;
        self.get(src);
        self.unbox_fixnum();
        Ok(())
    }

    fn checked_fixnum_arithmetic(
        &mut self,
        op: ArithOp,
        lhs: ValueId,
        rhs: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        let wide = self.wide.expect("checked arithmetic allocates a wide local");
        self.get(lhs);
        self.get(rhs);
        self.ins(match op {
            ArithOp::Add => Ins::I64Add,
            ArithOp::Sub => Ins::I64Sub,
            ArithOp::Mul => Ins::I64Mul,
        });
        self.ins(Ins::LocalTee(wide));
        let (min, max) = self.ssa.fixnum_bounds;
        self.ins(Ins::I64Const(min));
        self.ins(Ins::I64Sub);
        self.ins(Ins::I64Const(max - min));
        self.ins(Ins::I64LeU);
        self.guard_unless_nz(deopt)?;
        self.ins(Ins::LocalGet(wide));
        Ok(())
    }

    fn guard_primitive_binding(
        &mut self,
        symbol: usize,
        primitive: GuardedPrimitive,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        self.check_binding_index(symbol, primitive.abi_index());
        self.guard_unless_nz(deopt)
    }

    fn unbox_float_guard(
        &mut self,
        format: FloatFormat,
        value: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        let wide = self.wide.expect("float unbox guard allocates a wide local");
        self.ctx();
        self.get(value);
        self.call(match format {
            FloatFormat::F32 => RuntimeAbiFunction::UnboxF32,
            FloatFormat::F64 => RuntimeAbiFunction::UnboxF64,
        });
        self.ins(Ins::LocalTee(wide));
        self.ins(Ins::I64Const(-1));
        self.ins(Ins::I64Ne);
        self.guard_unless_nz(deopt)?;
        self.emit_float_from_wide(format, wide);
        Ok(())
    }

    fn float_arith_checked(
        &mut self,
        format: FloatFormat,
        op: FloatArithOp,
        lhs: ValueId,
        rhs: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        if op == FloatArithOp::Div {
            self.get(rhs);
            match format {
                FloatFormat::F32 => {
                    self.ins(Ins::F32Const(0.0_f32.into()));
                    self.ins(Ins::F32Ne);
                }
                FloatFormat::F64 => {
                    self.ins(Ins::F64Const(0.0_f64.into()));
                    self.ins(Ins::F64Ne);
                }
            }
            self.guard_unless_nz(deopt)?;
        }
        self.get(lhs);
        self.get(rhs);
        let (scratch, arith_ins) = match (format, op) {
            (FloatFormat::F32, FloatArithOp::Add) => (self.f32_scratch, Ins::F32Add),
            (FloatFormat::F32, FloatArithOp::Sub) => (self.f32_scratch, Ins::F32Sub),
            (FloatFormat::F32, FloatArithOp::Mul) => (self.f32_scratch, Ins::F32Mul),
            (FloatFormat::F32, FloatArithOp::Div) => (self.f32_scratch, Ins::F32Div),
            (FloatFormat::F64, FloatArithOp::Add) => (self.f64_scratch, Ins::F64Add),
            (FloatFormat::F64, FloatArithOp::Sub) => (self.f64_scratch, Ins::F64Sub),
            (FloatFormat::F64, FloatArithOp::Mul) => (self.f64_scratch, Ins::F64Mul),
            (FloatFormat::F64, FloatArithOp::Div) => (self.f64_scratch, Ins::F64Div),
        };
        let scratch = scratch.expect("checked float arithmetic allocates a scratch local");
        self.ins(arith_ins);
        self.ins(Ins::LocalTee(scratch));
        match format {
            FloatFormat::F32 => {
                self.ins(Ins::I32ReinterpretF32);
                self.ins(Ins::I32Const(0x7f80_0000));
                self.ins(Ins::I32And);
                self.ins(Ins::I32Const(0x7f80_0000));
                self.ins(Ins::I32Ne);
            }
            FloatFormat::F64 => {
                self.ins(Ins::I64ReinterpretF64);
                self.ins(Ins::I64Const(0x7ff0_0000_0000_0000));
                self.ins(Ins::I64And);
                self.ins(Ins::I64Const(0x7ff0_0000_0000_0000));
                self.ins(Ins::I64Ne);
            }
        }
        self.guard_unless_nz(deopt)?;
        self.ins(Ins::LocalGet(scratch));
        Ok(())
    }

    fn box_float(&mut self, format: FloatFormat, value: ValueId, pc: usize) {
        self.ctx();
        self.emit_float_bits(format, value);
        self.word(pc);
        self.ins(Ins::LocalGet(RESULT));
        self.call(match format {
            FloatFormat::F32 => RuntimeAbiFunction::BoxF32,
            FloatFormat::F64 => RuntimeAbiFunction::BoxF64,
        });
        self.check_result();
        self.single_value();
    }

    fn read_float_vector(
        &mut self,
        symbol: usize,
        format: FloatFormat,
        array: ValueId,
        index: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        let wide = self.wide.expect("read float vector allocates a wide local");
        self.ctx();
        self.target_word(self.frame.roots().code_slot.0);
        self.word(symbol);
        self.get(array);
        self.get(index);
        self.word(usize::from(format == FloatFormat::F64));
        self.call(RuntimeAbiFunction::ReadFloatVector);
        self.ins(Ins::LocalTee(wide));
        self.ins(Ins::I64Const(-1));
        self.ins(Ins::I64Ne);
        self.guard_unless_nz(deopt)?;
        self.emit_float_from_wide(format, wide);
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    fn write_float_vector(
        &mut self,
        symbol: usize,
        format: FloatFormat,
        setter: ValueId,
        array: ValueId,
        index: ValueId,
        value: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        self.ctx();
        self.target_word(self.frame.roots().code_slot.0);
        self.word(symbol);
        self.get(setter);
        self.get(array);
        self.get(index);
        self.emit_float_bits(format, value);
        self.word(usize::from(format == FloatFormat::F64));
        self.call(RuntimeAbiFunction::WriteFloatVector);
        self.guard_unless_nz(deopt)
    }

    fn popcount_u64(
        &mut self,
        input: ValueId,
        symbol: usize,
        binding: usize,
        deopt: Option<&DeoptState>,
    ) -> Result<(), UnsupportedOp> {
        let _ = deopt.ok_or(UnsupportedOp)?;
        self.ctx();
        self.target_word(self.frame.roots().code_slot.0);
        self.word(symbol);
        self.word(binding);
        self.get(input);
        self.scratch(0, 2);
        self.call(RuntimeAbiFunction::PrepareVopU64);
        self.guard_unless_nz(deopt)?;
        self.ins(Ins::LocalGet(SCRATCH));
        self.ins(Ins::I64Load(Self::mem(0, 2)));
        self.ins(Ins::I64Popcnt);
        self.box_fixnum();
        Ok(())
    }

    fn emit_word_op(&mut self, op: WordOp) {
        match op {
            WordOp::Constant(_) => unreachable!("constants are substituted at use sites"),
            WordOp::Copy(value) => self.get(value),
            WordOp::Select(cond, yes, no) => {
                self.immediate(yes);
                self.immediate(no);
                self.get(cond);
                self.ins(Ins::Select);
            }
            WordOp::BoxFixnum(value) => {
                self.get(value);
                self.box_fixnum();
            }
            WordOp::Test(value, test) => {
                self.get(value);
                if let Some(mask) = test.mask {
                    self.word(mask as usize);
                    self.ins(Ins::I32And);
                }
                self.word(test.bits as usize);
                self.ins(if test.equal { Ins::I32Eq } else { Ins::I32Ne });
            }
            WordOp::FixnumRange { value, low, span } => {
                self.is_fixnum(value);
                self.get(value);
                self.word(FIXNUM_SHIFT as usize);
                self.ins(Ins::I32ShrS);
                self.ins(Ins::I32Const(low as i32));
                self.ins(Ins::I32Sub);
                self.ins(Ins::I32Const(span as i32));
                self.ins(Ins::I32LeU);
                self.ins(Ins::I32And);
            }
            WordOp::Not(value) => {
                self.get(value);
                self.ins(Ins::I32Eqz);
            }
            WordOp::I64(op, lhs, rhs) => {
                self.get(lhs);
                self.get(rhs);
                self.ins(match op {
                    I64Arithmetic::Add => Ins::I64Add,
                    I64Arithmetic::Sub => Ins::I64Sub,
                    I64Arithmetic::Mul => Ins::I64Mul,
                });
            }
            WordOp::CompareI64(op, lhs, rhs) => {
                self.get(lhs);
                self.get(rhs);
                self.ins(match op {
                    CmpOp::Eq => Ins::I64Eq,
                    CmpOp::Ne => Ins::I64Ne,
                    CmpOp::Lt => Ins::I64LtS,
                    CmpOp::Le => Ins::I64LeS,
                    CmpOp::Gt => Ins::I64GtS,
                    CmpOp::Ge => Ins::I64GeS,
                });
            }
        }
    }

    fn checkpoint(
        &mut self,
        state: &DeoptState,
        values: Option<&[ValueId]>,
        protocol: SliceCharge,
    ) {
        self.poll_backedge(state.pc);
        let slice = self.poll_flag.expect("execution checkpoint uses a quantum");
        let offset =
            zkcl_opt::native::jit_call_context_offsets(WORD_BYTES as i32).slice_steps as u64;
        self.ctx();
        self.load(offset);
        self.ins(Ins::LocalTee(slice));
        self.scope(Ins::If(BlockType::Empty));
        self.ins(Ins::LocalGet(slice));
        self.ins(Ins::I32Load16U(Self::mem(0, 1)));
        self.scope(Ins::If(BlockType::Empty));
        self.ins(Ins::LocalGet(slice));
        self.ins(Ins::LocalGet(slice));
        self.ins(Ins::I32Load16U(Self::mem(0, 1)));
        self.word(protocol.cost().into());
        self.ins(Ins::I32Sub);
        self.ins(Ins::I32Store16(Self::mem(0, 1)));
        self.ins(Ins::Else);
        self.emit_simple_call(&protocol.refill());
        self.ins(Ins::I32Eqz);
        self.scope(Ins::If(BlockType::Empty));
        if let Some(values) = values {
            self.set_values(values);
        }
        self.restore_bytecode(state);
        self.end();
        self.end();
        self.end();
    }

    fn check_handoff(&mut self, deopt: Option<&DeoptState>, live: &[(ValueId, RootSlot)]) {
        if let Some(state) = deopt {
            self.tag();
            self.word(usize::from(
                zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_CALL,
            ));
            self.ins(Ins::I32GeU);
            self.scope(Ins::If(BlockType::Empty));
            self.reload(live);
            self.restore_bytecode(state);
            self.end();
        }
        self.check_result();
    }

    fn restore_bytecode(&mut self, state: &DeoptState) {
        let extra: Vec<_> = self.frame.unwind_restore(state).collect();
        let count = state.registers_to_restore.len() + extra.len();
        let has_floats = state
            .registers_to_restore
            .iter()
            .any(|(_, value)| matches!(value, DeoptRegisterValue::UnboxedFloat { .. }));
        let vals_word = (count + 1) & !1;
        let formats_word = vals_word + 2 * count;

        let store_entry_meta = |this: &mut Self, index: usize, reg: usize, format: u8| {
            this.ins(Ins::LocalGet(SCRATCH));
            this.word(reg);
            this.store(index as u64 * WORD_BYTES);
            if has_floats {
                this.ins(Ins::LocalGet(SCRATCH));
                this.word(usize::from(format));
                this.ins(Ins::I32Store8(Self::mem(
                    formats_word as u64 * WORD_BYTES + index as u64,
                    0,
                )));
            }
        };
        let store_val = |this: &mut Self, index: usize, emit_val: &dyn Fn(&mut Self)| {
            this.ins(Ins::LocalGet(SCRATCH));
            emit_val(this);
            if has_floats {
                this.ins(Ins::I64Store(Self::mem(
                    (vals_word + 2 * index) as u64 * WORD_BYTES,
                    2,
                )));
            } else {
                this.store((count + index) as u64 * WORD_BYTES);
            }
        };

        for (index, &(reg, value)) in state.registers_to_restore.iter().enumerate() {
            let format = match value {
                DeoptRegisterValue::UnboxedFloat { format, .. } => format as u8,
                _ => 0,
            };
            store_entry_meta(self, index, reg as usize, format);
            store_val(self, index, &|this| match value {
                DeoptRegisterValue::TaggedWord(value) => {
                    this.get(value);
                    if has_floats {
                        this.ins(Ins::I64ExtendI32U);
                    }
                }
                DeoptRegisterValue::ConstImmediate(imm) => {
                    this.immediate(imm);
                    if has_floats {
                        this.ins(Ins::I64ExtendI32U);
                    }
                }
                DeoptRegisterValue::UnboxedI64(value) => {
                    this.get(value);
                    this.ins(Ins::I32WrapI64);
                    this.word(FIXNUM_SHIFT as usize);
                    this.ins(Ins::I32Shl);
                    this.word(zkcl_opt::native::TAG_FIXNUM as usize);
                    this.ins(Ins::I32Or);
                    if has_floats {
                        this.ins(Ins::I64ExtendI32U);
                    }
                }
                DeoptRegisterValue::UnboxedFloat { format, value } => {
                    this.emit_float_bits(format, value);
                }
                DeoptRegisterValue::UnboxedI32x4 { .. } => {
                    unreachable!("Wasm recovery does not support SIMD packs")
                }
            });
        }
        for (i, (reg, slot)) in extra.into_iter().enumerate() {
            let index = state.registers_to_restore.len() + i;
            store_entry_meta(self, index, reg, 0);
            store_val(self, index, &|this| {
                this.read_root(slot);
                if has_floats {
                    this.ins(Ins::I64ExtendI32U);
                }
            });
        }

        self.ctx();
        self.target_word(self.frame.roots().code_slot.0);
        self.word(state.pc);
        self.scratch(0, count);
        if has_floats {
            self.scratch(vals_word, 2 * count);
            self.scratch(formats_word, count.div_ceil(4));
            self.word(count);
            self.ins(Ins::LocalGet(RESULT));
            self.call(RuntimeAbiFunction::DeoptFloatToVm);
        } else {
            self.scratch(count, count);
            self.word(count);
            self.ins(Ins::LocalGet(RESULT));
            self.call(RuntimeAbiFunction::DeoptToVm);
        }
        self.drop_frame();
        self.ins(Ins::Return);
    }

    fn emit_ssa_call(
        &mut self,
        block: BlockId,
        index: usize,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
        call: &RuntimeCall<'_>,
    ) {
        match call.when {
            CallWhen::ReleaseArguments if self.release_local_arguments() => return,
            CallWhen::OptionalArgument(index) if self.local_optional_argument(index) => return,
            _ => {}
        }
        let site = Some((block, index));
        self.emit_call_operands(call, site);
        self.finish_call(call, inst.deopt.as_ref(), live);
        if let Some(pc) = inst.pc.filter(|_| call.mark_scope) {
            self.emit_simple_call(&RuntimeCall::mark_scope(pc));
        }
    }

    fn emit_call_operands(&mut self, call: &RuntimeCall<'_>, site: Option<(BlockId, usize)>) {
        self.emit_call_operands_with_primary(call, site, Self::single_value);
    }

    fn emit_call_operands_with_primary(
        &mut self,
        call: &RuntimeCall<'_>,
        site: Option<(BlockId, usize)>,
        mut emit_primary: impl FnMut(&mut Self),
    ) {
        use CallOperand::*;
        let catch_slot = |frame: &zkcl_opt::native::NativeFrameLayout| {
            let (block, index) = site.expect("catch call belongs to a block");
            frame.catch_tag_slot(block, index)
        };
        for &operand in call.operands() {
            match operand {
                CatchTag(value) => {
                    self.root_slot(catch_slot(self.frame));
                    self.get(value);
                    self.store(0);
                }
                HandlerStates(targets) => self.store_handler_tables(targets),
                _ => {}
            }
        }
        for &operand in call.operands() {
            match operand {
                Context => self.ctx(),
                Frame => self.frame_index(),
                Code => self.target_word(self.frame.roots().code_slot.0),
                Word(word) => self.word(word),
                Bool(value) => self.word(usize::from(value)),
                Value(value) => self.get(value),
                SingleValue => emit_primary(self),
                Constant(index) => self.constant(index),
                Cell(cell) => self.cell(&cell),
                BoxLocation => self.word(0),
                Words(values) => self.store_values(values),
                Length(values) => self.word(values.len()),
                CatchTag(_) => self.word(catch_slot(self.frame)),
                UnwindState(target) => self.word(self.frame.unwind_table().state_for(target)),
                TagbodyBase => {
                    let (block, index) = site.expect("tagbody call belongs to a block");
                    self.word(self.frame.unwind_table().tagbody_base(block, index));
                }
                HandlerStates(targets) => self.scratch(0, targets.len()),
                HandlerSlots(targets) => self.scratch(targets.len(), targets.len()),
                RootCount => self.word(self.frame.root_slots()),
                ResultOut => self.ins(Ins::LocalGet(RESULT)),
            }
        }
    }

    fn finish_call(
        &mut self,
        call: &RuntimeCall<'_>,
        deopt: Option<&DeoptState>,
        live: &[(ValueId, RootSlot)],
    ) {
        self.call(call.function);
        match call.result {
            CallReturn::Void | CallReturn::Word => {}
            CallReturn::Effect | CallReturn::DebugPoint => self.check_result(),
            CallReturn::One => {
                self.check_result();
                self.single_value();
            }
            CallReturn::ReadCell => {
                self.check_handoff(deopt, live);
                self.single_value();
            }
            CallReturn::LispCall { primary } => {
                self.check_handoff(deopt, live);
                self.receive_call(primary);
            }
            CallReturn::Cleanup => {
                self.check_result();
                self.receive_call(false);
            }
        }
    }

    pub(super) fn emit_simple_call(&mut self, call: &RuntimeCall<'_>) {
        self.emit_call_operands(call, None);
        self.finish_call(call, None, &[]);
    }

    pub(super) fn publish_computed_primary(&mut self, emit_primary: impl FnMut(&mut Self)) {
        let call = RuntimeCall::preserve_single_value();
        self.emit_call_operands_with_primary(&call, None, emit_primary);
        self.finish_call(&call, None, &[]);
    }

    fn store_handler_tables(&mut self, targets: &[BlockId]) {
        for (index, target) in targets.iter().enumerate() {
            self.ins(Ins::LocalGet(SCRATCH));
            self.word(self.frame.unwind_table().state_for(*target));
            self.store(index as u64 * WORD_BYTES);
            self.ins(Ins::LocalGet(SCRATCH));
            self.word(self.frame.handler_condition_slot(*target));
            self.store((targets.len() + index) as u64 * WORD_BYTES);
        }
    }

    fn immediate(&mut self, value: Immediate) {
        self.ins(Ins::I32Const(value.encode_word(32).unwrap() as i32));
    }

    pub(super) fn ctx(&mut self) {
        self.ins(Ins::LocalGet(CONTEXT));
    }

    fn constant(&mut self, index: usize) {
        self.emit_simple_call(&RuntimeCall::constant(index));
    }

    fn capture(&mut self, index: usize) {
        self.ctx();
        self.target_word(self.frame.roots().closure_slot.0);
        self.word(index);
        self.word(0);
        self.call_one(RuntimeAbiFunction::ReadCapture);
    }

    fn cell(&mut self, cell: &CellTarget) {
        match *cell {
            CellTarget::Local(value) => self.get(value),
            CellTarget::Capture(index) => self.capture(index),
        }
    }

    fn store_values(&mut self, values: &[ValueId]) {
        for (index, &value) in values.iter().enumerate() {
            self.ins(Ins::LocalGet(SCRATCH));
            self.get(value);
            self.store(index as u64 * WORD_BYTES);
        }
        self.scratch(0, values.len());
    }

    pub(super) fn emit_numeric_fallback(
        &mut self,
        operation: NativeOp<'_>,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
    ) {
        let NativeOp::Call(call) = operation else {
            unreachable!("numeric fallback is a named call");
        };
        self.emit_call_operands(&call, None);
        self.finish_call(&call, inst.deopt.as_ref(), live);
    }

    pub(super) fn stage_tail_call(&mut self, tail: zkcl_opt::native::TailCall<'_>) {
        use zkcl_opt::native::TailCallee;

        self.ctx();
        match tail.callee {
            TailCallee::Named(symbol) => self.constant(symbol),
            TailCallee::Value(value) => self.get(value),
        }
        let args = tail.args.unwrap_or_default();
        self.store_values(args);
        self.word(args.len());
        self.word(tail.at);
        self.word(usize::from(tail.args.is_none()));
        self.call(RuntimeAbiFunction::StageTailCall);
    }

    pub(super) fn set_values(&mut self, values: &[ValueId]) {
        self.emit_simple_call(&RuntimeCall::set_values(values));
    }

    fn call_result(&mut self, function: RuntimeAbiFunction) {
        self.ins(Ins::LocalGet(RESULT));
        self.call(function);
        self.check_result();
    }

    fn call_one(&mut self, function: RuntimeAbiFunction) {
        self.call_result(function);
        self.single_value();
    }

    fn receive_call(&mut self, primary: bool) {
        self.tag();
        self.word(usize::from(JIT_RETURN_TAG_ONE));
        self.ins(Ins::I32Eq);
        let ty = if primary {
            BlockType::Result(ValType::I32)
        } else {
            BlockType::Empty
        };
        self.scope(Ins::If(ty));
        if primary {
            self.single_value();
            self.ins(Ins::Else);
            self.emit_simple_call(&RuntimeCall::read_value(0));
        } else {
            self.emit_simple_call(&RuntimeCall::preserve_single_value());
        }
        self.end();
    }
}
