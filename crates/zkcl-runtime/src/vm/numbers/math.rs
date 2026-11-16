use std::f64::consts::{FRAC_PI_2, PI};
use std::ops::Range;

use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::{Float, Signed, ToPrimitive, Zero};

use super::super::Values;
use super::{
    Op, Precision, arithmetic_fault, number_precision, numeric_type_error, oom, parts, precision,
    rational, run_integer, run_number, run_real,
};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::number::{Number, NumberKind, Real};
use crate::{EvalError, Runtime};

pub(super) trait FloatOps: Float + Sized {
    const SIGNIFICAND_DIGITS: i32;
    const MIN_EXP: i32;

    fn from_f64_lossy(v: f64) -> Self;
    fn real_from(self) -> Real;
    fn unwrap_real(real: &Real) -> Result<Self, Class>;
    fn two() -> Self {
        Self::one() + Self::one()
    }
    fn half() -> Self {
        Self::one() / Self::two()
    }
    fn half_pi() -> Self {
        Self::from_f64_lossy(FRAC_PI_2)
    }
    fn frexp(self) -> (Self, i32);
    fn scalbn(self, exp: i32) -> Self;
    fn asin_ext(self) -> Self;
    fn acos_ext(self) -> Self;
    fn atan2_ext(self, other: Self) -> Self;
}

impl FloatOps for f32 {
    const SIGNIFICAND_DIGITS: i32 = f32::MANTISSA_DIGITS as i32;
    const MIN_EXP: i32 = f32::MIN_EXP;

    fn from_f64_lossy(v: f64) -> Self {
        v as f32
    }
    fn real_from(self) -> Real {
        Real::Single(self)
    }
    fn unwrap_real(real: &Real) -> Result<Self, Class> {
        super::single(real)
    }
    fn frexp(self) -> (Self, i32) {
        libm::frexpf(self)
    }
    fn scalbn(self, exp: i32) -> Self {
        libm::scalbnf(self, exp)
    }
    fn asin_ext(self) -> Self {
        (f64::from(self).asin()) as f32
    }
    fn acos_ext(self) -> Self {
        (f64::from(self).acos()) as f32
    }
    fn atan2_ext(self, other: Self) -> Self {
        (f64::from(self).atan2(f64::from(other))) as f32
    }
}

impl FloatOps for f64 {
    const SIGNIFICAND_DIGITS: i32 = f64::MANTISSA_DIGITS as i32;
    const MIN_EXP: i32 = f64::MIN_EXP;

    fn from_f64_lossy(v: f64) -> Self {
        v
    }
    fn real_from(self) -> Real {
        Real::Double(self)
    }
    fn unwrap_real(real: &Real) -> Result<Self, Class> {
        super::double(real)
    }
    fn frexp(self) -> (Self, i32) {
        libm::frexp(self)
    }
    fn scalbn(self, exp: i32) -> Self {
        libm::scalbn(self, exp)
    }
    fn asin_ext(self) -> Self {
        f64::asin(self)
    }
    fn acos_ext(self) -> Self {
        f64::acos(self)
    }
    fn atan2_ext(self, other: Self) -> Self {
        f64::atan2(self, other)
    }
}

pub(super) fn divide_float<T: FloatOps>(a: T, b: T, c: T, d: T) -> Result<(T, T), Class> {
    divide_float_with(
        [a, b, c, d],
        T::min_positive_value().sqrt(),
        |a, b, c, d| a * b + c * d,
    )
}

fn divide_float_with<T: FloatOps>(
    [a, b, c, d]: [T; 4],
    lower: T,
    sum_products: impl Fn(T, T, T, T) -> T,
) -> Result<(T, T), Class> {
    let upper = T::max_value().sqrt() * T::half();
    // In this range nonzero products are normal, and summing two products
    // stays in range. At the format limits, round only the final quotient.
    if [a, b, c, d].into_iter().all(|value| {
        let magnitude = value.abs();
        magnitude.is_zero() || (lower <= magnitude && magnitude <= upper)
    }) {
        let divisor = sum_products(c, c, d, d);
        if divisor.is_zero() {
            return Err(Class::DivisionByZero);
        }
        return Ok((
            super::finite(sum_products(a, c, b, d) / divisor)?,
            super::finite(sum_products(b, c, -a, d) / divisor)?,
        ));
    }
    let parts = [a, b, c, d];
    let [a, b, c, d] = parts.map(|value| super::real_rational(&value.real_from()));
    let (re, im) = super::divide_exact(&a, &b, &c, &d)?;
    // Rationals have no signed zero. Use unit magnitudes to recover the
    // signs of zero components without overflowing any products.
    let [a, b, c, d] = parts.map(|value| {
        if value.is_zero() {
            value
        } else {
            value.signum()
        }
    });
    let round = |value: BigRational, sign: T| {
        if value.is_zero() {
            Ok(T::zero().copysign(sign))
        } else {
            T::unwrap_real(&Real::Ratio(value))
        }
    };
    Ok((round(re, a * c + b * d)?, round(im, b * c - a * d)?))
}

