use wasmparser::Operator;

use super::Trap;

macro_rules! unary {
    ($($name:ident($bits:ident) => $result:expr),* $(,)?) => {
        #[derive(Clone, Copy)]
        pub(super) enum Unary { $($name),* }

        impl Unary {
            pub(super) fn lower(operator: &Operator<'_>) -> Option<Self> {
                Some(match operator {
                    $(Operator::$name => Self::$name,)*
                    Operator::RefIsNull => Self::I64Eqz,
                    _ => return None,
                })
            }

            pub(super) fn eval(self, operand: u64) -> Result<u64, Trap> {
                Ok(match self {
                    $(Self::$name => { let $bits = operand; $result },)*
                })
            }
        }
    };
}

unary! {
    I32Eqz(bits) => u64::from(bits as u32 == 0),
    I64Eqz(bits) => u64::from(bits == 0),
    I32Clz(bits) => u64::from((bits as u32).leading_zeros()),
    I32Ctz(bits) => u64::from((bits as u32).trailing_zeros()),
    I32Popcnt(bits) => u64::from((bits as u32).count_ones()),
    I64Clz(bits) => u64::from(bits.leading_zeros()),
    I64Ctz(bits) => u64::from(bits.trailing_zeros()),
    I64Popcnt(bits) => u64::from(bits.count_ones()),
    I32WrapI64(bits) => u64::from(bits as u32),
    I64ExtendI32S(bits) => bits as i32 as i64 as u64,
    I64ExtendI32U(bits) => u64::from(bits as u32),
    I32Extend8S(bits) => u64::from(bits as i8 as i32 as u32),
    I32Extend16S(bits) => u64::from(bits as i16 as i32 as u32),
    I64Extend8S(bits) => bits as i8 as i64 as u64,
    I64Extend16S(bits) => bits as i16 as i64 as u64,
    I64Extend32S(bits) => bits as i32 as i64 as u64,
    F32Abs(bits) => bits & 0x7fff_ffff,
    F32Neg(bits) => bits ^ 0x8000_0000,
    F64Abs(bits) => bits & 0x7fff_ffff_ffff_ffff,
    F64Neg(bits) => bits ^ 0x8000_0000_0000_0000,
    F32Ceil(bits) => u64::from(f32::from_bits(bits as u32).ceil().to_bits()),
    F32Floor(bits) => u64::from(f32::from_bits(bits as u32).floor().to_bits()),
    F32Trunc(bits) => u64::from(f32::from_bits(bits as u32).trunc().to_bits()),
    F32Nearest(bits) => u64::from(f32::from_bits(bits as u32).round_ties_even().to_bits()),
    F32Sqrt(bits) => u64::from(f32::from_bits(bits as u32).sqrt().to_bits()),
    F64Ceil(bits) => f64::from_bits(bits).ceil().to_bits(),
    F64Floor(bits) => f64::from_bits(bits).floor().to_bits(),
    F64Trunc(bits) => f64::from_bits(bits).trunc().to_bits(),
    F64Nearest(bits) => f64::from_bits(bits).round_ties_even().to_bits(),
    F64Sqrt(bits) => f64::from_bits(bits).sqrt().to_bits(),
    F32ConvertI32S(bits) => u64::from((bits as i32 as f32).to_bits()),
    F32ConvertI32U(bits) => u64::from((bits as u32 as f32).to_bits()),
    F32ConvertI64S(bits) => u64::from((bits as i64 as f32).to_bits()),
    F32ConvertI64U(bits) => u64::from((bits as f32).to_bits()),
    F64ConvertI32S(bits) => (bits as i32 as f64).to_bits(),
    F64ConvertI32U(bits) => (bits as u32 as f64).to_bits(),
    F64ConvertI64S(bits) => (bits as i64 as f64).to_bits(),
    F64ConvertI64U(bits) => (bits as f64).to_bits(),
    F32DemoteF64(bits) => u64::from((f64::from_bits(bits) as f32).to_bits()),
    F64PromoteF32(bits) => f64::from(f32::from_bits(bits as u32)).to_bits(),
    I32TruncF32S(bits) => trunc_int::<32, true, false>(f64::from(f32::from_bits(bits as u32)))?,
    I32TruncF32U(bits) => trunc_int::<32, false, false>(f64::from(f32::from_bits(bits as u32)))?,
    I64TruncF32S(bits) => trunc_int::<64, true, false>(f64::from(f32::from_bits(bits as u32)))?,
    I64TruncF32U(bits) => trunc_int::<64, false, false>(f64::from(f32::from_bits(bits as u32)))?,
    I32TruncF64S(bits) => trunc_int::<32, true, false>(f64::from_bits(bits))?,
    I32TruncF64U(bits) => trunc_int::<32, false, false>(f64::from_bits(bits))?,
    I64TruncF64S(bits) => trunc_int::<64, true, false>(f64::from_bits(bits))?,
    I64TruncF64U(bits) => trunc_int::<64, false, false>(f64::from_bits(bits))?,
    I32TruncSatF32S(bits) => trunc_int::<32, true, true>(f64::from(f32::from_bits(bits as u32)))?,
    I32TruncSatF32U(bits) => trunc_int::<32, false, true>(f64::from(f32::from_bits(bits as u32)))?,
    I64TruncSatF32S(bits) => trunc_int::<64, true, true>(f64::from(f32::from_bits(bits as u32)))?,
    I64TruncSatF32U(bits) => trunc_int::<64, false, true>(f64::from(f32::from_bits(bits as u32)))?,
    I32TruncSatF64S(bits) => trunc_int::<32, true, true>(f64::from_bits(bits))?,
    I32TruncSatF64U(bits) => trunc_int::<32, false, true>(f64::from_bits(bits))?,
    I64TruncSatF64S(bits) => trunc_int::<64, true, true>(f64::from_bits(bits))?,
    I64TruncSatF64U(bits) => trunc_int::<64, false, true>(f64::from_bits(bits))?,
}

