//! Finalizer registrations belong to their runtime. The collector clears the
//! wrapper's weak field before this module can enter Lisp again.
use std::cell::{Cell, RefCell};
use std::mem::DropGuard;

use crate::condition::Fault;
use crate::heap::{RootSlot, RootSlots};
use crate::vm::{self, modules::arity_error};
use crate::{EvalError, Root, Runtime};

mod image;

struct FinalizerRegistration {
    pointer: RootSlot,
    function: RootSlot,
}

#[derive(Default)]
pub(crate) struct Finalizers {
    waiting: RefCell<Vec<FinalizerRegistration>>,
    pending: RefCell<Vec<RootSlot>>,
    last_collection: Cell<u64>,
    delivering: Cell<bool>,
}

impl Finalizers {
    pub(crate) fn deliver(&self, runtime: &Runtime) -> Result<(), EvalError> {
        if self.delivering.get() {
            return Ok(());
        }

        if !self.waiting.borrow().is_empty() {
            let epoch = runtime.heap.collections();
            if self.last_collection.replace(epoch) != epoch {
                let mut waiting = self.waiting.borrow_mut();
                let dead = waiting.extract_if(.., |entry| {
                    runtime
                        .heap
                        .weak_pointer_value(entry.pointer.get())
                        .is_none()
                });
                let mut pending = self.pending.borrow_mut();
                let older = pending.len();
                pending.extend(dead.map(|entry| entry.function));
                // Delivery pops from the end. Keep callbacks left by an error
                // ahead of new arrivals, even if a callback repeatedly triggers GC.
                pending.rotate_left(older);
            }
        }
        if self.pending.borrow().is_empty() {
            return Ok(());
        }

        self.delivering.set(true);
        let _guard = DropGuard::new(&self.delivering, |flag| flag.set(false));
        // Remove each callback before calling it. An error leaves every later
        // callback rooted for the next boundary. Nested collections are picked
        // up at that boundary, keeping the current delivery batch finite.
        while !self.pending.borrow().is_empty() {
            // Cancellation or an exhausted host budget must not consume a
            // callback before we have even attempted its invocation.
            runtime.check_execution_safepoint(0)?;
            let function = self.pending.borrow_mut().pop().unwrap();
            vm::run(runtime, function.get(), RootSlots::empty(), 0..0)?;
        }
        Ok(())
    }
}

pub(crate) fn register<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [object, function] = args else {
        return Err(arity_error(at, "FINALIZE expects an object and a function"));
    };
    if object.slot.get().is_immediate() {
        return Err(EvalError::new(at, "only heap-allocated objects can be finalized").into());
    }
    let function = runtime
        .heap
        .root_for_host(vm::designator(runtime, function.slot.get(), at)?);
    let pointer = runtime
        .heap
        .weak_pointer(&object.slot)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    runtime
        .finalizers
        .waiting
        .borrow_mut()
        .push(FinalizerRegistration {
            pointer: runtime.heap.root_for_host(pointer),
            function,
        });
    Ok(vec![object.clone()])
}

pub(crate) fn cancel<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [object] = args else {
        return Err(arity_error(at, "CANCEL-FINALIZATION expects one object"));
    };
    let object = object.slot.get();
    runtime
        .finalizers
        .waiting
        .borrow_mut()
        .retain(|entry| runtime.heap.weak_pointer_value(entry.pointer.get()) != Some(object));
    Ok(vec![runtime.nil()])
}

pub(crate) fn collect<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    if !args.is_empty() {
        return Err(arity_error(at, "GC expects no arguments"));
    }
    runtime.collect();
    runtime.finalizers.deliver(runtime)?;
    Ok(vec![runtime.nil()])
}
