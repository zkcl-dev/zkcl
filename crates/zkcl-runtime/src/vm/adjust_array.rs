use std::ops::Range;

use num_bigint::Sign;

use crate::condition::Fault;
use crate::heap::{ArrayElement, ArrayValue, RootSlots, Word};
use crate::symbol::Name;
use crate::{EvalError, OutOfMemory, Runtime};

use super::arrays::{
    checked_element, default_element, dimensions, flatten, natural, problem, size_word, store,
    total_size, type_error, upgraded_type,
};
use super::keyword_arguments;

pub(super) fn adjust(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let key_words = runtime.root_frame(7);
    for (index, key) in [
        "ELEMENT-TYPE",
        "INITIAL-ELEMENT",
        "INITIAL-CONTENTS",
        "FILL-POINTER",
        "DISPLACED-TO",
        "DISPLACED-INDEX-OFFSET",
        "ALLOW-OTHER-KEYS",
    ]
    .iter()
    .enumerate()
    {
        key_words.set(index, runtime.intern(&Name::keyword(key), at)?);
    }
    let keys: [Word; 6] = std::array::from_fn(|index| key_words.get(index));
    let options = keyword_arguments(
        runtime,
        &keys,
        args.start + 2..args.end,
        |index| frame.get(index),
        false,
        key_words.get(6),
    )
    .map_err(|error| error.program_error(at))?;
    let option = |index: usize| options[index].map(|slot| frame.get(slot));
    let original = || frame.get(args.start);
    let requested_type = option(0)
        .map(|specifier| upgraded_type(runtime, specifier, at))
        .transpose()?;
    let info = heap
        .array_info(original())
        .ok_or_else(|| type_error(runtime, original(), "ARRAY", at))?;
    let shape = dimensions(runtime, frame.get(args.start + 1), at)?;
    if shape.len() != info.rank {
        return Err(problem(at, "ADJUST-ARRAY must preserve the rank"));
    }
    let size = total_size(&shape, at)?;
    let element = info.element;
    if let Some(requested_type) = requested_type {
        let actual = (!info.nil_element).then_some(element);
        if requested_type != actual {
            return Err(problem(at, "ADJUST-ARRAY must preserve the element type"));
        }
    }
    let fill = match option(3).filter(|&word| word != Word::NIL) {
        Some(word) if info.fill_pointer.is_some() => Some(if word == Word::T {
            size
        } else {
            natural(runtime, word, at)?
        }),
        Some(_) => {
            return Err(Fault::vector_satisfying(
                runtime,
                original(),
                "ARRAY-HAS-FILL-POINTER-P",
                EvalError::new(at, "the original array has no fill pointer"),
            ));
        }
        None => info.fill_pointer,
    };
    if fill.is_some_and(|fill| fill > size) {
        return Err(problem(at, "the fill pointer exceeds the new vector size"));
    }

    let initial = option(1);
    let contents = option(2);
    let target = option(4).filter(|&word| word != Word::NIL);
    let offset = option(5);
    if initial.is_some() && contents.is_some() {
        return Err(problem(
            at,
            "initial-element and initial-contents are mutually exclusive",
        ));
    }
    if offset.is_some() && target.is_none() {
        return Err(problem(
            at,
            "displacement offset requires a displacement target",
        ));
    }
    let replacement = if let Some(target) = target {
        if initial.is_some() || contents.is_some() {
            return Err(problem(
                at,
                "a displaced array cannot supply initial elements",
            ));
        }
        let offset = offset.map_or(Ok(0), |word| natural(runtime, word, at))?;
        let target_info = heap
            .array_info(target)
            .ok_or_else(|| type_error(runtime, target, "ARRAY", at))?;
        if target_info.element != element || target_info.nil_element != info.nil_element {
            return Err(problem(
                at,
                "displaced arrays require matching element types",
            ));
        }
        if offset
            .checked_add(size)
            .is_none_or(|end| end > target_info.size)
        {
            return Err(problem(at, "displaced array does not fit its target"));
        }
        if info.adjustable {
            let mut cursor = target;
            loop {
                if cursor == original() {
                    return Err(problem(at, "array displacement would create a cycle"));
                }
                let Some((next, _)) = heap.array_info(cursor).and_then(|info| info.displacement)
                else {
                    break;
                };
                cursor = next;
            }
        }
        heap.displaced_array(&shape, element, info.adjustable, fill, offset, || {
            option(4).expect("rooted displacement target")
        })
    } else if let Some(contents) = contents {
        let roots = flatten(runtime, contents, &shape, size, at)?;
        for index in 0..size {
            let value = roots.get(index);
            if info.nil_element {
                return Err(type_error(runtime, value, "NIL", at));
            }
            checked_element(runtime, element, value, at)?;
        }
        heap.array(&shape, element, info.adjustable, fill, |index| {
            roots.get(index)
        })
    } else {
        let initial = initial
            .map(|word| {
                if info.nil_element {
                    return Err(type_error(runtime, word, "NIL", at));
                }
                checked_element(runtime, element, word, at)
            })
            .transpose()?;
        let default = || match initial {
            Some(ArrayValue::Word(_)) => ArrayValue::Word(option(1).expect("rooted initial value")),
            Some(value) => value,
            None => default_element(element),
        };
        let old_shape = heap.array_dimensions(original()).expect("checked array");
        let source_size = match (size, info.rank) {
            (0, _) => 0,
            (_, 1) => info.size.min(size),
            _ => info.size,
        };
        let characters = character_snapshot(runtime, original(), element, source_size, at)?;
        heap.array_from_values(&shape, element, info.adjustable, fill, |index| {
            overlapping_index(index, &old_shape, &shape)
                .and_then(|old_index| {
                    characters
                        .as_ref()
                        .map(|text| ArrayValue::Word(Word::character(text[old_index])))
                        .or_else(|| heap.array_value(original(), old_index))
                })
                .unwrap_or_else(default)
        })
    }
    .map_err(|error| EvalError::out_of_memory(at, error))?;

    if info.nil_element {
        heap.mark_nil_element(replacement).expect("fresh array");
    }
    if info.adjustable {
        let original = original();
        heap.replace_adjustable_array(original, replacement)
            .expect("adjustable array of unchanged rank and element type");
        Ok(original)
    } else {
        Ok(replacement)
    }
}

