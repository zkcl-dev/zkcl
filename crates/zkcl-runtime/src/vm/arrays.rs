use std::ops::Range;

use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, ArrayValue, ListSpine, RootFrame, RootSlot, RootSlots, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, Runtime};

use super::{Primitive, Values, keyword_arguments};

pub(crate) const RANK_LIMIT: usize = zkcl_bytecode::ARRAY_RANK_LIMIT;

#[derive(Clone, Copy)]
pub(crate) enum BitOp {
    And,
    Andc1,
    Andc2,
    Eqv,
    Ior,
    Nand,
    Nor,
    Orc1,
    Orc2,
    Xor,
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Make,
    Vector,
    Arrayp,
    Vectorp,
    SimpleVectorP,
    BitVectorP,
    SimpleBitVectorP,
    Rank,
    Dimension,
    Dimensions,
    TotalSize,
    ElementType,
    UpgradedType,
    AdjustableP,
    Displacement,
    HasFillPointerP,
    FillPointer,
    SetFillPointer,
    Push,
    Pop,
    InBounds,
    RowMajorIndex,
    Aref,
    SetAref,
    RowMajorAref,
    SetRowMajorAref,
    Svref,
    SetSvref,
    Bit,
    SetBit,
    Sbit,
    SetSbit,
    BitArray(BitOp),
    BitNot,
    Elt,
    SetElt,
    Adjust,
    PushExtend,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAKE-ARRAY", Op::Make),
    ("VECTOR", Op::Vector),
    ("ARRAYP", Op::Arrayp),
    ("VECTORP", Op::Vectorp),
    ("SIMPLE-VECTOR-P", Op::SimpleVectorP),
    ("BIT-VECTOR-P", Op::BitVectorP),
    ("SIMPLE-BIT-VECTOR-P", Op::SimpleBitVectorP),
    ("ARRAY-RANK", Op::Rank),
    ("ARRAY-DIMENSION", Op::Dimension),
    ("ARRAY-DIMENSIONS", Op::Dimensions),
    ("ARRAY-TOTAL-SIZE", Op::TotalSize),
    ("ARRAY-ELEMENT-TYPE", Op::ElementType),
    ("UPGRADED-ARRAY-ELEMENT-TYPE", Op::UpgradedType),
    ("ADJUSTABLE-ARRAY-P", Op::AdjustableP),
    ("ARRAY-DISPLACEMENT", Op::Displacement),
    ("ARRAY-HAS-FILL-POINTER-P", Op::HasFillPointerP),
    ("FILL-POINTER", Op::FillPointer),
    ("(SETF FILL-POINTER)", Op::SetFillPointer),
    ("VECTOR-PUSH", Op::Push),
    ("VECTOR-POP", Op::Pop),
    ("ARRAY-IN-BOUNDS-P", Op::InBounds),
    ("ARRAY-ROW-MAJOR-INDEX", Op::RowMajorIndex),
    ("AREF", Op::Aref),
    ("(SETF AREF)", Op::SetAref),
    ("ROW-MAJOR-AREF", Op::RowMajorAref),
    ("(SETF ROW-MAJOR-AREF)", Op::SetRowMajorAref),
    ("SVREF", Op::Svref),
    ("(SETF SVREF)", Op::SetSvref),
    ("BIT", Op::Bit),
    ("(SETF BIT)", Op::SetBit),
    ("SBIT", Op::Sbit),
    ("(SETF SBIT)", Op::SetSbit),
    ("BIT-AND", Op::BitArray(BitOp::And)),
    ("BIT-ANDC1", Op::BitArray(BitOp::Andc1)),
    ("BIT-ANDC2", Op::BitArray(BitOp::Andc2)),
    ("BIT-EQV", Op::BitArray(BitOp::Eqv)),
    ("BIT-IOR", Op::BitArray(BitOp::Ior)),
    ("BIT-NAND", Op::BitArray(BitOp::Nand)),
    ("BIT-NOR", Op::BitArray(BitOp::Nor)),
    ("BIT-ORC1", Op::BitArray(BitOp::Orc1)),
    ("BIT-ORC2", Op::BitArray(BitOp::Orc2)),
    ("BIT-XOR", Op::BitArray(BitOp::Xor)),
    ("BIT-NOT", Op::BitNot),
    ("ELT", Op::Elt),
    ("(SETF ELT)", Op::SetElt),
    ("ADJUST-ARRAY", Op::Adjust),
    ("VECTOR-PUSH-EXTEND", Op::PushExtend),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Vector => true,
            Self::UpgradedType => (1..=2).contains(&count),
            Self::Adjust => count >= 2,
            Self::PushExtend => (2..=3).contains(&count),
            Self::Make
            | Self::InBounds
            | Self::RowMajorIndex
            | Self::Aref
            | Self::Bit
            | Self::Sbit => count >= 1,
            Self::SetAref | Self::SetBit | Self::SetSbit => count >= 2,
            Self::BitArray(_) => (2..=3).contains(&count),
            Self::BitNot => (1..=2).contains(&count),
            Self::Dimension
            | Self::SetFillPointer
            | Self::Push
            | Self::RowMajorAref
            | Self::Svref
            | Self::Elt => count == 2,
            Self::SetRowMajorAref | Self::SetSvref | Self::SetElt => count == 3,
            _ => count == 1,
        }
    }
}

