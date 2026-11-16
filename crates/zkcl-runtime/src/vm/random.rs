use std::ops::Range;

use num_bigint::{BigInt, BigUint};
use num_traits::{Float, Signed, ToPrimitive};

use super::symbol_value;
use crate::condition::Fault;
use crate::heap::{RandomState, RootSlots, Word};
use crate::number::{Number, Real};
use crate::symbol::Name;
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Predicate,
    Make,
    Sample,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("RANDOM-STATE-P", Op::Predicate),
    ("MAKE-RANDOM-STATE", Op::Make),
    ("RANDOM", Op::Sample),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Predicate => count == 1,
            Self::Make => count <= 1,
            Self::Sample => (1..=2).contains(&count),
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::Predicate => Ok(if runtime.heap.is_random_state(frame.get(args.start)) {
            Word::T
        } else {
            Word::NIL
        }),
        Op::Make => {
            let argument = if args.is_empty() {
                Word::NIL
            } else {
                frame.get(args.start)
            };
            let state = match argument {
                Word::T => {
                    let mut seed = [0; 32];
                    runtime
                        .host_services
                        .fill_entropy(&mut seed)
                        .map_err(|err| EvalError::io_error(at, err))?;
                    RandomState::from_seed(seed)
                }
                Word::NIL => state_value(runtime, current_state(runtime, at)?, at)?,
                _ => runtime.heap.random_state_value(argument).ok_or_else(|| {
                    Fault::random_state_designator(runtime, 
                        argument,
                        EvalError::new(at, "expected a random state, T, or NIL"),
                    )
                })?,
            };
            runtime
                .heap
                .allocate_random_state(state)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        Op::Sample => {
            let state = if args.len() == 1 {
                current_state(runtime, at)?
            } else {
                frame.get(args.start + 1)
            };
            sample(runtime, frame.get(args.start), state, at)
        }
    }
}

fn current_state(runtime: &Runtime, at: usize) -> Result<Word, Fault> {
    let symbol = runtime.intern(&Name::common_lisp("*RANDOM-STATE*"), at)?;
    symbol_value(runtime, symbol, at)
}

fn state_value(runtime: &Runtime, word: Word, at: usize) -> Result<RandomState, Fault> {
    runtime.heap.random_state_value(word).ok_or_else(|| {
        Fault::type_error(runtime, 
            word,
            "RANDOM-STATE",
            EvalError::new(at, "expected a random state"),
        )
    })
}

fn sample(runtime: &Runtime, limit: Word, word: Word, at: usize) -> Result<Word, Fault> {
    let limit_error = || {
        Fault::random_limit(runtime, 
            limit,
            EvalError::new(at, "expected a positive integer or float"),
        )
    };
    let mut state = state_value(runtime, word, at)?;
    if let Some(limit) = limit.as_fixnum() {
        if limit <= 0 {
            return Err(limit_error());
        }
        let value = below_word(&mut state, limit as u64);
        runtime.heap.set_random_state(word, state);
        return Ok(Word::fixnum(value as isize).expect("sample fits fixnum limit"));
    }

    let value = match runtime.heap.number(limit) {
        Some(Number::Real(Real::Integer(limit))) if limit.is_positive() => {
            Real::Integer(below_integer(&mut state, limit.magnitude()))
        }
        Some(Number::Real(Real::Single(limit))) if limit > 0.0 && limit.is_finite() => {
            Real::Single(below_float(&mut state, limit, f32::MANTISSA_DIGITS))
        }
        Some(Number::Real(Real::Double(limit))) if limit > 0.0 && limit.is_finite() => {
            Real::Double(below_float(&mut state, limit, f64::MANTISSA_DIGITS))
        }
        _ => return Err(limit_error()),
    };
    runtime.heap.set_random_state(word, state);
    runtime
        .heap
        .allocate_number(&Number::Real(value))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn below_float<T: Float>(state: &mut RandomState, limit: T, digits: u32) -> T {
    let unit = T::from(1u64 << digits)
        .expect("power of two fits float")
        .recip();
    loop {
        let sample = state.next() >> (u64::BITS - digits);
        let fraction = T::from(sample).expect("sample fits significand") * unit;
        let value = fraction * limit;
        if value < limit {
            return value;
        }
    }
}

fn below_word(state: &mut RandomState, limit: u64) -> u64 {
    // Discard the incomplete residue band before reducing modulo the bound.
    let threshold = limit.wrapping_neg() % limit;
    loop {
        let value = state.next();
        if value >= threshold {
            return value % limit;
        }
    }
}

fn below_integer(state: &mut RandomState, limit: &BigUint) -> BigInt {
    if let Some(word) = limit.to_u64() {
        return below_word(state, word).into();
    }
    let bits = (limit - 1u8).bits();
    let len = usize::try_from(bits.div_ceil(8)).expect("byte count fits the existing integer");
    let mut bytes = vec![0u8; len];
    let shift = (8 - bits % 8) % 8;
    loop {
        for chunk in bytes.chunks_mut(size_of::<u64>()) {
            chunk.copy_from_slice(&state.next().to_le_bytes()[..chunk.len()]);
        }
        *bytes.last_mut().unwrap() &= u8::MAX >> shift;
        let value = BigUint::from_bytes_le(&bytes);
        if &value < limit {
            return value.into();
        }
    }
}