#[derive(Clone, Copy)]
struct ComplexVal<T> {
    re: T,
    im: T,
}

impl<T: FloatOps> ComplexVal<T> {
    fn new(re: T, im: T) -> Self {
        Self { re, im }
    }

    fn from_real(re: T) -> Self {
        Self::new(re, T::zero())
    }

    fn add(self, rhs: Self) -> Self {
        Self::new(self.re + rhs.re, self.im + rhs.im)
    }

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.re - rhs.re, self.im - rhs.im)
    }

    fn neg(self) -> Self {
        Self::new(-self.re, -self.im)
    }

    fn scale(self, s: T) -> Self {
        Self::new(self.re * s, self.im * s)
    }

    fn mul_i(self) -> Self {
        Self::new(-self.im, self.re)
    }

    fn mul_neg_i(self) -> Self {
        Self::new(self.im, -self.re)
    }

    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.re * rhs.re - self.im * rhs.im,
            self.re * rhs.im + self.im * rhs.re,
        )
    }

    fn div(self, rhs: Self) -> Result<Self, Class> {
        let denom = rhs.re * rhs.re + rhs.im * rhs.im;
        if denom == T::zero() {
            return Err(Class::DivisionByZero);
        }
        Ok(Self::new(
            (self.re * rhs.re + self.im * rhs.im) / denom,
            (self.im * rhs.re - self.re * rhs.im) / denom,
        ))
    }

    fn sqrt(self) -> Self {
        let zero = T::zero();
        if self.re == zero && self.im == zero {
            return self;
        }
        let r = self.re.hypot(self.im);
        let two = T::two();
        let abs_re = self.re.abs();
        // Halve before adding when the sum could overflow; the result keeps
        // the same precision as (r + |re|) / 2 for ordinary magnitudes.
        let re = if abs_re > T::max_value() / two || r > T::max_value() / two {
            (abs_re / two + r / two).sqrt()
        } else {
            ((abs_re + r) / two).sqrt()
        };
        let im = self.im.abs() / (two * re);
        if self.re >= zero {
            Self::new(re, if self.im.is_sign_negative() { -im } else { im })
        } else {
            // The principal square root excludes the negative imaginary axis,
            // so a zero imaginary part (including minus zero) still yields +i.
            let imaginary = if self.im.is_sign_negative() && self.im != zero {
                -re
            } else {
                re
            };
            Self::new(im, imaginary)
        }
    }

    fn ln(self) -> Result<Self, Class> {
        let zero = T::zero();
        if self.re == zero && self.im == zero {
            return Err(Class::DivisionByZero);
        }
        Ok(Self::new(
            self.re.hypot(self.im).ln(),
            self.im.atan2_ext(self.re),
        ))
    }

    fn exp(self) -> Self {
        let r = self.re.exp();
        Self::new(r * self.im.cos(), r * self.im.sin())
    }

    fn sin(self) -> Self {
        Self::new(
            self.re.sin() * self.im.cosh(),
            self.re.cos() * self.im.sinh(),
        )
    }

    fn cos(self) -> Self {
        Self::new(
            self.re.cos() * self.im.cosh(),
            -self.re.sin() * self.im.sinh(),
        )
    }

    fn tan(self) -> Result<Self, Class> {
        self.sin().div(self.cos())
    }

    fn sinh(self) -> Self {
        Self::new(
            self.re.sinh() * self.im.cos(),
            self.re.cosh() * self.im.sin(),
        )
    }

    fn cosh(self) -> Self {
        Self::new(
            self.re.cosh() * self.im.cos(),
            self.re.sinh() * self.im.sin(),
        )
    }

    fn tanh(self) -> Result<Self, Class> {
        self.sinh().div(self.cosh())
    }

    fn asinh(self) -> Result<Self, Class> {
        if self.re.is_zero() {
            let y = self.im;
            let one = T::one();
            if y.abs() <= one {
                return Ok(Self::new(T::zero(), y.asin_ext()));
            }
            let sign = if y.is_sign_negative() { -one } else { one };
            return Ok(Self::new(sign * y.abs().acosh(), sign * T::half_pi()));
        }
        if self.re.is_sign_negative() {
            return Ok(self.neg().asinh()?.neg());
        }
        let one = Self::from_real(T::one());
        let root = one.add(self.mul(self)).sqrt();
        self.add(root).ln()
    }

    fn asin(self) -> Result<Self, Class> {
        Ok(self.mul_i().asinh()?.mul_neg_i())
    }

    fn acos(self) -> Result<Self, Class> {
        let a = self.asin()?;
        Ok(Self::new(T::half_pi() - a.re, -a.im))
    }

    fn acosh(self) -> Result<Self, Class> {
        let half = T::half();
        let one = Self::from_real(T::one());
        let plus = self.add(one).scale(half).sqrt();
        let minus = self.sub(one).scale(half).sqrt();
        Ok(plus.add(minus).ln()?.scale(T::two()))
    }

    fn atanh(self) -> Result<Self, Class> {
        let one = Self::from_real(T::one());
        let num = one.add(self).ln()?;
        let den = one.sub(self).ln()?;
        Ok(num.sub(den).scale(T::half()))
    }

    fn atan(self) -> Result<Self, Class> {
        Ok(self.mul_i().atanh()?.mul_neg_i())
    }

    fn finish(self, complex_output: bool) -> Result<Number, Class> {
        let re = super::finite(self.re)?.real_from();
        if complex_output {
            let im = super::finite(self.im)?.real_from();
            Ok(Number::Complex(re, im))
        } else {
            Ok(Number::Real(re))
        }
    }
}

