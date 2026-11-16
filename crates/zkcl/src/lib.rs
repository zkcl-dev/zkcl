//! An embeddable Common Lisp implementation for Rust.

pub use zkcl_runtime::*;

/// Source projects (`project`) and prepared artifacts (`delivery`) for embedded hosts.
#[cfg(feature = "delivery")]
pub mod project;

#[cfg(any(feature = "jit", feature = "direct-jit"))]
pub mod jit {
    pub use zkcl_jit::*;
}

/// Enable the in-process Cranelift background JIT backend on `runtime` when `feature = "jit"` is active.
#[cfg(feature = "jit")]
pub fn enable_jit(
    runtime: &Runtime,
) -> Result<std::sync::Arc<zkcl_jit::CraneliftJitBackend>, zkcl_jit::JitError> {
    let backend = std::sync::Arc::new(zkcl_jit::CraneliftJitBackend::new()?);
    runtime.set_jit_backend(backend.clone());
    Ok(backend)
}

/// Install the direct machine-code backend for eligible fixed-arity functions.
#[cfg(feature = "direct-jit")]
pub fn enable_direct_jit(
    runtime: &Runtime,
) -> Result<std::sync::Arc<zkcl_jit::DirectJitBackend>, zkcl_jit::JitError> {
    let backend = std::sync::Arc::new(zkcl_jit::DirectJitBackend::new()?);
    runtime.set_jit_backend(backend.clone());
    Ok(backend)
}