fn trunc_int<const BITS: i32, const SIGNED: bool, const SATURATING: bool>(
    value: f64,
) -> Result<u64, Trap> {
    if !SATURATING {
        if value.is_nan() {
            return Err(Trap::InvalidConversion);
        }
        let limit = 2.0f64.powi(BITS - i32::from(SIGNED));
        let min = if SIGNED { -limit } else { 0.0 };
        if value.trunc() < min || value.trunc() >= limit {
            return Err(Trap::IntegerOverflow);
        }
    }
    Ok(match (BITS, SIGNED) {
        (32, true) => u64::from(value as i32 as u32),
        (32, false) => u64::from(value as u32),
        (64, true) => value as i64 as u64,
        (64, false) => value as u64,
        _ => unreachable!(),
    })
}

macro_rules! binary {
    ($($name:ident($left:ident, $right:ident) => $result:expr),* $(,)?) => {
        #[derive(Clone, Copy)]
        pub(super) enum Binary { $($name),* }

        impl Binary {
            pub(super) fn lower(operator: &Operator<'_>) -> Option<Self> {
                Some(match operator {
                    $(Operator::$name => Self::$name,)*
                    _ => return None,
                })
            }

            pub(super) fn eval(self, lhs: u64, rhs: u64) -> Result<u64, Trap> {
                Ok(match self {
                    $(Self::$name => { let ($left, $right) = (lhs, rhs); $result },)*
                })
            }
        }
    };
}