pub(super) fn run_multi(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let heap = &runtime.heap;
    let word = frame.get(args.start);
    let kind = heap.number_kind(word);
    match (op, kind) {
        (Op::DecodeFloat, Some(NumberKind::Single)) => {
            let Real::Single(value) = run_real(runtime, frame, args.start, at)? else {
                unreachable!()
            };
            let (sig, exp, sign) = decode_ieee(value);
            emit_decode_values(
                runtime,
                values,
                Number::Real(Real::Single(sig)),
                exp,
                Number::Real(Real::Single(sign)),
                at,
            )
        }
        (Op::DecodeFloat, Some(NumberKind::Double)) => {
            let Real::Double(value) = run_real(runtime, frame, args.start, at)? else {
                unreachable!()
            };
            let (sig, exp, sign) = decode_ieee(value);
            emit_decode_values(
                runtime,
                values,
                Number::Real(Real::Double(sig)),
                exp,
                Number::Real(Real::Double(sign)),
                at,
            )
        }
        (Op::IntegerDecodeFloat, Some(NumberKind::Single)) => {
            let Real::Single(value) = run_real(runtime, frame, args.start, at)? else {
                unreachable!()
            };
            let (sig, exp, sign) = integer_decode_ieee(value);
            emit_decode_values(
                runtime,
                values,
                Number::Real(Real::Integer(BigInt::from(sig))),
                exp,
                Number::Real(Real::Integer(BigInt::from(sign))),
                at,
            )
        }
        (Op::IntegerDecodeFloat, Some(NumberKind::Double)) => {
            let Real::Double(value) = run_real(runtime, frame, args.start, at)? else {
                unreachable!()
            };
            let (sig, exp, sign) = integer_decode_ieee(value);
            emit_decode_values(
                runtime,
                values,
                Number::Real(Real::Integer(BigInt::from(sig))),
                exp,
                Number::Real(Real::Integer(BigInt::from(sign))),
                at,
            )
        }
        _ => Err(numeric_type_error(runtime, word, "FLOAT", at)),
    }
}

fn emit_decode_values(
    runtime: &Runtime,
    values: &mut Values,
    first: Number,
    exp: i32,
    third: Number,
    at: usize,
) -> Result<(), Fault> {
    let w1 = runtime.root_slot(runtime.heap.allocate_number(&first).map_err(oom)?);
    let w2 = Word::fixnum(exp as isize)
        .ok_or_else(|| EvalError::new(at, "float exponent out of fixnum range"))?;
    let w3 = runtime.heap.allocate_number(&third).map_err(oom)?;
    values.resize(3);
    values.slots.set(0, w1.get());
    values.slots.set(1, w2);
    values.slots.set(2, w3);
    Ok(())
}

fn decode_ieee<T: FloatOps>(value: T) -> (T, i32, T) {
    let (significand, exponent) = value.abs().frexp();
    let sign = T::one().copysign(value);
    (significand, exponent, sign)
}

