//! Cranelift representation of the native runtime calling convention.

use cranelift_codegen::ir::{AbiParam, Signature, Type, types};
use cranelift_codegen::isa::CallConv;
pub use zkcl_opt::native::{
    FIXNUM_SHIFT, JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_UNWIND,
    JIT_RETURN_TAG_VALUES, RuntimeAbiFunction, RuntimeAbiType, TAG_FIXNUM, TAG_MASK, WORD_NIL,
    WORD_T, WORD_UNBOUND, jit_call_context_offsets, jit_return_layout,
};

pub(crate) fn abi_param(ty: RuntimeAbiType, ptr_ty: Type) -> AbiParam {
    match ty {
        RuntimeAbiType::Pointer | RuntimeAbiType::Word => AbiParam::new(ptr_ty),
        RuntimeAbiType::Bool => AbiParam::new(types::I8).uext(),
        RuntimeAbiType::I64 => AbiParam::new(types::I64),
    }
}

/// Cranelift signature for a runtime function on the selected target.
pub fn runtime_call_signature(
    runtime_function: RuntimeAbiFunction,
    ptr_ty: Type,
    call_conv: CallConv,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.extend(
        runtime_function
            .parameter_types()
            .iter()
            .map(|&ty| abi_param(ty, ptr_ty)),
    );
    sig.returns.extend(
        runtime_function
            .return_type()
            .map(|ty| abi_param(ty, ptr_ty)),
    );
    sig
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::ArgumentExtension;

    #[test]
    fn runtime_bool_parameters_and_results_are_zero_extended() {
        let signature = runtime_call_signature(
            RuntimeAbiFunction::WriteFloatVector,
            types::I64,
            CallConv::AppleAarch64,
        );
        let flag = signature.params.last().unwrap();
        let [result] = &signature.returns[..] else {
            panic!("expected a boolean result")
        };
        for boolean in [flag, result] {
            assert_eq!(boolean.value_type, types::I8);
            assert_eq!(boolean.extension, ArgumentExtension::Uext);
        }
    }
}
