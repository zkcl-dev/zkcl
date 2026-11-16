use cranelift_codegen::ir::{
    ExtFuncData, ExternalName, Function, Inst, InstBuilder, Opcode, UserFuncName, Value, types,
};
use cranelift_codegen::{isa, settings, verify_function};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use zkcl_bytecode::{Arity, BytecodeFunction, Instruction};
use zkcl_cranelift::{
    LoweringConfig, LoweringError, RuntimeAbiFunction, RuntimeCallEmitter, lower_ssa_function,
};
use zkcl_opt::{
    DeoptRegisterValue, DeoptState, NoSpecializationInputs, SsaFunction, SsaInst, SsaOp, TypeTag,
    lift_code,
};

struct ExternalRuntimeCallEmitter<'a> {
    isa: &'a dyn isa::TargetIsa,
    calls: Vec<(RuntimeAbiFunction, Inst)>,
}

impl RuntimeCallEmitter for ExternalRuntimeCallEmitter<'_> {
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
        let call = builder.ins().call(reference, args);
        self.calls.push((runtime_function, call));
        Ok(call)
    }
}

fn lift(arity: Arity, registers: usize, instructions: Vec<Instruction>) -> SsaFunction {
    let code = BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity,
        registers,
        instructions,
        constants: vec![],
        lambda_list: None,
        captures: vec![],
        notinline: vec![],
        vop_calls: Vec::new(),
        handlers: vec![],
        restarts: vec![],
        keywords: vec![],
        jump_tables: vec![],
        facts: Default::default(),
    };
    lift_code(&code, &NoSpecializationInputs).unwrap()
}

fn test_isa() -> isa::OwnedTargetIsa {
    isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(settings::builder()))
        .unwrap()
}

fn lower(ssa_function: &SsaFunction) -> (Function, Vec<(RuntimeAbiFunction, Inst)>) {
    let isa = test_isa();
    let mut emitter = ExternalRuntimeCallEmitter {
        isa: isa.as_ref(),
        calls: Vec::new(),
    };
    let function = lower_ssa_function(
        &LoweringConfig::new(ssa_function, UserFuncName::default()).unwrap(),
        isa.as_ref(),
        &mut FunctionBuilderContext::new(),
        &mut emitter,
    )
    .unwrap();
    verify_function(&function, isa.as_ref()).unwrap();
    (function, emitter.calls)
}

#[test]
fn lowering_rejects_fixnum_facts_for_a_different_word_layout() {
    let mut ssa = lift(Arity::ZERO, 0, vec![]);
    let expected = ssa.fixnum_bounds;
    let actual = (-1024, 1023);
    ssa.fixnum_bounds = actual;
    let config = LoweringConfig::new(&ssa, UserFuncName::default()).unwrap();
    let isa = test_isa();
    let mut emitter = ExternalRuntimeCallEmitter {
        isa: isa.as_ref(),
        calls: Vec::new(),
    };
    let error = lower_ssa_function(
        &config,
        isa.as_ref(),
        &mut FunctionBuilderContext::new(),
        &mut emitter,
    )
    .unwrap_err();
    assert!(
        matches!(error, LoweringError::FixnumBounds { expected: target, actual: input }
        if (target, input) == (expected, actual))
    );
    assert!(emitter.calls.is_empty());
}

#[test]
fn loop_backedge_polls_cancellation_atomically() {
    let (function, _) = lower(&lift(Arity::ZERO, 0, vec![Instruction::Jump(0)]));
    assert!(
        function
            .layout
            .blocks()
            .flat_map(|block| function.layout.block_insts(block))
            .any(|inst| {
                function.dfg.insts[inst].opcode() == Opcode::AtomicLoad
                    && function.dfg.value_type(function.dfg.first_result(inst)) == types::I8
            })
    );
}

#[test]
fn saved_values_calls_keep_the_argument_rooted() {
    let (function, calls) = lower(&lift(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        1,
        vec![
            Instruction::SaveValues,
            Instruction::AppendValues,
            Instruction::RestoreValues,
            Instruction::SetValues(0..1),
        ],
    ));
    let mut relocated = None;
    for runtime_function in [
        RuntimeAbiFunction::SaveValues,
        RuntimeAbiFunction::AppendValues,
        RuntimeAbiFunction::RestoreValues,
    ] {
        let &(_, call) = calls
            .iter()
            .find(|(name, _)| *name == runtime_function)
            .unwrap();
        let spill = function.layout.prev_inst(call).unwrap();
        let reload = function.layout.next_inst(call).unwrap();
        assert_eq!(
            function.dfg.insts[spill].opcode(),
            Opcode::Store,
            "{runtime_function:?}"
        );
        assert_eq!(
            function.dfg.insts[reload].opcode(),
            Opcode::Load,
            "{runtime_function:?}"
        );
        assert_eq!(
            function.dfg.inst_args(spill)[1],
            function.dfg.inst_args(reload)[0]
        );
        assert_eq!(
            function.dfg.insts[spill].load_store_offset(),
            function.dfg.insts[reload].load_store_offset(),
            "{runtime_function:?}"
        );
        if let Some(previous) = relocated {
            assert_eq!(
                function.dfg.inst_args(spill)[0],
                previous,
                "{runtime_function:?}"
            );
        }
        relocated = Some(function.dfg.first_result(reload));
    }
}

#[test]
fn deoptimization_scratch_fits_the_largest_restore_set() {
    let mut ssa = lift(
        Arity {
            required: 5,
            maximum: Some(5),
        },
        2_048,
        vec![Instruction::SetValues(0..1)],
    );
    let mut guards = Vec::new();
    for (pc, count) in [1, 5].into_iter().enumerate() {
        let op = SsaOp::UnboxFixnumGuard(ssa.entry_params[0]);
        let registers_to_restore = ssa.entry_params[..count]
            .iter()
            .enumerate()
            .map(|(reg, &value)| (reg as u32, DeoptRegisterValue::TaggedWord(value)))
            .collect();
        guards.push(SsaInst {
            id: Some(ssa.fresh_value()),

            op,
            ty: TypeTag::Fixnum,
            pc: Some(pc),
            deopt: Some(DeoptState {
                pc,
                registers_to_restore,
            }),
        });
    }
    ssa.blocks[0].insts.splice(..0, guards);

    let (function, calls) = lower(&ssa);
    assert_eq!(
        calls
            .iter()
            .filter(|(name, _)| *name == RuntimeAbiFunction::DeoptToVm)
            .count(),
        2,
    );
    let sizes: Vec<_> = function
        .sized_stack_slots
        .values()
        .map(|slot| slot.size)
        .collect();
    assert!(sizes.contains(&(5 * 4)));
    assert_eq!(sizes.into_iter().max(), Some(5 * 8));
}
