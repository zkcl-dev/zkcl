use std::cmp::Ordering;
use std::ops::Range;

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{Float, FromPrimitive, Signed, ToPrimitive, Zero};

use super::{Primitive, Values};
use crate::condition::{Class, Fault};
use crate::heap::{Heap, RootSlots, Word};
use crate::number::{MachineReal, Number, NumberKind, Real};
use crate::symbol::Name;
use crate::{EvalError, OutOfMemory, Runtime};

mod bitwise;
mod math;
pub(crate) use math::rationalize_real;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    OnePlus,
    OneMinus,
    Ash,
    Expt,
    NumberP,
    RealP,
    RationalP,
    IntegerP,
    FloatP,
    ComplexP,
    ZeroP,
    PlusP,
    MinusP,
    OddP,
    EvenP,
    Complex,
    RealPart,
    ImagPart,
    Numerator,
    Denominator,
    Abs,
    Signum,
    IntegerLength,
    Conjugate,
    Float,
    Rational,
    Max,
    Min,
    Floor,
    Ceiling,
    Truncate,
    Round,
    Ffloor,
    Fceiling,
    Ftruncate,
    Fround,
    Mod,
    Rem,
    DecodeFloat,
    IntegerDecodeFloat,
    ScaleFloat,
    FloatRadix,
    FloatSign,
    FloatDigits,
    FloatPrecision,
    Rationalize,
    UpgradedComplexPartType,
    Exp,
    Log,
    Sqrt,
    Isqrt,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Cis,
    Phase,
    Gcd,
    Lcm,
    Boole,
    Logand,
    Logior,
    Logxor,
    Logeqv,
    Lognand,
    Lognor,
    Logandc1,
    Logandc2,
    Logorc1,
    Logorc2,
    Lognot,
    Logtest,
    Logbitp,
    Logcount,
    Byte,
    ByteSize,
    BytePosition,
    Ldb,
    LdbTest,
    Dpb,
    DepositField,
    MaskField,
    ParseInteger,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("+", Op::Add),
    ("-", Op::Sub),
    ("*", Op::Mul),
    ("/", Op::Div),
    ("=", Op::Eq),
    ("/=", Op::Ne),
    ("<", Op::Lt),
    ("<=", Op::Le),
    (">", Op::Gt),
    (">=", Op::Ge),
    ("1+", Op::OnePlus),
    ("1-", Op::OneMinus),
    ("ASH", Op::Ash),
    ("EXPT", Op::Expt),
    ("NUMBERP", Op::NumberP),
    ("REALP", Op::RealP),
    ("RATIONALP", Op::RationalP),
    ("INTEGERP", Op::IntegerP),
    ("FLOATP", Op::FloatP),
    ("COMPLEXP", Op::ComplexP),
    ("ZEROP", Op::ZeroP),
    ("PLUSP", Op::PlusP),
    ("MINUSP", Op::MinusP),
    ("ODDP", Op::OddP),
    ("EVENP", Op::EvenP),
    ("COMPLEX", Op::Complex),
    ("REALPART", Op::RealPart),
    ("IMAGPART", Op::ImagPart),
    ("NUMERATOR", Op::Numerator),
    ("DENOMINATOR", Op::Denominator),
    ("ABS", Op::Abs),
    ("SIGNUM", Op::Signum),
    ("INTEGER-LENGTH", Op::IntegerLength),
    ("CONJUGATE", Op::Conjugate),
    ("FLOAT", Op::Float),
    ("RATIONAL", Op::Rational),
    ("MAX", Op::Max),
    ("MIN", Op::Min),
    ("FLOOR", Op::Floor),
    ("CEILING", Op::Ceiling),
    ("TRUNCATE", Op::Truncate),
    ("ROUND", Op::Round),
    ("FFLOOR", Op::Ffloor),
    ("FCEILING", Op::Fceiling),
    ("FTRUNCATE", Op::Ftruncate),
    ("FROUND", Op::Fround),
    ("MOD", Op::Mod),
    ("REM", Op::Rem),
    ("DECODE-FLOAT", Op::DecodeFloat),
    ("INTEGER-DECODE-FLOAT", Op::IntegerDecodeFloat),
    ("SCALE-FLOAT", Op::ScaleFloat),
    ("FLOAT-RADIX", Op::FloatRadix),
    ("FLOAT-SIGN", Op::FloatSign),
    ("FLOAT-DIGITS", Op::FloatDigits),
    ("FLOAT-PRECISION", Op::FloatPrecision),
    ("RATIONALIZE", Op::Rationalize),
    ("UPGRADED-COMPLEX-PART-TYPE", Op::UpgradedComplexPartType),
    ("EXP", Op::Exp),
    ("LOG", Op::Log),
    ("SQRT", Op::Sqrt),
    ("ISQRT", Op::Isqrt),
    ("SIN", Op::Sin),
    ("COS", Op::Cos),
    ("TAN", Op::Tan),
    ("ASIN", Op::Asin),
    ("ACOS", Op::Acos),
    ("ATAN", Op::Atan),
    ("SINH", Op::Sinh),
    ("COSH", Op::Cosh),
    ("TANH", Op::Tanh),
    ("ASINH", Op::Asinh),
    ("ACOSH", Op::Acosh),
    ("ATANH", Op::Atanh),
    ("CIS", Op::Cis),
    ("PHASE", Op::Phase),
    ("GCD", Op::Gcd),
    ("LCM", Op::Lcm),
    ("BOOLE", Op::Boole),
    ("LOGAND", Op::Logand),
    ("LOGIOR", Op::Logior),
    ("LOGXOR", Op::Logxor),
    ("LOGEQV", Op::Logeqv),
    ("LOGNAND", Op::Lognand),
    ("LOGNOR", Op::Lognor),
    ("LOGANDC1", Op::Logandc1),
    ("LOGANDC2", Op::Logandc2),
    ("LOGORC1", Op::Logorc1),
    ("LOGORC2", Op::Logorc2),
    ("LOGNOT", Op::Lognot),
    ("LOGTEST", Op::Logtest),
    ("LOGBITP", Op::Logbitp),
    ("LOGCOUNT", Op::Logcount),
    ("BYTE", Op::Byte),
    ("BYTE-SIZE", Op::ByteSize),
    ("BYTE-POSITION", Op::BytePosition),
    ("LDB", Op::Ldb),
    ("LDB-TEST", Op::LdbTest),
    ("DPB", Op::Dpb),
    ("DEPOSIT-FIELD", Op::DepositField),
    ("MASK-FIELD", Op::MaskField),
    ("PARSE-INTEGER", Op::ParseInteger),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Add
            | Self::Mul
            | Self::Gcd
            | Self::Lcm
            | Self::Logand
            | Self::Logior
            | Self::Logxor
            | Self::Logeqv => true,
            Self::Sub
            | Self::Div
            | Self::Eq
            | Self::Ne
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::Max
            | Self::Min
            | Self::ParseInteger => count >= 1,
            Self::Ash
            | Self::Expt
            | Self::Mod
            | Self::Rem
            | Self::ScaleFloat
            | Self::Lognand
            | Self::Lognor
            | Self::Logandc1
            | Self::Logandc2
            | Self::Logorc1
            | Self::Logorc2
            | Self::Logtest
            | Self::Logbitp
            | Self::Byte
            | Self::Ldb
            | Self::LdbTest
            | Self::MaskField => count == 2,
            Self::Boole | Self::Dpb | Self::DepositField => count == 3,
            Self::Complex
            | Self::Float
            | Self::Floor
            | Self::Ceiling
            | Self::Truncate
            | Self::Round
            | Self::Ffloor
            | Self::Fceiling
            | Self::Ftruncate
            | Self::Fround
            | Self::FloatSign
            | Self::Log
            | Self::Atan
            | Self::UpgradedComplexPartType => (1..=2).contains(&count),
            _ => count == 1,
        }
    }

    fn name(self) -> &'static str {
        PRIMITIVES.iter().find(|&&(_, op)| op == self).unwrap().0
    }

    fn comparison(self, order: Ordering) -> bool {
        match self {
            Self::Eq => order.is_eq(),
            Self::Ne => order.is_ne(),
            Self::Lt => order.is_lt(),
            Self::Le => !order.is_gt(),
            Self::Gt => order.is_gt(),
            Self::Ge => !order.is_lt(),
            _ => unreachable!("ordered comparison operator"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Precision {
    Exact,
    Single,
    Double,
}

pub(super) fn precision(value: &Real) -> Precision {
    match value {
        Real::Integer(_) | Real::Ratio(_) => Precision::Exact,
        Real::Single(_) => Precision::Single,
        Real::Double(_) => Precision::Double,
    }
}

fn zero(value: &Real) -> bool {
    match value {
        Real::Integer(number) => number.is_zero(),
        Real::Ratio(number) => number.is_zero(),
        Real::Single(number) => *number == 0.0,
        Real::Double(number) => *number == 0.0,
    }
}

fn exact(value: Real) -> BigRational {
    match value {
        Real::Integer(number) => BigRational::from_integer(number),
        Real::Ratio(number) => number,
        _ => unreachable!("expected an exact number"),
    }
}

fn rational(value: BigRational) -> Real {
    if value.is_integer() {
        Real::Integer(value.into_raw().0)
    } else {
        Real::Ratio(value)
    }
}

pub(crate) fn real_rational(value: &Real) -> BigRational {
    value.as_rational().expect("finite real number")
}

fn machine_integer_float_cmp(integer: i64, float: f64) -> Ordering {
    let order = (integer as f64).partial_cmp(&float).expect("finite float");
    if order.is_eq() && integer.unsigned_abs() > 1 << f64::MANTISSA_DIGITS {
        // A rounded i64 can tie a neighboring float, including +2^63.
        // That float is integral and fits exactly in i128.
        i128::from(integer).cmp(&(float as i128))
    } else {
        order
    }
}

fn integer_float_cmp(integer: &BigInt, float: f64) -> Ordering {
    if let Some(n) = integer.to_i64() {
        return machine_integer_float_cmp(n, float);
    }
    // All binary64 values with a fractional part have magnitude below 2^52.
    // For an integer outside i64, truncating the float preserves the ordering.
    integer.cmp(&BigInt::from_f64(float).expect("finite float"))
}

#[test]
fn integer_float_order_matches_the_exact_dyadic_value() {
    let mut integers = vec![
        BigInt::from(0),
        BigInt::from(i64::MIN),
        BigInt::from(i64::MAX),
    ];
    let mut floats = vec![0.0, -0.0, 0.5, f64::from_bits(1), f64::MAX];
    for exponent in [24, 52, 53, 60, 63, 64, 100, 1023] {
        let power = BigInt::from(1) << exponent as usize;
        for offset in -2..=2 {
            let n: BigInt = &power + offset;
            integers.push(-&n);
            integers.push(n);
        }
        let f = 2.0_f64.powi(exponent);
        floats.extend([f.next_down(), f, f.next_up()]);
    }
    for f in floats {
        for signed in [f, -f] {
            let exact = BigRational::from_float(signed).unwrap();
            for n in &integers {
                assert_eq!(
                    integer_float_cmp(n, signed),
                    BigRational::from_integer(n.clone()).cmp(&exact),
                    "{n} vs {signed:e}"
                );
            }
        }
    }
}

pub(crate) fn real_cmp(left: &Real, right: &Real) -> Ordering {
    match (left, right) {
        (Real::Integer(left), Real::Integer(right)) => left.cmp(right),
        (Real::Ratio(left), Real::Ratio(right)) => left.cmp(right),
        (Real::Single(left), Real::Single(right)) => {
            left.partial_cmp(right).expect("finite floats")
        }
        (Real::Double(left), Real::Double(right)) => {
            left.partial_cmp(right).expect("finite floats")
        }
        (Real::Single(left), Real::Double(right)) => {
            f64::from(*left).partial_cmp(right).expect("finite floats")
        }
        (Real::Double(left), Real::Single(right)) => {
            left.partial_cmp(&f64::from(*right)).expect("finite floats")
        }
        (Real::Integer(n), Real::Single(f)) => integer_float_cmp(n, f64::from(*f)),
        (Real::Integer(n), Real::Double(f)) => integer_float_cmp(n, *f),
        (Real::Single(f), Real::Integer(n)) => integer_float_cmp(n, f64::from(*f)).reverse(),
        (Real::Double(f), Real::Integer(n)) => integer_float_cmp(n, *f).reverse(),
        (Real::Ratio(ratio), other) => ratio.cmp(&real_rational(other)),
        (other, Real::Ratio(ratio)) => real_rational(other).cmp(ratio),
    }
}

pub(crate) fn numerical_equal(left: &Number, right: &Number) -> bool {
    match (left, right) {
        (Number::Real(left), Number::Real(right)) => real_cmp(left, right).is_eq(),
        (Number::Complex(lr, li), Number::Complex(rr, ri)) => {
            real_cmp(lr, rr).is_eq() && real_cmp(li, ri).is_eq()
        }
        (Number::Complex(re, im), Number::Real(real))
        | (Number::Real(real), Number::Complex(re, im)) => zero(im) && real_cmp(re, real).is_eq(),
    }
}

fn finite<T: Float>(value: T) -> Result<T, Class> {
    if value.is_finite() {
        Ok(value)
    } else if value.is_infinite() {
        Err(Class::FloatingPointOverflow)
    } else {
        Err(Class::FloatingPointInvalidOperation)
    }
}

fn single(value: &Real) -> Result<f32, Class> {
    finite(match value {
        Real::Integer(number) => number.to_f32().ok_or(Class::FloatingPointOverflow)?,
        Real::Ratio(number) => ratio_to_f32(number),
        Real::Single(number) => *number,
        Real::Double(number) => *number as f32,
    })
}

fn ratio_to_f32(value: &BigRational) -> f32 {
    let wide = value.to_f64().expect("conversion of a finite rational");
    let magnitude = wide.abs();
    let nearest = magnitude as f32;
    let widened = f64::from(nearest);
    if magnitude == widened {
        return wide as f32;
    }
    let (lower, upper) = if magnitude < widened {
        (nearest.next_down(), nearest)
    } else {
        (nearest, nearest.next_up())
    };
    // At the overflow boundary the next mathematical value is 2^128.
    let upper_wide = if upper.is_infinite() {
        2.0_f64.powi(f32::MAX_EXP)
    } else {
        f64::from(upper)
    };
    let midpoint = (f64::from(lower) + upper_wide) * 0.5;
    let rounded = if magnitude == midpoint {
        // Rounding through f64 can change the side of a midpoint only
        // when the rounded f64 equals it. The cast already handles ties.
        let midpoint = BigRational::from_float(midpoint).expect("finite f32 midpoint");
        match value.abs().cmp(&midpoint) {
            Ordering::Less => lower,
            Ordering::Greater => upper,
            Ordering::Equal => nearest,
        }
    } else {
        nearest
    };
    rounded.copysign(wide as f32)
}

fn double(value: &Real) -> Result<f64, Class> {
    finite(match value {
        Real::Integer(number) => number.to_f64().ok_or(Class::FloatingPointOverflow)?,
        Real::Ratio(number) => number.to_f64().ok_or(Class::FloatingPointOverflow)?,
        Real::Single(number) => f64::from(*number),
        Real::Double(number) => *number,
    })
}

pub(super) fn coerce(value: Real, format: Precision) -> Result<Real, Class> {
    match format {
        Precision::Exact => Ok(value),
        Precision::Single => single(&value).map(Real::Single),
        Precision::Double => double(&value).map(Real::Double),
    }
}

pub(crate) fn complex(real: Real, imaginary: Real) -> Result<Number, Class> {
    let format = precision(&real).max(precision(&imaginary));
    if format == Precision::Exact && zero(&imaginary) {
        return Ok(Number::Real(real));
    }
    Ok(Number::Complex(
        coerce(real, format)?,
        coerce(imaginary, format)?,
    ))
}

fn int(value: isize) -> Real {
    Real::Integer(BigInt::from(value))
}

fn real_int(value: isize) -> Number {
    Number::Real(int(value))
}

fn parts(number: Number) -> (Real, Real) {
    match number {
        Number::Real(real) => {
            let imaginary = match &real {
                Real::Single(float) => Real::Single(0.0 * float),
                Real::Double(float) => Real::Double(0.0 * float),
                _ => int(0),
            };
            (real, imaginary)
        }
        Number::Complex(real, imaginary) => (real, imaginary),
    }
}

fn real_binary(op: Op, left: Real, right: Real) -> Result<Real, Class> {
    match precision(&left).max(precision(&right)) {
        Precision::Single => binary_float(op, single(&left)?, single(&right)?).map(Real::Single),
        Precision::Double => binary_float(op, double(&left)?, double(&right)?).map(Real::Double),
        Precision::Exact if op == Op::Div && zero(&right) => Err(Class::DivisionByZero),
        Precision::Exact => match (left, right) {
            (Real::Integer(left), Real::Integer(right)) => Ok(match op {
                Op::Add => Real::Integer(left + right),
                Op::Sub => Real::Integer(left - right),
                Op::Mul => Real::Integer(left * right),
                Op::Div => rational(BigRational::new(left, right)),
                _ => unreachable!("arithmetic operator"),
            }),
            (left, right) => {
                let (left, right) = (exact(left), exact(right));
                Ok(rational(match op {
                    Op::Add => left + right,
                    Op::Sub => left - right,
                    Op::Mul => left * right,
                    Op::Div => left / right,
                    _ => unreachable!("arithmetic operator"),
                }))
            }
        },
    }
}

fn binary_float<T: Float>(op: Op, left: T, right: T) -> Result<T, Class> {
    finite(match op {
        Op::Add => left + right,
        Op::Sub => left - right,
        Op::Mul => left * right,
        Op::Div if right.is_zero() => return Err(Class::DivisionByZero),
        Op::Div => left / right,
        _ => unreachable!("arithmetic operator"),
    })
}

fn fold_float<T: Float>(
    op: Op,
    mut operands: impl ExactSizeIterator<Item = Result<T, Class>>,
) -> Result<T, Class> {
    let Some(first) = operands.next() else {
        unreachable!("floating arithmetic has at least one operand");
    };
    let first = first?;
    if operands.len() == 0 {
        return match op {
            Op::Sub => Ok(-first),
            Op::Div => binary_float(op, T::one(), first),
            _ => Ok(first),
        };
    }
    operands.try_fold(first, |left, right| binary_float(op, left, right?))
}

fn float_arithmetic(
    op: Op,
    left: MachineReal,
    right: MachineReal,
) -> Option<Result<MachineReal, Class>> {
    use MachineReal::{Double, Fixnum, Single};
    Some(match (left, right) {
        (Single(a), Single(b)) => binary_float(op, a, b).map(Single),
        (Single(a), Double(b)) => binary_float(op, f64::from(a), b).map(Double),
        (Double(a), Single(b)) => binary_float(op, a, f64::from(b)).map(Double),
        (Double(a), Double(b)) => binary_float(op, a, b).map(Double),
        (Fixnum(_), _) | (_, Fixnum(_)) => return None,
    })
}

#[inline]
#[inline]
pub(super) fn machine_comparison(op: Op, left: MachineReal, right: MachineReal) -> Option<bool> {
    use MachineReal::{Double, Fixnum, Single};
    let widen = |value| match value {
        Single(number) => f64::from(number),
        Double(number) => number,
        Fixnum(_) => unreachable!("floating operand"),
    };
    let order = match (left, right) {
        (Fixnum(a), Fixnum(b)) => a.cmp(&b),
        (Fixnum(a), b) => machine_integer_float_cmp(a as i64, widen(b)),
        (a, Fixnum(b)) => machine_integer_float_cmp(b as i64, widen(a)).reverse(),
        (a, b) => widen(a).partial_cmp(&widen(b))?,
    };
    Some(op.comparison(order))
}

fn binary(op: Op, left: Number, right: Number) -> Result<Number, Class> {
    let (left, right) = match (left, right) {
        (Number::Real(left), Number::Real(right)) => {
            return real_binary(op, left, right).map(Number::Real);
        }
        pair => pair,
    };
    let (a, b) = parts(left);
    let (c, d) = parts(right);
    let format = precision(&a).max(precision(&c));
    if format == Precision::Single {
        let (re, im) = complex_float(op, single(&a)?, single(&b)?, single(&c)?, single(&d)?)?;
        return Ok(Number::Complex(Real::Single(re), Real::Single(im)));
    }
    if format == Precision::Double {
        let (re, im) = complex_float(op, double(&a)?, double(&b)?, double(&c)?, double(&d)?)?;
        return Ok(Number::Complex(Real::Double(re), Real::Double(im)));
    }

    let (a, b, c, d) = (exact(a), exact(b), exact(c), exact(d));
    let (re, im) = match op {
        Op::Add => (a + c, b + d),
        Op::Sub => (a - c, b - d),
        Op::Mul => (&a * &c - &b * &d, a * d + b * c),
        Op::Div => divide_exact(&a, &b, &c, &d)?,
        _ => unreachable!("arithmetic operator"),
    };
    complex(rational(re), rational(im))
}

fn divide_exact(
    a: &BigRational,
    b: &BigRational,
    c: &BigRational,
    d: &BigRational,
) -> Result<(BigRational, BigRational), Class> {
    let divisor = c * c + d * d;
    if divisor.is_zero() {
        return Err(Class::DivisionByZero);
    }
    Ok(((a * c + b * d) / &divisor, (b * c - a * d) / divisor))
}

fn complex_float<T: math::FloatOps>(op: Op, a: T, b: T, c: T, d: T) -> Result<(T, T), Class> {
    let (re, im) = match op {
        Op::Add => (a + c, b + d),
        Op::Sub => (a - c, b - d),
        Op::Mul => (a * c - b * d, a * d + b * c),
        Op::Div => return math::divide_float(a, b, c, d),
        _ => unreachable!("arithmetic operator"),
    };
    Ok((finite(re)?, finite(im)?))
}

fn negate(value: Real) -> Real {
    match value {
        Real::Integer(number) => Real::Integer(-number),
        Real::Ratio(number) => Real::Ratio(-number),
        Real::Single(number) => Real::Single(-number),
        Real::Double(number) => Real::Double(-number),
    }
}

fn one_like(number: &Number) -> Number {
    match number {
        Number::Real(Real::Single(_)) => Number::Real(Real::Single(1.0)),
        Number::Real(Real::Double(_)) => Number::Real(Real::Double(1.0)),
        Number::Complex(Real::Single(_), _) => {
            Number::Complex(Real::Single(1.0), Real::Single(0.0))
        }
        Number::Complex(Real::Double(_), _) => {
            Number::Complex(Real::Double(1.0), Real::Double(0.0))
        }
        _ => real_int(1),
    }
}

fn zero_number(number: &Number) -> bool {
    match number {
        Number::Real(real) => zero(real),
        Number::Complex(real, imaginary) => zero(real) && zero(imaginary),
    }
}

fn number_precision(number: &Number) -> Precision {
    match number {
        Number::Real(real) | Number::Complex(real, _) => precision(real),
    }
}

fn coerce_number(number: Number, format: Precision) -> Result<Number, Class> {
    match number {
        Number::Real(real) => Ok(Number::Real(coerce(real, format)?)),
        Number::Complex(re, im) => Ok(Number::Complex(coerce(re, format)?, coerce(im, format)?)),
    }
}

fn ash(heap: &Heap, integer: BigInt, count: &BigInt) -> Result<BigInt, OutOfMemory> {
    if integer.is_zero() {
        return Ok(integer);
    }
    if count.is_negative() {
        let shift = count.magnitude();
        if shift.to_u64().is_none_or(|shift| shift >= integer.bits()) {
            return Ok(BigInt::from(if integer.is_negative() { -1 } else { 0 }));
        }
        return Ok(integer >> shift.to_usize().ok_or(OutOfMemory)?);
    }
    let bits = count.to_u64().ok_or(OutOfMemory)?;
    if bits > heap.max_integer_bits().saturating_sub(integer.bits()) {
        return Err(OutOfMemory);
    }
    Ok(integer << count.to_usize().ok_or(OutOfMemory)?)
}

enum PowerError {
    Arithmetic(Class),
    Memory,
}

impl From<Class> for PowerError {
    fn from(class: Class) -> Self {
        Self::Arithmetic(class)
    }
}

fn expt(heap: &Heap, base: Number, power: Number) -> Result<Number, PowerError> {
    if let Number::Real(Real::Integer(power)) = &power {
        if power.is_zero() {
            return Ok(one_like(&base));
        }
        if zero_number(&base) {
            return if power.is_positive() {
                Ok(base)
            } else {
                Err(Class::DivisionByZero.into())
            };
        }
        if let Number::Real(real) = &base
            && precision(real) == Precision::Exact
        {
            let ratio = real_rational(real);
            let bits = ratio.numer().bits().max(ratio.denom().bits());
            // For a nonunit integer with k bits, its nth power uses at
            // least n*(k-1)+1 bits. Respect the actual Lisp heap limit.
            if bits > 1
                && power.magnitude()
                    > &((heap.max_integer_bits().saturating_sub(1) / (bits - 1)).into())
            {
                return Err(PowerError::Memory);
            }
        }
        let mut factor = if power.is_negative() {
            binary(Op::Div, real_int(1), base)?
        } else {
            base
        };
        let mut result = one_like(&factor);
        let mut exponent = power.magnitude().clone();
        while !exponent.is_zero() {
            if exponent.bit(0) {
                result = binary(Op::Mul, result, factor.clone())?;
                if zero_number(&result) && !zero_number(&factor) {
                    return Err(Class::FloatingPointUnderflow.into());
                }
                check_power_size(heap, &result)?;
            }
            exponent >>= 1usize;
            if !exponent.is_zero() {
                factor = binary(Op::Mul, factor.clone(), factor)?;
                if zero_number(&factor) {
                    return Err(Class::FloatingPointUnderflow.into());
                }
                check_power_size(heap, &factor)?;
            }
        }
        return Ok(result);
    }
    let precision = number_precision(&base).max(number_precision(&power));
    let complex_input = matches!(base, Number::Complex(..)) || matches!(power, Number::Complex(..));
    let base_zero = zero_number(&base);
    let (c, d) = parts(power);
    let (re, im) = if base_zero {
        // Test the real part exactly: converting the exponent to a float can
        // erase a tiny positive value or overflow on a large exact value.
        if zero(&c) && zero(&d) {
            // Integer zero was handled above; the non-integer 0^0 case is unspecified.
            (1.0, complex_input.then_some(0.0))
        } else if real_cmp(&c, &int(0)).is_gt() {
            if precision == Precision::Exact && complex_input {
                return Ok(real_int(0));
            }
            (0.0, complex_input.then_some(0.0))
        } else {
            return Err(Class::DivisionByZero.into());
        }
    } else {
        let (a, b) = parts(base);
        approx_power(&a, &b, &c, &d, complex_input)?
    };
    if precision == Precision::Double {
        power_result(re, im, base_zero)
    } else {
        let re = finite(re as f32)?;
        let im = im.map(|value| finite(value as f32)).transpose()?;
        power_result(re, im, base_zero)
    }
}

fn power_result<T: math::FloatOps>(
    real: T,
    imaginary: Option<T>,
    base_zero: bool,
) -> Result<Number, PowerError> {
    if !base_zero && real.is_zero() && imaginary.is_none_or(|part| part.is_zero()) {
        return Err(Class::FloatingPointUnderflow.into());
    }
    Ok(match imaginary {
        Some(imaginary) => Number::Complex(real.real_from(), imaginary.real_from()),
        None => Number::Real(real.real_from()),
    })
}

fn check_power_size(heap: &Heap, number: &Number) -> Result<(), PowerError> {
    let fits = |real: &Real| {
        let max = heap.max_integer_bits();
        match real {
            Real::Integer(integer) => integer.bits() <= max,
            Real::Ratio(ratio) => ratio.numer().bits() <= max && ratio.denom().bits() <= max,
            _ => true,
        }
    };
    let fits = match number {
        Number::Real(real) => fits(real),
        Number::Complex(real, imaginary) => fits(real) && fits(imaginary),
    };
    if fits {
        Ok(())
    } else {
        Err(PowerError::Memory)
    }
}

fn leading_magnitude(value: &BigInt) -> (f64, u64) {
    let shift = value.bits().saturating_sub(53);
    let top = if shift == 0 {
        value.magnitude().to_f64()
    } else {
        (value.magnitude() >> shift).to_f64()
    };
    (top.expect("53-bit integer fits in f64"), shift)
}

fn ln_real_abs(value: &Real) -> Option<f64> {
    ln_real_abs_scaled(value, false).map(|(magnitude, _)| magnitude)
}

fn scale_log(magnitude: f64, exponent: i128) -> f64 {
    if exponent == 0 {
        magnitude
    } else {
        libm::scalbn(
            magnitude,
            exponent.clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32,
        )
    }
}

fn tiny_ratio_scaled(value: BigRational, keep_scale: bool) -> (f64, i128) {
    // Scale before the ratio can reach the binary64 subnormal range.
    let (value, exponent) = if keep_scale
        && !value.is_zero()
        && let shift = value.denom().bits().saturating_sub(value.numer().bits())
        && shift >= 1000
    {
        let (numerator, denominator) = value.into_raw();
        (
            BigRational::new_raw(numerator << shift, denominator),
            -i128::from(shift),
        )
    } else {
        (value, 0)
    };
    (value.to_f64().expect("bounded ratio"), exponent)
}

// Keep a very small logarithm scaled until the change-of-base division.
// Both logarithms can be below the float range even when their quotient fits.
fn ln_real_abs_scaled(value: &Real, keep_scale: bool) -> Option<(f64, i128)> {
    match value {
        Real::Integer(n) if !n.is_zero() => {
            let (top, shift) = leading_magnitude(n);
            Some((top.ln() + shift as f64 * std::f64::consts::LN_2, 0))
        }
        Real::Ratio(r) if !r.is_zero() => {
            let (n, d) = (r.numer(), r.denom());
            if n.bits().abs_diff(d.bits()) <= 1 {
                // |n|/d lies in (1/4, 4). Subtract exactly before rounding:
                // even when the quotient rounds to one, its logarithm can be normal.
                let delta = BigRational::new_raw(n.abs() - d, d.clone());
                let (delta, scale) = tiny_ratio_scaled(delta, keep_scale);
                if scale != 0 {
                    // At this scale ln(1 + delta/d) agrees with delta/d to
                    // much more than the precision of a binary64 significand.
                    return Some((delta, scale));
                }
                return Some((delta.ln_1p(), 0));
            }
            let (a, sa) = leading_magnitude(n);
            let (b, sb) = leading_magnitude(d);
            let shift = i128::from(sa) - i128::from(sb);
            Some(((a / b).ln() + shift as f64 * std::f64::consts::LN_2, 0))
        }
        _ => None,
    }
}

fn approx_power(
    a_real: &Real,
    b_real: &Real,
    c_real: &Real,
    d_real: &Real,
    complex_input: bool,
) -> Result<(f64, Option<f64>), Class> {
    let c = double(c_real)?;
    let d = double(d_real)?;
    if c == 0.0 && d == 0.0 {
        return Ok((1.0, complex_input.then_some(0.0)));
    }
    let b = double(b_real)?;
    let a_opt = double(a_real);
    if !complex_input && d == 0.0 {
        if let Ok(a) = a_opt {
            if a > 0.0 || (a < 0.0 && c.fract() == 0.0) {
                return Ok((finite(a.powf(c))?, None));
            }
        } else if real_cmp(a_real, &int(0)).is_gt()
            && let Some(ln_a) = ln_real_abs(a_real)
        {
            return Ok((finite((c * ln_a).exp())?, None));
        }
    }
    let phase_imaginary = if complex_input { b } else { 0.0 };
    let (logarithm, argument) = if b == 0.0
        && let Ok(a) = a_opt
        && a != 0.0
    {
        (a.abs().ln(), phase_imaginary.atan2(a))
    } else if b == 0.0
        && let Some(ln_a) = ln_real_abs(a_real)
    {
        let arg = if real_cmp(a_real, &int(0)).is_lt() {
            std::f64::consts::PI
        } else {
            0.0
        };
        (ln_a, arg)
    } else {
        let a = a_opt?;
        (a.hypot(b).ln(), phase_imaginary.atan2(a))
    };
    let magnitude = (c * logarithm - d * argument).exp();
    let phase = c * argument + d * logarithm;
    let (sin, cos) = phase.sin_cos();
    Ok((finite(magnitude * cos)?, Some(finite(magnitude * sin)?)))
}

fn magnitude(number: Number) -> Result<Real, Class> {
    match number {
        Number::Real(real) => Ok(match real {
            Real::Integer(real) => Real::Integer(real.abs()),
            Real::Ratio(real) => Real::Ratio(real.abs()),
            Real::Single(real) => Real::Single(real.abs()),
            Real::Double(real) => Real::Double(real.abs()),
        }),
        Number::Complex(Real::Single(re), Real::Single(im)) => {
            finite(re.hypot(im)).map(Real::Single)
        }
        Number::Complex(Real::Double(re), Real::Double(im)) => {
            finite(re.hypot(im)).map(Real::Double)
        }
        Number::Complex(re, im) => {
            let (re, im) = (exact(re), exact(im));
            let squared = &re * &re + &im * &im;
            let numerator = squared.numer().sqrt();
            let denominator = squared.denom().sqrt();
            if &numerator * &numerator == *squared.numer()
                && &denominator * &denominator == *squared.denom()
            {
                Ok(rational(BigRational::new(numerator, denominator)))
            } else {
                // Take the root before converting to single precision to
                // avoid overflowing on a representable magnitude squared.
                finite(squared.to_f64().ok_or(Class::FloatingPointOverflow)?.sqrt() as f32)
                    .map(Real::Single)
            }
        }
    }
}

fn signum(number: Number) -> Result<Number, Class> {
    match number {
        Number::Real(Real::Integer(number)) => Ok(Number::Real(Real::Integer(number.signum()))),
        Number::Real(Real::Ratio(number)) => {
            Ok(real_int(if number.is_negative() { -1 } else { 1 }))
        }
        Number::Real(Real::Single(number)) => Ok(Number::Real(Real::Single(if number == 0.0 {
            number
        } else {
            number.signum()
        }))),
        Number::Real(Real::Double(number)) => Ok(Number::Real(Real::Double(if number == 0.0 {
            number
        } else {
            number.signum()
        }))),
        number if zero_number(&number) => Ok(number),
        number => {
            let divisor = magnitude(number.clone())?;
            binary(Op::Div, number, Number::Real(divisor))
        }
    }
}

fn fast_fixnums(op: Op, frame: &RootSlots, args: Range<usize>) -> Option<Word> {
    let mut numbers = args.map(|reg| frame.get(reg).as_fixnum());
    match op {
        Op::Add | Op::Mul | Op::Sub | Op::Div => {
            let Some(first) = numbers.next() else {
                return Word::fixnum(if op == Op::Mul { 1 } else { 0 });
            };
            let mut result = first?;
            if numbers.len() == 0 {
                result = match op {
                    Op::Sub => result.checked_neg()?,
                    Op::Div => divide_fixnums(1, result)?,
                    _ => result,
                };
            }
            for number in numbers {
                result = arithmetic_fixnums(op, result, number?)?;
            }
            Word::fixnum(result)
        }
        Op::OnePlus => Word::fixnum(numbers.next()??.checked_add(1)?),
        Op::OneMinus => Word::fixnum(numbers.next()??.checked_sub(1)?),
        Op::ZeroP => Some(truth(numbers.next()?? == 0)),
        Op::PlusP => Some(truth(numbers.next()?? > 0)),
        Op::MinusP => Some(truth(numbers.next()?? < 0)),
        Op::OddP => Some(truth(numbers.next()?? & 1 != 0)),
        Op::EvenP => Some(truth(numbers.next()?? & 1 == 0)),
        Op::Abs => Word::fixnum(numbers.next()??.checked_abs()?),
        Op::IntegerLength => {
            let value = numbers.next()??;
            Word::fixnum(if value < 0 {
                isize::try_from(usize::BITS - ((!value) as usize).leading_zeros()).ok()?
            } else {
                isize::try_from(usize::BITS - (value as usize).leading_zeros()).ok()?
            })
        }
        Op::Signum => Word::fixnum(numbers.next()??.signum()),
        Op::Expt => {
            let base = numbers.next()??;
            let exponent = u32::try_from(numbers.next()??).ok()?;
            Word::fixnum(base.checked_pow(exponent)?)
        }
        Op::Ash => {
            let integer = numbers.next()??;
            let count = numbers.next()??;
            if count < 0 {
                let shifted = u32::try_from(count.unsigned_abs())
                    .ok()
                    .and_then(|count| integer.checked_shr(count))
                    .unwrap_or(if integer < 0 { -1 } else { 0 });
                Word::fixnum(shifted)
            } else {
                let count = u32::try_from(count).ok()?;
                Word::fixnum(integer.checked_mul(2_isize.checked_pow(count)?)?)
            }
        }
        Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            let mut previous = numbers.next()??;
            let mut result = true;
            for current in numbers {
                let current = current?;
                result &= op.comparison(previous.cmp(&current));
                previous = current;
            }
            Some(truth(result))
        }
        _ => None,
    }
}

#[inline]
fn arithmetic_fixnums(op: Op, left: isize, right: isize) -> Option<isize> {
    match op {
        Op::Add => left.checked_add(right),
        Op::Sub => left.checked_sub(right),
        Op::Mul => left.checked_mul(right),
        Op::Div => divide_fixnums(left, right),
        _ => unreachable!("arithmetic operator"),
    }
}

fn divide_fixnums(left: isize, right: isize) -> Option<isize> {
    if left.checked_rem(right)? == 0 {
        left.checked_div(right)
    } else {
        None
    }
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    match op {
        Op::Floor
        | Op::Ceiling
        | Op::Truncate
        | Op::Round
        | Op::Ffloor
        | Op::Fceiling
        | Op::Ftruncate
        | Op::Fround
        | Op::Mod
        | Op::Rem => rounding(runtime, frame, op, args, values, at),
        Op::DecodeFloat | Op::IntegerDecodeFloat => {
            math::run_multi(runtime, frame, op, args, values, at)
        }
        Op::ParseInteger => parse_integer(runtime, frame, args, values, at),
        _ => {
            values.one(run_single(runtime, frame, op, args, at)?);
            Ok(())
        }
    }
}

pub(super) fn oom(error: OutOfMemory) -> EvalError {
    EvalError::out_of_memory(0, error)
}

pub(super) fn run_number(
    runtime: &Runtime,
    frame: &RootSlots,
    reg: usize,
    at: usize,
) -> Result<Number, Fault> {
    let word = frame.get(reg);
    runtime
        .heap
        .number(word)
        .ok_or_else(|| numeric_type_error(runtime, word, "NUMBER", at))
}

pub(super) fn run_real(
    runtime: &Runtime,
    frame: &RootSlots,
    reg: usize,
    at: usize,
) -> Result<Real, Fault> {
    real_arg(runtime, frame.get(reg), at)
}

pub(super) fn run_integer(
    runtime: &Runtime,
    frame: &RootSlots,
    reg: usize,
    at: usize,
) -> Result<BigInt, Fault> {
    let word = frame.get(reg);
    runtime
        .heap
        .integer(word)
        .ok_or_else(|| numeric_type_error(runtime, word, "INTEGER", at))
}

fn numeric_type_error(runtime: &Runtime, word: Word, expected: &'static str, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        word,
        expected,
        EvalError::new(at, format!("expected a {expected}")),
    )
}

fn real_arg(runtime: &Runtime, word: Word, at: usize) -> Result<Real, Fault> {
    match runtime.heap.number(word) {
        Some(Number::Real(real)) => Ok(real),
        _ => Err(numeric_type_error(runtime, word, "REAL", at)),
    }
}

fn parse_integer(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let string_word = frame.get(args.start);
    let owned_text = match runtime.heap.string_contents(string_word) {
        Some(text) => text,
        // A zero-length vector is a string under the vacuous element-type
        // reading; PARSE-INTEGER has nothing to read from it.
        None if runtime.heap.array_info(string_word).is_some_and(|info| {
            info.rank == 1 && info.size == 0 && info.fill_pointer.is_none()
        }) =>
        {
            String::new()
        }
        None => return Err(numeric_type_error(runtime, string_word, "STRING", at)),
    };
    let [start_reg, end_reg, radix_reg, junk_reg] = super::named_keyword_arguments(
        runtime,
        frame,
        args.start + 1..args.end,
        ["START", "END", "RADIX", "JUNK-ALLOWED"],
        at,
    )?;
    let chars: Vec<char> = owned_text.chars().collect();
    let len = chars.len();
    let non_neg = |word: Word| {
        word.as_fixnum()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                Fault::non_negative_integer(
                    runtime,
                    word,
                    EvalError::new(at, "expected a non-negative integer"),
                )
            })
    };
    let start = start_reg
        .map(|r| non_neg(frame.get(r)))
        .transpose()?
        .unwrap_or(0);
    let end = match end_reg.map(|r| frame.get(r)) {
        None | Some(Word::NIL) => len,
        Some(word) => non_neg(word)?,
    };
    if start > end || end > len {
        return Err(Fault::plain(
            Class::Error,
            EvalError::new(at, "PARSE-INTEGER bounding indices are out of bounds"),
        ));
    }
    let radix: u32 = match radix_reg.map(|r| frame.get(r)) {
        None => 10,
        Some(word) => word
            .as_fixnum()
            .filter(|r| (2..=36).contains(r))
            .ok_or_else(|| {
                Fault::integer_range(
                    runtime,
                    word,
                    2,
                    36,
                    EvalError::new(at, "expected a radix in 2..=36"),
                )
            })? as u32,
    };
    let junk_allowed = junk_reg.is_some_and(|r| frame.get(r) != Word::NIL);
    let is_ws = |ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c');

    let mut idx = start;
    while idx < end && is_ws(chars[idx]) {
        idx += 1;
    }
    let mut negative = false;
    if idx < end {
        match chars[idx] {
            '+' => idx += 1,
            '-' => {
                negative = true;
                idx += 1;
            }
            _ => {}
        }
    }
    let mut digits = 0usize;
    let mut acc = BigInt::zero();
    while idx < end {
        let Some(d) = chars[idx].to_digit(radix) else {
            break;
        };
        acc = acc * radix + d;
        digits += 1;
        idx += 1;
    }
    let fail_parse = || Fault::plain(Class::ParseError, EvalError::new(at, "not an integer"));
    if digits == 0 {
        if junk_allowed {
            let pos = isize::try_from(idx)
                .ok()
                .and_then(Word::fixnum)
                .unwrap_or(Word::NIL);
            values.resize(2);
            values.slots.set(0, Word::NIL);
            values.slots.set(1, pos);
            return Ok(());
        }
        return Err(fail_parse());
    }
    if !junk_allowed {
        while idx < end && is_ws(chars[idx]) {
            idx += 1;
        }
        if idx < end {
            return Err(fail_parse());
        }
    }
    let result = if negative { -acc } else { acc };
    if result.bits() > runtime.heap.max_integer_bits() {
        return Err(Fault::plain(
            Class::ArithmeticError,
            EvalError::new(at, "parsed integer exceeds maximum integer size"),
        ));
    }
    let value_word = runtime
        .heap
        .allocate_number(&Number::Real(Real::Integer(result)))
        .map_err(oom)?;
    let pos_word = isize::try_from(idx)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| Fault::from(EvalError::new(at, "string index exceeds fixnum range")))?;
    values.resize(2);
    values.slots.set(0, value_word);
    values.slots.set(1, pos_word);
    Ok(())
}