pub(super) fn push(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    extend: bool,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let value = || frame.get(args.start);
    let vector = || frame.get(args.start + 1);
    let info = heap
        .array_info(vector())
        .ok_or_else(|| type_error(runtime, vector(), "VECTOR", at))?;
    let index = info.fill_pointer.ok_or_else(|| {
        Fault::vector_satisfying(
            runtime,
            vector(),
            "ARRAY-HAS-FILL-POINTER-P",
            EvalError::new(at, "expected a vector with a fill pointer"),
        )
    })?;
    let extension = if args.len() == 3 {
        let word = frame.get(args.start + 2);
        if let Some(positive) = word.as_fixnum().filter(|&n| n > 0) {
            positive as usize
        } else if heap
            .integer(word)
            .is_some_and(|integer| integer.sign() == Sign::Plus)
        {
            Word::FIXNUM_MAX as usize
        } else {
            return Err(Fault::positive_integer(
                runtime,
                word,
                EvalError::new(at, "expected a positive extension"),
            ));
        }
    } else {
        info.size.max(1)
    };
    if index < info.size {
        store(runtime, vector(), index, value(), at)?;
        heap.set_array_fill_pointer(vector(), index + 1)
            .expect("checked fill pointer");
    } else if extend {
        if !info.adjustable {
            return Err(Fault::vector_satisfying(
                runtime,
                vector(),
                "ADJUSTABLE-ARRAY-P",
                EvalError::new(at, "cannot extend a non-adjustable vector"),
            ));
        }
        if info.nil_element {
            return Err(type_error(runtime, value(), "NIL", at));
        }
        let new_value = checked_element(runtime, info.element, value(), at)?;
        let capacity = info
            .size
            .checked_add(extension)
            .filter(|&size| size < Word::FIXNUM_MAX as usize)
            .ok_or_else(|| problem(at, "array dimension exceeds the implementation limit"))?;
        let default = default_element(info.element);
        let characters = character_snapshot(runtime, vector(), info.element, info.size, at)?;
        let replacement =
            heap.array_from_values(&[capacity], info.element, true, Some(index + 1), |slot| {
                match slot.cmp(&index) {
                    std::cmp::Ordering::Less => characters
                        .as_ref()
                        .map(|text| ArrayValue::Word(Word::character(text[slot])))
                        .or_else(|| heap.array_value(vector(), slot))
                        .unwrap_or(default),
                    std::cmp::Ordering::Equal => match new_value {
                        ArrayValue::Word(_) => ArrayValue::Word(value()),
                        _ => new_value,
                    },
                    std::cmp::Ordering::Greater => default,
                }
            })
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        heap.replace_adjustable_array(vector(), replacement)
            .expect("adjustable vector of unchanged element type");
    } else {
        return Ok(Word::NIL);
    }
    size_word(index, at)
}

fn character_snapshot(
    runtime: &Runtime,
    array: Word,
    element: ArrayElement,
    count: usize,
    at: usize,
) -> Result<Option<Vec<char>>, Fault> {
    if element != ArrayElement::Character || count == 0 {
        return Ok(None);
    }
    let mut characters = Vec::new();
    let reserved = runtime
        .heap
        .with_array_character_slice(array, 0, count, |text| {
            characters
                .try_reserve_exact(count)
                .map(|()| characters.extend(text.chars()))
        });
    let Some(reserved) = reserved else {
        return Ok(None);
    };
    reserved.map_err(|_| EvalError::out_of_memory(at, OutOfMemory))?;
    Ok(Some(characters))
}

fn overlapping_index(mut index: usize, old: &[usize], new: &[usize]) -> Option<usize> {
    if old.contains(&0) {
        return None;
    }
    let mut old_index = 0;
    let mut stride = 1;
    for (&before, &after) in old.iter().zip(new).rev() {
        let coordinate = index % after;
        if coordinate >= before {
            return None;
        }
        old_index += coordinate * stride;
        index /= after;
        stride *= before;
    }
    Some(old_index)
}
