use inkwell::values::{BasicValueEnum, FloatValue, IntValue};
use inkwell::{FloatPredicate, IntPredicate};
use zkcl_bytecode::native_abi::I32x4Operation;
use zkcl_opt::{DeoptState, FloatArithOp, FloatFormat, SsaInst, SsaOp, ValueId};

use super::{Function, Result, RuntimeAbiFunction};

impl<'ctx> Function<'_, 'ctx> {
    pub(super) fn float_bits(
        &self,
        format: FloatFormat,
        value: FloatValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        let wide = self.ir.context.i64_type();
        let ty = match format {
            FloatFormat::F32 => self.ir.context.i32_type(),
            FloatFormat::F64 => wide,
        };
        let bits = self.builder.build_bit_cast(value, ty, "")?.into_int_value();
        Ok(self
            .builder
            .build_int_z_extend_or_bit_cast(bits, wide, "")?)
    }

    fn guarded_float_bits(
        &self,
        raw: IntValue<'ctx>,
        format: FloatFormat,
        deopt: Option<&DeoptState>,
    ) -> Result<FloatValue<'ctx>> {
        // Runtime unboxing and vector reads share the finite-float bit protocol.
        let valid = self.builder.build_int_compare(
            IntPredicate::NE,
            raw,
            raw.get_type().const_all_ones(),
            "",
        )?;
        self.guard(valid, deopt)?;
        let raw = match format {
            FloatFormat::F32 => {
                self.builder
                    .build_int_truncate(raw, self.ir.context.i32_type(), "")?
            }
            FloatFormat::F64 => raw,
        };
        Ok(self
            .builder
            .build_bit_cast(raw, self.ir.float_type(format), "")?
            .into_float_value())
    }

    fn checked_float_arithmetic(
        &self,
        format: FloatFormat,
        op: FloatArithOp,
        lhs: ValueId,
        rhs: ValueId,
        deopt: Option<&DeoptState>,
    ) -> Result<FloatValue<'ctx>> {
        let lhs = self.basic_value(lhs)?.into_float_value();
        let rhs = self.basic_value(rhs)?.into_float_value();
        if op == FloatArithOp::Div {
            let nonzero = self.builder.build_float_compare(
                FloatPredicate::ONE,
                rhs,
                self.ir.float_type(format).const_zero(),
                "",
            )?;
            self.guard(nonzero, deopt)?;
        }
        // Each Common Lisp operation rounds separately at its own width.
        let value = match op {
            FloatArithOp::Add => self.builder.build_float_add(lhs, rhs, "")?,
            FloatArithOp::Sub => self.builder.build_float_sub(lhs, rhs, "")?,
            FloatArithOp::Mul => self.builder.build_float_mul(lhs, rhs, "")?,
            FloatArithOp::Div => self.builder.build_float_div(lhs, rhs, "")?,
        };
        let bits = self.float_bits(format, value)?;
        let exponent_mask = match format {
            FloatFormat::F32 => 0x7f80_0000,
            FloatFormat::F64 => 0x7ff0_0000_0000_0000,
        };
        let mask = bits.get_type().const_int(exponent_mask, false);
        let exponent = self.builder.build_and(bits, mask, "")?;
        let finite = self
            .builder
            .build_int_compare(IntPredicate::NE, exponent, mask, "")?;
        self.guard(finite, deopt)?;
        Ok(value)
    }

    pub(super) fn emit_numeric(
        &self,
        inst: &SsaInst,
        op: &SsaOp,
    ) -> Result<Option<BasicValueEnum<'ctx>>> {
        let deopt = inst.deopt.as_ref();
        let at = self.word(inst.pc.unwrap_or(0));
        let result = match *op {
            SsaOp::UnboxFloatGuard { format, value } => {
                let runtime = match format {
                    FloatFormat::F32 => RuntimeAbiFunction::UnboxF32,
                    FloatFormat::F64 => RuntimeAbiFunction::UnboxF64,
                };
                let raw = self.call_word(runtime, &[self.ctx.into(), self.value(value)?.into()])?;
                self.guarded_float_bits(raw, format, deopt)?.into()
            }
            SsaOp::FloatArithChecked {
                format,
                op,
                lhs,
                rhs,
            } => self
                .checked_float_arithmetic(format, op, lhs, rhs, deopt)?
                .into(),
            SsaOp::BoxFloat { format, value } => {
                let runtime = match format {
                    FloatFormat::F32 => RuntimeAbiFunction::BoxF32,
                    FloatFormat::F64 => RuntimeAbiFunction::BoxF64,
                };
                let bits = self.float_bits(format, self.basic_value(value)?.into_float_value())?;
                self.call_one(
                    runtime,
                    &[self.ctx.into(), bits.into(), at.into(), self.result.into()],
                )?
                .into()
            }
            SsaOp::ReadFloatVector {
                symbol,
                format,
                array,
                index,
            } => {
                let raw = self.call_word(
                    RuntimeAbiFunction::ReadFloatVector,
                    &[
                        self.ctx.into(),
                        self.target_word(0)?.into(),
                        self.word(symbol).into(),
                        self.value(array)?.into(),
                        self.value(index)?.into(),
                        self.boolean(format == FloatFormat::F64).into(),
                    ],
                )?;
                self.guarded_float_bits(raw, format, deopt)?.into()
            }
            SsaOp::WriteFloatVector {
                symbol,
                format,
                setter,
                array,
                index,
                value,
            } => {
                let bits = self.float_bits(format, self.basic_value(value)?.into_float_value())?;
                let valid = self.call_word(
                    RuntimeAbiFunction::WriteFloatVector,
                    &[
                        self.ctx.into(),
                        self.target_word(0)?.into(),
                        self.word(symbol).into(),
                        self.value(setter)?.into(),
                        self.value(array)?.into(),
                        self.value(index)?.into(),
                        bits.into(),
                        self.boolean(format == FloatFormat::F64).into(),
                    ],
                )?;
                self.guard(valid, deopt)?;
                return Ok(None);
            }
            SsaOp::UnboxI32x4Guard(value) => {
                let pack = self.scratch.packs.reserve(self, 1)?;
                let valid = self.call_word(
                    RuntimeAbiFunction::UnboxI32x4,
                    &[self.ctx.into(), self.value(value)?.into(), pack.into()],
                )?;
                self.guard(valid, deopt)?;
                self.builder.build_load(self.ir.pack_type(), pack, "")?
            }
            SsaOp::I32x4Binary { op, lhs, rhs } => {
                let lhs = self.basic_value(lhs)?.into_vector_value();
                let rhs = self.basic_value(rhs)?.into_vector_value();
                match op {
                    I32x4Operation::Add => self.builder.build_int_add(lhs, rhs, "")?,
                    I32x4Operation::Sub => self.builder.build_int_sub(lhs, rhs, "")?,
                    I32x4Operation::BitAnd => self.builder.build_and(lhs, rhs, "")?,
                    I32x4Operation::BitOr => self.builder.build_or(lhs, rhs, "")?,
                    I32x4Operation::BitXor => self.builder.build_xor(lhs, rhs, "")?,
                }
                .into()
            }
            SsaOp::BoxI32x4(value) => {
                let bits = self
                    .scratch
                    .packs
                    .write(self, &[self.basic_value(value)?])?;
                self.call_one(
                    RuntimeAbiFunction::BoxI32x4,
                    &[self.ctx.into(), bits.into(), at.into(), self.result.into()],
                )?
                .into()
            }
            _ => return Err(self.error(format!("unsupported SSA operation: {op:?}"))),
        };
        Ok(Some(result))
    }
}
