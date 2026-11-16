//! Instruction selection for call-free, single-result Lisp functions.
//!
//! Selection operates on canonical bytecode under the destination's word
//! layout. Native JIT, native AOT, and Wasm use the same selected operations.

mod flow;
pub mod leaf;
mod locals;
#[cfg(feature = "native")]
mod native;
mod select;

#[cfg(feature = "native")]
pub use native::Target;
pub use select::{SelectionError, select, select_bytecode, select_compiled};

use leaf::{Block, Exit, Step};
use zkcl_bytecode::native_abi::LeafGuard;

/// A fixed-arity function with one tagged result on every successful path.
///
/// The caller checks arity and binding guards and keeps arguments rooted. The
/// body neither allocates nor enters Lisp. It may return `LEAF_MISS` to request
/// that the runtime execute the original bytecode with its original arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct LeafFunction {
    arity: usize,
    word_bits: u8,
    guards: Vec<LeafGuard>,
    blocks: Vec<Block>,
    locals: usize,
    loop_budget: Option<u32>,
}

impl LeafFunction {
    pub fn arity(&self) -> usize {
        self.arity
    }

    pub fn word_bits(&self) -> u8 {
        self.word_bits
    }

    /// Live function bindings required by the selected paths.
    pub fn guards(&self) -> &[LeafGuard] {
        &self.guards
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn locals(&self) -> usize {
        self.locals
    }

    /// Maximum number of backwards edges the body may take before returning a miss.
    pub fn loop_budget(&self) -> Option<u32> {
        self.loop_budget
    }

    /// Whether the selected body can request execution of the original bytecode.
    pub fn may_miss(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| block.exit == Exit::Miss || block.steps.iter().any(Step::may_miss))
    }

    /// Whether the generic C entry needs the runtime's guard and fallback dispatcher.
    pub fn needs_dispatch(&self) -> bool {
        !self.guards.is_empty() || self.may_miss()
    }
}
