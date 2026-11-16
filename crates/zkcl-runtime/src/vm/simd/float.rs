//! Integer reference operations on finite IEEE 754 lane encodings.

use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    DivisionByZero,
    Overflow,
    Invalid,
}

/// The exact magnitude is `significand * 2^exponent`. Keep zero's sign here
/// so exact operations can distinguish it from cancellation.
struct Dyadic {
    negative: bool,
    significand: u128,
    exponent: i32,
}

impl Dyadic {
    fn aligned(&self, exponent: i32) -> Option<u128> {
        let shift = (self.exponent - exponent) as u32;
        if shift > self.significand.leading_zeros() {
            return None;
        }
        self.significand.checked_shl(shift)
    }

    fn aligned_big(self, exponent: i32) -> BigInt {
        let sign = if self.negative {
            Sign::Minus
        } else {
            Sign::Plus
        };
        BigInt::from_biguint(sign, self.significand.into()) << (self.exponent - exponent) as usize
    }

    fn mul(self, other: Self) -> Self {
        // This takes decoded lanes, whose significands have at most 53 bits each.
        Self {
            negative: self.negative ^ other.negative,
            significand: self.significand * other.significand,
            exponent: self.exponent + other.exponent,
        }
    }
}

impl Format {
    fn fraction_bits(self) -> i32 {
        match self {
            Self::F32 => 23,
            Self::F64 => 52,
        }
    }

    fn bias(self) -> i32 {
        match self {
            Self::F32 => 127,
            Self::F64 => 1023,
        }
    }

    fn sign_bit(self) -> u64 {
        match self {
            Self::F32 => 1 << 31,
            Self::F64 => 1 << 63,
        }
    }

    fn sign_bits(self, negative: bool) -> u64 {
        if negative { self.sign_bit() } else { 0 }
    }

    fn decode(self, bits: u64) -> Dyadic {
        let fraction_bits = self.fraction_bits();
        let hidden_bit = 1 << fraction_bits;
        let magnitude = bits & (self.sign_bit() - 1);
        let biased_exponent = (magnitude >> fraction_bits) as i32;
        let fraction = magnitude & (hidden_bit - 1);
        let significand = if biased_exponent == 0 {
            fraction
        } else {
            hidden_bit | fraction
        };

        Dyadic {
            negative: bits & self.sign_bit() != 0,
            significand: significand as u128,
            exponent: biased_exponent.max(1) - self.bias() - fraction_bits,
        }
    }

    fn rounding_unit(self, exponent_floor: i32) -> i32 {
        // The spacing at this magnitude is clamped to the subnormal spacing.
        exponent_floor.max(1 - self.bias()) - self.fraction_bits()
    }

    fn pack_rounded(
        self,
        negative: bool,
        mut significand: u64,
        mut exponent: i32,
    ) -> Result<u64, Fault> {
        let sign = self.sign_bits(negative);
        let fraction_bits = self.fraction_bits();
        let hidden_bit = 1 << fraction_bits;
        if significand == hidden_bit << 1 {
            significand >>= 1;
            exponent += 1;
        }
        if significand < hidden_bit {
            return Ok(sign | significand);
        }
        let biased_exponent = exponent + fraction_bits + self.bias();
        if biased_exponent > 2 * self.bias() {
            return Err(Fault::Overflow);
        }
        Ok(sign | ((biased_exponent as u64) << fraction_bits) | (significand - hidden_bit))
    }

    fn round(self, value: Dyadic) -> Result<u64, Fault> {
        let Dyadic {
            negative,
            significand,
            exponent,
        } = value;
        if significand == 0 {
            return Ok(self.sign_bits(negative));
        }

        let exponent_floor = exponent + significand.ilog2() as i32;
        let unit = self.rounding_unit(exponent_floor);
        if exponent_floor < unit - 1 {
            return Ok(self.sign_bits(negative));
        }
        let discarded = unit - exponent;
        let rounded = if discarded <= 0 {
            (significand << -discarded) as u64
        } else {
            let halfway = 1_u128 << (discarded - 1);
            let kept = significand.checked_shr(discarded as u32).unwrap_or(0) as u64;
            let remainder = significand & (halfway | (halfway - 1));
            nearest_even(kept, remainder.cmp(&halfway))
        };
        self.pack_rounded(negative, rounded, unit)
    }