pub(crate) fn setf_primitive(accessor: &str) -> Option<Word> {
    let setter = match accessor {
        "AREF" => "(SETF AREF)",
        "ROW-MAJOR-AREF" => "(SETF ROW-MAJOR-AREF)",
        "SVREF" => "(SETF SVREF)",
        "BIT" => "(SETF BIT)",
        "SBIT" => "(SETF SBIT)",
        "FILL-POINTER" => "(SETF FILL-POINTER)",
        "ELT" => "(SETF ELT)",
        _ => return None,
    };
    Primitive::named(setter)
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let heap = &runtime.heap;
    let arg = |i| frame.get(args.start + i);
    let result = match op {
        Op::Make => make(runtime, frame, args, at)?,
        Op::Adjust => super::adjust_array::adjust(runtime, frame, args, at)?,
        Op::Push | Op::PushExtend => {
            super::adjust_array::push(runtime, frame, args, matches!(op, Op::PushExtend), at)?
        }
        Op::Vector => heap
            .array(&[args.len()], ArrayElement::General, false, None, arg)
            .map_err(|error| EvalError::out_of_memory(at, error))?,
        Op::UpgradedType => {
            element_type_specifier(runtime, upgraded_type(runtime, arg(0), at)?, at)?
        }
        Op::Arrayp => truth(heap.array_info(arg(0)).is_some()),
        Op::Vectorp => truth(vector_length(runtime, arg(0)).is_some()),
        Op::SimpleVectorP | Op::BitVectorP | Op::SimpleBitVectorP => {
            truth(heap.array_info(arg(0)).is_some_and(|info| {
                info.rank == 1
                    && match op {
                        Op::SimpleVectorP => info.simple() && info.element == ArrayElement::General,
                        Op::BitVectorP => info.element == ArrayElement::Bit,
                        _ => info.simple() && info.element == ArrayElement::Bit,
                    }
            }))
        }
        Op::Rank
        | Op::Dimension
        | Op::Dimensions
        | Op::TotalSize
        | Op::ElementType
        | Op::AdjustableP
        | Op::Displacement
        | Op::HasFillPointerP => {
            let array = arg(0);
            let info = heap
                .array_info(array)
                .ok_or_else(|| type_error(runtime, array, "ARRAY", at))?;
            match op {
                Op::Rank => size_word(info.rank, at)?,
                Op::Dimension => {
                    let axis = natural(runtime, arg(1), at)?;
                    let shape = shape(runtime, array, at)?;
                    size_word(
                        *shape
                            .get(axis)
                            .ok_or_else(|| problem(at, "array axis is out of bounds"))?,
                        at,
                    )?
                }
                Op::Dimensions => {
                    let shape = shape(runtime, array, at)?;
                    let mut dimensions = runtime.nil();
                    for dimension in shape.into_iter().rev() {
                        dimensions = runtime
                            .cons(&runtime.root(size_word(dimension, at)?), &dimensions)
                            .map_err(|error| EvalError::out_of_memory(at, error))?;
                    }
                    dimensions.slot.get()
                }
                Op::TotalSize => size_word(info.size, at)?,
                Op::ElementType => element_type_specifier(
                    runtime,
                    (!info.nil_element).then_some(info.element),
                    at,
                )?,
                Op::AdjustableP => truth(info.adjustable),
                Op::HasFillPointerP => truth(info.fill_pointer.is_some()),
                Op::Displacement => {
                    let (target, offset) = info.displacement.unwrap_or((Word::NIL, 0));
                    values.resize(2);
                    values.slots.set(0, target);
                    values.slots.set(1, size_word(offset, at)?);
                    return Ok(());
                }
                _ => unreachable!("array metadata operation"),
            }
        }
        Op::FillPointer => size_word(fill_pointer(runtime, arg(0), at)?, at)?,
        Op::SetFillPointer => {
            fill_pointer(runtime, arg(1), at)?;
            let fill = natural(runtime, arg(0), at)?;
            heap.set_array_fill_pointer(arg(1), fill)
                .ok_or_else(|| problem(at, "fill pointer is past the end of the vector"))?;
            arg(0)
        }
        Op::Pop => {
            let index = fill_pointer(runtime, arg(0), at)?
                .checked_sub(1)
                .ok_or_else(|| problem(at, "cannot pop an empty vector"))?;
            let value = slot(runtime, arg(0), index, at)?;
            heap.set_array_fill_pointer(arg(0), index)
                .expect("valid fill pointer");
            value
        }
        Op::BitArray(bit_op) => bit_array_op(
            runtime,
            arg(0),
            Some(arg(1)),
            (args.len() == 3).then(|| arg(2)),
            Some(bit_op),
            at,
        )?,
        Op::BitNot => bit_array_op(
            runtime,
            arg(0),
            None,
            (args.len() == 2).then(|| arg(1)),
            None,
            at,
        )?,
        Op::Elt | Op::SetElt => {
            let setter = matches!(op, Op::SetElt);
            let base = usize::from(setter);
            let raw_index = arg(base + 1);
            elt(
                runtime,
                arg(base),
                raw_index,
                natural(runtime, raw_index, at)?,
                setter.then(|| arg(0)),
                at,
            )?
        }
        _ => {
            let setter = matches!(
                op,
                Op::SetAref | Op::SetRowMajorAref | Op::SetSvref | Op::SetBit | Op::SetSbit
            );
            let base = usize::from(setter);
            let array = arg(base);
            let dimensions = shape(runtime, array, at)?;
            if matches!(op, Op::Svref | Op::SetSvref)
                && !heap.array_info(array).is_some_and(|info| {
                    info.rank == 1 && info.simple() && info.element == ArrayElement::General
                })
            {
                return Err(type_error(runtime, array, "SIMPLE-VECTOR", at));
            }
            if matches!(op, Op::Bit | Op::SetBit | Op::Sbit | Op::SetSbit)
                && !heap.array_info(array).is_some_and(|info| {
                    info.element == ArrayElement::Bit
                        && (!matches!(op, Op::Sbit | Op::SetSbit) || info.simple())
                })
            {
                return Err(problem(at, "expected a bit array of the requested kind"));
            }
            let index = if matches!(
                op,
                Op::RowMajorAref | Op::SetRowMajorAref | Op::Svref | Op::SetSvref
            ) {
                natural(runtime, arg(base + 1), at)?
            } else {
                let subscripts = (base + 1..args.len()).map(arg);
                let index = row_major_index(runtime, &dimensions, subscripts, at)?;
                if matches!(op, Op::InBounds) {
                    values.one(truth(index.is_some()));
                    return Ok(());
                }
                index.ok_or_else(|| problem(at, "array subscript is out of bounds"))?
            };
            if matches!(op, Op::RowMajorIndex) {
                size_word(index, at)?
            } else if setter {
                store(runtime, array, index, arg(0), at)?
            } else {
                slot(runtime, array, index, at)?
            }
        }
    };
    values.one(result);
    Ok(())
}