fn integer_decode_ieee<T: FloatOps>(value: T) -> (u64, i32, i32) {
    let (significand, exponent) = value.abs().frexp();
    let sign = if value.is_sign_negative() { -1 } else { 1 };
    if significand.is_zero() {
        return (0, 0, sign);
    }
    let significand = significand
        .scalbn(T::SIGNIFICAND_DIGITS)
        .to_u64()
        .expect("normalized significand fits in u64");
    (significand, exponent - T::SIGNIFICAND_DIGITS, sign)
}

fn float_precision_bits<T: FloatOps>(value: T) -> i32 {
    if value.is_zero() {
        0
    } else {
        let (_, exponent) = value.frexp();
        T::SIGNIFICAND_DIGITS + (exponent - T::MIN_EXP).min(0)
    }
}

fn scale_ieee<T: FloatOps>(value: T, scale: &BigInt) -> Result<T, Class> {
    let exp = scale.to_i32().unwrap_or_else(|| {
        if scale.sign() == Sign::Minus {
            i32::MIN
        } else {
            i32::MAX
        }
    });
    super::finite(value.scalbn(exp))
}

pub(super) fn run_word(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let heap = &runtime.heap;
    let arithmetic = |class| arithmetic_fault(runtime, frame, op, args.clone(), at, class);
    let real = |reg| run_real(runtime, frame, reg, at);
    let number = |reg| run_number(runtime, frame, reg, at);
    let integer = |reg| run_integer(runtime, frame, reg, at);

    let value = match op {
        Op::FloatRadix => {
            let w = frame.get(args.start);
            if !matches!(
                heap.number_kind(w),
                Some(NumberKind::Single | NumberKind::Double)
            ) {
                return Err(numeric_type_error(runtime, w, "FLOAT", at));
            }
            return Ok(Some(Word::fixnum(2).unwrap()));
        }
        Op::FloatDigits | Op::FloatPrecision => {
            let w = frame.get(args.start);
            let count = match heap.number_kind(w) {
                Some(NumberKind::Single) => {
                    if op == Op::FloatDigits {
                        f32::SIGNIFICAND_DIGITS
                    } else {
                        let Real::Single(v) = real(args.start)? else {
                            unreachable!()
                        };
                        float_precision_bits(v)
                    }
                }
                Some(NumberKind::Double) => {
                    if op == Op::FloatDigits {
                        f64::SIGNIFICAND_DIGITS
                    } else {
                        let Real::Double(v) = real(args.start)? else {
                            unreachable!()
                        };
                        float_precision_bits(v)
                    }
                }
                _ => return Err(numeric_type_error(runtime, w, "FLOAT", at)),
            };
            return Ok(Some(Word::fixnum(count as isize).unwrap()));
        }
        Op::FloatSign => {
            let f1_word = frame.get(args.start);
            let neg = match heap.number_kind(f1_word) {
                Some(NumberKind::Single) => {
                    let Real::Single(v) = real(args.start)? else {
                        unreachable!()
                    };
                    v.is_sign_negative()
                }
                Some(NumberKind::Double) => {
                    let Real::Double(v) = real(args.start)? else {
                        unreachable!()
                    };
                    v.is_sign_negative()
                }
                _ => return Err(numeric_type_error(runtime, f1_word, "FLOAT", at)),
            };
            let mag_word = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                f1_word
            };
            let result = match heap.number_kind(mag_word) {
                Some(NumberKind::Single) => {
                    let mag = if args.len() == 2 {
                        let Real::Single(v) = real(args.start + 1)? else {
                            unreachable!()
                        };
                        v.abs()
                    } else {
                        1.0f32
                    };
                    Real::Single(if neg { -mag } else { mag })
                }
                Some(NumberKind::Double) => {
                    let mag = if args.len() == 2 {
                        let Real::Double(v) = real(args.start + 1)? else {
                            unreachable!()
                        };
                        v.abs()
                    } else {
                        1.0f64
                    };
                    Real::Double(if neg { -mag } else { mag })
                }
                _ => return Err(numeric_type_error(runtime, mag_word, "FLOAT", at)),
            };
            Number::Real(result)
        }
        Op::ScaleFloat => {
            let w = frame.get(args.start);
            let scale = integer(args.start + 1)?;
            let scaled = match heap.number_kind(w) {
                Some(NumberKind::Single) => {
                    let Real::Single(v) = real(args.start)? else {
                        unreachable!()
                    };
                    Real::Single(scale_ieee(v, &scale).map_err(arithmetic)?)
                }
                Some(NumberKind::Double) => {
                    let Real::Double(v) = real(args.start)? else {
                        unreachable!()
                    };
                    Real::Double(scale_ieee(v, &scale).map_err(arithmetic)?)
                }
                _ => return Err(numeric_type_error(runtime, w, "FLOAT", at)),
            };
            Number::Real(scaled)
        }
        Op::Rationalize => {
            let val = real(args.start)?;
            if precision(&val) == Precision::Exact {
                return Ok(Some(frame.get(args.start)));
            }
            Number::Real(rational(rationalize_real(&val)))
        }
        Op::UpgradedComplexPartType => {
            return Ok(Some(super::super::types::upgraded_complex_part_type(
                runtime,
                frame.get(args.start),
                at,
            )?));
        }
        Op::Cis => {
            let r = real(args.start)?;
            let fmt = precision(&r).max(Precision::Single);
            if fmt == Precision::Double {
                let rad = f64::unwrap_real(&r).map_err(arithmetic)?;
                ComplexVal::new(rad.cos(), rad.sin())
                    .finish(true)
                    .map_err(arithmetic)?
            } else {
                let rad = f32::unwrap_real(&r).map_err(arithmetic)?;
                ComplexVal::new(rad.cos(), rad.sin())
                    .finish(true)
                    .map_err(arithmetic)?
            }
        }
        Op::Phase => {
            let z = number(args.start)?;
            let fmt = number_precision(&z).max(Precision::Single);
            let (re, im) = parts(z);
            if fmt == Precision::Double {
                let a = f64::unwrap_real(&re).map_err(arithmetic)?;
                let b = f64::unwrap_real(&im).map_err(arithmetic)?;
                Number::Real(Real::Double(
                    super::finite(b.atan2_ext(a)).map_err(arithmetic)?,
                ))
            } else {
                let a = f32::unwrap_real(&re).map_err(arithmetic)?;
                let b = f32::unwrap_real(&im).map_err(arithmetic)?;
                Number::Real(Real::Single(
                    super::finite(b.atan2_ext(a)).map_err(arithmetic)?,
                ))
            }
        }
        Op::Log => {
            let x = number(args.start)?;
            let base = if args.len() == 2 {
                Some(number(args.start + 1)?)
            } else {
                None
            };
            let format = base.as_ref().map_or(number_precision(&x), |b| {
                number_precision(&x).max(number_precision(b))
            });
            if format == Precision::Double {
                eval_log::<f64>(&x, base.as_ref())
            } else {
                eval_log::<f32>(&x, base.as_ref())
            }
            .map_err(arithmetic)?
        }
        Op::Atan if args.len() == 2 => {
            let y = real(args.start)?;
            let x = real(args.start + 1)?;
            let fmt = precision(&y).max(precision(&x)).max(Precision::Single);
            if fmt == Precision::Double {
                let yf = f64::unwrap_real(&y).map_err(arithmetic)?;
                let xf = f64::unwrap_real(&x).map_err(arithmetic)?;
                Number::Real(Real::Double(
                    super::finite(yf.atan2_ext(xf)).map_err(arithmetic)?,
                ))
            } else {
                let yf = f32::unwrap_real(&y).map_err(arithmetic)?;
                let xf = f32::unwrap_real(&x).map_err(arithmetic)?;
                Number::Real(Real::Single(
                    super::finite(yf.atan2_ext(xf)).map_err(arithmetic)?,
                ))
            }
        }
        Op::Exp
        | Op::Sqrt
        | Op::Sin
        | Op::Cos
        | Op::Tan
        | Op::Asin
        | Op::Acos
        | Op::Atan
        | Op::Sinh
        | Op::Cosh
        | Op::Tanh
        | Op::Asinh
        | Op::Acosh
        | Op::Atanh => {
            let x = number(args.start)?;
            eval_unary(op, x).map_err(arithmetic)?
        }
        _ => return Ok(None),
    };

    let allocated = heap.allocate_number(&value).map_err(oom)?;
    Ok(Some(allocated))
}

