use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;
use zkcl_bytecode::{LaneKind, PackShape, SimdMask, SimdPack};

use crate::condition::Fault;
use crate::heap::{ArrayElement, ArrayValue, RootSlots, Word};
use crate::number::{Number, Real};
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, Runtime};

use super::types::simd as types;

mod bulk;
mod conditions;
mod float;
mod memory;
mod native;
mod operations;

use conditions::bounds_error;
use float::Format;

macro_rules! primitives {
    ($($variant:ident, $name:literal => $arity:pat),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Op { $($variant),* }

        pub(super) const PRIMITIVES: &[(&str, Op)] = &[
            $((concat!("ZK-SIMD:", $name), Op::$variant)),*
        ];

        impl Op {
            fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),* }
            }

            pub(super) fn accepts(self, count: usize) -> bool {
                match self {
                    $(Self::$variant => matches!(count, $arity)),*
                }
            }
        }
    };
}

primitives! {
    MakePack, "MAKE-PACK" => 1..,
    Splat, "SPLAT" => 2,
    PackP, "PACK-P" => 1,
    MaskP, "MASK-P" => 1,
    LaneCount, "LANE-COUNT" => 1,
    Lane, "LANE" => 2,
    PackLanes, "PACK-LANES" => 1,
    Add, "ADD" => 2,
    Sub, "SUB" => 2,
    Mul, "MUL" => 2,
    Div, "DIV" => 2,
    Sqrt, "SQRT" => 1,
    Fma, "FMA" => 3,
    Min, "MIN" => 2,
    Max, "MAX" => 2,
    AddSaturating, "ADD-SATURATING" => 2,
    SubSaturating, "SUB-SATURATING" => 2,
    AddChecked, "ADD-CHECKED" => 2,
    SubChecked, "SUB-CHECKED" => 2,
    BitAnd, "BIT-AND" => 2,
    BitOr, "BIT-OR" => 2,
    BitXor, "BIT-XOR" => 2,
    BitNot, "BIT-NOT" => 1,
    ShiftLeft, "SHIFT-LEFT" => 2,
    ShiftRight, "SHIFT-RIGHT" => 2,
    CmpEq, "CMP=" => 2,
    CmpNe, "CMP/=" => 2,
    CmpLt, "CMP<" => 2,
    CmpLe, "CMP<=" => 2,
    CmpGt, "CMP>" => 2,
    CmpGe, "CMP>=" => 2,
    Select, "SELECT" => 3,
    MaskFromBits, "MASK-FROM-BITS" => 2,
    MaskFirst, "MASK-FIRST" => 2,
    MaskBits, "MASK-BITS" => 1,
    MaskAnd, "MASK-AND" => 2,
    MaskOr, "MASK-OR" => 2,
    MaskXor, "MASK-XOR" => 2,
    MaskNot, "MASK-NOT" => 1,
    MaskAnyP, "MASK-ANY-P" => 1,
    MaskAllP, "MASK-ALL-P" => 1,
    MaskCount, "MASK-COUNT" => 1,
    SameBitsP, "SAME-BITS-P" => 2,
    Permute, "PERMUTE" => 2,
    Shuffle, "SHUFFLE" => 3,
    Bitcast, "BITCAST" => 2,
    PackBits, "PACK-BITS" => 1,
    PackFromBits, "PACK-FROM-BITS" => 2,
    Convert, "CONVERT" => 2,
    LoadPack, "LOAD-PACK" => 3,
    StorePack, "STORE-PACK" => 3,
    LoadMasked, "LOAD-MASKED" => 4..=5,
    StoreMasked, "STORE-MASKED" => 4,
    Gather, "GATHER" => 4..=5,
    Scatter, "SCATTER" => 4,
    DivMasked, "DIV-MASKED" => 4,
    SqrtMasked, "SQRT-MASKED" => 3,
    ReduceSum, "REDUCE-SUM" => 1..,
    MulAddInto, "MUL-ADD-INTO" => 4..,
    OctetEqMask, "OCTET-EQ-MASK" => 3..=4,
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    use zkcl_syntax::simd_builtins::{CONDITIONS, FUNCTIONS, PACKAGE};

    types::initialize(runtime)?;
    let export = |name: &str| -> Result<Word, EvalError> {
        let word = runtime.intern_package(Package::SIMD, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .export(Package::SIMD, name, word)
            .map_err(|error| EvalError::from_error(0, error))?;
        Ok(word)
    };

    for &(qualified, op) in PRIMITIVES {
        let symbol = export(op.name())?;
        runtime.heap.set_symbol_cell(
            symbol,
            true,
            super::Primitive::named(qualified).expect("registered SIMD primitive"),
        );
    }
    runtime.load_builtin_source(include_str!("../../../lisp/runtime/simd/conditions.lisp"))?;
    // Readers on conditions are installed by the bundled definitions. Types and
    // operations already exist in the package by the time the file is loaded.
    for name in CONDITIONS.iter().copied().chain(
        FUNCTIONS
            .iter()
            .filter(|function| function.name.starts_with("CONDITION-"))
            .map(|function| function.name),
    ) {
        export(name)?;
    }
    let module = runtime
        .make_string(PACKAGE)
        .map_err(|error| EvalError::out_of_memory(0, error))?;
    super::modules::provide(runtime, module.slot.get(), 0).map_err(Fault::into_eval_error)?;
    Ok(())
}

pub(super) fn documentation(runtime: &Runtime, function: Word, at: usize) -> Result<Word, Fault> {
    match super::Primitive::decode(function) {
        Some(super::Primitive::Simd(op)) => {
            let doc = zkcl_syntax::simd_builtins::function(op.name())
                .expect("registered SIMD operation has public documentation");
            runtime
                .heap
                .string(doc.documentation)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        _ => Ok(Word::NIL),
    }
}

pub(super) fn invoke(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    use Op::*;
    match op {
        MakePack | Splat => make_pack(runtime, op, args, at),
        PackP => Ok(truth(runtime.heap.simd_pack_shape(args.get(0)).is_some())),
        MaskP => Ok(truth(runtime.heap.simd_mask(args.get(0)).is_some())),
        LaneCount => {
            let value = args.get(0);
            let lanes = runtime
                .heap
                .simd_pack_shape(value)
                .map(PackShape::lanes)
                .or_else(|| runtime.heap.simd_mask(value).map(SimdMask::lanes))
                .ok_or_else(|| expected_pack_or_mask(runtime, value, at))?;
            Ok(small_integer(lanes))
        }
        Lane => {
            let value = pack(runtime, args.get(0), at)?;
            let index = bounded_integer(runtime, args.get(1), value.shape().lanes() - 1, at)?;
            box_lane(runtime, value.shape().kind(), value.lane(index), at)
        }
        PackLanes => pack_lanes(runtime, args.get(0), at),
        MaskFromBits | MaskFirst | MaskBits | MaskAnd | MaskOr | MaskXor | MaskNot | MaskAnyP
        | MaskAllP | MaskCount => mask_operation(runtime, op, args, at),
        SameBitsP => {
            let (a, b) = (args.get(0), args.get(1));
            let same = if let Some(pack) = runtime.heap.simd_pack(a) {
                runtime.heap.simd_pack(b) == Some(pack)
            } else if let Some(mask) = runtime.heap.simd_mask(a) {
                runtime.heap.simd_mask(b) == Some(mask)
            } else {
                false
            };
            Ok(truth(same))
        }
        PackBits | PackFromBits | Bitcast | Convert | Permute | Shuffle => {
            operations::rearrange(runtime, op, args, at)
        }
        LoadPack | StorePack | LoadMasked | StoreMasked | Gather | Scatter => {
            memory::invoke(runtime, op, args, at)
        }
        MulAddInto => bulk::mul_add_into(runtime, args, at),
        OctetEqMask => bulk::octet_eq_mask(runtime, args, at),
        ReduceSum => operations::reduce_sum(runtime, args, at),
        CmpEq | CmpNe | CmpLt | CmpLe | CmpGt | CmpGe => operations::compare(runtime, op, args, at),
        Select => operations::select(runtime, args, at),
        DivMasked | SqrtMasked => operations::masked(runtime, op, args, at),
        Add | Sub | Mul | Div | Sqrt | Fma | Min | Max | AddSaturating | SubSaturating
        | AddChecked | SubChecked | BitAnd | BitOr | BitXor | BitNot | ShiftLeft | ShiftRight => {
            operations::arithmetic(runtime, op, args, at)
        }
    }
}

fn shape(runtime: &Runtime, value: Word, at: usize) -> Result<PackShape, Fault> {
    types::exact_pack_shape(runtime, value).ok_or_else(|| {
        Fault::type_error(
            runtime,
            value,
            "TYPE-SPECIFIER",
            EvalError::new(at, "expected a concrete ZK-SIMD pack type"),
        )
    })
}

fn pack(runtime: &Runtime, value: Word, at: usize) -> Result<SimdPack, Fault> {
    runtime.heap.simd_pack(value).ok_or_else(|| {
        type_error(
            runtime,
            value,
            types::pack_class_symbol(runtime),
            "expected a ZK-SIMD pack",
            at,
        )
    })
}

fn same_shape(
    runtime: &Runtime,
    value: Word,
    expected: PackShape,
    at: usize,
) -> Result<SimdPack, Fault> {
    runtime
        .heap
        .simd_pack(value)
        .filter(|value| value.shape() == expected)
        .ok_or_else(|| {
            type_error(
                runtime,
                value,
                types::pack_symbol(runtime, expected),
                "pack shapes must match",
                at,
            )
        })
}

fn mask(runtime: &Runtime, value: Word, at: usize) -> Result<SimdMask, Fault> {
    runtime.heap.simd_mask(value).ok_or_else(|| {
        type_error(
            runtime,
            value,
            types::mask_class_symbol(runtime),
            "expected a ZK-SIMD mask",
            at,
        )
    })
}

fn mask_at(runtime: &Runtime, value: Word, lanes: usize, at: usize) -> Result<SimdMask, Fault> {
    if let Some(mask) = runtime
        .heap
        .simd_mask(value)
        .filter(|mask| mask.lanes() == lanes)
    {
        return Ok(mask);
    }
    let value = runtime.root_slot(value);
    let expected = types::mask_type(runtime, lanes, at)?;
    Err(type_error(
        runtime,
        value.get(),
        expected,
        "mask lane count must match",
        at,
    ))
}

fn type_error(runtime: &Runtime, value: Word, expected: Word, message: &str, at: usize) -> Fault {
    Fault::type_error_specifier(runtime, value, expected, EvalError::new(at, message))
}

fn expected_pack_or_mask(runtime: &Runtime, value: Word, at: usize) -> Fault {
    let value = runtime.root_slot(value);
    let or = match runtime.intern(&Name::common_lisp("OR"), at) {
        Ok(or) => or,
        Err(error) => return error.into(),
    };
    let expected = super::methods::list_from_words(
        runtime,
        vec![
            or,
            types::pack_class_symbol(runtime),
            types::mask_class_symbol(runtime),
        ],
        at,
    );
    match expected {
        Ok(expected) => type_error(
            runtime,
            value.get(),
            expected,
            "expected a pack or a mask",
            at,
        ),
        Err(fault) => fault,
    }
}

fn nonnegative(runtime: &Runtime, value: Word, at: usize) -> Result<BigInt, Fault> {
    if let Some(integer) = runtime
        .heap
        .integer(value)
        .filter(|value| value.sign() != Sign::Minus)
    {
        return Ok(integer);
    }
    Err(integer_range_error(
        runtime,
        value,
        BigInt::from(0),
        None,
        at,
    ))
}

fn bounded_integer(
    runtime: &Runtime,
    value: Word,
    maximum: usize,
    at: usize,
) -> Result<usize, Fault> {
    let integer = match value.as_fixnum() {
        Some(value) => usize::try_from(value).ok(),
        None => runtime.heap.integer(value).and_then(|n| n.to_usize()),
    };
    if let Some(integer) = integer.filter(|n| *n <= maximum) {
        return Ok(integer);
    }
    Err(integer_range_error(
        runtime,
        value,
        0.into(),
        Some(maximum.into()),
        at,
    ))
}

fn integer_range_error(
    runtime: &Runtime,
    value: Word,
    minimum: BigInt,
    maximum: Option<BigInt>,
    at: usize,
) -> Fault {
    let value = runtime.root_slot(value);
    let expected = (|| -> Result<Word, Fault> {
        let integer = runtime.root_slot(runtime.intern(&Name::common_lisp("INTEGER"), at)?);
        let low = runtime.root_slot(box_integer(runtime, &minimum, at)?);
        let high = match maximum {
            Some(maximum) => box_integer(runtime, &maximum, at)?,
            None => runtime.intern(&Name::common_lisp("*"), at)?,
        };
        super::methods::list_from_words(runtime, vec![integer.get(), low.get(), high], at)
    })();
    match expected {
        Ok(expected) => type_error(
            runtime,
            value.get(),
            expected,
            "expected an integer in the specified range",
            at,
        ),
        Err(fault) => fault,
    }
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

fn small_integer(value: usize) -> Word {
    Word::fixnum(value as isize).expect("bounded SIMD index fits in a fixnum")
}

fn box_integer(runtime: &Runtime, value: &BigInt, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .allocate_integer(value)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn box_pack(runtime: &Runtime, pack: SimdPack, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .allocate_simd_pack(pack)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn box_mask(runtime: &Runtime, mask: SimdMask, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .allocate_simd_mask(mask)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn mask_word(runtime: &Runtime, lanes: usize, bits: u64, at: usize) -> Result<Word, Fault> {
    box_mask(
        runtime,
        SimdMask::new(lanes, bits).expect("valid mask result"),
        at,
    )
}

fn lane_mask(kind: LaneKind) -> u64 {
    u64::MAX >> (64 - 8 * kind.byte_len())
}

fn float_format(kind: LaneKind) -> Option<Format> {
    match kind {
        LaneKind::F32 => Some(Format::F32),
        LaneKind::F64 => Some(Format::F64),
        _ => None,
    }
}

fn signed(kind: LaneKind) -> bool {
    matches!(
        kind,
        LaneKind::I8 | LaneKind::I16 | LaneKind::I32 | LaneKind::I64
    )
}

fn integer_lane(kind: LaneKind, bits: u64) -> i128 {
    let shift = 64 - 8 * kind.byte_len();
    if signed(kind) {
        i128::from(((bits << shift) as i64) >> shift)
    } else {
        i128::from(bits)
    }
}

fn integer_range(kind: LaneKind) -> (i128, i128) {
    let width = 8 * kind.byte_len();
    if signed(kind) {
        let edge = 1i128 << (width - 1);
        (-edge, edge - 1)
    } else {
        (0, (1i128 << width) - 1)
    }
}

fn read_lane(runtime: &Runtime, kind: LaneKind, value: Word, at: usize) -> Result<u64, Fault> {
    let heap = &runtime.heap;
    if let Some(format) = float_format(kind) {
        let (bits, expected) = match format {
            Format::F32 => (
                heap.single_float(value).map(|x| u64::from(x.to_bits())),
                "SINGLE-FLOAT",
            ),
            Format::F64 => (heap.double_float(value).map(f64::to_bits), "DOUBLE-FLOAT"),
        };
        return bits
            .filter(|&bits| kind.valid_lane_bits(bits))
            .ok_or_else(|| {
                Fault::type_error(
                    runtime,
                    value,
                    expected,
                    EvalError::new(at, "expected a finite float of the pack's format"),
                )
            });
    }
    let (minimum, maximum) = integer_range(kind);
    let integer = value
        .as_fixnum()
        .map(|value| value as i128)
        .or_else(|| heap.integer(value).and_then(|value| value.to_i128()));
    integer
        .filter(|integer| (minimum..=maximum).contains(integer))
        .map(|integer| integer as u64 & lane_mask(kind))
        .ok_or_else(|| {
            integer_range_error(runtime, value, minimum.into(), Some(maximum.into()), at)
        })
}

fn box_lane(runtime: &Runtime, kind: LaneKind, bits: u64, at: usize) -> Result<Word, Fault> {
    let real = match float_format(kind) {
        Some(Format::F32) => Real::Single(f32::from_bits(bits as u32)),
        Some(Format::F64) => Real::Double(f64::from_bits(bits)),
        None => {
            let value = integer_lane(kind, bits);
            if let Some(word) = isize::try_from(value).ok().and_then(Word::fixnum) {
                return Ok(word);
            }
            Real::Integer(value.into())
        }
    };
    runtime
        .heap
        .allocate_number(&Number::Real(real))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn build_pack(
    runtime: &Runtime,
    shape: PackShape,
    at: usize,
    mut lane: impl FnMut(usize) -> Result<u64, Fault>,
) -> Result<Word, Fault> {
    let mut lanes = [0; 64];
    let lanes = &mut lanes[..shape.lanes()];
    for (index, slot) in lanes.iter_mut().enumerate() {
        *slot = lane(index)?;
    }
    box_pack(
        runtime,
        SimdPack::from_lanes(shape, lanes).expect("valid SIMD operation result"),
        at,
    )
}

fn make_pack(runtime: &Runtime, op: Op, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let shape = shape(runtime, args.get(0), at)?;
    if op == Op::Splat {
        let bits = read_lane(runtime, shape.kind(), args.get(1), at)?;
        return build_pack(runtime, shape, at, |_| Ok(bits));
    }
    if args.len() - 1 != shape.lanes() {
        return Err(super::modules::arity_error(
            at,
            &format!(
                "{} requires exactly {} lane values",
                shape.name(),
                shape.lanes()
            ),
        ));
    }
    build_pack(runtime, shape, at, |lane| {
        read_lane(runtime, shape.kind(), args.get(lane + 1), at)
    })
}

fn pack_lanes(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let value = pack(runtime, value, at)?;
    let shape = value.shape();
    let lanes = runtime.root_frame(shape.lanes());
    for i in 0..shape.lanes() {
        lanes.set(i, box_lane(runtime, shape.kind(), value.lane(i), at)?);
    }
    runtime
        .heap
        .array_from_values(&[shape.lanes()], ArrayElement::General, false, None, |i| {
            ArrayValue::Word(lanes.get(i))
        })
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn mask_operation(runtime: &Runtime, op: Op, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    use Op::*;
    if matches!(op, MaskFromBits | MaskFirst) {
        let lane_count = args.get(0);
        let shape = lane_count
            .as_fixnum()
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|lanes| SimdMask::allowed_bits(lanes).map(|bits| (lanes, bits)));
        let Some((lanes, allowed)) = shape else {
            let lane_count = runtime.root_slot(lane_count);
            let member = runtime.intern(&Name::common_lisp("MEMBER"), at)?;
            let expected = super::methods::list_from_words(
                runtime,
                std::iter::once(member)
                    .chain(SimdMask::LANE_COUNTS.map(small_integer))
                    .collect(),
                at,
            )?;
            return Err(type_error(
                runtime,
                lane_count.get(),
                expected,
                "invalid mask lane count",
                at,
            ));
        };
        let value = args.get(1);
        let bits = match op {
            MaskFirst => {
                let count = bounded_integer(runtime, value, lanes, at)?;
                allowed
                    & if count == 0 {
                        0
                    } else {
                        u64::MAX >> (64 - count)
                    }
            }
            MaskFromBits => {
                let Some(bits) = runtime
                    .heap
                    .integer(value)
                    .and_then(|n| n.to_u64())
                    .filter(|n| *n <= allowed)
                else {
                    return Err(integer_range_error(
                        runtime,
                        value,
                        0.into(),
                        Some(allowed.into()),
                        at,
                    ));
                };
                bits
            }
            _ => unreachable!(),
        };
        return mask_word(runtime, lanes, bits, at);
    }
    let a = mask(runtime, args.get(0), at)?;
    let lanes = a.lanes();
    let a = a.bits();
    match op {
        MaskAnd | MaskOr | MaskXor => {
            let b = mask_at(runtime, args.get(1), lanes, at)?.bits();
            let bits = match op {
                MaskAnd => a & b,
                MaskOr => a | b,
                _ => a ^ b,
            };
            mask_word(runtime, lanes, bits, at)
        }
        MaskNot => mask_word(
            runtime,
            lanes,
            SimdMask::allowed_bits(lanes).unwrap() ^ a,
            at,
        ),
        MaskBits => box_integer(runtime, &a.into(), at),
        MaskAnyP => Ok(truth(a != 0)),
        MaskAllP => Ok(truth(a == SimdMask::allowed_bits(lanes).unwrap())),
        MaskCount => Ok(small_integer(a.count_ones() as usize)),
        _ => unreachable!(),
    }
}

fn operands(args: &RootSlots) -> impl ExactSizeIterator<Item = Word> + '_ {
    (0..args.len()).map(|index| args.get(index))
}

pub(crate) fn printable_lanes(pack: &SimdPack) -> impl Iterator<Item = Real> + '_ {
    let shape = pack.shape();
    (0..shape.lanes()).map(move |lane| {
        let bits = pack.lane(lane);
        match shape.kind() {
            LaneKind::F32 => Real::Single(f32::from_bits(bits as u32)),
            LaneKind::F64 => Real::Double(f64::from_bits(bits)),
            kind => Real::Integer(integer_lane(kind, bits).into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_catalog_covers_primitive_and_shape_names() {
        use zkcl_syntax::simd_builtins::{FUNCTIONS, PACK_TYPES};
        assert!(
            PackShape::ALL
                .into_iter()
                .map(PackShape::name)
                .eq(PACK_TYPES.iter().copied())
        );
        assert_eq!(FUNCTIONS.len(), PRIMITIVES.len() + 4);
        assert!(
            PRIMITIVES
                .iter()
                .all(|(_, op)| zkcl_syntax::simd_builtins::function(op.name()).is_some())
        );
    }
}