fn make(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let roots = runtime.root_frame(8);
    for (index, name) in [
        "ELEMENT-TYPE",
        "INITIAL-ELEMENT",
        "INITIAL-CONTENTS",
        "ADJUSTABLE",
        "FILL-POINTER",
        "DISPLACED-TO",
        "DISPLACED-INDEX-OFFSET",
        "ALLOW-OTHER-KEYS",
    ]
    .into_iter()
    .enumerate()
    {
        roots.set(index, runtime.intern(&Name::keyword(name), at)?);
    }
    let keys: [Word; 7] = std::array::from_fn(|index| roots.get(index));
    let options = keyword_arguments(
        runtime,
        &keys,
        args.start + 1..args.end,
        |i| frame.get(i),
        false,
        roots.get(7),
    )
    .map_err(|error| error.program_error(at))?;
    let option = |index: usize| options[index].map(|index| frame.get(index));
    let dimensions = dimensions(runtime, frame.get(args.start), at)?;
    let count = total_size(&dimensions, at)?;
    let element_type = match option(0) {
        Some(value) => upgraded_type(runtime, value, at)?,
        None => Some(ArrayElement::General),
    };
    let adjustable = option(3).is_some_and(|value| value != Word::NIL);
    let fill = if let Some(value) = option(4).filter(|value| *value != Word::NIL) {
        if dimensions.len() != 1 {
            return Err(problem(at, "only a vector can have a fill pointer"));
        }
        let fill = if value == Word::T {
            count
        } else {
            natural(runtime, value, at)?
        };
        if fill > count {
            return Err(problem(at, "fill pointer is past the end of the vector"));
        }
        Some(fill)
    } else {
        None
    };
    let target = option(5).filter(|word| *word != Word::NIL);
    let initial = option(1);
    let contents = option(2);
    if initial.is_some() && contents.is_some() {
        return Err(problem(
            at,
            "initial-element and initial-contents are mutually exclusive",
        ));
    }
    if target.is_none() && option(6).is_some() {
        return Err(problem(
            at,
            "displacement offset requires a displacement target",
        ));
    }
    let nil_element = element_type.is_none();
    let element = element_type.unwrap_or(if dimensions.len() == 1 {
        ArrayElement::Character
    } else {
        ArrayElement::General
    });
    if let Some(target) = target {
        if initial.is_some() || contents.is_some() {
            return Err(problem(
                at,
                "a displaced array cannot supply initial elements",
            ));
        }
        let offset = option(6).map_or(Ok(0), |word| natural(runtime, word, at))?;
        let info = runtime
            .heap
            .array_info(target)
            .ok_or_else(|| type_error(runtime, target, "ARRAY", at))?;
        if info.element != element || info.nil_element != nil_element {
            return Err(problem(
                at,
                "displaced arrays require matching element types",
            ));
        }
        if offset.checked_add(count).is_none_or(|end| end > info.size) {
            return Err(problem(at, "displaced array does not fit its target"));
        }
        let array = runtime
            .heap
            .displaced_array(&dimensions, element, adjustable, fill, offset, || {
                option(5).expect("rooted displacement target")
            })
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        if nil_element {
            runtime
                .heap
                .mark_nil_element(array)
                .expect("fresh displaced array");
        }
        return Ok(array);
    }
    if nil_element {
        if let Some(contents) = contents {
            let items = flatten(runtime, contents, &dimensions, count, at)?;
            if !items.is_empty() {
                return Err(type_error(runtime, items.get(0), "NIL", at));
            }
        }
        if let Some(initial) = initial.filter(|_| count > 0) {
            return Err(type_error(runtime, initial, "NIL", at));
        }
        return runtime
            .heap
            .nil_element_array(&dimensions, adjustable, fill, count, |_| '\0')
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    if let Some(contents) = contents {
        let input = flatten(runtime, contents, &dimensions, count, at)?;
        for index in 0..count {
            checked_element(runtime, element, input.get(index), at)?;
        }
        return runtime
            .heap
            .array(&dimensions, element, adjustable, fill, |i| input.get(i))
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    let value = initial
        .map(|word| checked_element(runtime, element, word, at))
        .transpose()?
        .unwrap_or_else(|| default_element(element));
    if element == ArrayElement::Character && dimensions.len() == 1 && !adjustable && fill.is_none()
    {
        let character = value
            .expect_word()
            .as_character()
            .expect("validated character");
        return runtime
            .heap
            .repeated_string(character, count)
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    runtime
        .heap
        .array_from_values(&dimensions, element, adjustable, fill, |_| match value {
            ArrayValue::Word(_) => option(1).map_or(value, ArrayValue::Word),
            _ => value,
        })
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(super) fn dimensions(runtime: &Runtime, value: Word, at: usize) -> Result<Vec<usize>, Fault> {
    if value.as_fixnum().is_some() {
        return Ok(vec![dimension(runtime, value, at)?]);
    }
    let mut result = Vec::new();
    let mut spine = ListSpine::new(value);
    let invalid = || type_error(runtime, value, "LIST", at);
    while let Some((word, _)) = spine.next(&runtime.heap).map_err(|_| invalid())? {
        result.push(dimension(runtime, word, at)?);
        if result.len() >= RANK_LIMIT {
            return Err(problem(at, "array rank exceeds the implementation limit"));
        }
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    Ok(result)
}

fn dimension(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    let dimension = natural(runtime, word, at)?;
    if dimension >= Word::FIXNUM_MAX as usize {
        return Err(problem(
            at,
            "array dimension exceeds the implementation limit",
        ));
    }
    Ok(dimension)
}

pub(super) fn total_size(dimensions: &[usize], at: usize) -> Result<usize, Fault> {
    if dimensions.contains(&0) {
        return Ok(0);
    }
    dimensions.iter().try_fold(1usize, |n, &dimension| {
        n.checked_mul(dimension)
            .filter(|&total| total < Word::FIXNUM_MAX as usize)
            .ok_or_else(|| problem(at, "array total size exceeds the implementation limit"))
    })
}

fn fill_pointer(runtime: &Runtime, array: Word, at: usize) -> Result<usize, Fault> {
    runtime
        .heap
        .array_info(array)
        .and_then(|info| info.fill_pointer)
        .ok_or_else(|| {
            Fault::vector_satisfying(
                runtime,
                array,
                "ARRAY-HAS-FILL-POINTER-P",
                EvalError::new(at, "expected a vector with a fill pointer"),
            )
        })
}

fn shape(runtime: &Runtime, array: Word, at: usize) -> Result<Vec<usize>, Fault> {
    runtime
        .heap
        .array_dimensions(array)
        .ok_or_else(|| type_error(runtime, array, "ARRAY", at))
}

pub(crate) fn vector_length(runtime: &Runtime, word: Word) -> Option<usize> {
    let info = runtime.heap.array_info(word)?;
    (info.rank == 1).then_some(info.fill_pointer.unwrap_or(info.size))
}

pub(super) fn matches_type(
    runtime: &Runtime,
    word: Word,
    kind: &str,
    mut args: Word,
    at: usize,
    depth: usize,
) -> Result<bool, Fault> {
    let heap = &runtime.heap;
    let mut next = || -> Result<Option<Word>, Fault> {
        if args == Word::NIL {
            return Ok(None);
        }
        let (arg, rest) = heap
            .cons_fields(args)
            .ok_or_else(|| problem(at, "invalid array type arguments"))?;
        args = rest;
        Ok(Some(arg))
    };
    let named = |word, name| {
        crate::condition::named_word(runtime, word, Package::COMMON_LISP).as_deref() == Some(name)
    };
    let specifier = match kind {
        "SIMPLE-VECTOR" | "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" => None,
        _ => next()?.filter(|&word| !named(word, "*")),
    };
    let dimensions = next()?
        .filter(|&word| !named(word, "*"))
        .map(|word| runtime.root_slot(word));
    if args != Word::NIL {
        return Err(problem(at, "too many arguments to array type"));
    }
    let word = runtime.root_slot(word);
    let element = match kind {
        "SIMPLE-VECTOR" => Some(Some(ArrayElement::General)),
        "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" => Some(Some(ArrayElement::Bit)),
        _ => specifier
            .map(|word| upgraded_type_at_depth(runtime, word, at, depth + 1))
            .transpose()?,
    };
    let word = word.get();
    let dimensions = dimensions.as_ref().map(RootSlot::get);
    let Some(info) = heap.array_info(word) else {
        return Ok(false);
    };
    let element_matches = match element {
        None => true,
        Some(None) => info.nil_element,
        Some(Some(element)) => info.element == element && !info.nil_element,
    };
    if !element_matches || kind.starts_with("SIMPLE-") && !info.simple() {
        return Ok(false);
    }
    let shape = shape(runtime, word, at)?;
    if matches!(kind, "ARRAY" | "SIMPLE-ARRAY") {
        let Some(dimensions) = dimensions else {
            return Ok(true);
        };
        if dimensions.as_fixnum().is_some() {
            return Ok(natural(runtime, dimensions, at)? == shape.len());
        }
        let mut spine = ListSpine::new(dimensions);
        let mut axis = 0;
        let mut same = true;
        let invalid = || problem(at, "expected a proper list of array type dimensions");
        while let Some((dimension, _)) = spine.next(heap).map_err(|_| invalid())? {
            let actual = shape.get(axis).copied();
            if !named(dimension, "*") {
                same &= actual == Some(natural(runtime, dimension, at)?);
            }
            axis += 1;
        }
        if spine.tail() != Word::NIL {
            return Err(invalid());
        }
        return Ok(same && axis == shape.len());
    }
    let length = dimensions
        .map(|word| natural(runtime, word, at))
        .transpose()?;
    Ok(shape.len() == 1 && length.is_none_or(|length| shape[0] == length))
}

fn row_major_index(
    runtime: &Runtime,
    shape: &[usize],
    indices: impl ExactSizeIterator<Item = Word>,
    at: usize,
) -> Result<Option<usize>, Fault> {
    if indices.len() != shape.len() {
        return Err(problem(at, "wrong number of array subscripts"));
    }
    let mut in_bounds = !shape.contains(&0);
    let mut result = 0usize;
    for (&dimension, index) in shape.iter().zip(indices) {
        let index = if let Some(index) = index.as_fixnum() {
            usize::try_from(index).ok()
        } else if runtime.heap.integer(index).is_some() {
            // A bignum cannot be a valid subscript for the current limits.
            None
        } else {
            return Err(type_error(runtime, index, "INTEGER", at));
        };
        if let Some(index) = index.filter(|&index| index < dimension) {
            if in_bounds {
                result = result * dimension + index;
            }
        } else {
            in_bounds = false;
        }
    }
    Ok(in_bounds.then_some(result))
}

pub(super) fn slot(runtime: &Runtime, array: Word, index: usize, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .array_slot(array, index)
        .map_err(|error| EvalError::out_of_memory(at, error))?
        .ok_or_else(|| problem(at, "array index is out of bounds"))
}

pub(super) fn store(
    runtime: &Runtime,
    array: Word,
    index: usize,
    value: Word,
    at: usize,
) -> Result<Word, Fault> {
    let info = runtime
        .heap
        .array_info(array)
        .ok_or_else(|| type_error(runtime, array, "ARRAY", at))?;
    if info.nil_element {
        return Err(type_error(runtime, value, "NIL", at));
    }
    let element = checked_element(runtime, info.element, value, at)?;
    if !runtime
        .heap
        .set_array_value(array, index, element)
        .map_err(|error| EvalError::out_of_memory(at, error))?
    {
        return Err(problem(at, "array index is out of bounds"));
    }
    Ok(value)
}

fn elt(
    runtime: &Runtime,
    sequence: Word,
    raw_index: Word,
    index: usize,
    store_value: Option<Word>,
    at: usize,
) -> Result<Word, Fault> {
    let index_out_of_bounds = |len: usize| -> Fault {
        let host = EvalError::new(at, "sequence index is out of bounds");
        if len == 0 {
            Fault::type_error(runtime, raw_index, "NIL", host)
        } else if let Ok(upper) = isize::try_from(len - 1) {
            Fault::integer_range(runtime, raw_index, 0, upper, host)
        } else {
            Fault::non_negative_integer(runtime, raw_index, host)
        }
    };
    if let Some(length) = vector_length(runtime, sequence) {
        if index >= length {
            return Err(index_out_of_bounds(length));
        }
        return if let Some(value) = store_value {
            store(runtime, sequence, index, value, at)
        } else {
            slot(runtime, sequence, index, at)
        };
    }
    if let Some(length) = runtime.heap.treelist_len(sequence) {
        if index >= length {
            return Err(index_out_of_bounds(length));
        }
        return if let Some(value) = store_value {
            super::treelist::expect_mutable_treelist(runtime, sequence, at)?;
            let seq = runtime.root_slot(sequence);
            let val = runtime.root_slot(value);
            runtime
                .heap
                .mutable_treelist_set(&seq, index, &val)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            Ok(val.get())
        } else {
            Ok(runtime
                .heap
                .treelist_get(sequence, index)
                .expect("validated treelist index"))
        };
    }
    let heap = &runtime.heap;
    let mut spine = ListSpine::new(sequence);
    for step in 0..=index {
        let cursor = spine.tail();
        let Some((car, _)) = spine
            .next(heap)
            .map_err(|_| type_error(runtime, sequence, "SEQUENCE", at))?
        else {
            return if cursor == Word::NIL {
                Err(index_out_of_bounds(step))
            } else {
                Err(type_error(runtime, sequence, "SEQUENCE", at))
            };
        };
        if step == index {
            return Ok(if let Some(value) = store_value {
                heap.set_cons_car(cursor, value);
                value
            } else {
                car
            });
        }
    }
    unreachable!("inclusive sequence walk")
}

pub(crate) fn upgraded_type(
    runtime: &Runtime,
    spec: Word,
    at: usize,
) -> Result<Option<ArrayElement>, Fault> {
    upgraded_type_at_depth(runtime, spec, at, 0)
}

pub(super) fn upgraded_type_at_depth(
    runtime: &Runtime,
    spec: Word,
    at: usize,
    depth: usize,
) -> Result<Option<ArrayElement>, Fault> {
    let spec = super::type_check::expand_derived_type(runtime, spec, at)?;
    upgraded_expanded_type(runtime, spec, at, depth)
}

pub(super) fn upgraded_expanded_type(
    runtime: &Runtime,
    spec: Word,
    at: usize,
    depth: usize,
) -> Result<Option<ArrayElement>, Fault> {
    let name = |word| crate::condition::named_word(runtime, word, Package::COMMON_LISP);
    let spec = if let Some(class) = runtime.heap.class_info(spec) {
        if !super::classes::is_system_class(runtime, class.origin_name) {
            return Ok(Some(ArrayElement::General));
        }
        class.origin_name
    } else {
        spec
    };
    if spec == Word::NIL {
        return Ok(None);
    }
    if let Some(name) = name(spec) {
        return Ok(Some(match name.as_str() {
            "BIT" => ArrayElement::Bit,
            "STANDARD-CHAR" | "BASE-CHAR" | "CHARACTER" | "EXTENDED-CHAR" => {
                ArrayElement::Character
            }
            "SHORT-FLOAT" | "SINGLE-FLOAT" => ArrayElement::SingleFloat,
            "DOUBLE-FLOAT" | "LONG-FLOAT" => ArrayElement::DoubleFloat,
            _ => ArrayElement::General,
        }));
    }
    if runtime.heap.is_symbol(spec) {
        return Ok(Some(ArrayElement::General));
    }
    if runtime.heap.cons_fields(spec).is_some() {
        return Ok(super::types::upgraded_numeric_array_type(
            runtime,
            spec,
            depth + 1,
        ));
    }
    Err(type_error(runtime, spec, "NIL", at))
}

pub(crate) fn sequence_elements(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<RootFrame, Fault> {
    if let Some(length) = vector_length(runtime, word) {
        let elements = runtime.root_frame(length);
        if runtime
            .heap
            .with_string_slice(word, |text| {
                for (index, character) in text.chars().enumerate() {
                    elements.set(index, Word::character(character));
                }
            })
            .is_none()
        {
            let word = runtime.root_slot(word);
            for index in 0..length {
                elements.set(index, slot(runtime, word.get(), index, at)?);
            }
        }
        return Ok(elements);
    }
    if let Some(length) = runtime.heap.treelist_len(word) {
        let elements = runtime.root_frame(length);
        let mut cursor = 0usize;
        runtime.heap.treelist_for_each(word, |item| {
            elements.set(cursor, item);
            cursor += 1;
        });
        return Ok(elements);
    }
    let mut result = Vec::new();
    let invalid = || type_error(runtime, word, "SEQUENCE", at);
    let mut spine = ListSpine::new(word);
    while let Some((car, _)) = spine.next(&runtime.heap).map_err(|_| invalid())? {
        result.push(car);
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    let elements = runtime.root_frame(result.len());
    for (index, word) in result.into_iter().enumerate() {
        elements.set(index, word);
    }
    Ok(elements)
}

pub(super) fn flatten(
    runtime: &Runtime,
    word: Word,
    dimensions: &[usize],
    count: usize,
    at: usize,
) -> Result<RootFrame, Fault> {
    fn visit(
        runtime: &Runtime,
        word: Word,
        dimensions: &[usize],
        rows: &mut Vec<RootFrame>,
        at: usize,
    ) -> Result<(), Fault> {
        let Some((&count, tail)) = dimensions.split_first() else {
            let scalar = runtime.root_frame(1);
            scalar.set(0, word);
            rows.push(scalar);
            return Ok(());
        };
        let elements = sequence_elements(runtime, word, at)?;
        if elements.len() != count {
            return Err(problem(
                at,
                "initial contents do not match the array dimensions",
            ));
        }
        if tail.is_empty() {
            if count != 0 {
                rows.push(elements);
            }
        } else {
            for index in 0..count {
                visit(runtime, elements.get(index), tail, rows, at)?;
            }
        }
        Ok(())
    }

    let mut rows = Vec::new();
    visit(runtime, word, dimensions, &mut rows, at)?;
    if rows.len() == 1 {
        return Ok(rows.pop().expect("one row"));
    }
    let output = runtime.root_frame(count);
    let mut offset = 0;
    for row in rows {
        for index in 0..row.len() {
            output.set(offset + index, row.get(index));
        }
        offset += row.len();
    }
    Ok(output)
}

pub(crate) fn array_from_contents(
    runtime: &Runtime,
    contents: Word,
    rank: usize,
    at: usize,
) -> Result<Word, Fault> {
    let contents = runtime.root_slot(contents);
    let mut dimensions = Vec::with_capacity(rank);
    let mut first = Some(contents.get());
    for _ in 0..rank {
        if let Some(word) = first {
            let elements = sequence_elements(runtime, word, at)?;
            dimensions.push(elements.len());
            first = (!elements.is_empty()).then(|| elements.get(0));
        } else {
            dimensions.push(0);
        }
    }
    let count = total_size(&dimensions, at)?;
    let input = flatten(runtime, contents.get(), &dimensions, count, at)?;
    runtime
        .heap
        .array(&dimensions, ArrayElement::General, false, None, |index| {
            input.get(index)
        })
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(super) fn checked_element(
    runtime: &Runtime,
    element: ArrayElement,
    value: Word,
    at: usize,
) -> Result<ArrayValue, Fault> {
    ArrayValue::from_word(&runtime.heap, element, value).ok_or_else(|| match element {
        ArrayElement::Character => type_error(runtime, value, "CHARACTER", at),
        ArrayElement::Octet => Fault::integer_range(
            runtime,
            value,
            0,
            255,
            EvalError::new(at, "expected an octet"),
        ),
        element => type_error(runtime, value, named_element_type(element), at),
    })
}

pub(super) fn default_element(element: ArrayElement) -> ArrayValue {
    match element {
        ArrayElement::General => ArrayValue::Word(Word::NIL),
        ArrayElement::Bit => ArrayValue::Word(Word::fixnum(0).expect("zero bit")),
        ArrayElement::Character => ArrayValue::Word(Word::character('\0')),
        ArrayElement::Octet => ArrayValue::Octet(0),
        ArrayElement::SingleFloat => ArrayValue::SingleFloat(0.0),
        ArrayElement::DoubleFloat => ArrayValue::DoubleFloat(0.0),
    }
}

fn named_element_type(element: ArrayElement) -> &'static str {
    match element {
        ArrayElement::General => "T",
        ArrayElement::Bit => "BIT",
        ArrayElement::Character => "BASE-CHAR",
        ArrayElement::Octet => unreachable!("octets have a compound type specifier"),
        ArrayElement::SingleFloat => "SINGLE-FLOAT",
        ArrayElement::DoubleFloat => "DOUBLE-FLOAT",
    }
}

pub(super) fn element_type_specifier(
    runtime: &Runtime,
    element: Option<ArrayElement>,
    at: usize,
) -> Result<Word, Fault> {
    match element {
        None => Ok(Word::NIL),
        Some(ArrayElement::Octet) => {
            let head = runtime.root_slot(runtime.intern(&Name::common_lisp("UNSIGNED-BYTE"), at)?);
            let tail = runtime.root_slot(
                runtime
                    .heap
                    .cons(|| (Word::fixnum(8).expect("octet width"), Word::NIL))
                    .map_err(|error| EvalError::out_of_memory(at, error))?,
            );
            runtime
                .heap
                .cons(|| (head.get(), tail.get()))
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        }
        Some(element) => Ok(runtime.intern(&Name::common_lisp(named_element_type(element)), at)?),
    }
}

pub(super) fn natural(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    word.as_fixnum()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                word,
                EvalError::new(at, "expected a non-negative fixnum"),
            )
        })
}

pub(super) fn size_word(size: usize, at: usize) -> Result<Word, Fault> {
    isize::try_from(size)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| problem(at, "array size exceeds the implementation limit"))
}

pub(super) fn type_error(
    runtime: &Runtime,
    word: Word,
    expected: &'static str,
    at: usize,
) -> Fault {
    Fault::type_error(
        runtime,
        word,
        expected,
        EvalError::new(at, format!("expected {expected}")),
    )
}

pub(super) fn problem(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn bit_array_op(
    runtime: &Runtime,
    first: Word,
    second: Option<Word>,
    opt: Option<Word>,
    op: Option<BitOp>,
    at: usize,
) -> Result<Word, Fault> {
    let bit_array = |word: Word| -> Result<(Vec<usize>, usize), Fault> {
        let info = runtime
            .heap
            .array_info(word)
            .filter(|info| info.element == ArrayElement::Bit)
            .ok_or_else(|| bit_array_type_error(runtime, word, at))?;
        Ok((shape(runtime, word, at)?, info.size))
    };
    let (shape1, size) = bit_array(first)?;
    if let Some(second) = second {
        let (shape2, _) = bit_array(second)?;
        if shape1 != shape2 {
            return Err(Fault::plain(
                Class::SimpleError,
                EvalError::new(at, "bit array arguments must have the same dimensions"),
            ));
        }
    }
    let destination = match opt {
        None | Some(Word::NIL) => None,
        Some(Word::T) => Some(first),
        Some(target) => {
            let (target_shape, _) = bit_array(target)?;
            if target_shape != shape1 {
                return Err(Fault::plain(
                    Class::SimpleError,
                    EvalError::new(at, "destination bit array must have the same dimensions"),
                ));
            }
            Some(target)
        }
    };
    let bit_at = |array: Word, index: usize| -> Result<bool, Fault> {
        Ok(slot(runtime, array, index, at)?.as_fixnum() == Some(1))
    };
    let mut values = Vec::with_capacity(size);
    for index in 0..size {
        let a = bit_at(first, index)?;
        let out = match (op, second) {
            (None, None) => !a,
            (Some(bit_op), Some(second)) => {
                let b = bit_at(second, index)?;
                match bit_op {
                    BitOp::And => a && b,
                    BitOp::Andc1 => !a && b,
                    BitOp::Andc2 => a && !b,
                    BitOp::Eqv => a == b,
                    BitOp::Ior => a || b,
                    BitOp::Nand => !(a && b),
                    BitOp::Nor => !(a || b),
                    BitOp::Orc1 => !a || b,
                    BitOp::Orc2 => a || !b,
                    BitOp::Xor => a ^ b,
                }
            }
            _ => unreachable!("bit_array_op mode mismatch"),
        };
        values.push(Word::fixnum(isize::from(out)).unwrap());
    }
    if let Some(target) = destination {
        for (index, value) in values.into_iter().enumerate() {
            runtime
                .heap
                .set_array_slot(target, index, value)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        return Ok(target);
    }
    runtime
        .heap
        .array(&shape1, ArrayElement::Bit, false, None, |index| {
            values[index]
        })
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn bit_array_type_error(runtime: &Runtime, word: Word, at: usize) -> Fault {
    let host = EvalError::new(at, "expected a bit array");
    let Ok(array_sym) = runtime.intern(&Name::common_lisp("ARRAY"), at) else {
        return type_error(runtime, word, "ARRAY", at);
    };
    let Ok(bit_sym) = runtime.intern(&Name::common_lisp("BIT"), at) else {
        return type_error(runtime, word, "ARRAY", at);
    };
    let array_sym = runtime.root_slot(array_sym);
    let bit_sym = runtime.root_slot(bit_sym);
    let word = runtime.root_slot(word);
    let Ok(tail) = runtime.heap.cons(|| (bit_sym.get(), Word::NIL)) else {
        return Fault::from(EvalError::out_of_memory(at, crate::OutOfMemory));
    };
    let tail = runtime.root_slot(tail);
    let Ok(spec) = runtime.heap.cons(|| (array_sym.get(), tail.get())) else {
        return Fault::from(EvalError::out_of_memory(at, crate::OutOfMemory));
    };
    Fault::type_error_specifier(runtime, word.get(), spec, host)
}

fn truth(flag: bool) -> Word {
    if flag { Word::T } else { Word::NIL }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn flatten_keeps_boxed_float_elements_live_across_collections() {
        let mut config = Heap::builder();
        config.stress_bytes = Some(1);
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let heap = &runtime.heap;
        let first = runtime.root_slot(
            heap.array_from_values(&[2], ArrayElement::SingleFloat, false, None, |index| {
                ArrayValue::SingleFloat([-0.0, f32::from_bits(1)][index])
            })
            .unwrap(),
        );
        let second = runtime.root_slot(
            heap.array_from_values(&[2], ArrayElement::DoubleFloat, false, None, |index| {
                ArrayValue::DoubleFloat([-0.0, f64::from_bits(1)][index])
            })
            .unwrap(),
        );
        let contents = heap
            .array(&[2], ArrayElement::General, false, None, |index| {
                [first.get(), second.get()][index]
            })
            .unwrap();
        let output = flatten(&runtime, contents, &[2, 2], 4, 0).unwrap();
        assert_eq!(
            heap.single_float(output.get(0)).unwrap().to_bits(),
            (-0.0f32).to_bits()
        );
        assert_eq!(heap.single_float(output.get(1)).unwrap().to_bits(), 1);
        assert_eq!(
            heap.double_float(output.get(2)).unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
        assert_eq!(heap.double_float(output.get(3)).unwrap().to_bits(), 1);
    }
}
