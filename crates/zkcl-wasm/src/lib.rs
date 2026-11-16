//! Core WebAssembly execution and code generation without an OS ABI.

#[cfg(feature = "abi")]
pub mod abi;
#[cfg(feature = "codegen")]
pub mod codegen;
#[cfg(feature = "engine")]
mod engine;
#[cfg(any(feature = "codegen", feature = "engine"))]
pub mod source_map;
#[cfg(feature = "engine")]
pub use engine::*;
