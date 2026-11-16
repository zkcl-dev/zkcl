//! In-process native compilers with a shared code cache and background queue.

use thiserror::Error;

#[cfg(any(feature = "direct", feature = "cranelift"))]
pub mod backend;
pub mod code_cache;
#[cfg(feature = "cranelift")]
pub mod compiler;
#[cfg(feature = "cranelift")]
mod cranelift_backend;
#[cfg(feature = "direct")]
mod direct;

#[cfg(feature = "cranelift")]
pub use backend::CraneliftJitBackend;
#[cfg(feature = "direct")]
pub use backend::DirectJitBackend;
#[cfg(all(feature = "counters", any(feature = "direct", feature = "cranelift")))]
pub use backend::QueueStopPointCounters;
pub use code_cache::{CodeCache, ExecutableCode, ExecutableMemoryError, MappedJitFunction};
#[cfg(feature = "cranelift")]
pub use compiler::{
    CompiledMachineCode, compile_ssa_function, compile_ssa_function_for_host, host_isa,
};

/// Errors returned by the native JIT backends.
#[derive(Debug, Error)]
pub enum JitError {
    #[cfg(feature = "cranelift")]
    #[error("failed to lift bytecode to SSA IR: {0}")]
    Lift(#[from] zkcl_opt::BytecodeLiftError),
    #[error("unsupported SSA operation in JIT: {0}")]
    UnsupportedOp(String),
    #[cfg(feature = "cranelift")]
    #[error("invalid SSA IR: {0}")]
    InvalidIr(#[source] zkcl_cranelift::LoweringError),
    #[error("unsupported host target: {0}")]
    UnsupportedTarget(String),
    #[error("JIT compilation error: {0}")]
    Codegen(String),
    #[cfg(feature = "cranelift")]
    #[error("JIT compilation error: failed to set opt_level: {0}")]
    CraneliftOptLevel(#[source] cranelift_codegen::settings::SetError),
    #[cfg(feature = "cranelift")]
    #[error("unsupported host target: ISA finish failed: {0}")]
    CraneliftIsa(#[source] cranelift_codegen::CodegenError),
    #[cfg(feature = "cranelift")]
    #[error("JIT compilation error: {0:?}")]
    CraneliftCodegen(#[source] cranelift_codegen::CodegenError),
    #[error("JIT compilation error: failed to spawn JIT worker thread: {0}")]
    SpawnWorker(#[source] std::io::Error),
    #[error("executable memory error: {0}")]
    Memory(#[from] ExecutableMemoryError),
}
