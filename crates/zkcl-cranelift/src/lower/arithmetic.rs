use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{InstBuilder, Type, Value};
use cranelift_frontend::FunctionBuilder;
use zkcl_opt::native::{FIXNUM_SHIFT, TAG_FIXNUM, TAG_MASK, WORD_NIL};

use super::{is_fixnum, word_const};

#[derive(Clone, Copy)]
pub(super) enum CheckedFixnumArithmetic {
    Add,
    Sub,
    Mul,
}

impl CheckedFixnumArithmetic {
    pub(super) fn emit(
        self,
        builder: &mut FunctionBuilder<'_>,
        left: Value,
        right: Value,
        portable: bool,
    ) -> (Value, Value) {
        if !portable {
            return match self {
                Self::Add => builder.ins().sadd_overflow(left, right),
                Self::Sub => builder.ins().ssub_overflow(left, right),
                Self::Mul => builder.ins().smul_overflow(left, right),
            };
        }

        match self {
            Self::Add => {
                let result = builder.ins().iadd(left, right);
                // Overflow means both inputs have the opposite sign to the sum.
                let left_changed = builder.ins().bxor(left, result);
                let right_changed = builder.ins().bxor(right, result);
                let changed = builder.ins().band(left_changed, right_changed);
                let overflow = builder.ins().icmp_imm(IntCC::SignedLessThan, changed, 0);
                (result, overflow)
            }
            Self::Sub => {
                let result = builder.ins().isub(left, right);
                // Only opposite-sign inputs can overflow, changing the left input's sign.
                let different = builder.ins().bxor(left, right);
                let changed = builder.ins().bxor(left, result);
                let changed = builder.ins().band(different, changed);
                let overflow = builder.ins().icmp_imm(IntCC::SignedLessThan, changed, 0);
                (result, overflow)
            }
            Self::Mul => {
                let result = builder.ins().imul(left, right);
                let high = builder.ins().smulhi(left, right);
                let bits = builder.func.dfg.value_type(result).bits();
                let sign = builder.ins().sshr_imm(result, i64::from(bits - 1));
                // A signed product fits when its high word sign-extends its low word.
                let overflow = builder.ins().icmp(IntCC::NotEqual, high, sign);
                (result, overflow)
            }
        }
    }
}

pub(super) fn try_fixnum_arithmetic(
    builder: &mut FunctionBuilder<'_>,
    op: zkcl_bytecode::FixnumArithmetic,
    lhs: Value,
    rhs: Value,
    bound: Value,
    word: Type,
    portable: bool,
) -> Value {
    use zkcl_bytecode::FixnumArithmetic::*;

    let left_fixnum = is_fixnum(builder, lhs);
    let right_fixnum = is_fixnum(builder, rhs);
    let fixnums = builder.ins().band(left_fixnum, right_fixnum);
    // Keep the result scaled by 2^FIXNUM_SHIFT. Signed word overflow then
    // checks exactly the target fixnum interval, including for multiplication.
    let left = builder.ins().band_imm(lhs, !(TAG_MASK as i64));
    let (arithmetic, right) = match op {
        Add | Inc => (
            CheckedFixnumArithmetic::Add,
            builder.ins().band_imm(rhs, !(TAG_MASK as i64)),
        ),
        Sub | Dec => (
            CheckedFixnumArithmetic::Sub,
            builder.ins().band_imm(rhs, !(TAG_MASK as i64)),
        ),
        Mul => (
            CheckedFixnumArithmetic::Mul,
            builder.ins().sshr_imm(rhs, i64::from(FIXNUM_SHIFT)),
        ),
    };
    let (result, overflow) = arithmetic.emit(builder, left, right, portable);
    let tagged = builder.ins().bor_imm(result, TAG_FIXNUM as i64);
    let bound_fixnums = builder.ins().band(bound, fixnums);
    let valid = builder.ins().band_not(bound_fixnums, overflow);
    let nil = word_const(builder, word, WORD_NIL);
    builder.ins().select(valid, tagged, nil)
}
