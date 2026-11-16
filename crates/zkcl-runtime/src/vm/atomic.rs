use std::sync::Arc;
use std::sync::atomic::{self, Ordering};

use crate::condition::{Class, Fault};
use crate::heap::SimpleVectorError;
use crate::package::Package;
use crate::vm::modules::arity_error;
use crate::{EvalError, Root, Runtime, vm};

#[derive(Clone, Copy)]
enum ConsSlot {
    Car,
    Cdr,
}

#[derive(Clone, Copy)]
enum Op {
    CasCons(ConsSlot),
    CasSvref,
    CasSymbolValue,
    LoadCons(ConsSlot),
    LoadSvref,
    LoadSymbolValue,
    StoreCons(ConsSlot),
    StoreSvref,
    StoreSymbolValue,
    Fence,
}

const PRIMITIVES: &[(&str, Op)] = &[
    ("CAS-CAR", Op::CasCons(ConsSlot::Car)),
    ("CAS-CDR", Op::CasCons(ConsSlot::Cdr)),
    ("CAS-SVREF", Op::CasSvref),
    ("CAS-SYMBOL-VALUE", Op::CasSymbolValue),
    ("ATOMIC-LOAD-CAR", Op::LoadCons(ConsSlot::Car)),
    ("ATOMIC-LOAD-CDR", Op::LoadCons(ConsSlot::Cdr)),
    ("ATOMIC-LOAD-SVREF", Op::LoadSvref),
    ("ATOMIC-LOAD-SYMBOL-VALUE", Op::LoadSymbolValue),
    ("ATOMIC-STORE-CAR", Op::StoreCons(ConsSlot::Car)),
    ("ATOMIC-STORE-CDR", Op::StoreCons(ConsSlot::Cdr)),
    ("ATOMIC-STORE-SVREF", Op::StoreSvref),
    ("ATOMIC-STORE-SYMBOL-VALUE", Op::StoreSymbolValue),
    ("FENCE", Op::Fence),
];

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for &(name, op) in PRIMITIVES {
        let symbol = runtime.intern_package(Package::ATOMIC, name, 0)?.0;
        runtime
            .packages
            .borrow_mut()
            .export(Package::ATOMIC, name, symbol)
            .map_err(|error| EvalError::from_error(0, error))?;
        vm::install_host_function(
            runtime,
            &format!("ZK-ATOMIC:{name}"),
            Arc::new(move |rt, args, at| execute(rt, op, args, at)),
        )?;
    }
    Ok(())
}