fn arithmetic_fault(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
    class: Class,
) -> Fault {
    named_arithmetic_fault(runtime, frame, op.name(), args, at, class)
}

pub(super) fn named_arithmetic_fault(
    runtime: &Runtime,
    frame: &RootSlots,
    name: &'static str,
    args: Range<usize>,
    at: usize,
    class: Class,
) -> Fault {
    match runtime.intern(&Name::common_lisp(name), at) {
        Ok(operation) => Fault::arithmetic(
            runtime,
            class,
            operation,
            args.map(|reg| frame.get(reg)),
            EvalError::new(at, format!("{name}: {}", class.name())),
        ),
        Err(error) => error.into(),
    }
}

fn rounding_fixnum(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    (number, divisor): (isize, isize),
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    if divisor == 0 {
        return Err(arithmetic_fault(
            runtime,
            frame,
            op,
            args,
            at,
            Class::DivisionByZero,
        ));
    }
    let mut remainder = number % divisor;
    if matches!(op, Op::Mod | Op::Rem) {
        if op == Op::Mod && remainder != 0 && (remainder < 0) != (divisor < 0) {
            remainder += divisor;
        }
        values.one(Word::fixnum(remainder).expect("remainder fits a fixnum"));
        return Ok(());
    }

    // The fixnum payload leaves room to double the remainder and to
    // represent most-negative-fixnum divided by -1 in an isize.
    let mut quotient = number / divisor;
    if remainder != 0 {
        let negative = (number < 0) != (divisor < 0);
        let adjustment = match op {
            Op::Floor | Op::Ffloor if negative => -1,
            Op::Ceiling | Op::Fceiling if !negative => 1,
            Op::Round | Op::Fround => {
                let order = (2 * remainder.unsigned_abs()).cmp(&divisor.unsigned_abs());
                if order.is_gt() || (order.is_eq() && quotient & 1 != 0) {
                    if negative { -1 } else { 1 }
                } else {
                    0
                }
            }
            _ => 0,
        };
        quotient += adjustment;
        remainder -= adjustment * divisor;
    }
    let heap = &runtime.heap;
    let oom = |error| Fault::from(EvalError::out_of_memory(at, error));
    let quotient = match op {
        Op::Ffloor | Op::Fceiling | Op::Ftruncate | Op::Fround => heap
            .allocate_number(&Number::Real(Real::Single(quotient as f32)))
            .map_err(oom)?,
        _ => match Word::fixnum(quotient) {
            Some(word) => word,
            None => heap
                .allocate_integer(&BigInt::from(quotient))
                .map_err(oom)?,
        },
    };
    values.resize(2);
    values.slots.set(0, quotient);
    values
        .slots
        .set(1, Word::fixnum(remainder).expect("remainder fits a fixnum"));
    Ok(())
}

