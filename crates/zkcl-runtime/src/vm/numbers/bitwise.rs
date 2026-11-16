use std::ops::Range;

use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};

use crate::condition::{Class, Fault};
use crate::heap::{Heap, RootSlots, Word};
use crate::number::Real;
use crate::{EvalError, OutOfMemory, Runtime};

use super::{Op, arithmetic_fault, ash, numeric_type_error, oom, run_integer};

pub(super) fn run_word(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let heap = &runtime.heap;
    let integer = |reg| run_integer(runtime, frame, reg, at);
    let boolean = |yes| if yes { Word::T } else { Word::NIL };

    let result = match op {
        Op::Logand => fold_bitwise(args.clone(), -1, integer, |a, b| a & b)?,
        Op::Logior => fold_bitwise(args.clone(), 0, integer, |a, b| a | b)?,
        Op::Logxor => fold_bitwise(args.clone(), 0, integer, |a, b| a ^ b)?,
        Op::Logeqv => fold_bitwise(args.clone(), -1, integer, |a, b| not(&(a ^ b)))?,
        Op::Lognot => not(&integer(args.start)?),
        Op::Lognand => not(&(integer(args.start)? & integer(args.start + 1)?)),
        Op::Lognor => not(&(integer(args.start)? | integer(args.start + 1)?)),
        Op::Logandc1 => not(&integer(args.start)?) & integer(args.start + 1)?,
        Op::Logandc2 => integer(args.start)? & not(&integer(args.start + 1)?),
        Op::Logorc1 => not(&integer(args.start)?) | integer(args.start + 1)?,
        Op::Logorc2 => integer(args.start)? | not(&integer(args.start + 1)?),
        Op::Boole => {
            let code = integer(args.start)?;
            let Some(op_code) = code.to_isize().filter(|c| (0..=15).contains(c)) else {
                return Err(Fault::integer_range(
                    runtime,
                    frame.get(args.start),
                    0,
                    15,
                    EvalError::new(at, "expected an integer in 0..=15"),
                ));
            };
            let a = integer(args.start + 1)?;
            let b = integer(args.start + 2)?;
            match op_code {
                0 => BigInt::zero(),
                1 => BigInt::from(-1),
                2 => a,
                3 => b,
                4 => not(&a),
                5 => not(&b),
                6 => a & b,
                7 => a | b,
                8 => a ^ b,
                9 => not(&(a ^ b)),
                10 => not(&(a & b)),
                11 => not(&(a | b)),
                12 => not(&a) & b,
                13 => a & not(&b),
                14 => not(&a) | b,
                15 => a | not(&b),
                _ => unreachable!("boole code checked 0..=15"),
            }
        }
        Op::Logtest => {
            let result = integer(args.start)? & integer(args.start + 1)?;
            return Ok(Some(boolean(!result.is_zero())));
        }
        Op::Logbitp => {
            let index = non_neg_index(runtime, frame.get(args.start), at)?;
            let n = integer(args.start + 1)?;
            return Ok(Some(boolean(
                index
                    .to_u64()
                    .map_or_else(|| n.is_negative(), |bit| n.bit(bit)),
            )));
        }
        Op::Logcount => {
            let n = integer(args.start)?;
            let bits = if n.is_negative() { not(&n) } else { n };
            let count: BigInt = bits
                .magnitude()
                .to_u64_digits()
                .into_iter()
                .map(|limb| limb.count_ones() as u64)
                .sum::<u64>()
                .into();
            count
        }
        Op::Gcd => {
            let mut acc = BigInt::zero();
            for reg in args.clone() {
                acc = acc.gcd(&integer(reg)?);
            }
            acc
        }
        Op::Lcm => {
            if args.is_empty() {
                BigInt::from(1)
            } else {
                let mut acc = integer(args.start)?.abs();
                for reg in args.start + 1..args.end {
                    let next = integer(reg)?.abs();
                    if acc.is_zero() || next.is_zero() {
                        acc = BigInt::zero();
                    } else {
                        acc = acc.lcm(&next);
                    }
                }
                acc
            }
        }
        Op::Isqrt => {
            let n = integer(args.start)?;
            if n.is_negative() {
                return Err(Fault::non_negative_integer(
                    runtime,
                    frame.get(args.start),
                    EvalError::new(at, "ISQRT expects a non-negative integer"),
                ));
            }
            n.sqrt()
        }
        Op::Byte => {
            non_neg_index(runtime, frame.get(args.start), at)?;
            non_neg_index(runtime, frame.get(args.start + 1), at)?;
            let pair = heap
                .cons(|| (frame.get(args.start), frame.get(args.start + 1)))
                .map_err(|e| EvalError::out_of_memory(at, e))?;
            return Ok(Some(pair));
        }
        Op::ByteSize => bytespec_parts(runtime, frame.get(args.start), at)?.0,
        Op::BytePosition => bytespec_parts(runtime, frame.get(args.start), at)?.1,
        Op::Ldb | Op::LdbTest | Op::MaskField => {
            let (size, pos) = bytespec_parts(runtime, frame.get(args.start), at)?;
            let n = integer(args.start + 1)?;
            let shifted = ash(heap, n, &(-&pos)).map_err(oom)?;
            if op == Op::LdbTest {
                return Ok(Some(boolean(
                    shifted
                        .trailing_zeros()
                        .is_some_and(|zeros| size > BigInt::from(zeros)),
                )));
            }
            let extracted = low_bits(heap, shifted, &size).map_err(oom)?;
            if op == Op::MaskField {
                ash(heap, extracted, &pos).map_err(oom)?
            } else {
                extracted
            }
        }
        Op::Dpb | Op::DepositField => {
            let newbyte = integer(args.start)?;
            let (size, pos) = bytespec_parts(runtime, frame.get(args.start + 1), at)?;
            let target = integer(args.start + 2)?;
            let shift = -&pos;
            let oldbyte = ash(heap, target.clone(), &shift).map_err(oom)?;
            let newbyte = if op == Op::DepositField {
                ash(heap, newbyte, &shift).map_err(oom)?
            } else {
                newbyte
            };
            // Flip only the bits that differ inside the field.
            let changed = low_bits(heap, oldbyte ^ newbyte, &size).map_err(oom)?;
            target ^ ash(heap, changed, &pos).map_err(oom)?
        }
        _ => return Ok(None),
    };

    if result.bits() > heap.max_integer_bits() {
        return Err(arithmetic_fault(
            runtime,
            frame,
            op,
            args,
            at,
            Class::ArithmeticError,
        ));
    }
    let word = heap
        .allocate_number(&crate::number::Number::Real(Real::Integer(result)))
        .map_err(oom)?;
    Ok(Some(word))
}

