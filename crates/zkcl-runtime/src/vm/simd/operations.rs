use std::cmp::Ordering;

use super::conditions::{self, Arithmetic};
use super::*;

pub(super) fn arithmetic(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    use Op::*;

    let first = pack(runtime, args.get(0), at)?;
    let shape = first.shape();
    let kind = shape.kind();
    let format = float_format(kind);
    if matches!(op, Div | Sqrt | Fma) && format.is_none() {
        return Err(wrong_domain(runtime, args.get(0), true, at));
    }
    if matches!(
        op,
        AddSaturating
            | SubSaturating
            | AddChecked
            | SubChecked
            | BitAnd
            | BitOr
            | BitXor
            | BitNot
            | ShiftLeft
            | ShiftRight
    ) && format.is_some()
    {
        return Err(wrong_domain(runtime, args.get(0), false, at));
    }
    if matches!(op, ShiftLeft | ShiftRight) {
        let value = args.get(1);
        let width = 8 * kind.byte_len();
        let shift = if let Some(count) = value.as_fixnum().filter(|&value| value >= 0) {
            (count as usize).min(width)
        } else {
            nonnegative(runtime, value, at)?
                .to_usize()
                .unwrap_or(width)
                .min(width)
        };
        return build_pack(runtime, shape, at, |i| {
            let bits = first.lane(i);
            let shifted = if op == ShiftLeft {
                if shift < width { bits << shift } else { 0 }
            } else {
                (integer_lane(kind, bits) >> shift) as u64
            };
            Ok(shifted & lane_mask(kind))
        });
    }
    let second = (args.len() > 1)
        .then(|| same_shape(runtime, args.get(1), shape, at))
        .transpose()?;
    let third = (op == Fma)
        .then(|| same_shape(runtime, args.get(2), shape, at))
        .transpose()?;
    if let Some(second) = second.as_ref() {
        let computed = match third.as_ref() {
            Some(third) => native::packed_fma(&first, second, third),
            None => native_operation(op).and_then(|arithmetic| {
                if format.is_some() {
                    native::packed_float(arithmetic, &first, second)
                } else {
                    native::packed_integer(arithmetic, &first, second)
                }
            }),
        };
        if let Some(bytes) = computed {
            return box_checked_bytes(
                runtime,
                op,
                shape,
                args,
                &bytes[..shape.byte_len()],
                Arithmetic::Overflow,
                at,
            );
        }
    }
    if let Some(format) = format {
        let fault = |failure, lane| float_fault(runtime, failure, op, args, lane, None, at);
        build_pack(runtime, shape, at, |i| {
            let first = first.lane(i);
            let b = || second.as_ref().expect("binary operation").lane(i);
            let bits = match op {
                Add => format.add(first, b()),
                Sub => format.sub(first, b()),
                Mul => format.mul(first, b()),
                Div => format.div(first, b()),
                Sqrt => format.sqrt(first),
                Fma => format.fma(
                    first,
                    b(),
                    third.as_ref().expect("ternary operation").lane(i),
                ),
                Min | Max => Ok(float_min_max(format, op, first, b())),
                _ => unreachable!(),
            };
            bits.map_err(|failure| fault(failure, i))
        })
    } else {
        build_pack(runtime, shape, at, |i| {
            let a = first.lane(i);
            let b = || second.as_ref().expect("binary operation").lane(i);
            let result = match op {
                Add => a.wrapping_add(b()),
                Sub => a.wrapping_sub(b()),
                Mul => a.wrapping_mul(b()),
                BitAnd => a & b(),
                BitOr => a | b(),
                BitXor => a ^ b(),
                BitNot => !a,
                Min | Max => {
                    let (left, right) = (integer_lane(kind, a), integer_lane(kind, b()));
                    if op == Min {
                        left.min(right) as u64
                    } else {
                        left.max(right) as u64
                    }
                }
                AddSaturating | SubSaturating | AddChecked | SubChecked => {
                    let (left, right) = (integer_lane(kind, a), integer_lane(kind, b()));
                    let exact = if matches!(op, AddChecked | AddSaturating) {
                        left + right
                    } else {
                        left - right
                    };
                    let (min, max) = integer_range(kind);
                    if matches!(op, AddChecked | SubChecked) && !(min..=max).contains(&exact) {
                        return Err(conditions::arithmetic(
                            runtime,
                            Arithmetic::IntegerOverflow,
                            op.name(),
                            operands(args),
                            i,
                            None,
                            at,
                        ));
                    }
                    exact.clamp(min, max) as u64
                }
                _ => unreachable!(),
            };
            Ok(result & lane_mask(kind))
        })
    }
}