fn program_error(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn parse_ordering(runtime: &Runtime, word: crate::heap::Word, at: usize) -> Result<Ordering, Fault> {
    let is_keyword = runtime
        .heap
        .symbol_attributes(word)
        .is_some_and(|attrs| attrs.home == Some(Package::KEYWORD));
    if !is_keyword {
        return Err(program_error(
            at,
            "expected an atomic memory ordering keyword (:RELAXED, :ACQUIRE, :RELEASE, :ACQ-REL, or :SEQ-CST)",
        ));
    }
    match runtime.heap.symbol_name(word).as_deref() {
        Some("RELAXED") => Ok(Ordering::Relaxed),
        Some("ACQUIRE") => Ok(Ordering::Acquire),
        Some("RELEASE") => Ok(Ordering::Release),
        Some("ACQ-REL") => Ok(Ordering::AcqRel),
        Some("SEQ-CST") => Ok(Ordering::SeqCst),
        _ => Err(program_error(
            at,
            "unknown atomic memory ordering keyword (expected :RELAXED, :ACQUIRE, :RELEASE, :ACQ-REL, or :SEQ-CST)",
        )),
    }
}

fn optional_order(
    runtime: &Runtime,
    args: &[Root<'_>],
    index: usize,
    at: usize,
) -> Result<Ordering, Fault> {
    match args.get(index) {
        Some(order) => parse_ordering(runtime, order.slot.get(), at),
        None => Ok(Ordering::SeqCst),
    }
}

fn validate_load_order(order: Ordering, at: usize) -> Result<Ordering, Fault> {
    match order {
        Ordering::Relaxed | Ordering::Acquire | Ordering::SeqCst => Ok(order),
        Ordering::Release | Ordering::AcqRel => Err(program_error(
            at,
            "invalid atomic load memory ordering (expected :RELAXED, :ACQUIRE, or :SEQ-CST)",
        )),
        _ => unreachable!("handled ordering variant"),
    }
}

fn validate_store_order(order: Ordering, at: usize) -> Result<Ordering, Fault> {
    match order {
        Ordering::Relaxed | Ordering::Release | Ordering::SeqCst => Ok(order),
        Ordering::Acquire | Ordering::AcqRel => Err(program_error(
            at,
            "invalid atomic store memory ordering (expected :RELAXED, :RELEASE, or :SEQ-CST)",
        )),
        _ => unreachable!("handled ordering variant"),
    }
}

fn cas_orderings(order: Ordering) -> (Ordering, Ordering) {
    match order {
        Ordering::Relaxed => (Ordering::Relaxed, Ordering::Relaxed),
        Ordering::Acquire => (Ordering::Acquire, Ordering::Acquire),
        Ordering::Release => (Ordering::Release, Ordering::Relaxed),
        Ordering::AcqRel => (Ordering::AcqRel, Ordering::Acquire),
        Ordering::SeqCst => (Ordering::SeqCst, Ordering::SeqCst),
        _ => unreachable!("handled ordering variant"),
    }
}

fn validate_fence_order(order: Ordering, at: usize) -> Result<Ordering, Fault> {
    match order {
        Ordering::Acquire | Ordering::Release | Ordering::AcqRel | Ordering::SeqCst => Ok(order),
        Ordering::Relaxed => Err(program_error(
            at,
            "invalid atomic fence memory ordering (expected :ACQUIRE, :RELEASE, :ACQ-REL, or :SEQ-CST)",
        )),
        _ => unreachable!("handled ordering variant"),
    }
}

fn check_arity(args: &[Root<'_>], min: usize, max: usize, name: &str, at: usize) -> Result<(), Fault> {
    if args.len() < min || args.len() > max {
        return Err(arity_error(
            at,
            &format!("ZK-ATOMIC:{name} expects between {min} and {max} arguments"),
        ));
    }
    Ok(())
}

fn natural_index(runtime: &Runtime, word: crate::heap::Word, at: usize) -> Result<usize, Fault> {
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

fn map_sv_error(
    runtime: &Runtime,
    vector: crate::heap::Word,
    index: usize,
    error: SimpleVectorError,
    at: usize,
) -> Fault {
    match error {
        SimpleVectorError::NotSimpleVector => {
            Fault::type_error(runtime, vector, "SIMPLE-VECTOR", EvalError::new(at, "expected SIMPLE-VECTOR"))
        }
        SimpleVectorError::OutOfBounds { len } => program_error(
            at,
            format!("simple-vector index {index} is out of bounds for length {len}"),
        ),
    }
}

fn expected_cons(runtime: &Runtime, value: crate::heap::Word, at: usize) -> Fault {
    Fault::type_error(runtime, value, "CONS", EvalError::new(at, "expected a cons"))
}

fn require_symbol(runtime: &Runtime, symbol: crate::heap::Word, at: usize) -> Result<(), Fault> {
    if !runtime.heap.is_symbol(symbol) {
        return Err(Fault::type_error(
            runtime,
            symbol,
            "SYMBOL",
            EvalError::new(at, "expected a symbol"),
        ));
    }
    Ok(())
}

fn require_mutable_variable(
    runtime: &Runtime,
    symbol: crate::heap::Word,
    at: usize,
) -> Result<(), Fault> {
    require_symbol(runtime, symbol, at)?;
    if runtime
        .heap
        .symbol_attributes(symbol)
        .is_none_or(|attrs| attrs.is_constant_variable())
    {
        return Err(program_error(at, "cannot change a constant variable"));
    }
    Ok(())
}

fn unbound_variable(runtime: &Runtime, symbol: crate::heap::Word, at: usize) -> Fault {
    let name = runtime
        .heap
        .symbol_name(symbol)
        .unwrap_or_else(|| "<symbol>".into());
    Fault::cell(
        runtime,
        Class::UnboundVariable,
        symbol,
        EvalError::new(at, format!("unbound variable {name}")),
    )
}

fn cas_result<'rt>(
    runtime: &'rt Runtime,
    outcome: Result<crate::heap::Word, crate::heap::Word>,
) -> Vec<Root<'rt>> {
    match outcome {
        Ok(previous) => vec![runtime.root(previous), runtime.t()],
        Err(previous) => vec![runtime.root(previous), runtime.nil()],
    }
}

fn execute<'rt>(
    runtime: &'rt Runtime,
    op: Op,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    match op {
        Op::LoadCons(slot) => {
            let name = match slot {
                ConsSlot::Car => "ATOMIC-LOAD-CAR",
                ConsSlot::Cdr => "ATOMIC-LOAD-CDR",
            };
            check_arity(args, 1, 2, name, at)?;
            let order = validate_load_order(optional_order(runtime, args, 1, at)?, at)?;
            let cons = args[0].slot.get();
            let value = match slot {
                ConsSlot::Car => runtime.heap.atomic_load_cons_car(cons, order),
                ConsSlot::Cdr => runtime.heap.atomic_load_cons_cdr(cons, order),
            }
            .ok_or_else(|| expected_cons(runtime, cons, at))?;
            Ok(vec![runtime.root(value)])
        }
        Op::StoreCons(slot) => {
            let name = match slot {
                ConsSlot::Car => "ATOMIC-STORE-CAR",
                ConsSlot::Cdr => "ATOMIC-STORE-CDR",
            };
            check_arity(args, 2, 3, name, at)?;
            let order = validate_store_order(optional_order(runtime, args, 2, at)?, at)?;
            let cons = args[0].slot.get();
            let value = args[1].slot.get();
            match slot {
                ConsSlot::Car => runtime.heap.atomic_store_cons_car(cons, value, order),
                ConsSlot::Cdr => runtime.heap.atomic_store_cons_cdr(cons, value, order),
            }
            .ok_or_else(|| expected_cons(runtime, cons, at))?;
            Ok(vec![args[1].clone()])
        }
        Op::CasCons(slot) => {
            let name = match slot {
                ConsSlot::Car => "CAS-CAR",
                ConsSlot::Cdr => "CAS-CDR",
            };
            check_arity(args, 3, 4, name, at)?;
            let (success, failure) = cas_orderings(optional_order(runtime, args, 3, at)?);
            let cons = args[0].slot.get();
            let expected = args[1].slot.get();
            let new = args[2].slot.get();
            let outcome = match slot {
                ConsSlot::Car => runtime
                    .heap
                    .atomic_cas_cons_car(cons, expected, new, success, failure),
                ConsSlot::Cdr => runtime
                    .heap
                    .atomic_cas_cons_cdr(cons, expected, new, success, failure),
            }
            .ok_or_else(|| expected_cons(runtime, cons, at))?;
            Ok(cas_result(runtime, outcome))
        }
        Op::LoadSvref => {
            check_arity(args, 2, 3, "ATOMIC-LOAD-SVREF", at)?;
            let index = natural_index(runtime, args[1].slot.get(), at)?;
            let order = validate_load_order(optional_order(runtime, args, 2, at)?, at)?;
            let vector = args[0].slot.get();
            let value = runtime
                .heap
                .atomic_load_svref(vector, index, order)
                .map_err(|error| map_sv_error(runtime, vector, index, error, at))?;
            Ok(vec![runtime.root(value)])
        }
        Op::StoreSvref => {
            check_arity(args, 3, 4, "ATOMIC-STORE-SVREF", at)?;
            let index = natural_index(runtime, args[1].slot.get(), at)?;
            let order = validate_store_order(optional_order(runtime, args, 3, at)?, at)?;
            let vector = args[0].slot.get();
            let value = args[2].slot.get();
            runtime
                .heap
                .atomic_store_svref(vector, index, value, order)
                .map_err(|error| map_sv_error(runtime, vector, index, error, at))?;
            Ok(vec![args[2].clone()])
        }
        Op::CasSvref => {
            check_arity(args, 4, 5, "CAS-SVREF", at)?;
            let index = natural_index(runtime, args[1].slot.get(), at)?;
            let (success, failure) = cas_orderings(optional_order(runtime, args, 4, at)?);
            let vector = args[0].slot.get();
            let expected = args[2].slot.get();
            let new = args[3].slot.get();
            let outcome = runtime
                .heap
                .atomic_cas_svref(vector, index, expected, new, success, failure)
                .map_err(|error| map_sv_error(runtime, vector, index, error, at))?;
            Ok(cas_result(runtime, outcome))
        }
        Op::LoadSymbolValue => {
            check_arity(args, 1, 2, "ATOMIC-LOAD-SYMBOL-VALUE", at)?;
            let symbol = args[0].slot.get();
            require_symbol(runtime, symbol, at)?;
            let order = validate_load_order(optional_order(runtime, args, 1, at)?, at)?;
            let value = runtime
                .heap
                .atomic_load_symbol_value(symbol, order)
                .ok_or_else(|| unbound_variable(runtime, symbol, at))?;
            Ok(vec![runtime.root(value)])
        }
        Op::StoreSymbolValue => {
            check_arity(args, 2, 3, "ATOMIC-STORE-SYMBOL-VALUE", at)?;
            let symbol = args[0].slot.get();
            require_mutable_variable(runtime, symbol, at)?;
            let order = validate_store_order(optional_order(runtime, args, 2, at)?, at)?;
            let value = args[1].slot.get();
            runtime
                .heap
                .atomic_store_symbol_value(symbol, value, order)
                .expect("mutable variable symbol is backed by Symbol");
            Ok(vec![args[1].clone()])
        }
        Op::CasSymbolValue => {
            check_arity(args, 3, 4, "CAS-SYMBOL-VALUE", at)?;
            let symbol = args[0].slot.get();
            require_mutable_variable(runtime, symbol, at)?;
            let (success, failure) = cas_orderings(optional_order(runtime, args, 3, at)?);
            let expected = args[1].slot.get();
            let new = args[2].slot.get();
            let outcome = runtime
                .heap
                .atomic_cas_symbol_value(symbol, expected, new, success, failure)
                .expect("mutable variable symbol is backed by Symbol");
            if outcome == Err(crate::heap::Word::UNBOUND) {
                return Err(unbound_variable(runtime, symbol, at));
            }
            Ok(cas_result(runtime, outcome))
        }
        Op::Fence => {
            check_arity(args, 0, 1, "FENCE", at)?;
            let order = validate_fence_order(optional_order(runtime, args, 0, at)?, at)?;
            atomic::fence(order);
            Ok(vec![runtime.nil()])
        }
    }
}
