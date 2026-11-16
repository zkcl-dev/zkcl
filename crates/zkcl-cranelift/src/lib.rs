//! Shared Cranelift SSA-to-IR lowering backend for `zkcl-jit` and `zkcl-aot`.
//!
//! The JIT and offline Cranelift object writer use the same lowering path.

pub mod abi;
pub mod lower;

pub use abi::{
    FIXNUM_SHIFT, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_UNWIND,
    JIT_RETURN_TAG_VALUES, RuntimeAbiFunction, RuntimeAbiType, TAG_FIXNUM, TAG_MASK, WORD_NIL,
    WORD_T, WORD_UNBOUND, jit_return_layout,
};
pub use lower::{
    LoweringConfig, LoweringError, RuntimeCallEmitter, lower_ssa_function, lower_ssa_function_into,
};
pub use zkcl_opt::native::{NativeFrameLayout, native_frame_layout};