fn native_operation(op: Op) -> Option<native::Arithmetic> {
    match op {
        Op::Add => Some(native::Arithmetic::Add),
        Op::Sub => Some(native::Arithmetic::Sub),
        Op::Mul => Some(native::Arithmetic::Mul),
        _ => None,
    }
}

fn float_min_max(format: Format, op: Op, a: u64, b: u64) -> u64 {
    match format.compare(a, b) {
        Ordering::Equal => {
            if op == Op::Min {
                a | b
            } else {
                a & b
            }
        }
        Ordering::Less if op == Op::Min => a,
        Ordering::Greater if op == Op::Max => a,
        _ => b,
    }
}

pub(super) fn float_class(failure: float::Fault) -> Arithmetic {
    match failure {
        float::Fault::DivisionByZero => Arithmetic::DivisionByZero,
        float::Fault::Overflow => Arithmetic::Overflow,
        float::Fault::Invalid => Arithmetic::Invalid,
    }
}

fn float_fault(
    runtime: &Runtime,
    failure: float::Fault,
    op: Op,
    args: &RootSlots,
    lane: usize,
    level: Option<usize>,
    at: usize,
) -> Fault {
    conditions::arithmetic(
        runtime,
        float_class(failure),
        op.name(),
        operands(args),
        lane,
        level,
        at,
    )
}

fn wrong_domain(runtime: &Runtime, value: Word, float: bool, at: usize) -> Fault {
    let message = if float {
        "expected a floating-point pack"
    } else {
        "expected an integer pack"
    };
    shape_constraint(
        runtime,
        value,
        |shape| float_format(shape.kind()).is_some() == float,
        message,
        at,
    )
}

pub(super) fn compare(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    let a = pack(runtime, args.get(0), at)?;
    let shape = a.shape();
    let b = same_shape(runtime, args.get(1), shape, at)?;
    let kind = shape.kind();
    let mut bits = 0;
    if kind == LaneKind::U8 && matches!(op, Op::CmpEq | Op::CmpNe) {
        for (chunk, (a, b)) in a
            .as_bytes()
            .as_chunks::<16>()
            .0
            .iter()
            .zip(b.as_bytes().as_chunks::<16>().0)
            .enumerate()
        {
            bits |= u64::from(native::u8x16_eq(*a, *b)) << (16 * chunk);
        }
        if op == Op::CmpNe {
            bits ^= SimdMask::allowed_bits(shape.lanes()).unwrap();
        }
    } else {
        let format = float_format(kind);
        for lane in 0..shape.lanes() {
            let (a, b) = (a.lane(lane), b.lane(lane));
            let order = match format {
                Some(format) => format.compare(a, b),
                None => integer_lane(kind, a).cmp(&integer_lane(kind, b)),
            };
            let matched = match op {
                Op::CmpEq => order.is_eq(),
                Op::CmpNe => order.is_ne(),
                Op::CmpLt => order.is_lt(),
                Op::CmpLe => order.is_le(),
                Op::CmpGt => order.is_gt(),
                Op::CmpGe => order.is_ge(),
                _ => unreachable!(),
            };
            bits |= u64::from(matched) << lane;
        }
    }
    mask_word(runtime, shape.lanes(), bits, at)
}

pub(super) fn select(runtime: &Runtime, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let mask = mask(runtime, args.get(0), at)?;
    let a = pack(runtime, args.get(1), at)?;
    let shape = a.shape();
    if mask.lanes() != shape.lanes() {
        mask_at(runtime, args.get(0), shape.lanes(), at)?;
    }
    let b = same_shape(runtime, args.get(2), shape, at)?;
    build_pack(runtime, shape, at, |i| {
        Ok(if mask.bits() & (1 << i) != 0 {
            a.lane(i)
        } else {
            b.lane(i)
        })
    })
}

pub(super) fn masked(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    let mask = mask(runtime, args.get(0), at)?;
    let a = pack(runtime, args.get(1), at)?;
    let shape = a.shape();
    let format =
        float_format(shape.kind()).ok_or_else(|| wrong_domain(runtime, args.get(1), true, at))?;
    if mask.lanes() != shape.lanes() {
        mask_at(runtime, args.get(0), shape.lanes(), at)?;
    }
    let b = (op == Op::DivMasked)
        .then(|| same_shape(runtime, args.get(2), shape, at))
        .transpose()?;
    let passthrough = same_shape(runtime, args.get(args.len() - 1), shape, at)?;
    build_pack(runtime, shape, at, |i| {
        if mask.bits() & (1 << i) == 0 {
            return Ok(passthrough.lane(i));
        }
        let bits = if let Some(b) = b.as_ref() {
            format.div(a.lane(i), b.lane(i))
        } else {
            format.sqrt(a.lane(i))
        };
        bits.map_err(|failure| float_fault(runtime, failure, op, args, i, None, at))
    })
}