    fn sum(self, a: Dyadic, b: Dyadic) -> Result<u64, Fault> {
        if a.significand == 0 && b.significand == 0 {
            return Ok(self.sign_bits(a.negative && b.negative));
        }
        if a.significand == 0 {
            return self.round(b);
        }
        if b.significand == 0 {
            return self.round(a);
        }

        let exponent = a.exponent.min(b.exponent);
        if let (Some(left), Some(right)) = (a.aligned(exponent), b.aligned(exponent)) {
            if a.negative != b.negative {
                let negative = if left < right {
                    b.negative
                } else {
                    left != right && a.negative
                };
                return self.round(Dyadic {
                    negative,
                    significand: left.abs_diff(right),
                    exponent,
                });
            }
            if let Some(significand) = left.checked_add(right) {
                return self.round(Dyadic {
                    negative: a.negative,
                    significand,
                    exponent,
                });
            }
        }

        let sum = a.aligned_big(exponent) + b.aligned_big(exponent);
        let negative = sum.sign() == Sign::Minus;
        let magnitude = sum.magnitude();
        if let Some(significand) = magnitude.to_u128() {
            return self.round(Dyadic {
                negative,
                significand,
                exponent,
            });
        }

        // The magnitude exceeds 128 bits, so the rounding unit always discards bits.
        let unit = self.rounding_unit(exponent + magnitude.bits() as i32 - 1);
        let discarded = (unit - exponent) as u64;
        let kept = (magnitude >> discarded as usize).to_u64().unwrap();
        let halfway_bit = discarded - 1;
        let round_up = magnitude.bit(halfway_bit)
            && (kept & 1 != 0 || magnitude.trailing_zeros().unwrap() < halfway_bit);
        self.pack_rounded(negative, kept + u64::from(round_up), unit)
    }

    pub(super) fn add(self, a: u64, b: u64) -> Result<u64, Fault> {
        self.sum(self.decode(a), self.decode(b))
    }

    pub(super) fn sub(self, a: u64, b: u64) -> Result<u64, Fault> {
        self.add(a, b ^ self.sign_bit())
    }

    pub(super) fn mul(self, a: u64, b: u64) -> Result<u64, Fault> {
        self.round(self.decode(a).mul(self.decode(b)))
    }

    pub(super) fn div(self, a: u64, b: u64) -> Result<u64, Fault> {
        let a = self.decode(a);
        let b = self.decode(b);
        if b.significand == 0 {
            return Err(Fault::DivisionByZero);
        }
        let negative = a.negative ^ b.negative;
        if a.significand == 0 {
            return Ok(self.sign_bits(negative));
        }

        let exponent = a.exponent - b.exponent;
        let exponent_floor = exponent + floor_log2_ratio(a.significand, b.significand);
        let unit = self.rounding_unit(exponent_floor);
        if exponent_floor < unit - 1 {
            return Ok(self.sign_bits(negative));
        }

        // The scaled numerator uses at most 106 bits: the divisor's bit width
        // plus the quotient's precision. If we scale the denominator instead,
        // it is at most twice the numerator because the quotient is at least 1/2.
        let shift = exponent - unit;
        let (numerator, denominator) = if shift >= 0 {
            (a.significand << shift, b.significand)
        } else {
            (a.significand, b.significand << -shift)
        };
        let quotient = (numerator / denominator) as u64;
        let remainder = numerator % denominator;
        let rounded = nearest_even(quotient, (2 * remainder).cmp(&denominator));
        self.pack_rounded(negative, rounded, unit)
    }

    pub(super) fn sqrt(self, a: u64) -> Result<u64, Fault> {
        let a = self.decode(a);
        if a.significand == 0 {
            return Ok(self.sign_bits(a.negative));
        }
        if a.negative {
            return Err(Fault::Invalid);
        }

        // The root of every positive input is normal. In units of the root's
        // spacing, its square is an integer with at most 106 bits (binary64).
        let exponent_floor = a.exponent + a.significand.ilog2() as i32;
        let unit = exponent_floor.div_euclid(2) - self.fraction_bits();
        let square = a.significand << (a.exponent - 2 * unit);
        let lower = square.isqrt();
        // Four times the radicand cannot equal the square of an odd integer.
        let midpoint = (2 * lower + 1).pow(2);
        let round_up = 4 * square > midpoint;
        self.pack_rounded(false, lower as u64 + u64::from(round_up), unit)
    }