fn eval_unary(op: Op, x: Number) -> Result<Number, Class> {
    let fmt = number_precision(&x).max(Precision::Single);
    let is_complex = matches!(x, Number::Complex(..));
    let (re, im) = parts(x);
    if fmt == Precision::Double {
        eval_unary_float::<f64>(op, &re, &im, is_complex)
    } else {
        eval_unary_float::<f32>(op, &re, &im, is_complex)
    }
}

fn eval_unary_float<T: FloatOps>(
    op: Op,
    re: &Real,
    im: &Real,
    is_complex: bool,
) -> Result<Number, Class> {
    let a = T::unwrap_real(re)?;
    let b = T::unwrap_real(im)?;
    let z = ComplexVal::new(a, b);
    if !is_complex {
        let zero = T::zero();
        let one = T::one();
        match op {
            Op::Exp => {
                let value = a.exp();
                if value.is_zero() {
                    return Err(Class::FloatingPointUnderflow);
                }
                return ComplexVal::from_real(value).finish(false);
            }
            Op::Sqrt if a >= zero => return ComplexVal::from_real(a.sqrt()).finish(false),
            Op::Sin => return ComplexVal::from_real(a.sin()).finish(false),
            Op::Cos => return ComplexVal::from_real(a.cos()).finish(false),
            Op::Tan => return ComplexVal::from_real(a.tan()).finish(false),
            Op::Asin if a.abs() <= one => return ComplexVal::from_real(a.asin_ext()).finish(false),
            Op::Acos if a.abs() <= one => return ComplexVal::from_real(a.acos_ext()).finish(false),
            Op::Atan => return ComplexVal::from_real(a.atan()).finish(false),
            Op::Sinh => return ComplexVal::from_real(a.sinh()).finish(false),
            Op::Cosh => return ComplexVal::from_real(a.cosh()).finish(false),
            Op::Tanh => return ComplexVal::from_real(a.tanh()).finish(false),
            Op::Asinh => return ComplexVal::from_real(a.asinh()).finish(false),
            Op::Acosh if a >= one => return ComplexVal::from_real(a.acosh()).finish(false),
            Op::Atanh if a.abs() < one => return ComplexVal::from_real(a.atanh()).finish(false),
            Op::Atanh if a.abs() == one => return Err(Class::DivisionByZero),
            _ => {}
        }
    }
    let out = match op {
        Op::Exp => z.exp(),
        Op::Sqrt => z.sqrt(),
        Op::Sin => z.sin(),
        Op::Cos => z.cos(),
        Op::Tan => z.tan()?,
        Op::Asin => z.asin()?,
        Op::Acos => z.acos()?,
        Op::Atan => z.atan()?,
        Op::Sinh => z.sinh(),
        Op::Cosh => z.cosh(),
        Op::Tanh => z.tanh()?,
        Op::Asinh => z.asinh()?,
        Op::Acosh => z.acosh()?,
        Op::Atanh => z.atanh()?,
        _ => unreachable!(),
    };
    out.finish(true)
}

