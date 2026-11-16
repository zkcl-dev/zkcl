//! Core Wasm emission for Lisp functions.

mod cfg;
mod leaf;
mod link;
mod ssa;

pub use leaf::{LoweredLeaf, compile_leaf};
pub use ssa::{LoweredFunction, LoweringError, lower_ssa_function};

pub use link::{LinkError, LinkedModule, link, link_compiled_file, link_project, target};
