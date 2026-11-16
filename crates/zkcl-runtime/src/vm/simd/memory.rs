use num_bigint::BigInt;
use num_traits::ToPrimitive;
use zkcl_bytecode::{LaneKind, PackShape, SimdMask, SimdPack};

use crate::Runtime;
use crate::condition::Fault;
use crate::heap::{ArrayElement, RootSlot, RootSlots, Word};
use crate::symbol::Name;

use super::super::arrays::element_type_specifier;
use super::super::methods::list_from_words;
use super::super::types::simd::pack_symbol;
use super::conditions::{self, Arithmetic};
use super::{
    Op, bounds_error, box_pack, integer_lane, mask, mask_at, nonnegative, pack, same_shape, shape,
};

const ELEMENTS: [(LaneKind, ArrayElement); 3] = [
    (LaneKind::U8, ArrayElement::Octet),
    (LaneKind::F32, ArrayElement::SingleFloat),
    (LaneKind::F64, ArrayElement::DoubleFloat),
];

pub(super) fn invoke(
    runtime: &Runtime,
    op: Op,
    args: &RootSlots,
    at: usize,
) -> Result<Word, Fault> {
    match op {
        Op::LoadPack | Op::LoadMasked | Op::Gather => load(runtime, op, args, at),
        Op::StorePack | Op::StoreMasked | Op::Scatter => store(runtime, op, args, at),
        _ => unreachable!("SIMD memory operation"),
    }
}

fn load(runtime: &Runtime, op: Op, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let specifier = args.get(0);
    let shape = shape(runtime, specifier, at)?;
    let Some(element) = array_element(shape.kind()) else {
        return Err(type_fault(
            runtime,
            specifier,
            at,
            "pack shape has no specialized array storage",
            || {
                shape_type(
                    runtime,
                    "MEMBER",
                    |shape| array_element(shape.kind()).is_some(),
                    at,
                )
            },
        ));
    };
    checked_array(runtime, args.get(1), Some(element), at)?;
    let indices = Indices::parse(runtime, args.get(2), op == Op::Gather, Some(shape), at)?;
    let mask = if op != Op::LoadPack {
        mask_at(runtime, args.get(3), shape.lanes(), at)?.bits()
    } else {
        SimdMask::allowed_bits(shape.lanes()).expect("pack lane count")
    };
    let mut bytes = [0u8; 64];
    let bytes = &mut bytes[..shape.byte_len()];
    if args.len() == 5 {
        bytes.copy_from_slice(same_shape(runtime, args.get(4), shape, at)?.as_bytes());
    }
    if mask != 0 {
        let array = args.get(1);
        let result = match &indices {
            Indices::Packed(pack) => {
                let indices = native_indices(pack, mask);
                runtime
                    .heap
                    .gather_numeric_pack(array, element, &indices, mask, bytes)
            }
            _ => indices.native_start(mask).and_then(|start| {
                runtime
                    .heap
                    .load_numeric_pack(array, element, start, mask, bytes)
            }),
        };
        result
            .map_err(|lane| bounds_error(runtime, args.get(1), &indices.logical(lane), lane, at))?;
    }
    match SimdPack::from_bytes(shape, bytes) {
        Some(value) => box_pack(runtime, value, at),
        None => {
            let lane = bytes
                .chunks_exact(shape.kind().byte_len())
                .position(|bytes| {
                    let mut bits = [0; 8];
                    bits[..bytes.len()].copy_from_slice(bytes);
                    !shape.kind().valid_lane_bits(u64::from_le_bytes(bits))
                })
                .expect("the load supplied a nonfinite lane");
            Err(conditions::arithmetic(
                runtime,
                Arithmetic::Invalid,
                op.name(),
                (0..args.len()).map(|i| args.get(i)),
                lane,
                None,
                at,
            ))
        }
    }
}

fn store(runtime: &Runtime, op: Op, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let value_arg = args.len() - 1;
    // The expected array type and mask width depend on the final argument. Probe
    // its representation now, and report any type error at its argument position.
    let candidate = runtime.heap.simd_pack(args.get(value_arg));
    let shape = candidate.map(|pack| pack.shape());
    let element = checked_array(
        runtime,
        args.get(0),
        shape.and_then(|shape| array_element(shape.kind())),
        at,
    )?;
    let indices = Indices::parse(runtime, args.get(1), op == Op::Scatter, shape, at)?;
    let mask = if op != Op::StorePack {
        let value = args.get(2);
        Some(match shape {
            Some(shape) => mask_at(runtime, value, shape.lanes(), at)?,
            None => mask(runtime, value, at)?,
        })
    } else {
        None
    };
    let value_word = args.get(value_arg);
    let value = match candidate {
        Some(pack) => pack,
        None => pack(runtime, value_word, at)?,
    };
    if array_element(value.shape().kind()) != Some(element) {
        return Err(type_fault(
            runtime,
            value_word,
            at,
            "pack lane kind must match the array element type",
            || {
                shape_type(
                    runtime,
                    "OR",
                    |shape| array_element(shape.kind()) == Some(element),
                    at,
                )
            },
        ));
    }
    let mask = mask
        .map(SimdMask::bits)
        .unwrap_or_else(|| SimdMask::allowed_bits(value.shape().lanes()).expect("pack lane count"));
    if mask != 0 {
        let array = args.get(0);
        let result = match &indices {
            Indices::Packed(pack) => {
                let indices = native_indices(pack, mask);
                runtime
                    .heap
                    .scatter_numeric_pack(array, element, &indices, mask, value.as_bytes())
            }
            _ => indices.native_start(mask).and_then(|start| {
                runtime
                    .heap
                    .store_numeric_pack(array, element, start, mask, value.as_bytes())
            }),
        };
        result
            .map_err(|lane| bounds_error(runtime, args.get(0), &indices.logical(lane), lane, at))?;
    }
    Ok(args.get(value_arg))
}

