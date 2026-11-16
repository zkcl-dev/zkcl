//! Calling convention, tagged values, GC roots and unwind state shared by native code generators.

mod features;
mod frame;
mod runtime_call;
mod tail;
mod word_ops;

pub use features::{
    CRANELIFT_FEATURES, ClassifiedFunction, LLVM_FEATURES, NativeFeature, WASM_FEATURES,
    supports_lowering,
};
pub use frame::{NativeArguments, NativeFrameLayout, OptionalArgumentSlots, native_frame_layout};
pub use runtime_call::{CallOperand, CallReturn, CallWhen, NativeOp, RuntimeCall, SliceCharge};
pub use tail::{TailCall, TailCallee, tail_call};
pub(crate) use tail::{empty_return_blocks, returns_dynamic_values};
pub use word_ops::{I64Arithmetic, ScalarConstant, WordOp, WordPredicate};
pub use zkcl_bytecode::native_abi::*;
pub use zkcl_bytecode::{FIXNUM_SHIFT, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T, WORD_UNBOUND};

/// Classification counts for tests that assert a single derivation pass.
#[cfg(feature = "counters")]
mod counters {
    use std::cell::Cell;

    thread_local! {
        static CALLS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn record() {
        CALLS.with(|calls| calls.set(calls.get() + 1));
    }

    pub fn read() -> u64 {
        CALLS.with(Cell::get)
    }

    pub fn reset() {
        CALLS.with(|calls| calls.set(0));
    }
}

#[cfg(feature = "counters")]
pub use counters::{read as for_op_calls, reset as reset_for_op_calls};