fn lane_bytes(bytes: &[u8]) -> u64 {
    let mut bits = [0; 8];
    bits[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(bits)
}

fn box_checked_bytes(
    runtime: &Runtime,
    op: Op,
    shape: PackShape,
    args: &RootSlots,
    bytes: &[u8],
    class: Arithmetic,
    at: usize,
) -> Result<Word, Fault> {
    let pack = SimdPack::from_bytes(shape, bytes).ok_or_else(|| {
        let kind = shape.kind();
        let lane = bytes
            .chunks_exact(kind.byte_len())
            .position(|bytes| !kind.valid_lane_bits(lane_bytes(bytes)))
            .expect("non-finite lane");
        conditions::arithmetic(runtime, class, op.name(), operands(args), lane, None, at)
    })?;
    box_pack(runtime, pack, at)
}

fn vector_length(runtime: &Runtime, value: Word, at: usize) -> Result<usize, Fault> {
    runtime
        .heap
        .array_info(value)
        .filter(|array| array.rank == 1)
        .map(|array| array.fill_pointer.unwrap_or(array.size))
        .ok_or_else(|| {
            Fault::type_error(
                runtime,
                value,
                "VECTOR",
                EvalError::new(at, "expected a vector"),
            )
        })
}

pub(super) fn rearrange(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::PackBits => {
            let pack = pack(runtime, args.get(0), at)?;
            let bytes = pack.as_bytes();
            runtime
                .heap
                .array_from_values(&[bytes.len()], ArrayElement::Octet, false, None, |i| {
                    ArrayValue::Octet(bytes[i])
                })
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        Op::PackFromBits => {
            let shape = shape(runtime, args.get(0), at)?;
            let len = vector_length(runtime, args.get(1), at)?;
            if len != shape.byte_len() {
                return Err(super::super::modules::arity_error(
                    at,
                    &format!(
                        "{} requires exactly {} bytes",
                        shape.name(),
                        shape.byte_len()
                    ),
                ));
            }
            let mut bytes = [0; 64];
            let bytes = &mut bytes[..len];
            for (i, byte) in bytes.iter_mut().enumerate() {
                let value = runtime
                    .heap
                    .array_slot(args.get(1), i)
                    .map_err(|error| EvalError::out_of_memory(at, error))?
                    .ok_or_else(|| bounds_error(runtime, args.get(1), &i.into(), i, at))?;
                *byte = read_lane(runtime, LaneKind::U8, value, at)? as u8;
            }
            box_checked_bytes(runtime, op, shape, args, bytes, Arithmetic::Invalid, at)
        }
        Op::Bitcast | Op::Convert => {
            let shape = shape(runtime, args.get(0), at)?;
            let value = pack(runtime, args.get(1), at)?;
            if op == Op::Bitcast {
                if shape.byte_len() != value.shape().byte_len() {
                    return Err(shape_constraint(
                        runtime,
                        args.get(1),
                        |candidate| candidate.byte_len() == shape.byte_len(),
                        "bitcast requires equal pack widths",
                        at,
                    ));
                }
                box_checked_bytes(
                    runtime,
                    op,
                    shape,
                    args,
                    value.as_bytes(),
                    Arithmetic::Invalid,
                    at,
                )
            } else {
                if shape.lanes() != value.shape().lanes() {
                    return Err(shape_constraint(
                        runtime,
                        args.get(1),
                        |candidate| candidate.lanes() == shape.lanes(),
                        "conversion requires equal lane counts",
                        at,
                    ));
                }
                let (from, to) = (value.shape().kind(), shape.kind());
                build_pack(runtime, shape, at, |i| {
                    convert_lane(from, to, value.lane(i)).map_err(|failure| {
                        conditions::arithmetic(
                            runtime,
                            failure,
                            op.name(),
                            operands(args),
                            i,
                            None,
                            at,
                        )
                    })
                })
            }
        }
        Op::Permute | Op::Shuffle => {
            let a = pack(runtime, args.get(0), at)?;
            let shape = a.shape();
            let b = (op == Op::Shuffle)
                .then(|| same_shape(runtime, args.get(1), shape, at))
                .transpose()?;
            let index_arg = args.len() - 1;
            let len = vector_length(runtime, args.get(index_arg), at)?;
            if len != shape.lanes() {
                return Err(super::super::modules::arity_error(
                    at,
                    &format!("{} requires exactly {} indices", op.name(), shape.lanes()),
                ));
            }
            let max = shape.lanes() * if b.is_some() { 2 } else { 1 } - 1;
            build_pack(runtime, shape, at, |i| {
                let value = runtime
                    .heap
                    .array_slot(args.get(index_arg), i)
                    .map_err(|error| EvalError::out_of_memory(at, error))?
                    .ok_or_else(|| bounds_error(runtime, args.get(index_arg), &i.into(), i, at))?;
                let index = bounded_integer(runtime, value, max, at)?;
                Ok(if index < shape.lanes() {
                    a.lane(index)
                } else {
                    b.as_ref()
                        .expect("index is in the second pack")
                        .lane(index - shape.lanes())
                })
            })
        }
        _ => unreachable!(),
    }
}

fn shape_constraint(
    runtime: &Runtime,
    value: Word,
    matches: impl FnMut(&PackShape) -> bool,
    message: &str,
    at: usize,
) -> Fault {
    let value = runtime.root_slot(value);
    let expected = (|| -> Result<Word, Fault> {
        let or = runtime.intern(&Name::common_lisp("OR"), at)?;
        let aliases = PackShape::ALL
            .into_iter()
            .filter(matches)
            .map(|shape| types::pack_symbol(runtime, shape));
        super::super::methods::list_from_words(
            runtime,
            std::iter::once(or).chain(aliases).collect(),
            at,
        )
    })();
    match expected {
        Ok(expected) => type_error(runtime, value.get(), expected, message, at),
        Err(fault) => fault,
    }
}

fn convert_lane(from: LaneKind, to: LaneKind, bits: u64) -> Result<u64, Arithmetic> {
    match (float_format(from), float_format(to)) {
        (Some(from), Some(to)) => from.convert(bits, to).map_err(float_class),
        (None, Some(to)) => to
            .from_integer(integer_lane(from, bits))
            .map_err(float_class),
        (prior, None) => {
            let integer = match prior {
                Some(from) => from.truncate(bits).ok_or(Arithmetic::IntegerOverflow)?,
                None => integer_lane(from, bits),
            };
            let (min, max) = integer_range(to);
            if !(min..=max).contains(&integer) {
                return Err(Arithmetic::IntegerOverflow);
            }
            Ok(integer as u64 & lane_mask(to))
        }
    }
}

pub(super) fn reduce_sum(runtime: &Runtime, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let values = pack(runtime, args.get(0), at)?;
    let [order] =
        super::super::named_keyword_arguments(runtime, args, 1..args.len(), ["ORDER"], at)?;
    let pairwise = if let Some(order) = order {
        let left = runtime.root_slot(runtime.intern_package(Package::KEYWORD, "LEFT", at)?.0);
        let pairwise = runtime.intern_package(Package::KEYWORD, "PAIRWISE", at)?.0;
        if args.get(order) == pairwise {
            true
        } else if args.get(order) == left.get() {
            false
        } else {
            let member = runtime.root_slot(runtime.intern(&Name::common_lisp("MEMBER"), at)?);
            let pairwise = runtime.intern_package(Package::KEYWORD, "PAIRWISE", at)?.0;
            let expected = super::super::methods::list_from_words(
                runtime,
                vec![member.get(), left.get(), pairwise],
                at,
            )?;
            return Err(type_error(
                runtime,
                args.get(order),
                expected,
                "expected :LEFT or :PAIRWISE",
                at,
            ));
        }
    } else {
        false
    };
    let shape = values.shape();
    let kind = shape.kind();
    let lanes = shape.lanes();
    let bits = if let Some(format) = float_format(kind) {
        let fail =
            |failure, i, level| float_fault(runtime, failure, Op::ReduceSum, args, i, level, at);
        if pairwise {
            let mut state: [u64; 64] = [0; 64];
            for (i, lane) in state[..lanes].iter_mut().enumerate() {
                *lane = values.lane(i);
            }
            for level in 0..lanes.ilog2() {
                let stride = 1 << level;
                for i in (0..lanes).step_by(stride * 2) {
                    state[i] = format
                        .add(state[i], state[i + stride])
                        .map_err(|failure| fail(failure, i, Some(level as usize)))?;
                }
            }
            state[0]
        } else {
            let mut sum = values.lane(0);
            for i in 1..lanes {
                sum = format
                    .add(sum, values.lane(i))
                    .map_err(|failure| fail(failure, i, None))?;
            }
            sum
        }
    } else {
        (0..lanes).fold(0u64, |acc, i| acc.wrapping_add(values.lane(i))) & lane_mask(kind)
    };
    box_lane(runtime, kind, bits, at)
}