fn rounding(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let fixnum_divisor = if args.len() == 2 {
        frame.get(args.start + 1).as_fixnum()
    } else {
        Some(1)
    };
    if let (Some(number), Some(divisor)) = (frame.get(args.start).as_fixnum(), fixnum_divisor) {
        return rounding_fixnum(runtime, frame, op, args, (number, divisor), values, at);
    }
    let arithmetic = |class| arithmetic_fault(runtime, frame, op, args.clone(), at, class);
    let number = real_arg(runtime, frame.get(args.start), at)?;
    let divisor = if args.len() == 2 {
        real_arg(runtime, frame.get(args.start + 1), at)?
    } else {
        int(1)
    };
    if zero(&divisor) {
        return Err(arithmetic(Class::DivisionByZero));
    }
    let format = precision(&number).max(precision(&divisor));
    let number = real_rational(&coerce(number, format).map_err(arithmetic)?);
    let divisor = real_rational(&coerce(divisor, format).map_err(arithmetic)?);
    if divisor.is_zero() {
        return Err(arithmetic(Class::DivisionByZero));
    }

    let exact_quotient = &number / &divisor;
    let (mut quotient, part) = exact_quotient.numer().div_rem(exact_quotient.denom());
    match op {
        Op::Floor | Op::Ffloor | Op::Mod if part.is_negative() => quotient -= 1,
        Op::Ceiling | Op::Fceiling if part.is_positive() => quotient += 1,
        Op::Round | Op::Fround => {
            let doubled = part.magnitude() << 1usize;
            let order = doubled.cmp(exact_quotient.denom().magnitude());
            if order.is_gt() || (order.is_eq() && quotient.is_odd()) {
                quotient += part.signum();
            }
        }
        _ => {}
    }
    let remainder = rational(number - divisor * &quotient);
    let remainder = Number::Real(coerce(remainder, format).map_err(arithmetic)?);
    let allocate = |value: &Number| {
        runtime
            .heap
            .allocate_number(value)
            .map_err(|error| Fault::from(EvalError::out_of_memory(at, error)))
    };
    if matches!(op, Op::Mod | Op::Rem) {
        values.one(allocate(&remainder)?);
        return Ok(());
    }

    let quotient_format = match op {
        Op::Ffloor | Op::Fceiling | Op::Ftruncate | Op::Fround => format.max(Precision::Single),
        _ => Precision::Exact,
    };
    let quotient =
        Number::Real(coerce(Real::Integer(quotient), quotient_format).map_err(arithmetic)?);
    let quotient = runtime.root_slot(allocate(&quotient)?);
    let remainder = allocate(&remainder)?;
    values.resize(2);
    values.slots.set(0, quotient.get());
    values.slots.set(1, remainder);
    Ok(())
}