fn eval_log<T: FloatOps>(x: &Number, base: Option<&Number>) -> Result<Number, Class> {
    if base.is_some_and(super::zero_number) {
        return Ok(Number::Real(T::zero().real_from()));
    }

    if let (Number::Real(real), Some(Number::Real(base))) = (x, base)
        && (matches!(base, Real::Integer(n) if n.to_u8() == Some(2))
            || matches!(base, Real::Single(2.0) | Real::Double(2.0)))
    {
        if let Real::Integer(n) = real
            && n.is_positive()
        {
            let (top, shift) = super::leading_magnitude(n);
            return ComplexVal::from_real(T::from_f64_lossy(top.log2() + shift as f64))
                .finish(false);
        }
        if precision(real) != Precision::Exact {
            let value = T::unwrap_real(real)?;
            if value > T::zero() {
                return ComplexVal::from_real(value.log2()).finish(false);
            }
        }
    }

    let complex_input =
        matches!(x, Number::Complex(..)) || matches!(base, Some(Number::Complex(..)));
    let (numerator, numerator_scale) = log_operand(x)?;
    let (result, scale) = if let Some(base) = base {
        let (denominator, denominator_scale) = log_operand(base)?;
        if denominator.re == 0.0 && denominator.im == 0.0 {
            return Err(Class::DivisionByZero);
        }
        let scale = numerator_scale - denominator_scale;
        let (numerator, denominator, scale) = if scale == 0 {
            (numerator, denominator, scale)
        } else {
            let (numerator, a) = normalize_log(numerator);
            let (denominator, b) = normalize_log(denominator);
            (
                numerator,
                denominator,
                scale + i128::from(a) - i128::from(b),
            )
        };
        let result = divide_log(numerator, denominator)?;
        (result, scale)
    } else {
        (numerator, numerator_scale)
    };
    let complex_out = complex_input || result.im != 0.0;
    ComplexVal::new(
        T::from_f64_lossy(super::scale_log(result.re, scale)),
        T::from_f64_lossy(super::scale_log(result.im, scale)),
    )
    .finish(complex_out)
}

