//! Runtime-owned, unboxed machine words. The moving heap contains only the
//! existing traced foreign handle, never an address used by atomic operations.
use std::sync::{Arc, atomic::{AtomicUsize, Ordering::SeqCst}};

use crate::condition::Fault;
use crate::heap::Word;
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime, vm};

struct AtomicWord(AtomicUsize);

#[derive(Clone, Copy)]
enum Operation {
    Make,
    Read,
    Store,
    CompareExchange,
}

pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, op) in [
        ("ZK::%MAKE-ATOMIC-WORD", Operation::Make),
        ("ZK::%ATOMIC-WORD-VALUE", Operation::Read),
        ("ZK::%SET-ATOMIC-WORD", Operation::Store),
        (
            "ZK::%ATOMIC-WORD-COMPARE-EXCHANGE",
            Operation::CompareExchange,
        ),
    ] {
        vm::install_host_function(
            runtime,
            name,
            Arc::new(move |rt, args, at| execute(rt, op, args, at)),
        )?;
    }
    Ok(())
}

fn machine_word(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<usize, Fault> {
    let word = value.slot.get();
    let number = match word.as_fixnum() {
        Some(n) => usize::try_from(n).ok(),
        None => runtime.heap.integer(word).and_then(|n| n.try_into().ok()),
    };
    if let Some(number) = number {
        return Ok(number);
    }
    let unsigned = runtime.intern(&Name::common_lisp("UNSIGNED-BYTE"), at)?;
    let width = Word::fixnum(usize::BITS as isize).unwrap();
    let spec = vm::methods::list_from_words(runtime, vec![unsigned, width], at)?;
    Err(Fault::type_error_specifier(runtime, 
        value.slot.get(),
        spec,
        EvalError::new(at, "expected an unsigned machine word"),
    ))
}

fn with_cell<R>(
    runtime: &Runtime,
    cell: &Root<'_>,
    at: usize,
    f: impl FnOnce(&AtomicUsize) -> R,
) -> Result<R, Fault> {
    cell.try_with_foreign_ref::<AtomicWord, _>(runtime, |word| f(&word.0))
        .map_err(|_| EvalError::new(at, "expected an atomic word belonging to this runtime").into())
}

fn execute<'rt>(
    runtime: &'rt Runtime,
    op: Operation,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let result = match (op, args) {
        (Operation::Make, [initial]) => {
            let word = machine_word(runtime, initial, at)?;
            runtime
                .foreign(AtomicWord(AtomicUsize::new(word)))
                .map_err(|error| EvalError::out_of_memory(at, error))?
        }
        (Operation::Read, [cell]) => {
            let value = with_cell(runtime, cell, at, |word| word.load(SeqCst))?;
            runtime
                .integer_from(value)
                .map_err(|error| EvalError::out_of_memory(at, error))?
        }
        (Operation::Store, [cell, new_value]) => {
            let value = machine_word(runtime, new_value, at)?;
            with_cell(runtime, cell, at, |word| word.store(value, SeqCst))?;
            new_value.clone()
        }
        (Operation::CompareExchange, [cell, old, new]) => {
            let old = machine_word(runtime, old, at)?;
            let new = machine_word(runtime, new, at)?;
            let changed = with_cell(runtime, cell, at, |word| {
                word.compare_exchange(old, new, SeqCst, SeqCst).is_ok()
            })?;
            runtime.root(if changed { Word::T } else { Word::NIL })
        }
        _ => {
            return Err(super::arity_error(
                at,
                "wrong number of arguments to atomic word operation",
            ));
        }
    };
    Ok(vec![result])
}