fn run_single(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if let Some(result) = fast_fixnums(op, frame, args.clone()) {
        return Ok(result);
    }
    if args.len() == 2
        && matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div)
        && let Some(left) = runtime.heap.machine_real(frame.get(args.start))
        && let Some(right) = runtime.heap.machine_real(frame.get(args.start + 1))
        && let Some(result) = float_arithmetic(op, left, right)
    {
        return finish_float_arithmetic(runtime, frame, op, args, || at, result);
    }
    run_single_general(runtime, frame, op, args, at)
}

fn finish_float_arithmetic(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: impl FnOnce() -> usize,
    result: Result<MachineReal, Class>,
) -> Result<Word, Fault> {
    let real = match result {
        Ok(real) => real,
        Err(class) => return Err(arithmetic_fault(runtime, frame, op, args, at(), class)),
    };
    runtime
        .heap
        .allocate_machine_float(real)
        .map_err(|error| EvalError::out_of_memory(at(), error).into())
}

pub(super) fn run_decoded_binary(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: impl FnOnce() -> usize,
    [left, right]: [Option<MachineReal>; 2],
) -> Result<Word, Fault> {
    if let (Some(left), Some(right)) = (left, right) {
        if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div) {
            if let (MachineReal::Fixnum(a), MachineReal::Fixnum(b)) = (left, right) {
                if let Some(word) = arithmetic_fixnums(op, a, b).and_then(Word::fixnum) {
                    return Ok(word);
                }
            } else if let Some(result) = float_arithmetic(op, left, right) {
                return finish_float_arithmetic(runtime, frame, op, args, at, result);
            }
        } else if let Some(result) = machine_comparison(op, left, right) {
            return Ok(truth(result));
        }
    }
    run_single_general(runtime, frame, op, args, at())
}