fn divide_log(
    numerator: ComplexVal<f64>,
    denominator: ComplexVal<f64>,
) -> Result<ComplexVal<f64>, Class> {
    let ComplexVal { re: a, im: b } = numerator;
    let ComplexVal { re: c, im: d } = denominator;
    let (re, im) = if d == 0.0 {
        (a / c, b / c)
    } else if a == 0.0 || b == 0.0 || c == 0.0 {
        divide_float(a, b, c, d)?
    } else {
        // Leave room for all low bits of the separate product so that its
        // FMA rounding residual is representable even at the subnormal limit.
        let lower = f64::MIN_POSITIVE.sqrt() * f64::from(1_u32 << (f64::MANTISSA_DIGITS / 2));
        divide_float_with([a, b, c, d], lower, |a, b, c, d| {
            // Kahan's compensated sum of two products. The second FMA
            // recovers the rounding error in the separately rounded product.
            let product = c * d;
            a.mul_add(b, product) + c.mul_add(d, -product)
        })?
    };
    Ok(ComplexVal::new(re, im))
}

fn normalize_log(value: ComplexVal<f64>) -> (ComplexVal<f64>, i32) {
    let (_, exponent) = libm::frexp(value.re.abs().max(value.im.abs()));
    (
        ComplexVal::new(
            libm::scalbn(value.re, -exponent),
            libm::scalbn(value.im, -exponent),
        ),
        exponent,
    )
}

fn log_operand(number: &Number) -> Result<(ComplexVal<f64>, i128), Class> {
    match number {
        Number::Real(real) => {
            let exact_log = || super::ln_real_abs_scaled(real, true).ok_or(Class::DivisionByZero);
            let ((magnitude, scale), negative) = match real {
                Real::Integer(n) => (exact_log()?, n.is_negative()),
                Real::Ratio(r) => (exact_log()?, r.is_negative()),
                Real::Single(value) => ((float_log_abs(f64::from(*value))?, 0), *value < 0.0),
                Real::Double(value) => ((float_log_abs(*value)?, 0), *value < 0.0),
            };
            if negative {
                Ok((ComplexVal::new(super::scale_log(magnitude, scale), PI), 0))
            } else {
                Ok((ComplexVal::from_real(magnitude), scale))
            }
        }
        Number::Complex(re, im) if precision(re) == Precision::Exact => {
            let re = super::real_rational(re);
            let im = super::real_rational(im);
            let (magnitude, magnitude_scale) = exact_complex_log_abs(&re, &im)?;
            let (phase, phase_scale) = if re.abs() >= im.abs() {
                let (slope, scale) = super::tiny_ratio_scaled(&im / re.abs(), re.is_positive());
                if scale == 0 {
                    (slope.atan2(re.signum().to_f64().unwrap()), 0)
                } else {
                    // For positive real part and this small a slope, atan(slope)
                    // agrees with the slope beyond binary64 precision.
                    (slope, scale)
                }
            } else {
                let slope = (&re / im.abs()).to_f64().expect("bounded slope");
                (im.signum().to_f64().unwrap().atan2(slope), 0)
            };
            let scale = if magnitude == 0.0 {
                phase_scale
            } else {
                magnitude_scale.max(phase_scale)
            };
            Ok((
                ComplexVal::new(
                    super::scale_log(magnitude, magnitude_scale - scale),
                    super::scale_log(phase, phase_scale - scale),
                ),
                scale,
            ))
        }
        Number::Complex(re, im) => {
            let re = super::double(re)?;
            let im = super::double(im)?;
            let magnitude = complex_log_abs(re, im)?;
            Ok((ComplexVal::new(magnitude, im.atan2(re)), 0))
        }
    }
}

fn float_log_abs(value: f64) -> Result<f64, Class> {
    let value = super::finite(value)?;
    (value != 0.0)
        .then(|| value.abs().ln())
        .ok_or(Class::DivisionByZero)
}

fn exact_complex_log_abs(re: &BigRational, im: &BigRational) -> Result<(f64, i128), Class> {
    let norm_squared = Real::Ratio(re * re + im * im);
    let (magnitude, scale) =
        super::ln_real_abs_scaled(&norm_squared, true).ok_or(Class::DivisionByZero)?;
    Ok((0.5 * magnitude, scale))
}