    pub(super) fn fma(self, a: u64, b: u64, c: u64) -> Result<u64, Fault> {
        self.sum(self.decode(a).mul(self.decode(b)), self.decode(c))
    }

    pub(super) fn compare(self, a: u64, b: u64) -> Ordering {
        let sign = self.sign_bit();
        // The two zeros meet at the same key. Other keys increase with value.
        let key = |bits| {
            let magnitude = bits & (sign - 1);
            if bits & sign == 0 {
                sign + magnitude
            } else {
                sign - magnitude
            }
        };
        key(a).cmp(&key(b))
    }

    pub(super) fn convert(self, bits: u64, to: Self) -> Result<u64, Fault> {
        to.round(self.decode(bits))
    }

    #[expect(clippy::wrong_self_convention, reason = "self is the target format")]
    pub(super) fn from_integer(self, signed: i128) -> Result<u64, Fault> {
        self.round(Dyadic {
            negative: signed < 0,
            significand: signed.unsigned_abs(),
            exponent: 0,
        })
    }

    pub(super) fn truncate(self, bits: u64) -> Option<i128> {
        let value = self.decode(bits);
        let magnitude = if value.exponent >= 0 {
            value.aligned(0)?
        } else {
            value
                .significand
                .checked_shr((-value.exponent) as u32)
                .unwrap_or(0)
        };
        if value.negative && magnitude == 1 << 127 {
            return Some(i128::MIN);
        }
        let signed = i128::try_from(magnitude).ok()?;
        Some(if value.negative { -signed } else { signed })
    }
}

fn floor_log2_ratio(numerator: u128, denominator: u128) -> i32 {
    let estimate = numerator.ilog2() as i32 - denominator.ilog2() as i32;
    let comparison = if estimate >= 0 {
        numerator.cmp(&(denominator << estimate))
    } else {
        (numerator << -estimate).cmp(&denominator)
    };
    estimate - i32::from(comparison.is_lt())
}

fn nearest_even(kept: u64, midpoint: Ordering) -> u64 {
    kept + u64::from(midpoint.is_gt() || (midpoint.is_eq() && kept & 1 != 0))
}

#[cfg(test)]
mod tests {
    use super::{Fault, Format, Ordering};

    #[test]
    fn signed_zeros_and_float_faults() {
        for format in [Format::F32, Format::F64] {
            let negative_zero = format.sign_bit();
            let one = (format.bias() as u64) << format.fraction_bits();
            let negative_one = one | negative_zero;

            assert_eq!(format.add(negative_zero, negative_zero), Ok(negative_zero));
            assert_eq!(format.add(negative_zero, 0), Ok(0));
            assert_eq!(format.add(one, negative_one), Ok(0));
            assert_eq!(format.sub(negative_zero, 0), Ok(negative_zero));
            assert_eq!(format.mul(negative_zero, negative_one), Ok(0));
            assert_eq!(format.div(0, negative_one), Ok(negative_zero));
            assert_eq!(format.div(0, negative_zero), Err(Fault::DivisionByZero));
            assert_eq!(format.sqrt(negative_zero), Ok(negative_zero));
            assert_eq!(format.sqrt(negative_zero | 1), Err(Fault::Invalid));
            assert_eq!(
                format.fma(0, negative_one, negative_zero),
                Ok(negative_zero)
            );
            assert_eq!(format.fma(0, negative_one, 0), Ok(0));
            assert_eq!(format.compare(0, negative_zero), Ordering::Equal);
            assert_eq!(format.compare(negative_zero | 1, 0), Ordering::Less);
        }
    }

