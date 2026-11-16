use super::conditions::arithmetic_at_index;
use super::operations::float_class;
use super::*;
use crate::condition::Class;
use crate::heap::F32SpanError;

// Charge one step per bounded block, including the tail. This budget unit is independent of ISA.
const ARRAY_BLOCK: usize = 256;

pub(super) fn mul_add_into(runtime: &Runtime, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    let [out_start, a_start, b_start, count] = super::super::named_keyword_arguments(
        runtime,
        args,
        4..args.len(),
        ["OUT-START", "A-START", "B-START", "COUNT"],
        at,
    )?;
    let indices = [0, 1, 3];
    let array = |i: usize| args.get(indices[i]);
    memory::checked_array(runtime, array(0), Some(ArrayElement::SingleFloat), at)?;
    memory::checked_array(runtime, array(1), Some(ArrayElement::SingleFloat), at)?;
    let scale = read_lane(runtime, LaneKind::F32, args.get(2), at)? as u32;
    memory::checked_array(runtime, array(2), Some(ArrayElement::SingleFloat), at)?;

    let mut starts = [0; 3];
    let mut available = usize::MAX;
    for (i, parameter) in [out_start, a_start, b_start].into_iter().enumerate() {
        let start = parameter.map_or_else(
            || Ok(0.into()),
            |arg| nonnegative(runtime, args.get(arg), at),
        )?;
        // checked_array established the header kind. Sizes use row-major array, not sequence semantics.
        let size = runtime
            .heap
            .array_info(array(i))
            .expect("checked array")
            .size;
        let offset = start
            .to_usize()
            .filter(|offset| *offset <= size)
            .ok_or_else(|| bounds_error(runtime, array(i), &start, 0, at))?;
        starts[i] = offset;
        available = available.min(size - offset);
    }
    let count = match count {
        None => available,
        Some(index) => nonnegative(runtime, args.get(index), at)?
            .to_usize()
            .unwrap_or(usize::MAX),
    };
    let ranges = |position: usize| std::array::from_fn(|i| (array(i), starts[i] + position));
    let fault = |error| match error {
        F32SpanError::Bounds { array: i, index } => {
            bounds_error(runtime, array(i), &index.into(), index - starts[i], at)
        }
        F32SpanError::PartialOverlap { input } => Fault::plain(
            Class::Error,
            EvalError::new(
                at,
                format!(
                    "ZK-SIMD:MUL-ADD-INTO: output partially overlaps input {}",
                    ["output", "A", "B"][input]
                ),
            ),
        ),
    };
    runtime
        .heap
        .check_f32_array_spans(ranges(0), count)
        .map_err(fault)?;
    for position in (0..count).step_by(ARRAY_BLOCK) {
        runtime.check_execution_safepoint(at)?;
        let length = (count - position).min(ARRAY_BLOCK);
        let mut done = 0;
        if let Some(leaf) = native::f32_scaled_add() {
            // SAFETY: The heap checks all accessed spans, including exact aliasing, and
            // excludes GC for the lifetime of the raw pointers. The leaf does no calls into Lisp.
            done = unsafe {
                runtime
                    .heap
                    .visit_f32_array_spans(ranges(position), length, scale, leaf)
            }
            .map_err(fault)?;
        }
        for lane in position + done..position + length {
            let read = |i| match runtime.heap.array_value(array(i), starts[i] + lane) {
                Some(ArrayValue::SingleFloat(value)) => u64::from(value.to_bits()),
                _ => unreachable!("validated single-float array spans"),
            };
            let failure = |class, operation| {
                arithmetic_at_index(
                    runtime,
                    float_class(class),
                    operation,
                    (0..4).map(|i| args.get(i)),
                    lane,
                    starts[0] + lane,
                    at,
                )
            };
            let a = read(1);
            if !LaneKind::F32.valid_lane_bits(a) {
                return Err(failure(float::Fault::Invalid, "MUL"));
            }
            let product = Format::F32
                .mul(a, u64::from(scale))
                .map_err(|class| failure(class, "MUL"))?;
            let b = read(2);
            if !LaneKind::F32.valid_lane_bits(b) {
                return Err(failure(float::Fault::Invalid, "ADD"));
            }
            let result = Format::F32
                .add(product, b)
                .map_err(|class| failure(class, "ADD"))?;
            let value = ArrayValue::SingleFloat(f32::from_bits(result as u32));
            let stored = runtime
                .heap
                .set_array_value(array(0), starts[0] + lane, value)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            debug_assert!(stored, "validated single-float array span");
        }
    }
    Ok(array(0))
}

pub(super) fn octet_eq_mask(runtime: &Runtime, args: &RootSlots, at: usize) -> Result<Word, Fault> {
    memory::checked_array(runtime, args.get(0), Some(ArrayElement::Octet), at)?;
    let start = nonnegative(runtime, args.get(1), at)?;
    let octet = read_lane(runtime, LaneKind::U8, args.get(2), at)? as u8;
    let count = if args.len() == 4 {
        bounded_integer(runtime, args.get(3), 16, at)?
    } else {
        16
    };
    if count == 0 {
        return mask_word(runtime, 16, 0, at);
    }
    let active = u64::from(u16::MAX >> (16 - count));
    let array = args.get(0);
    let mut bytes = [0; 16];
    start
        .to_usize()
        .ok_or(0)
        .and_then(|offset| {
            runtime
                .heap
                .load_numeric_pack(array, ArrayElement::Octet, offset, active, &mut bytes)
        })
        .map_err(|lane| bounds_error(runtime, args.get(0), &(&start + lane), lane, at))?;
    let bits = u64::from(native::u8x16_eq(bytes, [octet; 16])) & active;
    mask_word(runtime, 16, bits, at)
}