binary! {
    I32Add(lhs, rhs) => u64::from((lhs as u32).wrapping_add(rhs as u32)),
    I32Sub(lhs, rhs) => u64::from((lhs as u32).wrapping_sub(rhs as u32)),
    I32Mul(lhs, rhs) => u64::from((lhs as u32).wrapping_mul(rhs as u32)),
    I64Add(lhs, rhs) => lhs.wrapping_add(rhs),
    I64Sub(lhs, rhs) => lhs.wrapping_sub(rhs),
    I64Mul(lhs, rhs) => lhs.wrapping_mul(rhs),
    I32DivS(lhs, rhs) => u64::from(divide_i32(lhs as i32, rhs as i32)? as u32),
    I32DivU(lhs, rhs) => u64::from((lhs as u32).checked_div(rhs as u32).ok_or(Trap::DivisionByZero)?),
    I64DivS(lhs, rhs) => divide_i64(lhs as i64, rhs as i64)? as u64,
    I64DivU(lhs, rhs) => lhs.checked_div(rhs).ok_or(Trap::DivisionByZero)?,
    I32RemS(lhs, rhs) => u64::from((lhs as i32).wrapping_rem(nonzero(rhs as i32)?) as u32),
    I32RemU(lhs, rhs) => u64::from((lhs as u32).checked_rem(rhs as u32).ok_or(Trap::DivisionByZero)?),
    I64RemS(lhs, rhs) => (lhs as i64).wrapping_rem(nonzero(rhs as i64)?) as u64,
    I64RemU(lhs, rhs) => lhs.checked_rem(rhs).ok_or(Trap::DivisionByZero)?,
    I32And(lhs, rhs) => u64::from(lhs as u32 & rhs as u32),
    I32Or(lhs, rhs) => u64::from(lhs as u32 | rhs as u32),
    I32Xor(lhs, rhs) => u64::from(lhs as u32 ^ rhs as u32),
    I64And(lhs, rhs) => lhs & rhs,
    I64Or(lhs, rhs) => lhs | rhs,
    I64Xor(lhs, rhs) => lhs ^ rhs,
    I32Shl(lhs, rhs) => u64::from((lhs as u32).wrapping_shl(rhs as u32)),
    I32ShrS(lhs, rhs) => u64::from((lhs as i32).wrapping_shr(rhs as u32) as u32),
    I32ShrU(lhs, rhs) => u64::from((lhs as u32).wrapping_shr(rhs as u32)),
    I32Rotl(lhs, rhs) => u64::from((lhs as u32).rotate_left(rhs as u32)),
    I32Rotr(lhs, rhs) => u64::from((lhs as u32).rotate_right(rhs as u32)),
    I64Shl(lhs, rhs) => lhs.wrapping_shl(rhs as u32),
    I64ShrS(lhs, rhs) => (lhs as i64).wrapping_shr(rhs as u32) as u64,
    I64ShrU(lhs, rhs) => lhs.wrapping_shr(rhs as u32),
    I64Rotl(lhs, rhs) => lhs.rotate_left(rhs as u32),
    I64Rotr(lhs, rhs) => lhs.rotate_right(rhs as u32),
    I32Eq(lhs, rhs) => u64::from(lhs as u32 == rhs as u32),
    I32Ne(lhs, rhs) => u64::from(lhs as u32 != rhs as u32),
    I32LtS(lhs, rhs) => u64::from((lhs as i32) < rhs as i32),
    I32LtU(lhs, rhs) => u64::from((lhs as u32) < rhs as u32),
    I32GtS(lhs, rhs) => u64::from(lhs as i32 > rhs as i32),
    I32GtU(lhs, rhs) => u64::from(lhs as u32 > rhs as u32),
    I32LeS(lhs, rhs) => u64::from(lhs as i32 <= rhs as i32),
    I32LeU(lhs, rhs) => u64::from(lhs as u32 <= rhs as u32),
    I32GeS(lhs, rhs) => u64::from(lhs as i32 >= rhs as i32),
    I32GeU(lhs, rhs) => u64::from(lhs as u32 >= rhs as u32),
    I64Eq(lhs, rhs) => u64::from(lhs == rhs),
    I64Ne(lhs, rhs) => u64::from(lhs != rhs),
    I64LtS(lhs, rhs) => u64::from((lhs as i64) < rhs as i64),
    I64LtU(lhs, rhs) => u64::from(lhs < rhs),
    I64GtS(lhs, rhs) => u64::from(lhs as i64 > rhs as i64),
    I64GtU(lhs, rhs) => u64::from(lhs > rhs),
    I64LeS(lhs, rhs) => u64::from(lhs as i64 <= rhs as i64),
    I64LeU(lhs, rhs) => u64::from(lhs <= rhs),
    I64GeS(lhs, rhs) => u64::from(lhs as i64 >= rhs as i64),
    I64GeU(lhs, rhs) => u64::from(lhs >= rhs),
    F32Add(lhs, rhs) => f32_bits(f32_value(lhs) + f32_value(rhs)),
    F32Sub(lhs, rhs) => f32_bits(f32_value(lhs) - f32_value(rhs)),
    F32Mul(lhs, rhs) => f32_bits(f32_value(lhs) * f32_value(rhs)),
    F32Div(lhs, rhs) => f32_bits(f32_value(lhs) / f32_value(rhs)),
    F32Min(lhs, rhs) => f32_bits(f32_min(f32_value(lhs), f32_value(rhs))),
    F32Max(lhs, rhs) => f32_bits(f32_max(f32_value(lhs), f32_value(rhs))),
    F32Copysign(lhs, rhs) => f32_bits(f32_value(lhs).copysign(f32_value(rhs))),
    F32Eq(lhs, rhs) => u64::from(f32_value(lhs) == f32_value(rhs)),
    F32Ne(lhs, rhs) => u64::from(f32_value(lhs) != f32_value(rhs)),
    F32Lt(lhs, rhs) => u64::from(f32_value(lhs) < f32_value(rhs)),
    F32Gt(lhs, rhs) => u64::from(f32_value(lhs) > f32_value(rhs)),
    F32Le(lhs, rhs) => u64::from(f32_value(lhs) <= f32_value(rhs)),
    F32Ge(lhs, rhs) => u64::from(f32_value(lhs) >= f32_value(rhs)),
    F64Add(lhs, rhs) => (f64::from_bits(lhs) + f64::from_bits(rhs)).to_bits(),
    F64Sub(lhs, rhs) => (f64::from_bits(lhs) - f64::from_bits(rhs)).to_bits(),
    F64Mul(lhs, rhs) => (f64::from_bits(lhs) * f64::from_bits(rhs)).to_bits(),
    F64Div(lhs, rhs) => (f64::from_bits(lhs) / f64::from_bits(rhs)).to_bits(),
    F64Min(lhs, rhs) => f64_min(f64::from_bits(lhs), f64::from_bits(rhs)).to_bits(),
    F64Max(lhs, rhs) => f64_max(f64::from_bits(lhs), f64::from_bits(rhs)).to_bits(),
    F64Copysign(lhs, rhs) => f64::from_bits(lhs).copysign(f64::from_bits(rhs)).to_bits(),
    F64Eq(lhs, rhs) => u64::from(f64::from_bits(lhs) == f64::from_bits(rhs)),
    F64Ne(lhs, rhs) => u64::from(f64::from_bits(lhs) != f64::from_bits(rhs)),
    F64Lt(lhs, rhs) => u64::from(f64::from_bits(lhs) < f64::from_bits(rhs)),
    F64Gt(lhs, rhs) => u64::from(f64::from_bits(lhs) > f64::from_bits(rhs)),
    F64Le(lhs, rhs) => u64::from(f64::from_bits(lhs) <= f64::from_bits(rhs)),
    F64Ge(lhs, rhs) => u64::from(f64::from_bits(lhs) >= f64::from_bits(rhs)),
}