    #[test]
    fn midpoint_rounding_at_normal_subnormal_and_overflow_boundaries() {
        for format in [Format::F32, Format::F64] {
            let fraction_bits = format.fraction_bits();
            let encode_power = |power| ((format.bias() + power) as u64) << fraction_bits;
            let midpoint = encode_power(-fraction_bits - 1);
            let one = encode_power(0);
            let two = encode_power(1);
            let max = encode_power(format.bias() + 1) - 1;
            let overflow_midpoint = encode_power(format.bias() - fraction_bits - 1);
            let min_normal = 1 << fraction_bits;

            assert_eq!(format.add(one, midpoint), Ok(one));
            assert_eq!(format.add(one + 1, midpoint), Ok(one + 2));
            assert_eq!(format.div(1, two), Ok(0));
            assert_eq!(format.div(3, two), Ok(2));
            assert_eq!(
                format.div(format.sign_bit() | 1, two),
                Ok(format.sign_bit())
            );
            assert_eq!(format.div(2 * min_normal - 1, two), Ok(min_normal));
            assert_eq!(format.add(max, overflow_midpoint - 1), Ok(max));
            assert_eq!(format.add(max, overflow_midpoint), Err(Fault::Overflow));
            assert_eq!(format.mul(max, two), Err(Fault::Overflow));
            assert_eq!(format.fma(max, two, format.sign_bit() | max), Ok(max));
        }
    }

    #[test]
    fn fma_preserves_the_exact_product_until_rounding_the_sum() {
        let a = 0x3f80_0001;
        let b = 0x3f7f_fffe;
        let c = 0xbf80_0000;
        assert_eq!(Format::F32.add(Format::F32.mul(a, b).unwrap(), c), Ok(0));
        assert_eq!(Format::F32.fma(a, b, c), Ok(0xa880_0000));

        let a = 0x3ff0_0000_0000_0001;
        let b = 0x3fef_ffff_ffff_fffe;
        let c = 0xbff0_0000_0000_0000;
        assert_eq!(Format::F64.add(Format::F64.mul(a, b).unwrap(), c), Ok(0));
        assert_eq!(Format::F64.fma(a, b, c), Ok(0xb970_0000_0000_0000));
    }

    #[test]
    fn fma_preserves_bits_far_below_the_rounding_unit() {
        for format in [Format::F32, Format::F64] {
            let fraction_bits = format.fraction_bits();
            let one = (format.bias() as u64) << fraction_bits;
            let one_and_half = one | (1 << (fraction_bits - 1));
            let negative_min = format.sign_bit() | 1;

            assert_eq!(format.fma(one + 1, one_and_half, 0), Ok(one_and_half + 2));
            assert_eq!(
                format.fma(one + 1, one_and_half, negative_min),
                Ok(one_and_half + 1)
            );
        }
    }

    #[test]
    fn square_roots_of_normal_and_subnormal_values() {
        assert_eq!(Format::F32.sqrt(0x4000_0000), Ok(0x3fb5_04f3));
        assert_eq!(Format::F32.sqrt(1), Ok(0x1a35_04f3));
        assert_eq!(
            Format::F64.sqrt(0x4000_0000_0000_0000),
            Ok(0x3ff6_a09e_667f_3bcd)
        );
        assert_eq!(Format::F64.sqrt(1), Ok(0x1e60_0000_0000_0000));
    }

    #[test]
    fn conversions_round_once_and_check_the_signed_integer_boundary() {
        let half_to_next_single = 0x3ff0_0000_1000_0000;
        assert_eq!(
            Format::F64.convert(half_to_next_single, Format::F32),
            Ok(0x3f80_0000)
        );
        assert_eq!(
            Format::F64.convert(half_to_next_single + 1, Format::F32),
            Ok(0x3f80_0001)
        );
        assert_eq!(
            Format::F32.convert(0xffff_ffff_8000_0000, Format::F64),
            Ok(1 << 63)
        );
        assert_eq!(Format::F32.from_integer((1 << 24) + 1), Ok(0x4b80_0000));
        assert_eq!(
            Format::F64.from_integer(i128::MIN),
            Ok(0xc7e0_0000_0000_0000)
        );
        assert_eq!(Format::F64.truncate(0xbfff_ffff_ffff_ffff), Some(-1));
        assert_eq!(Format::F64.truncate(0x47e0_0000_0000_0000), None);
        assert_eq!(Format::F64.truncate(0xc7e0_0000_0000_0000), Some(i128::MIN));
        assert_eq!(Format::F64.truncate(0xc7e0_0000_0000_0001), None);
    }
}