fn complex_log_abs(re: f64, im: f64) -> Result<f64, Class> {
    if re == 0.0 || im == 0.0 {
        return float_log_abs(re.abs().max(im.abs()));
    }
    let radius = re.hypot(im);
    if (0.75..=1.25).contains(&radius) {
        let a = re.abs().max(im.abs());
        let b = re.abs().min(im.abs());
        let a_squared = a * a;
        let b_squared = b * b;
        let sum = a_squared + b_squared;
        if sum == 1.0 {
            let (magnitude, scale) = exact_complex_log_abs(
                &BigRational::from_float(re).expect("finite real part"),
                &BigRational::from_float(im).expect("finite imaginary part"),
            )?;
            return Ok(super::scale_log(magnitude, scale));
        }
        // Recover roundoff in both squares and in their sum near the unit circle.
        let error =
            ((a_squared - sum) + b_squared) + a.mul_add(a, -a_squared) + b.mul_add(b, -b_squared);
        return Ok(0.5 * ((sum - 1.0) + error).ln_1p());
    }
    if radius.is_infinite() || radius < f64::MIN_POSITIVE {
        let large = re.abs().max(im.abs());
        let small = re.abs().min(im.abs());
        return Ok(large.ln() + 0.5 * (small / large).powi(2).ln_1p());
    }
    Ok(radius.ln())
}

pub(crate) fn rationalize_real(value: &Real) -> BigRational {
    match value {
        Real::Integer(n) => BigRational::from_integer(n.clone()),
        Real::Ratio(r) => r.clone(),
        &Real::Single(f) => simplest_rational_in_interval(
            f64::from(f),
            f64::from(f.next_down()),
            f64::from(f.next_up()),
        ),
        &Real::Double(f) => simplest_rational_in_interval(f, f.next_down(), f.next_up()),
    }
}

fn simplest_rational_in_interval(x: f64, down: f64, up: f64) -> BigRational {
    if x == 0.0 {
        return BigRational::zero();
    }
    if x < 0.0 {
        return -simplest_rational_in_interval(-x, -up, -down);
    }
    let rx = BigRational::from_float(x).expect("finite float");
    if !up.is_finite() {
        return rx;
    }
    let mut lo = if down <= 0.0 {
        &rx / BigInt::from(2)
    } else {
        (BigRational::from_float(down).unwrap() + &rx) / BigInt::from(2)
    };
    let mut hi = (&rx + BigRational::from_float(up).unwrap()) / BigInt::from(2);
    if lo == hi {
        return lo;
    }
    let shift = (&hi - &lo) / (BigInt::from(1) << 100usize);
    lo += &shift;
    hi -= &shift;
    simplest_between(lo, hi)
}

fn simplest_between(lo: BigRational, hi: BigRational) -> BigRational {
    if lo == hi {
        return lo;
    }
    let a = floor_rational(&lo);
    let b = floor_rational(&hi);
    if a < b {
        return BigRational::from_integer(a + 1u8);
    }
    let integer = BigRational::from_integer(a);
    let rem_lo = &lo - &integer;
    let rem_hi = &hi - &integer;
    if rem_lo.is_zero() {
        return integer;
    }
    integer + simplest_between(rem_hi.recip(), rem_lo.recip()).recip()
}

fn floor_rational(r: &BigRational) -> BigInt {
    let (q, rem) = num_integer::Integer::div_rem(r.numer(), r.denom());
    if !rem.is_zero() && r.is_negative() {
        q - 1u8
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn logarithmic_quotient_recovers_roundoff_before_cancellation() {
        for (delta, scale) in [(2.0_f64.powi(-27), 1.0), (f64::EPSILON, 2.0_f64.powi(-500))] {
            let numerator = ComplexVal::new((1.0 + delta) * scale, scale);
            let denominator = ComplexVal::new(scale, (1.0 - delta) * scale);
            let quotient = divide_log(numerator, denominator).unwrap();
            let im = delta * delta / (1.0 + (1.0 - delta).powi(2));
            assert!(quotient.im.to_bits().abs_diff(im.to_bits()) <= 1);
        }
    }

    #[test]
    fn decode_float_keeps_both_results_live_during_collection() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(0);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let args = runtime.root_frame(1);
        let input = Number::Real(Real::Double(-12.0));
        args.set(0, runtime.heap.allocate_number(&input).unwrap());

        let mut values = Values::new(&runtime);
        run_multi(&runtime, &args, Op::DecodeFloat, 0..1, &mut values, 0)
            .unwrap_or_else(|fault| panic!("{}", fault.host));
        assert_eq!(
            runtime.heap.number(values.get(0)),
            Some(Number::Real(Real::Double(0.75)))
        );
        assert_eq!(values.get(1).as_fixnum(), Some(4));
        assert_eq!(
            runtime.heap.number(values.get(2)),
            Some(Number::Real(Real::Double(-1.0)))
        );
    }
}
