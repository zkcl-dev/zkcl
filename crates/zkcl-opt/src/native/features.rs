//! Shared target-feature checks for backends using the native runtime ABI.

use std::ops::Range;

use crate::{DeoptRegisterValue, DeoptState, SsaFunction, SsaInst, ValueRepr};

use super::runtime_call::{CallRequirement, CallReturn, NativeOp};

/// Operation families that a backend implements in addition to the common ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFeature {
    CheckedFixnums,
    PrimitiveGuards,
    Float,
    I32x4,
    Declarations,
}

/// Feature sets the three native code generators advertise.
pub const CRANELIFT_FEATURES: &[NativeFeature] = &[
    NativeFeature::CheckedFixnums,
    NativeFeature::PrimitiveGuards,
    NativeFeature::Float,
    NativeFeature::I32x4,
];
pub const LLVM_FEATURES: &[NativeFeature] = &[
    NativeFeature::CheckedFixnums,
    NativeFeature::PrimitiveGuards,
    NativeFeature::Float,
    NativeFeature::I32x4,
];
pub const WASM_FEATURES: &[NativeFeature] = &[
    NativeFeature::Declarations,
    NativeFeature::CheckedFixnums,
    NativeFeature::PrimitiveGuards,
    NativeFeature::Float,
];

/// One classification pass over an SSA function, consumed by the target emitters.
///
/// Target capability checks, local or register assignment, and emission all need
/// the same [`NativeOp`] classification. Deriving it once here keeps a function's
/// lowering from walking its instructions again for each consumer, and gives
/// the three backends one sequence to share instead of three independent
/// matches over `SsaOp`.
///
/// The classifications live in one allocation with a block index, so classifying
/// a function costs two allocations regardless of how many blocks it has.
pub struct ClassifiedFunction<'a> {
    function: &'a SsaFunction,
    operations: Vec<NativeOp<'a>>,
    blocks: Vec<Range<usize>>,
}

impl<'a> ClassifiedFunction<'a> {
    /// Classify every instruction exactly once, in block and instruction order.
    pub fn new(function: &'a SsaFunction) -> Self {
        let instructions: usize = function.blocks.iter().map(|block| block.insts.len()).sum();
        let mut operations = Vec::with_capacity(instructions);
        let mut blocks = Vec::with_capacity(function.blocks.len());
        for block in &function.blocks {
            let start = operations.len();
            operations.extend(block.insts.iter().map(|inst| NativeOp::for_op(&inst.op)));
            blocks.push(start..operations.len());
        }
        Self {
            function,
            operations,
            blocks,
        }
    }

    /// Classification of one instruction; block and indices match the function.
    pub fn operation(&self, block: usize, instruction: usize) -> NativeOp<'a> {
        self.operations[self.blocks[block].start + instruction]
    }

    pub fn instructions(&self, block: usize) -> usize {
        self.blocks[block].len()
    }

    /// Whether every parameter representation, operation and recovery value of
    /// this function is supported by `features`.
    pub fn is_supported(&self, features: &[NativeFeature]) -> bool {
        self.function
            .blocks
            .iter()
            .enumerate()
            .all(|(block, body)| {
                body.params
                    .iter()
                    .all(|param| supports_repr(param.repr, features))
                    && body.insts.iter().enumerate().all(|(index, inst)| {
                        supports_instruction(self.operation(block, index), inst, features)
                    })
            })
    }
}

/// Check operations, block parameters and register recovery before emitting the function.
pub fn supports_lowering(function: &SsaFunction, features: &[NativeFeature]) -> bool {
    ClassifiedFunction::new(function).is_supported(features)
}

pub(super) fn supports_repr(repr: ValueRepr, features: &[NativeFeature]) -> bool {
    match repr {
        ValueRepr::TaggedWord | ValueRepr::Bool | ValueRepr::UnboxedI64 | ValueRepr::UnboxedU64 => {
            true
        }
        ValueRepr::UnboxedF32 | ValueRepr::UnboxedF64 => features.contains(&NativeFeature::Float),
        ValueRepr::UnboxedI32x4 => features.contains(&NativeFeature::I32x4),
    }
}

/// Check one instruction from its already derived classification.
pub(super) fn supports_instruction(
    operation: NativeOp<'_>,
    inst: &SsaInst,
    features: &[NativeFeature],
) -> bool {
    let recovery_supported = |state: &DeoptState| {
        state
            .registers_to_restore
            .iter()
            .all(|(_, value)| match value {
                DeoptRegisterValue::UnboxedFloat { .. } => features.contains(&NativeFeature::Float),
                DeoptRegisterValue::UnboxedI32x4 { .. } => features.contains(&NativeFeature::I32x4),
                DeoptRegisterValue::TaggedWord(_)
                | DeoptRegisterValue::UnboxedI64(_)
                | DeoptRegisterValue::ConstImmediate(_) => true,
            })
    };
    match operation {
        NativeOp::Call(call) => {
            let supported = match call.requirement {
                CallRequirement::Always => true,
                CallRequirement::Unassociated { associate } => !associate,
                CallRequirement::CaseHandlers { function_targets } => !function_targets,
                CallRequirement::Requires(feature) => features.contains(&feature),
            };
            supported
                && (!matches!(
                    call.result,
                    CallReturn::ReadCell | CallReturn::LispCall { .. }
                ) || inst.deopt.as_ref().is_none_or(recovery_supported))
        }
        NativeOp::Checkpoint { .. } => inst.deopt.as_ref().is_some_and(recovery_supported),
        NativeOp::Target {
            feature,
            checks_recovery,
        } => {
            feature.is_none_or(|feature| features.contains(&feature))
                && (!checks_recovery || inst.deopt.as_ref().is_some_and(recovery_supported))
        }
        NativeOp::Word(_)
        | NativeOp::StoreUnwindRoot { .. }
        | NativeOp::LoadHandlerConditionRoot => true,
        NativeOp::Unsupported => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FloatFormat, SsaOp, TypeTag, ValueId};

    #[test]
    fn lowering_checks_the_recovery_representation_as_well_as_the_guard() {
        let value = ValueId(0);
        let mut guard = SsaInst {
            id: Some(ValueId(1)),
            op: SsaOp::UnboxFixnumGuard(value),
            ty: TypeTag::Fixnum,
            pc: Some(0),
            deopt: None,
        };
        let checked_integers = &[NativeFeature::CheckedFixnums];
        let supported = |inst: &SsaInst, features: &[NativeFeature]| {
            supports_instruction(NativeOp::for_op(&inst.op), inst, features)
        };
        assert!(!supported(&guard, checked_integers));
        guard.deopt = Some(DeoptState {
            pc: 0,
            registers_to_restore: Vec::new(),
        });
        assert!(supported(&guard, checked_integers));
        guard.deopt = Some(DeoptState {
            pc: 0,
            registers_to_restore: vec![(
                0,
                DeoptRegisterValue::UnboxedFloat {
                    value,
                    format: FloatFormat::F64,
                },
            )],
        });
        assert!(!supported(&guard, checked_integers));
        assert!(supported(
            &guard,
            &[NativeFeature::CheckedFixnums, NativeFeature::Float]
        ));
    }
}
