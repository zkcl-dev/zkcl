use num_bigint::BigInt;

use crate::classes::{Representation, RuntimeClassKind, SlotAllocation, class_definition};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlot, Word};
use crate::symbol::Package;
use crate::vm::{classes, methods};
use crate::{EvalError, Root, Runtime};

pub(super) enum Arithmetic {
    DivisionByZero,
    Overflow,
    Invalid,
    IntegerOverflow,
}

impl Arithmetic {
    fn classes(self) -> (Class, &'static str, &'static str) {
        match self {
            Self::DivisionByZero => (
                Class::DivisionByZero,
                "SIMD-DIVISION-BY-ZERO",
                "DIVISION-BY-ZERO",
            ),
            Self::Overflow => (
                Class::FloatingPointOverflow,
                "SIMD-FLOATING-POINT-OVERFLOW",
                "FLOATING-POINT-OVERFLOW",
            ),
            Self::Invalid => (
                Class::FloatingPointInvalidOperation,
                "SIMD-FLOATING-POINT-INVALID-OPERATION",
                "FLOATING-POINT-INVALID-OPERATION",
            ),
            Self::IntegerOverflow => (
                Class::ArithmeticError,
                "INTEGER-OVERFLOW",
                "ZK-SIMD:INTEGER-OVERFLOW",
            ),
        }
    }
}

pub(super) fn arithmetic(
    runtime: &Runtime,
    class: Arithmetic,
    operation: &str,
    args: impl ExactSizeIterator<Item = Word>,
    lane: usize,
    level: Option<usize>,
    at: usize,
) -> Fault {
    arithmetic_fault(runtime, class, operation, args, lane, (level, None), at)
}

pub(super) fn arithmetic_at_index(
    runtime: &Runtime,
    class: Arithmetic,
    operation: &str,
    args: impl ExactSizeIterator<Item = Word>,
    lane: usize,
    index: usize,
    at: usize,
) -> Fault {
    arithmetic_fault(
        runtime,
        class,
        operation,
        args,
        lane,
        (None, Some(index)),
        at,
    )
}

fn arithmetic_fault(
    runtime: &Runtime,
    class: Arithmetic,
    operation: &str,
    args: impl ExactSizeIterator<Item = Word>,
    lane: usize,
    (level, index): (Option<usize>, Option<usize>),
    at: usize,
) -> Fault {
    let (class, condition_name, report_name) = class.classes();
    let mut message = format!("ZK-SIMD:{operation} signaled {report_name} at lane {lane}");
    if let Some(level) = level {
        message.push_str(&format!(" at reduction level {level}"));
    }
    if let Some(index) = index {
        message.push_str(&format!(" at array index {index}"));
    }
    let host = EvalError::new(at, message).with_condition(class);

    let args_roots = runtime.root_frame(args.len());
    for (index, arg) in args.enumerate() {
        args_roots.set(index, arg);
    }
    let args = || (0..args_roots.len()).map(|index| args_roots.get(index));
    let build = || -> Result<Fault, Fault> {
        let oom = |error| EvalError::out_of_memory(at, error);
        let operation = runtime.root_slot(runtime.intern_package(Package::SIMD, operation, at)?.0);
        let operands = runtime.root_slot(methods::list_from_words(runtime, args().collect(), at)?);
        let lane = runtime.integer_from(lane).map_err(oom)?;
        let level = level.map_or_else(|| Ok(runtime.nil()), |level| runtime.integer_from(level));
        let level = level.map_err(oom)?;
        let index = index.map_or_else(|| Ok(runtime.nil()), |index| runtime.integer_from(index));
        let index = index.map_err(oom)?;

        let condition = instantiate(
            runtime,
            condition_name,
            [
                (Package::COMMON_LISP, "OPERATION", &operation),
                (Package::COMMON_LISP, "OPERANDS", &operands),
                (Package::SIMD, "LANE", &lane.slot),
                (Package::SIMD, "LEVEL", &level.slot),
                (Package::SIMD, "INDEX", &index.slot),
            ],
            at,
        )?;
        Ok(match condition {
            Some(condition) => Fault::existing_condition(&condition, host),
            None => Fault::arithmetic(runtime, class, operation.get(), args(), host),
        })
    };
    build().unwrap_or_else(|fault| fault)
}

pub(super) fn bounds_error(
    runtime: &Runtime,
    array: Word,
    index: &BigInt,
    lane: usize,
    at: usize,
) -> Fault {
    let host = EvalError::new(
        at,
        format!("ZK-SIMD array index {index} is out of bounds at lane {lane}"),
    )
    .with_condition(Class::Error);
    let array = runtime.root_slot(array);
    let build = || -> Result<Fault, Fault> {
        let oom = |error| EvalError::out_of_memory(at, error);
        let index = runtime.make_integer(index).map_err(oom)?;
        let lane = runtime.integer_from(lane).map_err(oom)?;
        let level = runtime.root_slot(Word::NIL);
        let condition = instantiate(
            runtime,
            "BOUNDS-ERROR",
            [
                (Package::SIMD, "ARRAY", &array),
                (Package::SIMD, "INDEX", &index.slot),
                (Package::SIMD, "LANE", &lane.slot),
                (Package::SIMD, "LEVEL", &level),
            ],
            at,
        )?;
        Ok(match condition {
            Some(condition) => Fault::existing_condition(&condition, host),
            None => Fault::plain(Class::Error, host),
        })
    };
    build().unwrap_or_else(|fault| fault)
}

fn instantiate<'rt, const N: usize>(
    runtime: &'rt Runtime,
    name: &str,
    slots: [(Package, &str, &RootSlot); N],
    at: usize,
) -> Result<Option<Root<'rt>>, Fault> {
    let present = |package, name| {
        runtime
            .packages
            .borrow()
            .present(package, name)
            .map(|(symbol, _)| symbol)
    };
    let Some(name) = present(Package::SIMD, name) else {
        return Ok(None);
    };
    let Some(class) = classes::canonical_class(runtime, name, at)? else {
        return Ok(None);
    };
    let class = runtime.root_slot(class);
    let Some(definition) = class_definition(runtime, class.get()).filter(|definition| {
        definition.kind == RuntimeClassKind::Condition
            && definition.representation == Representation::Instance
    }) else {
        return Ok(None);
    };

    // These conditions have only explicit VM-supplied values. Resolve their
    // current instance layout without invoking Lisp while handling a fault.
    let mut indexes = [0; N];
    for ((package, name, _), index) in slots.iter().zip(&mut indexes) {
        let Some(slot) = present(*package, name).and_then(|name| definition.slot(runtime, name))
        else {
            return Ok(None);
        };
        let SlotAllocation::Instance(slot_index) = slot.allocation else {
            return Ok(None);
        };
        *index = slot_index;
    }
    let instance = runtime
        .heap
        .instance(&class, &definition.instance_layout)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    for (index, (_, _, value)) in indexes.into_iter().zip(slots) {
        if runtime
            .heap
            .set_instance_slot(instance, index, value.get())
            .is_none()
        {
            return Ok(None);
        }
    }
    Ok(Some(runtime.root(instance)))
}
