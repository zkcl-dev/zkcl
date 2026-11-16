//! Prepare verified bytecode for SSA-based native backends.

#[cfg(any(feature = "llvm", feature = "cranelift"))]
use crate::{AotError, AotRegistrationEntry};
use zkcl_bytecode::{BytecodeFunction, CompiledFile};
#[cfg(any(feature = "llvm", feature = "cranelift"))]
use zkcl_opt::VopTarget;
use zkcl_opt::{FaslSpecializationInputs, SsaFunction};

#[cfg(any(feature = "llvm", feature = "cranelift"))]
pub(super) fn optimize_native_entry(
    file: &CompiledFile,
    code: &BytecodeFunction,
    index: usize,
    vop_target: Option<VopTarget>,
) -> Result<(u32, SsaFunction), AotError> {
    let entry_index = u32::try_from(index).map_err(|_| AotError::TooManyCodeEntries)?;
    let mut inputs = FaslSpecializationInputs::new(file, code);
    if let Some(target) = vop_target {
        inputs = inputs.with_vop_target(target);
    }
    let ssa = inputs.optimize().map_err(|source| AotError::Lift {
        entry_index,
        source,
    })?;
    Ok((entry_index, ssa))
}

#[cfg(any(feature = "llvm", feature = "cranelift"))]
pub(super) fn register_native_entry(
    prefix: &str,
    entry_index: u32,
    code: &BytecodeFunction,
    root_slots: usize,
) -> AotRegistrationEntry {
    AotRegistrationEntry {
        native_debug: code.debug.retain_frame,
        entry_index,
        symbol_name: format!("{prefix}{entry_index}"),
        arity: code.arity,
        root_slots,
        leaf_symbol: None,
        leaf_guards: Vec::new(),
    }
}
