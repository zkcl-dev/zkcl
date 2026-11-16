//! Every operation the shared feature table supports must reach a Cranelift emitter.
//!
//! The table answers "is this encoding implemented"; lowering the sample cases shows
//! that the answer is backed by an arm instead of falling through to the
//! unsupported-operation tail.

use cranelift_codegen::ir::{ExtFuncData, ExternalName, Inst, InstBuilder, UserFuncName, Value};
use cranelift_codegen::isa::{self, TargetIsa};
use cranelift_codegen::settings::{self, Flags};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use zkcl_cranelift::{
    LoweringConfig, LoweringError, RuntimeAbiFunction, RuntimeCallEmitter, lower_ssa_function,
};
use zkcl_opt::native::{CRANELIFT_FEATURES, for_op_calls, reset_for_op_calls, supports_lowering};
use zkcl_opt::operation_samples::lowering_cases;
use zkcl_opt::{DeoptState, SsaFunction};

/// Declares the runtime symbol each call names; the test never inspects code.
struct DeclarationEmitter<'a> {
    isa: &'a dyn TargetIsa,
}

impl RuntimeCallEmitter for DeclarationEmitter<'_> {
    fn emit_runtime_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        runtime_function: RuntimeAbiFunction,
        args: &[Value],
    ) -> Result<Inst, LoweringError> {
        let signature = builder.import_signature(zkcl_cranelift::abi::runtime_call_signature(
            runtime_function,
            self.isa.pointer_type(),
            self.isa.default_call_conv(),
        ));
        let reference = builder.import_function(ExtFuncData {
            name: ExternalName::testcase("runtime_call"),
            signature,
            colocated: false,
        });
        Ok(builder.ins().call(reference, args))
    }
}

fn host_isa() -> isa::OwnedTargetIsa {
    isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(Flags::new(settings::builder()))
        .unwrap()
}

/// A guard carries a recovery map by construction; the sample cases also build the
/// absent-map variant, which only checks whether a map is required.
/// Lowering needs the map, so restore the shape real IR always has. The sample cases
/// target 32-bit fixnums; lowering checks that fact against the host layout.
fn well_formed(mut function: SsaFunction, isa: &dyn TargetIsa) -> SsaFunction {
    function.fixnum_bounds = zkcl_target::fixnum_bounds(isa.frontend_config().pointer_width);
    let inst = &mut function.blocks[0].insts[0];
    inst.deopt.get_or_insert(DeoptState {
        pc: 0,
        registers_to_restore: Vec::new(),
    });
    function
}

#[test]
fn every_supported_operation_has_a_cranelift_emitter() {
    let isa = host_isa();
    let mut emitter = DeclarationEmitter { isa: isa.as_ref() };
    let mut lowered_operations = 0;
    for case in lowering_cases() {
        if !supports_lowering(&case, CRANELIFT_FEATURES) {
            continue;
        }
        let operation = case.blocks[0].insts[0].op.clone();
        let instructions: usize = case.blocks.iter().map(|block| block.insts.len()).sum();
        let case = well_formed(case, isa.as_ref());
        reset_for_op_calls();
        let config = LoweringConfig::new(&case, UserFuncName::default())
            .expect("supported operation produces a lowering config");
        match lower_ssa_function(
            &config,
            isa.as_ref(),
            &mut FunctionBuilderContext::new(),
            &mut emitter,
        ) {
            Ok(_) => {
                assert_eq!(
                    for_op_calls() as usize,
                    instructions,
                    "Cranelift classified the function more than once: {operation:?}"
                );
                lowered_operations += 1;
            }
            Err(LoweringError::UnsupportedOp(report)) => {
                panic!("supported without a Cranelift emitter: {operation:?} ({report})")
            }
            Err(error) => panic!("supported operation failed to lower: {operation:?}: {error}"),
        }
    }
    assert!(
        lowered_operations > 0,
        "no sample operations are supported for Cranelift"
    );
}