enum Indices {
    Start(usize),
    LargeStart(BigInt),
    Packed(SimdPack),
}

impl Indices {
    fn parse(
        runtime: &Runtime,
        word: Word,
        indexed: bool,
        shape: Option<PackShape>,
        at: usize,
    ) -> Result<Self, Fault> {
        if !indexed {
            if let Some(start) = word
                .as_fixnum()
                .and_then(|value| usize::try_from(value).ok())
            {
                return Ok(Self::Start(start));
            }
            return Ok(Self::LargeStart(nonnegative(runtime, word, at)?));
        }
        let allowed = |candidate: PackShape| {
            !matches!(candidate.kind(), LaneKind::F32 | LaneKind::F64)
                && shape.is_none_or(|shape| candidate.lanes() == shape.lanes())
        };
        runtime
            .heap
            .simd_pack(word)
            .filter(|value| allowed(value.shape()))
            .map(Self::Packed)
            .ok_or_else(|| {
                type_fault(
                    runtime,
                    word,
                    at,
                    "expected an integer index pack with the value's lane count",
                    || shape_type(runtime, "OR", allowed, at),
                )
            })
    }

    fn native_start(&self, mask: u64) -> Result<usize, usize> {
        match self {
            Self::Start(start) => Ok(*start),
            Self::LargeStart(start) => start.to_usize().ok_or(mask.trailing_zeros() as usize),
            Self::Packed(_) => unreachable!("contiguous access"),
        }
    }

    fn logical(&self, lane: usize) -> BigInt {
        match self {
            Self::Start(start) => BigInt::from(*start) + lane,
            Self::LargeStart(start) => start + lane,
            Self::Packed(pack) => integer_lane(pack.shape().kind(), pack.lane(lane)).into(),
        }
    }
}

fn native_indices(pack: &SimdPack, mut mask: u64) -> [Option<usize>; 64] {
    let mut indices = [None; 64];
    let kind = pack.shape().kind();
    while mask != 0 {
        let lane = mask.trailing_zeros() as usize;
        indices[lane] = usize::try_from(integer_lane(kind, pack.lane(lane))).ok();
        mask &= mask - 1;
    }
    indices
}

fn array_element(kind: LaneKind) -> Option<ArrayElement> {
    ELEMENTS
        .iter()
        .find_map(|&(lane, element)| (lane == kind).then_some(element))
}

// Used by the bulk adapter as well as pack memory operations.
pub(super) fn checked_array(
    runtime: &Runtime,
    word: Word,
    expected: Option<ArrayElement>,
    at: usize,
) -> Result<ArrayElement, Fault> {
    if let Some(info) = runtime.heap.array_info(word) {
        let matches = match expected {
            Some(element) => info.element == element,
            None => ELEMENTS.iter().any(|&(_, element)| element == info.element),
        };
        if matches {
            return Ok(info.element);
        }
    }
    Err(type_fault(
        runtime,
        word,
        at,
        "expected a specialized array matching the pack lane kind",
        || {
            if let Some(element) = expected {
                return array_type(runtime, element, at);
            }
            let operator = runtime.root_slot(runtime.intern(&Name::common_lisp("OR"), at)?);
            let types = ELEMENTS
                .into_iter()
                .map(|(_, element)| {
                    array_type(runtime, element, at).map(|specifier| runtime.root_slot(specifier))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let words = std::iter::once(operator.get())
                .chain(types.iter().map(RootSlot::get))
                .collect();
            list_from_words(runtime, words, at)
        },
    ))
}

fn array_type(runtime: &Runtime, element: ArrayElement, at: usize) -> Result<Word, Fault> {
    let head = runtime.root_slot(runtime.intern(&Name::common_lisp("ARRAY"), at)?);
    let element = element_type_specifier(runtime, Some(element), at)?;
    list_from_words(runtime, vec![head.get(), element], at)
}

fn shape_type(
    runtime: &Runtime,
    operator: &str,
    allowed: impl Fn(PackShape) -> bool,
    at: usize,
) -> Result<Word, Fault> {
    let operator = runtime.intern(&Name::common_lisp(operator), at)?;
    let shapes = PackShape::ALL
        .into_iter()
        .filter(|&shape| allowed(shape))
        .map(|shape| pack_symbol(runtime, shape));
    list_from_words(
        runtime,
        std::iter::once(operator).chain(shapes).collect(),
        at,
    )
}

fn type_fault(
    runtime: &Runtime,
    datum: Word,
    at: usize,
    message: &str,
    expected: impl FnOnce() -> Result<Word, Fault>,
) -> Fault {
    let datum = runtime.root_slot(datum);
    match expected() {
        Ok(expected) => super::type_error(runtime, datum.get(), expected, message, at),
        Err(fault) => fault,
    }
}