fn numeric_type_predicate(heap: &crate::heap::Heap, word: Word, op: Op) -> Word {
    use NumberKind::{Bignum, Complex, Double, Fixnum, Ratio, Single};

    truth(match op {
        Op::NumberP => heap.number_kind(word).is_some(),
        Op::RealP => matches!(
            heap.number_kind(word),
            Some(Fixnum | Bignum | Ratio | Single | Double)
        ),
        Op::RationalP => matches!(heap.number_kind(word), Some(Fixnum | Bignum | Ratio)),
        Op::IntegerP => matches!(heap.number_kind(word), Some(Fixnum | Bignum)),
        Op::FloatP => matches!(heap.number_kind(word), Some(Single | Double)),
        Op::ComplexP => matches!(heap.number_kind(word), Some(Complex)),
        _ => unreachable!("numeric type predicate"),
    })
}

fn compare_equality_general(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let mut numbers = args.map(|reg| run_number(runtime, frame, reg, at));
    let first = numbers.next().unwrap()?;
    if op == Op::Eq {
        let mut equal = true;
        for next in numbers {
            equal &= numerical_equal(&first, &next?);
        }
        return Ok(truth(equal));
    }
    let mut previous = vec![first];
    let mut distinct = true;
    for next in numbers {
        let next = next?;
        distinct &= !previous.iter().any(|prior| numerical_equal(prior, &next));
        previous.push(next);
    }
    Ok(truth(distinct))
}