fn not(n: &BigInt) -> BigInt {
    -(n + 1u8)
}

fn fold_bitwise(
    mut args: Range<usize>,
    identity: isize,
    mut integer: impl FnMut(usize) -> Result<BigInt, Fault>,
    mut combine: impl FnMut(BigInt, BigInt) -> BigInt,
) -> Result<BigInt, Fault> {
    let Some(first) = args.next() else {
        return Ok(BigInt::from(identity));
    };
    let mut acc = integer(first)?;
    for reg in args {
        acc = combine(acc, integer(reg)?);
    }
    Ok(acc)
}

fn non_neg_index(runtime: &Runtime, word: Word, at: usize) -> Result<BigInt, Fault> {
    let Some(n) = runtime
        .heap
        .integer(word)
        .filter(|v| v.sign() != Sign::Minus)
    else {
        return Err(Fault::non_negative_integer(
            runtime,
            word,
            EvalError::new(at, "expected a non-negative integer"),
        ));
    };
    Ok(n)
}

fn bytespec_parts(runtime: &Runtime, word: Word, at: usize) -> Result<(BigInt, BigInt), Fault> {
    let Some((size, pos)) = runtime.heap.cons_fields(word) else {
        return Err(numeric_type_error(runtime, word, "CONS", at));
    };
    Ok((
        non_neg_index(runtime, size, at)?,
        non_neg_index(runtime, pos, at)?,
    ))
}

fn low_bits(heap: &Heap, value: BigInt, size: &BigInt) -> Result<BigInt, OutOfMemory> {
    if !value.is_negative() && size >= &BigInt::from(value.bits()) {
        return Ok(value);
    }
    let width = size
        .to_u64()
        .filter(|&bits| bits <= heap.max_integer_bits())
        .and_then(|bits| usize::try_from(bits).ok())
        .ok_or(OutOfMemory)?;
    Ok(value & ((BigInt::from(1u8) << width) - 1u8))
}