fn nonzero<T: Default + PartialEq>(rhs: T) -> Result<T, Trap> {
    if rhs == T::default() {
        Err(Trap::DivisionByZero)
    } else {
        Ok(rhs)
    }
}

fn divide_i32(lhs: i32, rhs: i32) -> Result<i32, Trap> {
    lhs.checked_div(nonzero(rhs)?).ok_or(Trap::IntegerOverflow)
}

fn divide_i64(lhs: i64, rhs: i64) -> Result<i64, Trap> {
    lhs.checked_div(nonzero(rhs)?).ok_or(Trap::IntegerOverflow)
}

fn f32_value(bits: u64) -> f32 {
    f32::from_bits(bits as u32)
}

fn f32_bits(value: f32) -> u64 {
    u64::from(value.to_bits())
}

macro_rules! float_min_max {
    ($ty:ty, $min:ident, $max:ident) => {
        fn $min(lhs: $ty, rhs: $ty) -> $ty {
            if lhs.is_nan() || rhs.is_nan() {
                <$ty>::NAN
            } else if lhs == rhs {
                <$ty>::from_bits(lhs.to_bits() | rhs.to_bits())
            } else {
                lhs.min(rhs)
            }
        }

        fn $max(lhs: $ty, rhs: $ty) -> $ty {
            if lhs.is_nan() || rhs.is_nan() {
                <$ty>::NAN
            } else if lhs == rhs {
                <$ty>::from_bits(lhs.to_bits() & rhs.to_bits())
            } else {
                lhs.max(rhs)
            }
        }
    };
}

float_min_max!(f32, f32_min, f32_max);
float_min_max!(f64, f64_min, f64_max);