fn compare_ordered_general(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let mut previous = real_arg(runtime, frame.get(args.start), at)?;
    let mut ordered = true;
    for reg in args.start + 1..args.end {
        let current = real_arg(runtime, frame.get(reg), at)?;
        ordered &= op.comparison(real_cmp(&previous, &current));
        previous = current;
    }
    Ok(truth(ordered))
}

fn variadic_precision(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(Precision, bool), Fault> {
    use NumberKind::{Complex, Double, Single};

    let heap = &runtime.heap;
    let mut format = Precision::Exact;
    let mut has_complex = false;
    for reg in args {
        let word = frame.get(reg);
        let mut kind = heap
            .number_kind(word)
            .ok_or_else(|| numeric_type_error(runtime, word, "NUMBER", at))?;
        if kind == Complex {
            has_complex = true;
            let (real, _) = heap.complex_fields(word).expect("complex operand");
            kind = heap.number_kind(real).expect("complex component is real");
        }
        format = format.max(match kind {
            Single => Precision::Single,
            Double => Precision::Double,
            _ => Precision::Exact,
        });
    }
    Ok((format, has_complex))
}

fn variadic_float_arithmetic(
    heap: &crate::heap::Heap,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    format: Precision,
) -> Result<Result<Number, Class>, Fault> {
    let decoded = |word| heap.number(word).expect("validated numeric operand");
    let real = |word| match decoded(word) {
        Number::Real(real) => real,
        Number::Complex(..) => unreachable!("validated real operand"),
    };
    let words = args.map(|reg| frame.get(reg));
    let outcome = match format {
        Precision::Single => fold_float(
            op,
            words.map(|word| {
                if let Some(value) = word.as_fixnum() {
                    Ok(value as f32)
                } else if let Some(value) = heap.single_float(word) {
                    finite(value)
                } else {
                    single(&real(word))
                }
            }),
        )
        .map(Real::Single),
        Precision::Double => fold_float(
            op,
            words.map(|word| {
                if let Some(value) = word.as_fixnum() {
                    Ok(value as f64)
                } else if let Some(value) = heap.double_float(word) {
                    finite(value)
                } else if let Some(value) = heap.single_float(word) {
                    finite(f64::from(value))
                } else {
                    double(&real(word))
                }
            }),
        )
        .map(Real::Double),
        Precision::Exact => unreachable!("floating arithmetic"),
    };
    Ok(outcome.map(Number::Real))
}

fn variadic_arithmetic(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let arithmetic = |class| arithmetic_fault(runtime, frame, op, args.clone(), at, class);
    // Validate every operand before arithmetic can signal; float format governs the fold.
    let (format, has_complex) = variadic_precision(runtime, frame, args.clone(), at)?;
    let number = if !has_complex && format != Precision::Exact {
        variadic_float_arithmetic(heap, frame, op, args.clone(), format)?.map_err(arithmetic)?
    } else {
        let mut operands = args
            .clone()
            .map(|reg| coerce_number(heap.number(frame.get(reg)).expect("validated"), format));
        let Some(first) = operands.next().transpose().map_err(arithmetic)? else {
            return Ok(Word::fixnum(if op == Op::Mul { 1 } else { 0 }).unwrap());
        };
        let mut result = first;
        if operands.len() == 0 {
            result = match (op, result) {
                (Op::Sub, Number::Real(real)) => Number::Real(negate(real)),
                (Op::Sub, Number::Complex(re, im)) => Number::Complex(negate(re), negate(im)),
                (Op::Div, number) => binary(Op::Div, real_int(1), number).map_err(arithmetic)?,
                (_, number) => number,
            };
        }
        for next in operands {
            result = binary(op, result, next.map_err(arithmetic)?).map_err(arithmetic)?;
        }
        result
    };
    heap.allocate_number(&number)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn coerce_to_float(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Option<(Real, Precision)>, Fault> {
    let real = real_arg(runtime, frame.get(args.start), at)?;
    let format = if args.len() == 2 {
        match runtime.heap.number_kind(frame.get(args.start + 1)) {
            Some(NumberKind::Single) => Precision::Single,
            Some(NumberKind::Double) => Precision::Double,
            _ => {
                return Err(numeric_type_error(
                    runtime,
                    frame.get(args.start + 1),
                    "FLOAT",
                    at,
                ));
            }
        }
    } else if precision(&real) != Precision::Exact {
        return Ok(None);
    } else {
        Precision::Single
    };
    Ok(Some((real, format)))
}

fn select_extremum(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let mut best = real_arg(runtime, frame.get(args.start), at)?;
    let mut selected = args.start;
    for reg in args.start + 1..args.end {
        let candidate = real_arg(runtime, frame.get(reg), at)?;
        let order = real_cmp(&candidate, &best);
        if (op == Op::Max && order.is_gt()) || (op == Op::Min && order.is_lt()) {
            best = candidate;
            selected = reg;
        }
    }
    Ok(frame.get(selected))
}

fn run_single_general(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if let Some(word) = bitwise::run_word(runtime, frame, op, args.clone(), at)? {
        return Ok(word);
    }
    if let Some(word) = math::run_word(runtime, frame, op, args.clone(), at)? {
        return Ok(word);
    }
    crate::metrics::record_slow_path();
    let heap = &runtime.heap;
    let number = |reg| run_number(runtime, frame, reg, at);
    let real = |reg| run_real(runtime, frame, reg, at);
    let integer = |reg| run_integer(runtime, frame, reg, at);
    let arithmetic = |class| arithmetic_fault(runtime, frame, op, args.clone(), at, class);
    let oom = |error| EvalError::out_of_memory(at, error);
    let result = match op {
        Op::NumberP | Op::RealP | Op::RationalP | Op::IntegerP | Op::FloatP | Op::ComplexP => {
            return Ok(numeric_type_predicate(heap, frame.get(args.start), op));
        }
        Op::ZeroP => return Ok(truth(zero_number(&number(args.start)?))),
        Op::PlusP | Op::MinusP => {
            let order = real_cmp(&real(args.start)?, &int(0));
            return Ok(truth(if op == Op::PlusP {
                order.is_gt()
            } else {
                order.is_lt()
            }));
        }
        Op::OddP | Op::EvenP => {
            return Ok(truth(integer(args.start)?.bit(0) == (op == Op::OddP)));
        }
        Op::Eq | Op::Ne => return compare_equality_general(runtime, frame, op, args, at),
        Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            return compare_ordered_general(runtime, frame, op, args, at);
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div => {
            return variadic_arithmetic(runtime, frame, op, args, at);
        }
        Op::OnePlus | Op::OneMinus => {
            let operation = if op == Op::OnePlus { Op::Add } else { Op::Sub };
            binary(operation, number(args.start)?, real_int(1)).map_err(arithmetic)?
        }
        Op::Ash => {
            let integer_value = integer(args.start)?;
            let count = integer(args.start + 1)?;
            Number::Real(Real::Integer(
                ash(heap, integer_value, &count).map_err(oom)?,
            ))
        }
        Op::Expt => {
            expt(heap, number(args.start)?, number(args.start + 1)?).map_err(
                |error| match error {
                    PowerError::Arithmetic(class) => arithmetic(class),
                    PowerError::Memory => oom(OutOfMemory).into(),
                },
            )?
        }
        Op::Complex => {
            let re = real(args.start)?;
            let imaginary = if args.len() == 2 {
                real(args.start + 1)?
            } else {
                int(0)
            };
            complex(re, imaginary).map_err(arithmetic)?
        }
        Op::RealPart | Op::ImagPart => {
            if let Some((re, im)) = heap.complex_fields(frame.get(args.start)) {
                return Ok(if op == Op::RealPart { re } else { im });
            }
            let value = number(args.start)?;
            if op == Op::RealPart {
                return Ok(frame.get(args.start));
            }
            Number::Real(parts(value).1)
        }
        Op::Numerator | Op::Denominator => {
            let word = frame.get(args.start);
            if let Some((numerator, denominator)) = heap.ratio_fields(word) {
                return Ok(if op == Op::Numerator {
                    numerator
                } else {
                    denominator
                });
            }
            if matches!(
                heap.number_kind(word),
                Some(NumberKind::Fixnum | NumberKind::Bignum)
            ) {
                return Ok(if op == Op::Numerator {
                    word
                } else {
                    Word::fixnum(1).unwrap()
                });
            }
            return Err(numeric_type_error(runtime, word, "RATIONAL", at));
        }
        Op::Abs => Number::Real(magnitude(number(args.start)?).map_err(arithmetic)?),
        Op::IntegerLength => {
            let value = integer(args.start)?;
            let bits = if value.sign() == Sign::Minus {
                (value.magnitude() - 1u32).bits()
            } else {
                value.bits()
            };
            Number::Real(Real::Integer(BigInt::from(bits)))
        }
        Op::Signum => signum(number(args.start)?).map_err(arithmetic)?,
        Op::Conjugate => match number(args.start)? {
            Number::Real(_) => return Ok(frame.get(args.start)),
            Number::Complex(re, im) => Number::Complex(re, negate(im)),
        },
        Op::Float => {
            let Some((real, format)) = coerce_to_float(runtime, frame, args.clone(), at)? else {
                return Ok(frame.get(args.start));
            };
            Number::Real(coerce(real, format).map_err(arithmetic)?)
        }
        Op::Rational => {
            let real = real(args.start)?;
            if precision(&real) == Precision::Exact {
                return Ok(frame.get(args.start));
            }
            Number::Real(rational(real_rational(&real)))
        }
        Op::Max | Op::Min => return select_extremum(runtime, frame, op, args, at),
        Op::Floor
        | Op::Ceiling
        | Op::Truncate
        | Op::Round
        | Op::Ffloor
        | Op::Fceiling
        | Op::Ftruncate
        | Op::Fround
        | Op::Mod
        | Op::Rem
        | Op::DecodeFloat
        | Op::IntegerDecodeFloat => unreachable!("multi-value numeric dispatch"),
        _ => unreachable!("handled by bitwise or math submodule"),
    };
    heap.allocate_number(&result)
        .map_err(oom)
        .map_err(Fault::from)
}
