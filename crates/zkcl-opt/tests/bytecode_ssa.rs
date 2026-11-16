use rustc_hash::FxHashMap;
use zkcl_bytecode::{
    Arity, BytecodeFunction, Callee, CaptureKind, Handler, HandlerTable, HandlerTarget, Immediate,
    Instruction, JumpTable, KeywordTable, RestartSpec, RestartTable, RestartTarget, Test,
};
use zkcl_opt::{
    AliasDomain, CallSiteProfile, CmpOp, DeoptRegisterValue, FIXNUM_MAX, FloatFormat,
    GuardedPrimitive, NoSpecializationInputs, NumericOperand, RootSlot, SpecializationInputs,
    SsaOp, Terminator, TypeTag, ValueRepr, compute_root_layout, lift_code, optimize,
};

struct SpecializationInputTables {
    fixnum_bounds: (i64, i64),
    call_site_profiles: FxHashMap<usize, CallSiteProfile>,
    symbols: FxHashMap<usize, (&'static str, &'static str)>,
}

impl SpecializationInputTables {
    fn new() -> Self {
        Self {
            fixnum_bounds: (zkcl_opt::FIXNUM_MIN, FIXNUM_MAX),
            call_site_profiles: FxHashMap::default(),
            symbols: FxHashMap::default(),
        }
    }
}

impl SpecializationInputs for SpecializationInputTables {
    fn fixnum_bounds(&self) -> (i64, i64) {
        self.fixnum_bounds
    }

    fn call_site_profile(&self, pc: usize) -> CallSiteProfile {
        self.call_site_profiles
            .get(&pc)
            .copied()
            .unwrap_or(CallSiteProfile::Unprofiled)
    }

    fn named_symbol(&self, const_idx: usize) -> Option<(&str, &str)> {
        self.symbols.get(&const_idx).copied()
    }
}

fn code_without_side_tables(
    arity: Arity,
    registers: usize,
    instructions: Vec<Instruction>,
) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity,
        registers,
        instructions,
        constants: Vec::new(),
        lambda_list: None,
        captures: Vec::new(),
        notinline: Vec::new(),
        vop_calls: Vec::new(),
        handlers: Vec::new(),
        restarts: Vec::new(),
        keywords: Vec::new(),
        jump_tables: Vec::new(),
        facts: Default::default(),
    }
}

#[test]
fn unused_keyword_read_preserves_its_runtime_error() {
    let mut code = code_without_side_tables(
        Arity {
            required: 0,
            maximum: None,
        },
        1,
        vec![
            Instruction::CheckKeywords { start: 0, table: 0 },
            Instruction::ReleaseArguments,
            Instruction::KeywordArgument(0, 0),
            Instruction::SetValues(0..0),
        ],
    );
    code.constants = vec![0, 1];
    code.keywords.push(KeywordTable {
        names: vec![0],
        allow_other_keys: false,
        override_key: 1,
    });

    let mut function = lift_code(&code, &NoSpecializationInputs).unwrap();
    optimize(&mut function, &NoSpecializationInputs);
    assert!(
        function
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .any(|inst| {
                matches!(inst.op, SsaOp::KeywordArgument(0)) && inst.op.effects().may_unwind()
            })
    );
}

#[test]
fn profiled_float_loop_records_unboxed_accumulators_for_deoptimization() {
    let call = |symbol, out, args| Instruction::Call {
        callee: Callee::Named(symbol),
        out: Some(out),
        args,
        at: 0,
        associate: false,
        notinline: false,
    };
    let mut code = code_without_side_tables(
        Arity {
            required: 4,
            maximum: Some(4),
        },
        9,
        vec![
            Instruction::Copy(1, 1),
            Instruction::Load(4, Immediate::Fixnum(0)),
            Instruction::Load(5, Immediate::Fixnum(1)),
            Instruction::Copy(6, 4),
            Instruction::Copy(7, 0),
            call(0, 8, 6..8),
            Instruction::Unless(8, 16),
            Instruction::Copy(6, 1),
            Instruction::Copy(7, 2),
            call(1, 6, 6..8),
            Instruction::Copy(7, 3),
            call(2, 1, 6..8),
            Instruction::Copy(6, 4),
            Instruction::Copy(7, 5),
            call(2, 4, 6..8),
            Instruction::Jump(3),
            Instruction::SetValues(1..2),
        ],
    );
    code.constants = vec![0, 1, 2, 3];
    for (operand, format, seed) in [
        (
            NumericOperand::F32,
            FloatFormat::F32,
            Instruction::Copy(1, 1),
        ),
        (
            NumericOperand::F64,
            FloatFormat::F64,
            Instruction::Constant(1, 3),
        ),
    ] {
        code.instructions[0] = seed;
        let mut inputs = SpecializationInputTables::new();
        inputs
            .symbols
            .extend([(0, ("CL", "<")), (1, ("CL", "*")), (2, ("CL", "+"))]);
        inputs.call_site_profiles.extend([
            (5, CallSiteProfile::FixnumArithmetic),
            (
                9,
                CallSiteProfile::FloatArithmetic {
                    lhs: operand,
                    rhs: operand,
                },
            ),
            (
                11,
                CallSiteProfile::FloatArithmetic {
                    lhs: operand,
                    rhs: operand,
                },
            ),
            (14, CallSiteProfile::FixnumArithmetic),
        ]);
        let mut func = lift_code(&code, &inputs).unwrap();
        optimize(&mut func, &inputs);
        let param = func
            .blocks
            .iter()
            .flat_map(|b| &b.params)
            .find(|p| p.origin_reg == Some(1) && p.repr == format.repr())
            .expect("loop carries the accumulator in its observed format");
        let metadata = func.value_metadata();
        let layout = compute_root_layout(&func);
        for block in &func.blocks {
            block.terminator.visit_targets(|edge| {
                for (arg, param) in edge
                    .args
                    .iter()
                    .zip(&func.block(edge.block).unwrap().params)
                {
                    assert_eq!(metadata[arg].0, param.repr);
                }
            });
        }
        let insts = || func.blocks.iter().flat_map(|b| &b.insts);
        assert_eq!(
            insts()
                .filter(|i| matches!(i.op, SsaOp::BoxFloat { .. }))
                .count(),
            1
        );
        assert!(!layout.value_slots.contains_key(&param.value));
        for pc in [9, 11] {
            let arithmetic = insts()
                .find(|i| i.pc == Some(pc) && matches!(i.op, SsaOp::FloatArithChecked { .. }))
                .unwrap();
            let state = arithmetic.deopt.as_ref().unwrap();
            assert_eq!(state.pc, pc);
            assert!(state.registers_to_restore.iter().any(|&(_, value)| {
                matches!(value, DeoptRegisterValue::UnboxedFloat { format: found, .. } if found == format)
            }));
        }
        let entry_seed = if format == FloatFormat::F32 {
            func.entry_params[1]
        } else {
            func.block(func.entry)
                .unwrap()
                .insts
                .iter()
                .find_map(|inst| {
                    matches!(inst.op, SsaOp::LoadConst(3))
                        .then_some(inst.id)
                        .flatten()
                })
                .expect("constant seed is loaded before execution")
        };
        assert!(insts().any(|i| {
            matches!(i.op, SsaOp::UnboxFloatGuard { value, .. } if value == entry_seed)
                && i.deopt.as_ref().is_some_and(|d| d.pc == 0)
        }));
    }
}

#[test]
fn proven_float_typep_folds_only_a_canonical_inlinable_check() {
    for (package, expected, notinline, folded) in [
        ("CL", "SINGLE-FLOAT", false, true),
        ("CL", "DOUBLE-FLOAT", false, true),
        ("CL", "SINGLE-FLOAT", true, false),
        ("CL-USER", "SINGLE-FLOAT", false, false),
    ] {
        let call = |symbol, out, args, notinline| Instruction::Call {
            callee: Callee::Named(symbol),
            out: Some(out),
            args,
            at: 0,
            associate: false,
            notinline,
        };
        let mut code = code_without_side_tables(
            Arity {
                required: 2,
                maximum: Some(2),
            },
            6,
            vec![
                Instruction::Constant(3, 2),
                Instruction::Copy(2, 0),
                call(1, 4, 2..4, false),
                call(0, 2, 0..2, false),
                call(1, 4, 2..4, notinline),
                Instruction::Unless(4, 8),
                Instruction::Load(5, Immediate::True),
                Instruction::Jump(9),
                Instruction::Load(5, Immediate::Nil),
                Instruction::SetValues(5..6),
            ],
        );
        code.constants = vec![0, 1, 2];
        let mut inputs = SpecializationInputTables::new();
        inputs.symbols.extend([
            (0, ("CL", "+")),
            (1, ("CL", "TYPEP")),
            (2, (package, expected)),
        ]);
        inputs.call_site_profiles.insert(
            3,
            CallSiteProfile::FloatArithmetic {
                lhs: NumericOperand::F32,
                rhs: NumericOperand::F32,
            },
        );
        let mut func = lift_code(&code, &inputs).unwrap();
        optimize(&mut func, &inputs);
        let instructions = || func.blocks.iter().flat_map(|block| &block.insts);
        assert_eq!(
            instructions().any(|inst| matches!(inst.op, SsaOp::BoxFloat { .. })),
            !folded
        );
        assert_eq!(
            instructions()
                .filter(|inst| matches!(inst.op, SsaOp::CallNamed { symbol: 1, .. }))
                .count(),
            if folded { 1 } else { 2 }
        );
        assert!(instructions().any(|inst| {
            inst.pc == Some(2) && matches!(inst.op, SsaOp::CallNamed { symbol: 1, .. })
        }));
        if folded {
            let state = instructions()
                .find_map(|inst| {
                    if matches!(
                        inst.op,
                        SsaOp::GuardPrimitiveBinding {
                            primitive: GuardedPrimitive::Typep,
                            ..
                        }
                    ) {
                        inst.deopt.as_ref()
                    } else {
                        None
                    }
                })
                .expect("TYPEP binding check remains");
            assert_eq!(state.pc, 4);
            assert!(state.registers_to_restore.iter().any(|&(_, k)| {
                matches!(
                    k,
                    DeoptRegisterValue::UnboxedFloat {
                        format: FloatFormat::F32,
                        ..
                    }
                )
            }));
            let expected = if expected == "SINGLE-FLOAT" {
                Immediate::True
            } else {
                Immediate::Nil
            };
            assert!(
                instructions()
                    .any(|inst| matches!(inst.op, SsaOp::ConstWord(imm) if imm == expected))
            );
            assert!(
                !func
                    .blocks
                    .iter()
                    .any(|block| matches!(block.terminator, Terminator::BranchTruthy { .. }))
            );
        }
    }
}

#[test]
fn type_tag_lattice_laws() {
    use TypeTag::*;
    let leaves = [Nil, True, Fixnum, Float, Char, Cons, Symbol, Function];

    assert_eq!(Bottom.join(Fixnum), Fixnum);
    assert_eq!(Fixnum.join(Bottom), Fixnum);
    assert_eq!(Fixnum.join(Float), Any);
    assert_eq!(Any.join(Fixnum), Any);
    assert_eq!(Top.join(Any), Top);

    assert_eq!(Top.meet(Fixnum), Fixnum);
    assert_eq!(Any.meet(Fixnum), Fixnum);
    assert_eq!(Fixnum.meet(Float), Bottom);
    assert_eq!(Bottom.meet(Any), Bottom);

    for &a in &leaves {
        assert_eq!(a.join(a), a);
        assert_eq!(a.meet(a), a);
        assert!(Bottom.is_subtype_of(a));
        assert!(a.is_subtype_of(Any));
        assert!(a.is_subtype_of(Top));
        assert_eq!(a.is_definitely_truthy(), a != Nil);
    }
    assert!(Nil.is_definitely_nil());
    assert!(!Fixnum.is_definitely_nil());
}

#[test]
fn ssa_lift_resolves_control_table_targets() {
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(2),
        },
        4,
        vec![
            Instruction::EnterHandlers { table: 0 },
            Instruction::EnterRestarts { table: 0, at: 1 },
            Instruction::EnterProtect {
                cleanup: 5,
                clear: 2,
            },
            Instruction::Load(1, Immediate::Fixnum(7)),
            Instruction::BeginCleanup,
            Instruction::EndCleanup,
            Instruction::LeaveRestarts,
            Instruction::LeaveHandlers,
            Instruction::SetValues(1..2),
        ],
    );
    code.constants = vec![10, 11, 12];
    code.captures = vec![CaptureKind::Value, CaptureKind::MutableCell];
    code.notinline = vec![2];
    code.handlers = vec![HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 8,
                condition: 2,
                clear: 3,
            },
        }],
    }];
    code.restarts = vec![RestartTable {
        restarts: vec![RestartSpec {
            name: 0,
            object: 1,
            test: None,
            interactive: None,
            report: None,
            target: RestartTarget::Case { pc: 7, clear: 2 },
        }],
    }];
    code.keywords = vec![KeywordTable {
        names: vec![1],
        allow_other_keys: false,
        override_key: 0,
    }];
    code.jump_tables = vec![JumpTable { targets: vec![8] }];

    let func = lift_code(&code, &NoSpecializationInputs).expect("lift succeeds");
    assert_eq!(func.arity, code.arity);
    assert_eq!(func.constant_count, code.constants.len());
    assert_eq!(func.entry_params.len(), 1);
    let cleanup = func.pc_to_block[&5];
    let handler = func.pc_to_block[&8];
    let entry = func.block(func.entry).unwrap();
    let recovery_destination = |target| match &func.block(target).unwrap().terminator {
        Terminator::Jump(edge) => edge.block,
        _ => panic!("recovery entry must branch to its bytecode destination"),
    };
    assert!(entry.insts.iter().any(|inst| {
        matches!(inst.op, SsaOp::EnterProtect { cleanup: target, .. } if recovery_destination(target) == cleanup)
    }));
    assert!(entry.insts.iter().any(|inst| {
        matches!(&inst.op, SsaOp::EnterHandlers { targets, .. } if targets.len() == 1 && recovery_destination(targets[0]) == handler)
    }));
}

fn code_that_installs_a_restart_report() -> BytecodeFunction {
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        3,
        vec![
            Instruction::ReadCell {
                out: 2,
                symbol: 0,
                function: true,
                at: 0,
            },
            Instruction::EnterRestarts { table: 0, at: 0 },
            Instruction::LeaveRestarts,
        ],
    );
    code.constants.push(0);
    code.restarts.push(RestartTable {
        restarts: vec![RestartSpec {
            name: 0,
            object: 1,
            test: None,
            interactive: None,
            report: Some(0),
            target: RestartTarget::Function(2),
        }],
    });

    code
}

#[test]
fn deoptimization_state_retains_the_restart_report_register() {
    let code = code_that_installs_a_restart_report();
    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let deopt = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .find_map(|inst| inst.deopt.as_ref())
        .unwrap();
    assert_eq!(deopt.pc, 0);
    assert!(deopt.registers_to_restore.iter().any(|&(reg, _)| reg == 0));
}

#[test]
fn restart_report_register_must_fit_the_frame() {
    let mut code = code_that_installs_a_restart_report();
    code.restarts[0].restarts[0].report = Some(3);
    assert!(matches!(
        lift_code(&code, &NoSpecializationInputs),
        Err(zkcl_opt::BytecodeLiftError::RegisterOutOfBounds {
            reg: 3,
            registers: 3,
            ..
        })
    ));
}

#[test]
fn local_go_keeps_the_token_check_when_an_unwind_can_replace_it() {
    for (tagbody_clear, condition, handler_clear, direct) in [
        (1, 1, 1, true),
        (1, 0, 1, false),
        (1, 1, 0, false),
        (0, 1, 1, false),
    ] {
        let mut code = code_without_side_tables(
            Arity::ZERO,
            2,
            vec![
                Instruction::ExitToken(0, 0),
                Instruction::EnterTagbody {
                    token: 0,
                    jumps: 0,
                    clear: tagbody_clear,
                },
                Instruction::EnterHandlers { table: 0 },
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: None,
                    args: 0..0,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::LeaveHandlers,
                Instruction::Jump(7),
                Instruction::Go {
                    token: 0,
                    label: 0,
                    at: 0,
                },
                Instruction::LeaveTarget,
            ],
        );
        code.constants.push(0);
        code.jump_tables.push(JumpTable { targets: vec![7] });
        code.handlers.push(HandlerTable {
            handlers: vec![Handler {
                test: Test::Any,
                target: HandlerTarget::Case {
                    pc: 6,
                    condition,
                    clear: handler_clear,
                },
            }],
        });

        let function = lift_code(&code, &NoSpecializationInputs).unwrap();
        let go = &function.block(function.pc_to_block[&6]).unwrap().terminator;
        if direct {
            assert!(
                matches!(go, Terminator::Jump(target) if target.block == function.pc_to_block[&7])
            );
        } else {
            assert!(matches!(
                go,
                Terminator::Unwind(zkcl_opt::UnwindOp::Go { .. })
            ));
        }
    }
}

#[test]
fn nested_tagbodies_track_local_go_and_outer_token_escape_separately() {
    for outer_token_escapes in [false, true] {
        let mut code = code_without_side_tables(
            Arity::ZERO,
            3,
            vec![
                Instruction::ExitToken(0, 0),
                Instruction::EnterTagbody {
                    token: 0,
                    jumps: 0,
                    clear: 1,
                },
                Instruction::ExitToken(1, 1),
                Instruction::EnterTagbody {
                    token: 1,
                    jumps: 1,
                    clear: 2,
                },
                if outer_token_escapes {
                    Instruction::Copy(2, 0)
                } else {
                    Instruction::Load(2, Immediate::Nil)
                },
                Instruction::Unless(2, 7),
                Instruction::Go {
                    token: 1,
                    label: 0,
                    at: 0,
                },
                Instruction::Go {
                    token: 0,
                    label: 0,
                    at: 0,
                },
                Instruction::LeaveTarget,
                Instruction::Go {
                    token: 0,
                    label: 0,
                    at: 0,
                },
                Instruction::LeaveTarget,
            ],
        );
        code.jump_tables = vec![
            JumpTable { targets: vec![10] },
            JumpTable { targets: vec![8] },
        ];
        let function = lift_code(&code, &NoSpecializationInputs).unwrap();
        let terminator = |pc| {
            &function
                .blocks
                .iter()
                .find(|block| block.terminator_pc == pc)
                .unwrap()
                .terminator
        };
        assert!(
            matches!(terminator(6), Terminator::Jump(edge) if edge.block == function.pc_to_block[&8])
        );
        assert!(matches!(
            terminator(7),
            Terminator::Unwind(zkcl_opt::UnwindOp::Go { .. })
        ));
        if outer_token_escapes {
            assert!(matches!(
                terminator(9),
                Terminator::Unwind(zkcl_opt::UnwindOp::Go { .. })
            ));
        } else {
            assert!(
                matches!(terminator(9), Terminator::Jump(edge) if edge.block == function.pc_to_block[&10])
            );
        }
    }
}

#[test]
fn unwind_states_refer_to_their_own_tagbody_label() {
    let mut code = code_without_side_tables(
        Arity {
            required: 0,
            maximum: Some(0),
        },
        2,
        vec![
            Instruction::ExitToken(0, 0),
            Instruction::EnterTagbody {
                token: 0,
                jumps: 0,
                clear: 1,
            },
            Instruction::ExitToken(1, 1),
            Instruction::EnterBlock {
                token: 1,
                resume: 5,
                clear: 2,
            },
            Instruction::LeaveTarget,
            Instruction::LeaveTarget,
            Instruction::SetValues(0..1),
        ],
    );
    code.jump_tables.push(JumpTable {
        targets: vec![2, 5],
    });
    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let table = zkcl_opt::build_unwind_table(&function, &zkcl_opt::compute_rpo(&function));
    let labels = [function.pc_to_block[&2], function.pc_to_block[&5]];
    let (block, index) = function
        .blocks
        .iter()
        .find_map(|block| {
            block
                .insts
                .iter()
                .position(|inst| matches!(inst.op, SsaOp::EnterTagbody { .. }))
                .map(|index| (block.id, index))
        })
        .unwrap();
    let base = table.tagbody_base(block, index);
    let targets = table.targets();
    for (&recovery, label) in targets[base..base + labels.len()].iter().zip(labels) {
        let Terminator::Jump(edge) = &function.block(recovery).unwrap().terminator else {
            panic!("recovery entry must branch to its tagbody label");
        };
        assert_eq!(edge.block, label);
        assert_eq!(targets[table.state_for(recovery)], recovery);
    }
}

#[test]
fn mixed_arity_branches_materialize_values_at_the_join() {
    let code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        2,
        vec![
            Instruction::Unless(0, 3),
            Instruction::SetValues(0..1),
            Instruction::Jump(4),
            Instruction::SetValues(0..2),
        ],
    );

    let mut func = lift_code(&code, &NoSpecializationInputs).unwrap();
    optimize(&mut func, &NoSpecializationInputs);
    let entry = func.block(func.entry).unwrap();
    let Terminator::BranchTruthy { truthy, falsy, .. } = &entry.terminator else {
        panic!("argument should still select the return path");
    };
    for (edge, arity) in [(truthy, 1), (falsy, 2)] {
        assert!(
            func.block(edge.block).unwrap().insts.iter().any(|inst| {
                matches!(&inst.op, SsaOp::SetValues(values) if values.len() == arity)
            })
        );
    }
    let exit = func.block(func.pc_to_block[&4]).unwrap();
    assert_eq!(exit.terminator, Terminator::ReturnDynamicValues);
}

#[test]
fn return_from_preserves_values_at_the_block_resume() {
    let code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        2,
        vec![
            Instruction::ExitToken(1, 0),
            Instruction::EnterBlock {
                token: 1,
                resume: 8,
                clear: 1,
            },
            Instruction::Unless(0, 5),
            Instruction::SetValues(0..1),
            Instruction::ReturnFrom { token: 1, at: 3 },
            Instruction::Load(0, Immediate::True),
            Instruction::LeaveTarget,
            Instruction::Jump(8),
            Instruction::SetValues(0..1),
        ],
    );

    let mut func = lift_code(&code, &NoSpecializationInputs).unwrap();
    let resume = func.pc_to_block[&8];
    assert!(
        func.block(resume)
            .unwrap()
            .params
            .iter()
            .any(|p| p.origin_reg == Some(0))
    );
    optimize(&mut func, &NoSpecializationInputs);
    assert!(
        func.block(resume)
            .unwrap()
            .params
            .iter()
            .any(|p| p.origin_reg == Some(0))
    );
    let unwind = func.block(func.pc_to_block[&3]).unwrap();
    assert!(matches!(unwind.terminator, Terminator::Unwind(_)));
    assert!(unwind.insts.iter().any(|inst| matches!(
        &inst.op,
        SsaOp::SetValues(values) if values.len() == 1
    )));
}

#[test]
fn cleanup_restores_the_protected_values() {
    let code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        2,
        vec![
            Instruction::EnterProtect {
                cleanup: 3,
                clear: 2,
            },
            Instruction::SetValues(0..1),
            Instruction::BeginCleanup,
            Instruction::SetValues(0..2),
            Instruction::EndCleanup,
        ],
    );

    let mut func = lift_code(&code, &NoSpecializationInputs).unwrap();
    optimize(&mut func, &NoSpecializationInputs);
    let protected = func.block(func.entry).unwrap();
    assert!(protected.insts.iter().any(|inst| matches!(
        &inst.op,
        SsaOp::SetValues(values) if values.len() == 1
    )));
    let cleanup = func.block(func.pc_to_block[&3]).unwrap();
    assert_eq!(cleanup.terminator, Terminator::ReturnDynamicValues);
}

#[test]
fn named_tail_calls_preserve_dynamic_function_lookup() {
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        1,
        vec![Instruction::Call {
            callee: Callee::Named(0),
            out: None,
            args: 0..1,
            at: 0,
            associate: false,
            notinline: false,
        }],
    );
    code.constants = vec![0];

    let mut func = lift_code(&code, &NoSpecializationInputs).unwrap();
    optimize(&mut func, &NoSpecializationInputs);

    assert!(func.blocks.iter().any(|block| {
        block.insts.iter().any(|inst| {
            matches!(
                inst.op,
                SsaOp::CallNamed {
                    symbol: 0,
                    has_result: false,
                    ..
                }
            )
        })
    }));
}

#[test]
fn folding_a_truthy_sum_eliminates_the_untaken_branch() {
    // (let ((x 10) (y 32))
    //   (if (+ x y) ...))
    // Specifically: `r0 = 10`, `r1 = 32`, `r2 = (+ r0 r1)`, `unless r2 -> else`, `then: return 100`, `else: return 200`
    let mut code = code_without_side_tables(
        Arity::ZERO,
        3,
        vec![
            Instruction::Load(0, Immediate::Fixnum(10)), // 0
            Instruction::Load(1, Immediate::Fixnum(32)), // 1
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(2),
                args: 0..2,
                at: 2,
                associate: false,
                notinline: false,
            }, // 2
            Instruction::Unless(2, 6),                   // 3
            Instruction::Load(0, Immediate::Fixnum(100)), // 4
            Instruction::Jump(7),                        // 5
            Instruction::Load(0, Immediate::Fixnum(200)), // 6
            Instruction::SetValues(0..1),                // 7
        ],
    );
    code.constants = vec![0];

    let mut specialization = SpecializationInputTables::new();
    specialization.symbols.insert(0, ("COMMON-LISP", "+"));
    specialization
        .call_site_profiles
        .insert(2, CallSiteProfile::FixnumArithmetic);

    let mut func = lift_code(&code, &specialization).expect("lift succeeds");
    optimize(&mut func, &specialization);

    // The branch was known truthy (42 is a Fixnum), so CFG simplifies to a single block returning 100.
    assert_eq!(func.blocks.len(), 1);
    assert!(matches!(
        func.blocks[0].terminator,
        Terminator::ReturnOne(_)
    ));
    let has_100 = func.blocks[0]
        .insts
        .iter()
        .any(|inst| inst.op == SsaOp::ConstWord(Immediate::Fixnum(100)));
    assert!(has_100);
    let has_200 = func.blocks[0]
        .insts
        .iter()
        .any(|inst| inst.op == SsaOp::ConstWord(Immediate::Fixnum(200)));
    assert!(!has_200);
}

#[test]
fn deoptimization_keeps_arguments_that_alias_the_call_output() {
    let mut code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        3,
        vec![
            Instruction::Load(2, Immediate::True),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(0),
                args: 0..2,
                at: 0,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(0..1),
        ],
    );
    code.constants.push(0);

    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let call = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .find(|inst| matches!(inst.op, SsaOp::CallNamed { .. }))
        .unwrap();
    assert_eq!(
        call.deopt.as_ref().unwrap().registers_to_restore,
        [
            (0, DeoptRegisterValue::TaggedWord(function.entry_params[0])),
            (1, DeoptRegisterValue::TaggedWord(function.entry_params[1])),
        ],
    );
}

#[test]
fn normal_and_handler_edges_keep_their_live_registers_across_bitset_words() {
    let mut code = code_without_side_tables(
        Arity::ZERO,
        66,
        vec![
            Instruction::Load(63, Immediate::Fixnum(1)),
            Instruction::Load(64, Immediate::Fixnum(2)),
            Instruction::Load(65, Immediate::Fixnum(3)),
            Instruction::Jump(4),
            Instruction::Call {
                callee: Callee::Named(0),
                out: None,
                args: 0..0,
                at: 4,
                associate: false,
                notinline: false,
            },
            Instruction::EnterHandlers { table: 0 },
            Instruction::LeaveHandlers,
            Instruction::Jump(8),
            Instruction::SetValues(63..66),
        ],
    );
    code.constants.push(0);
    code.handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 8,
                condition: 64,
                clear: 65,
            },
        }],
    });
    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let source = function.block(function.pc_to_block[&4]).unwrap();
    let state = source
        .insts
        .iter()
        .find_map(|inst| inst.deopt.as_ref())
        .unwrap();
    let registers: Vec<_> = state
        .registers_to_restore
        .iter()
        .map(|&(reg, _)| reg)
        .collect();
    assert_eq!(registers, [63, 64, 65]);

    let recovery = function.block(source.unwind_succs[0]).unwrap();
    assert_eq!(recovery.params.len(), 1);
    assert_eq!(recovery.params[0].origin_reg, Some(63));
}

#[test]
fn profiled_calls_preserve_recovery_state_through_tagbody() {
    let add = |out, args| Instruction::Call {
        callee: Callee::Named(0),
        out: Some(out),
        args,
        at: 0,
        associate: false,
        notinline: false,
    };
    let mut code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        4,
        vec![
            Instruction::ExitToken(3, 0),
            Instruction::EnterTagbody {
                token: 3,
                jumps: 0,
                clear: 4,
            },
            add(2, 0..2),
            Instruction::LeaveTarget,
            add(0, 1..3),
            Instruction::SetValues(0..1),
        ],
    );
    code.constants.push(0);
    code.jump_tables.push(JumpTable { targets: vec![2] });

    let mut specialization = SpecializationInputTables::new();
    specialization.symbols.insert(0, ("COMMON-LISP", "+"));
    specialization.call_site_profiles.extend([
        (2, CallSiteProfile::FixnumArithmetic),
        (4, CallSiteProfile::FixnumArithmetic),
    ]);
    let mut function = lift_code(&code, &specialization).unwrap();
    let call = function
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .find(|inst| inst.pc == Some(2) && matches!(inst.op, SsaOp::CallNamed { .. }))
        .expect("bytecode call inside TAGBODY");
    let recovery = call.deopt.as_ref().expect("state for a call handoff");
    assert_eq!(recovery.pc, 2);
    assert_eq!(
        recovery
            .registers_to_restore
            .iter()
            .map(|&(reg, _)| reg)
            .collect::<Vec<_>>(),
        [0, 1]
    );

    optimize(&mut function, &specialization);
    for pc in [2, 4] {
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .any(|inst| {
                    inst.pc == Some(pc)
                        && matches!(inst.op, SsaOp::AddI64Checked(_, _))
                        && inst.deopt.as_ref().is_some_and(|state| state.pc == pc)
                }),
            "expected a checked addition with a recovery state at bytecode PC {pc}"
        );
    }
}

#[test]
fn variadic_fixnum_subtraction_guards_all_inputs_then_folds_left() {
    let mut code = code_without_side_tables(
        Arity {
            required: 3,
            maximum: Some(3),
        },
        3,
        vec![Instruction::Call {
            callee: Callee::Named(0),
            out: None,
            args: 0..3,
            at: 0,
            associate: false,
            notinline: false,
        }],
    );
    code.constants.push(0);
    let mut specialization = SpecializationInputTables::new();
    specialization.symbols.insert(0, ("CL", "-"));

    let mut func = lift_code(&code, &specialization).unwrap();
    optimize(&mut func, &specialization);
    let entry = func.block(func.entry).unwrap();
    let mut args = Vec::new();
    let mut differences = Vec::new();
    for inst in &entry.insts {
        match inst.op {
            SsaOp::UnboxFixnumGuard(arg) => {
                assert!(differences.is_empty());
                assert_eq!(arg, func.entry_params[args.len()]);
                args.push(inst.id.unwrap());
            }
            SsaOp::SubI64Checked(lhs, rhs) => differences.push((lhs, rhs, inst.id.unwrap())),
            _ => continue,
        }
        assert_eq!(inst.deopt.as_ref().unwrap().pc, 0);
    }
    assert_eq!(args.len(), 3);
    let [(first, second, intermediate), (acc, third, _)] = differences.as_slice() else {
        panic!("expected two checked subtractions");
    };
    assert_eq!([*first, *second, *third], args.as_slice());
    assert_eq!(acc, intermediate);
    assert!(matches!(entry.terminator, Terminator::ReturnOne(_)));
}

#[test]
fn fixnum_overflow_is_not_folded_past_tag_range() {
    for bits in [32, 64] {
        let min = -(1i64 << (bits - 4));
        let max = -min - 1;
        let mut code = code_without_side_tables(
            Arity::ZERO,
            3,
            vec![
                Instruction::Load(0, Immediate::Fixnum(max)), // 0
                Instruction::Load(1, Immediate::Fixnum(1)),   // 1
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: Some(2),
                    args: 0..2,
                    at: 2,
                    associate: false,
                    notinline: false,
                }, // 2
                Instruction::SetValues(2..3),                 // 3
            ],
        );
        code.constants = vec![0];

        let mut specialization = SpecializationInputTables::new();
        specialization.fixnum_bounds = (min, max);
        specialization.symbols.insert(0, ("COMMON-LISP", "+"));
        specialization
            .call_site_profiles
            .insert(2, CallSiteProfile::FixnumArithmetic);

        let mut func = lift_code(&code, &specialization).expect("lift succeeds");
        optimize(&mut func, &specialization);

        // Must retain AddI64Checked with DeoptState because FIXNUM_MAX + 1 overflows tagged fixnum.
        let has_checked_add = func.blocks[0]
            .insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::AddI64Checked(_, _)) && inst.deopt.is_some());
        assert!(has_checked_add);
        code.instructions[0] = Instruction::Load(0, Immediate::Fixnum(max + 1));
        assert!(matches!(
            lift_code(&code, &specialization),
            Err(zkcl_opt::BytecodeLiftError::InvalidFixnum { .. })
        ));
    }
}

#[test]
fn unboxed_loop_values_use_deopt_state_without_gc_roots() {
    // Function (n):
    //   r0 = n (arg 0)
    //   r1 = sum = 0
    //   r2 = i = 0
    // loop (pc 2):
    //   r3 = i, r4 = n
    //   r3 = (< r3 r4)     ; pc 4
    //   unless r3 -> exit  ; pc 5 -> 12
    //   r1 = (+ r1 r2)     ; pc 6
    //   r4 = 1             ; pc 7
    //   r3 = i             ; pc 8
    //   r2 = (+ r3 r4)     ; pc 9
    //   bar(n)             ; pc 10 (safepoint inside loop)
    //   jump loop          ; pc 11 -> 2
    // exit (pc 12):
    //   set-values r1..r2  ; pc 12
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        5,
        vec![
            Instruction::Load(1, Immediate::Fixnum(0)), // 0: sum = 0
            Instruction::Load(2, Immediate::Fixnum(0)), // 1: i = 0
            // loop (pc 2):
            Instruction::Copy(3, 2), // 2: r3 = i
            Instruction::Copy(4, 0), // 3: r4 = n
            Instruction::Call {
                callee: Callee::Named(0), // "<"
                out: Some(3),
                args: 3..5,
                at: 4,
                associate: false,
                notinline: false,
            }, // 4
            Instruction::Unless(3, 12), // 5 -> exit (12)
            Instruction::Call {
                callee: Callee::Named(1), // "+"
                out: Some(1),
                args: 1..3, // r1 (sum), r2 (i)
                at: 6,
                associate: false,
                notinline: false,
            }, // 6: sum += i
            Instruction::Load(4, Immediate::Fixnum(1)), // 7: r4 = 1
            Instruction::Copy(3, 2), // 8: r3 = i
            Instruction::Call {
                callee: Callee::Named(1), // "+"
                out: Some(2),
                args: 3..5, // r3 (i), r4 (1)
                at: 9,
                associate: false,
                notinline: false,
            }, // 9: i += 1
            Instruction::Call {
                callee: Callee::Named(2), // external hook/safepoint `bar(n)` using r0
                out: None,
                args: 0..1,
                at: 10,
                associate: false,
                notinline: false,
            }, // 10: safepoint inside loop while `sum`, `i`, `n` are live!
            Instruction::Jump(2),    // 11
            Instruction::SetValues(1..2), // 12
        ],
    );
    code.constants = vec![0, 1, 2];

    let mut specialization = SpecializationInputTables::new();
    specialization.symbols.insert(0, ("COMMON-LISP", "<"));
    specialization.symbols.insert(1, ("COMMON-LISP", "+"));
    specialization.symbols.insert(2, ("CL-USER", "BAR"));
    specialization
        .call_site_profiles
        .insert(4, CallSiteProfile::FixnumArithmetic);
    specialization
        .call_site_profiles
        .insert(6, CallSiteProfile::FixnumArithmetic);
    specialization
        .call_site_profiles
        .insert(9, CallSiteProfile::FixnumArithmetic);

    let mut func = lift_code(&code, &specialization).expect("lift succeeds");
    optimize(&mut func, &specialization);

    // Loop header block should have UnboxedI64 block params for `sum` and `i` and BranchBool(CmpI64).
    let loop_block = func
        .blocks
        .iter()
        .find(|b| matches!(b.terminator, Terminator::BranchBool { .. }))
        .expect("loop block uses BranchBool");
    assert!(
        loop_block
            .params
            .iter()
            .filter(|p| p.repr == ValueRepr::UnboxedI64)
            .count()
            >= 2,
        "expected unboxed loop scalars, got {:?}",
        loop_block.params
    );
    assert!(
        loop_block
            .insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::CmpI64 { op: CmpOp::Lt, .. }))
    );

    // Backedges carry raw loop scalars, while publishing the sum to VALUES requires a word.
    let metadata = func.value_metadata();
    for block in &func.blocks {
        block.terminator.visit_targets(|edge| {
            let params = &func.block(edge.block).unwrap().params;
            assert_eq!(edge.args.len(), params.len());
            for (arg, param) in edge.args.iter().zip(params) {
                assert_eq!(metadata[arg].0, param.repr);
            }
        });
    }
    let sum = func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .find_map(|inst| {
            if let SsaOp::SetValues(values) = &inst.op
                && let [sum] = values.as_slice()
            {
                Some(sum)
            } else {
                None
            }
        })
        .expect("exit publishes the sum");
    assert_eq!(metadata[sum].0, ValueRepr::TaggedWord);

    // Deopt state inside the loop must reference `UnboxedI64` for unboxed scalars and `ConstImmediate` for constants.
    let has_unboxed_deopt = func.blocks.iter().any(|b| {
        b.insts.iter().any(|inst| {
            inst.deopt.as_ref().is_some_and(|d| {
                d.registers_to_restore
                    .iter()
                    .any(|(_, k)| matches!(k, DeoptRegisterValue::UnboxedI64(_)))
            })
        })
    });
    assert!(has_unboxed_deopt);

    // Compute root layout: code_slot=0, closure_slot=1, arg(n)=2, and unboxed i64 loop phis must NOT occupy RootSlots!
    let layout = compute_root_layout(&func);
    assert_eq!(layout.code_slot, RootSlot(0));
    assert_eq!(layout.closure_slot, RootSlot(1));
    assert_eq!(layout.arg_slots.len(), 1);
    assert_eq!(layout.arg_slots[0].1, RootSlot(2));
    for p in &loop_block.params {
        if p.repr == ValueRepr::UnboxedI64 {
            assert!(
                !layout.value_slots.contains_key(&p.value),
                "unboxed i64 {:?} must not occupy a RootSlot",
                p.value
            );
        }
    }
    assert!(
        !layout.safepoint_roots.is_empty(),
        "expected safepoint at external CallNamed"
    );
}

fn optimize_profiled_calls() -> zkcl_opt::SsaFunction {
    // 0: r0 = 1
    // 1: r1 = 2
    // 2: Call "+" (notinline: true) -> r2
    // 3: WriteCell(sym 1, r2)
    // 4: Call "FOO" -> r2
    // 5: Load(3, 999) ; dead pure constant
    // 6: SetValues(2..3)
    let mut code = code_without_side_tables(
        Arity::ZERO,
        4,
        vec![
            Instruction::Load(0, Immediate::Fixnum(1)),
            Instruction::Load(1, Immediate::Fixnum(2)),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(2),
                args: 0..2,
                at: 2,
                associate: false,
                notinline: true,
            },
            Instruction::WriteCell { symbol: 1, src: 2 },
            Instruction::Call {
                callee: Callee::Named(2),
                out: Some(2),
                args: 0..1,
                at: 4,
                associate: false,
                notinline: false,
            },
            Instruction::Load(3, Immediate::Fixnum(999)),
            Instruction::SetValues(2..3),
        ],
    );
    code.constants = vec![0, 1, 2];

    let mut specialization = SpecializationInputTables::new();
    specialization.symbols.insert(0, ("COMMON-LISP", "+"));
    specialization.symbols.insert(1, ("CL-USER", "*G*"));
    specialization.symbols.insert(2, ("CL-USER", "FOO"));
    specialization
        .call_site_profiles
        .insert(2, CallSiteProfile::FixnumArithmetic);
    let mut func = lift_code(&code, &specialization).expect("lift succeeds");
    optimize(&mut func, &specialization);

    func
}

#[test]
fn notinline_keeps_a_profiled_addition_as_a_named_call() {
    let func = optimize_profiled_calls();
    let insts = &func.blocks[0].insts;
    assert!(insts.iter().any(|i| matches!(
        i.op,
        SsaOp::CallNamed {
            symbol: 0,
            notinline: true,
            ..
        }
    )));
    assert!(
        !insts
            .iter()
            .any(|i| matches!(i.op, SsaOp::AddI64Checked(_, _)))
    );
}

#[test]
fn optimization_retains_a_variable_cell_write() {
    let func = optimize_profiled_calls();
    let insts = &func.blocks[0].insts;
    assert!(insts.iter().any(|i| {
        matches!(i.op, SsaOp::WriteCell { symbol: 1, .. })
            && i.op.effects().writes.contains(AliasDomain::SYMBOL_VALUE)
    }));
}

#[test]
fn optimization_removes_an_unused_constant() {
    let func = optimize_profiled_calls();
    let insts = &func.blocks[0].insts;
    assert!(
        !insts
            .iter()
            .any(|i| i.op == SsaOp::ConstWord(Immediate::Fixnum(999)))
    );
}

#[test]
fn dead_loop_parameters_are_kept_only_when_deoptimization_needs_them() {
    for can_deoptimize in [false, true] {
        let mut instructions = vec![
            Instruction::Load(2, Immediate::Fixnum(7)),
            Instruction::Load(3, Immediate::Fixnum(8)),
            Instruction::Copy(4, 0),
            Instruction::Unless(4, 0), // Header at PC 3.
        ];
        if can_deoptimize {
            instructions.push(Instruction::ReadCell {
                out: 6,
                symbol: 0,
                function: false,
                at: 0,
            });
        }
        instructions.extend([
            Instruction::Copy(5, 2),
            Instruction::Copy(2, 3),
            Instruction::Copy(3, 5),
            Instruction::Load(4, Immediate::Nil),
            Instruction::Jump(3),
        ]);
        instructions[3] = Instruction::Unless(4, instructions.len());
        instructions.push(Instruction::SetValues(1..2));

        let mut code = code_without_side_tables(
            Arity {
                required: 2,
                maximum: Some(2),
            },
            7,
            instructions,
        );
        code.constants.push(0);
        let mut function = lift_code(&code, &NoSpecializationInputs).unwrap();
        if !can_deoptimize {
            // Exercise DCE on the same loop without a recovery consumer. A
            // backedge in lifted bytecode normally retains a checkpoint.
            for block in &mut function.blocks {
                block
                    .insts
                    .retain(|inst| !matches!(inst.op, SsaOp::Checkpoint { .. }));
            }
        }
        let header = function.pc_to_block[&3];
        let carried = |function: &zkcl_opt::SsaFunction| {
            function
                .block(header)
                .unwrap()
                .params
                .iter()
                .filter(|param| matches!(param.origin_reg, Some(2 | 3)))
                .count()
        };
        assert_eq!(carried(&function), 2);
        optimize(&mut function, &NoSpecializationInputs);
        assert_eq!(carried(&function), if can_deoptimize { 2 } else { 0 });
        let header = function.block(header).unwrap();
        assert!(matches!(
            &header.terminator,
            Terminator::BranchTruthy { cond, .. }
                if header.params.last().is_some_and(|param|
                    param.origin_reg == Some(4) && param.value == *cond)
        ));
        if can_deoptimize {
            let snapshot = function
                .blocks
                .iter()
                .flat_map(|block| &block.insts)
                .find(|inst| matches!(inst.op, SsaOp::ReadCell { .. }))
                .and_then(|inst| inst.deopt.as_ref())
                .unwrap();
            for param in &header.params[..2] {
                let register = param.origin_reg.unwrap() as u32;
                assert!(
                    snapshot
                        .registers_to_restore
                        .contains(&(register, DeoptRegisterValue::TaggedWord(param.value),))
                );
            }
        }
        for block in &function.blocks {
            block.terminator.visit_targets(|target| {
                assert_eq!(
                    target.args.len(),
                    function.block(target.block).unwrap().params.len()
                );
            });
        }
    }
}

#[test]
fn jump_chain_merges_in_execution_order_and_updates_lifted_locations() {
    let mut code = code_without_side_tables(
        Arity::ZERO,
        1,
        vec![
            Instruction::Load(0, Immediate::True),
            Instruction::Jump(4),
            Instruction::ReadCell {
                out: 0,
                symbol: 1,
                function: false,
                at: 2,
            },
            Instruction::Jump(6),
            Instruction::WriteCell { symbol: 0, src: 0 },
            Instruction::Jump(2),
            Instruction::SetValues(0..1),
        ],
    );
    code.constants.extend([0, 1]);
    let mut function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let expected = [0, 4, 2, 6];
    assert!(
        expected
            .iter()
            .all(|pc| function.pc_to_block.contains_key(pc))
    );
    optimize(&mut function, &NoSpecializationInputs);
    assert_eq!(function.blocks.len(), 1);
    assert!(
        expected
            .iter()
            .all(|pc| function.pc_to_block[pc] == function.entry)
    );
    let effects: Vec<_> = function.blocks[0]
        .insts
        .iter()
        .filter_map(|inst| match inst.op {
            SsaOp::WriteCell { .. } | SsaOp::ReadCell { .. } => inst.pc,
            _ => None,
        })
        .collect();
    assert_eq!(effects, [4, 2]);
}

#[test]
fn root_layout_assigns_slots_for_boxed_values_across_safepoints() {
    // fn(x):
    //   r1 = ReadCell(sym 0)   ; boxed value
    //   r2 = CallNamed(sym 1, [r0]) ; safepoint! r1 is live across it!
    //   WriteCell(sym 0, r1)   ; uses r1 after safepoint
    //   SetValues(2..3)
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        3,
        vec![
            Instruction::ReadCell {
                out: 1,
                symbol: 0,
                function: false,
                at: 0,
            },
            Instruction::Call {
                callee: Callee::Named(1),
                out: Some(2),
                args: 0..1,
                at: 1,
                associate: false,
                notinline: false,
            },
            Instruction::WriteCell { symbol: 0, src: 1 },
            Instruction::SetValues(2..3),
        ],
    );
    code.constants = vec![0, 1];

    let mut func = lift_code(&code, &NoSpecializationInputs).expect("lift succeeds");
    optimize(&mut func, &NoSpecializationInputs);
    let layout = compute_root_layout(&func);

    assert_eq!(layout.code_slot, RootSlot(0));
    assert_eq!(layout.closure_slot, RootSlot(1));
    assert_eq!(layout.arg_slots.len(), 1);
    assert_eq!(layout.arg_slots[0].1, RootSlot(2));

    let read_val = func.blocks[0]
        .insts
        .iter()
        .find_map(|i| {
            matches!(i.op, SsaOp::ReadCell { .. })
                .then_some(i.id)
                .flatten()
        })
        .expect("ReadCell present");
    let slot = layout
        .value_slots
        .get(&read_val)
        .copied()
        .expect("ReadCell result live across CallNamed safepoint must have a RootSlot");
    assert!(slot.0 >= 3);
    assert_eq!(layout.total_slots, 4);
    assert!(layout.safepoint_roots.iter().any(|sp| {
        sp.live_slots
            .iter()
            .any(|&(v, s)| v == read_val && s == slot)
    }));
}

#[test]
fn native_root_slots_are_reused_between_disjoint_safepoint_lifetimes() {
    let read = |out| Instruction::ReadCell {
        out,
        symbol: 0,
        function: false,
        at: 0,
    };
    let call = Instruction::Call {
        callee: Callee::Named(1),
        out: None,
        args: 0..0,
        at: 0,
        associate: false,
        notinline: false,
    };
    let write = |src| Instruction::WriteCell { symbol: 0, src };
    let mut code = code_without_side_tables(
        Arity::ZERO,
        3,
        vec![
            read(0),
            call.clone(),
            write(0),
            Instruction::Clear(0..1),
            read(1),
            read(2),
            call,
            write(1),
            write(2),
            Instruction::SetValues(0..0),
        ],
    );
    code.constants = vec![0, 1];
    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let layout = compute_root_layout(&function);
    let first = &layout
        .safepoint_roots
        .iter()
        .find(|site| site.pc == Some(1))
        .unwrap()
        .live_slots;
    let second = &layout
        .safepoint_roots
        .iter()
        .find(|site| site.pc == Some(6))
        .unwrap()
        .live_slots;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 2);
    assert_ne!(second[0].1, second[1].1);
    assert!(
        second
            .iter()
            .any(|&(value, slot)| value != first[0].0 && slot == first[0].1)
    );
    assert_eq!(layout.total_slots, 4);
    assert_eq!(
        zkcl_opt::native::native_frame_layout(&function).root_slots(),
        4
    );
}

#[test]
fn immediates_live_across_a_call_do_not_need_gc_roots() {
    let mut code = code_without_side_tables(
        Arity::ZERO,
        2,
        vec![
            Instruction::Load(0, Immediate::Fixnum(10)),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(1),
                args: 0..0,
                at: 0,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(0..2),
        ],
    );
    code.constants.push(0);
    let function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let layout = compute_root_layout(&function);
    assert_eq!(layout.total_slots, 2);
    assert_eq!(layout.safepoint_roots.len(), 1);
    assert!(layout.safepoint_roots[0].live_slots.is_empty());
}

#[test]
fn heap_value_used_only_by_deoptimization_survives_an_earlier_call() {
    use zkcl_opt::{DeoptState, SsaInst};

    let mut code = code_without_side_tables(
        Arity::ZERO,
        2,
        vec![
            Instruction::Constant(0, 0),
            Instruction::Call {
                callee: Callee::Named(1),
                out: None,
                args: 0..0,
                at: 1,
                associate: false,
                notinline: true,
            },
            Instruction::Jump(3),
            Instruction::Load(1, Immediate::Fixnum(10)),
            Instruction::SetValues(1..2),
        ],
    );
    code.constants = vec![0, 1];
    let mut function = lift_code(&code, &NoSpecializationInputs).unwrap();
    let heap_value = function
        .block(function.entry)
        .unwrap()
        .insts
        .iter()
        .find(|inst| inst.op == SsaOp::LoadConst(0))
        .and_then(|inst| inst.id)
        .unwrap();
    let resume = function.pc_to_block[&3];
    let guard = SsaOp::GuardPrimitiveBinding {
        symbol: 1,
        primitive: GuardedPrimitive::Funcall,
    };
    function.block_mut(resume).unwrap().insts.insert(
        0,
        SsaInst {
            id: None,
            ty: TypeTag::Top,

            op: guard,
            pc: Some(3),
            deopt: Some(DeoptState {
                pc: 3,
                registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(heap_value))],
            }),
        },
    );

    let layout = compute_root_layout(&function);
    assert_eq!(layout.safepoint_roots.len(), 1);
    assert_eq!(layout.safepoint_roots[0].pc, Some(1));
    assert_eq!(
        layout.safepoint_roots[0].live_slots,
        [(heap_value, RootSlot(2))]
    );
}

#[test]
fn lift_requires_an_existing_slot_for_capture_operands() {
    use zkcl_opt::BytecodeLiftError;
    let mut code = code_without_side_tables(Arity::ZERO, 1, vec![Instruction::Capture(0, 0)]);
    assert!(matches!(
        lift_code(&code, &NoSpecializationInputs),
        Err(BytecodeLiftError::CaptureOutOfBounds { index: 0, .. })
    ));
    code.captures = vec![CaptureKind::ExitToken];
    lift_code(&code, &NoSpecializationInputs).unwrap();
}

#[test]
fn lift_requires_a_mutable_capture_for_box_writes() {
    use zkcl_bytecode::CellLocation;
    use zkcl_opt::BytecodeLiftError;

    let mut code = code_without_side_tables(
        Arity::ZERO,
        1,
        vec![Instruction::WriteBox {
            cell: CellLocation::Capture(0),
            src: 0,
        }],
    );
    code.captures = vec![CaptureKind::MutableCell];
    for kind in [CaptureKind::ExitToken, CaptureKind::Value] {
        code.captures[0] = kind;
        assert!(matches!(
            lift_code(&code, &NoSpecializationInputs),
            Err(BytecodeLiftError::ImmutableCapture { index: 0, .. })
        ));
    }
    code.captures[0] = CaptureKind::MutableCell;
    lift_code(&code, &NoSpecializationInputs).unwrap();
}

#[test]
fn lifted_ssa_preserves_nested_control_scope_tree_and_block_entry_scopes() {
    use zkcl_opt::{ControlScopeKind, ScopeId};

    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        3,
        vec![
            Instruction::EnterProtect {
                cleanup: 10,
                clear: 0,
            },
            Instruction::EnterHandlers { table: 0 },
            Instruction::BindSpecial {
                symbol: 0,
                value: 0,
            },
            Instruction::Unless(0, 5),
            Instruction::Load(1, Immediate::Fixnum(42)),
            Instruction::Unbind(1),
            Instruction::LeaveHandlers,
            Instruction::BeginCleanup,
            Instruction::Load(1, Immediate::Nil),
            Instruction::BeginCleanup,
            Instruction::Load(2, Immediate::True),
            Instruction::EndCleanup,
            Instruction::SetValues(1..2),
        ],
    );
    code.constants = vec![0];
    code.handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 8,
                condition: 1,
                clear: 2,
            },
        }],
    });

    let func = lift_code(&code, &NoSpecializationInputs).unwrap();
    assert!(func.scopes.has_non_root_scopes());

    let entry_block = func.block(func.pc_to_block[&0]).unwrap();
    assert_eq!(entry_block.entry_scope, ScopeId::ROOT);
    assert_eq!(func.scopes.get(ScopeId::ROOT).kind, ControlScopeKind::Root);
    assert_eq!(func.scopes.depth(ScopeId::ROOT), 0);

    let dynamic_block = func.block(func.pc_to_block[&4]).unwrap();
    let dynamic_scope = dynamic_block.entry_scope;
    assert_ne!(dynamic_scope, ScopeId::ROOT);
    assert_eq!(
        func.scopes.get(dynamic_scope).kind,
        ControlScopeKind::Dynamic
    );
    assert_eq!(func.scopes.depth(dynamic_scope), 3);

    let handler_scope = func.scopes.parent(dynamic_scope).unwrap();
    assert_eq!(
        func.scopes.get(handler_scope).kind,
        ControlScopeKind::Handler
    );
    assert_eq!(func.scopes.depth(handler_scope), 2);

    let protect_scope = func.scopes.parent(handler_scope).unwrap();
    assert_eq!(
        func.scopes.get(protect_scope).kind,
        ControlScopeKind::Protect
    );
    assert_eq!(func.scopes.depth(protect_scope), 1);
    assert_eq!(func.scopes.parent(protect_scope), Some(ScopeId::ROOT));

    let handler_case_block = func.block(func.pc_to_block[&8]).unwrap();
    assert_eq!(handler_case_block.entry_scope, protect_scope);

    let cleanup_block = func.block(func.pc_to_block[&10]).unwrap();
    let cleanup_scope = cleanup_block.entry_scope;
    assert_eq!(
        func.scopes.get(cleanup_scope).kind,
        ControlScopeKind::Cleanup
    );
    assert_eq!(func.scopes.depth(cleanup_scope), 1);
    assert_eq!(func.scopes.parent(cleanup_scope), Some(ScopeId::ROOT));
}

#[test]
fn sccp_and_ssi_branch_refinement_eliminates_checked_overflow_and_dead_branch() {
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        4,
        vec![
            Instruction::UnlessFixnumInRange(0, 0, 100, 9),
            Instruction::UnlessFixnum(0, 6),
            Instruction::Load(1, Immediate::Fixnum(1)),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(2),
                args: 0..2,
                at: 3,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(2..3),
            Instruction::Jump(11),
            Instruction::Load(3, Immediate::Fixnum(777)),
            Instruction::SetValues(3..4),
            Instruction::Jump(11),
            Instruction::Load(3, Immediate::Fixnum(-1)),
            Instruction::SetValues(3..4),
        ],
    );
    code.constants = vec![0];
    let mut inputs = SpecializationInputTables::new();
    inputs.symbols.insert(0, ("CL", "+"));
    inputs
        .call_site_profiles
        .insert(3, CallSiteProfile::FixnumArithmetic);

    let mut func = lift_code(&code, &inputs).unwrap();
    optimize(&mut func, &inputs);

    let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
    assert!(
        !insts
            .iter()
            .any(|inst| inst.op == SsaOp::ConstWord(Immediate::Fixnum(777))),
        "SSI edge refinement should fold redundant fixnum check and dead branch"
    );
    assert!(
        !insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::AddI64Checked(_, _))),
        "bounded [0, 100] + 1 should downgrade AddI64Checked to AddI64Unchecked"
    );
    assert!(
        insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::AddI64Unchecked(_, _)) && inst.deopt.is_none()),
        "expected unchecked addition with deopt cleared"
    );
}

fn loop_with_binding_guards() -> zkcl_opt::SsaFunction {
    use zkcl_bytecode::FixnumArithmetic;
    use zkcl_opt::{
        BasicBlock, BlockId, BlockParam, BranchTarget, DeoptState, ScopeId, ScopeTree, SsaFunction,
        SsaInst, ValueId,
    };

    let guard = |pc: usize, param: ValueId| SsaInst {
        id: None,
        op: SsaOp::GuardPrimitiveBinding {
            symbol: 0,
            primitive: GuardedPrimitive::FixnumArithmetic(FixnumArithmetic::Add),
        },
        ty: TypeTag::Top,
        pc: Some(pc),
        deopt: Some(DeoptState {
            pc,
            registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(param))],
        }),
    };
    SsaFunction {
        fixnum_bounds: (zkcl_opt::FIXNUM_MIN, FIXNUM_MAX),
        arity: Arity {
            required: 3,
            maximum: Some(3),
        },
        constant_count: 1,
        entry: BlockId(0),
        entry_params: vec![ValueId(0), ValueId(1), ValueId(2)],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                params: vec![
                    BlockParam {
                        value: ValueId(0),
                        ty: TypeTag::Any,
                        repr: ValueRepr::TaggedWord,
                        origin_reg: Some(0),
                    },
                    BlockParam {
                        value: ValueId(1),
                        ty: TypeTag::Fixnum,
                        repr: ValueRepr::UnboxedI64,
                        origin_reg: Some(1),
                    },
                    BlockParam {
                        value: ValueId(2),
                        ty: TypeTag::Fixnum,
                        repr: ValueRepr::UnboxedI64,
                        origin_reg: Some(2),
                    },
                ],
                insts: Vec::new(),
                terminator: Terminator::Jump(BranchTarget {
                    block: BlockId(1),
                    args: vec![ValueId(0)],
                }),
                terminator_pc: 0,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(1),
                params: vec![BlockParam {
                    value: ValueId(3),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                }],
                insts: vec![guard(1, ValueId(3))],
                terminator: Terminator::BranchTruthy {
                    cond: ValueId(3),
                    truthy: BranchTarget {
                        block: BlockId(2),
                        args: Vec::new(),
                    },
                    falsy: BranchTarget {
                        block: BlockId(3),
                        args: Vec::new(),
                    },
                },
                terminator_pc: 1,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(2),
                params: Vec::new(),
                insts: vec![
                    guard(2, ValueId(3)),
                    SsaInst {
                        id: Some(ValueId(4)),
                        op: SsaOp::CmpI64 {
                            op: CmpOp::Eq,
                            lhs: ValueId(1),
                            rhs: ValueId(2),
                        },
                        ty: TypeTag::Any,
                        pc: Some(2),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(5)),
                        op: SsaOp::CmpI64 {
                            op: CmpOp::Eq,
                            lhs: ValueId(2),
                            rhs: ValueId(1),
                        },
                        ty: TypeTag::Any,
                        pc: Some(3),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(6)),
                        op: SsaOp::SelectBool {
                            cond: ValueId(5),
                            when_true: Immediate::True,
                            when_false: Immediate::Nil,
                        },
                        ty: TypeTag::Any,
                        pc: Some(4),
                        deopt: None,
                    },
                ],
                terminator: Terminator::Jump(BranchTarget {
                    block: BlockId(1),
                    args: vec![ValueId(6)],
                }),
                terminator_pc: 4,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(3),
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Terminator::ReturnOne(ValueId(3)),
                terminator_pc: 5,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
        ],
        scopes: ScopeTree::new(),
        pc_to_block: FxHashMap::default(),
        next_value_id: 7,
    }
}

#[test]
fn gvn_deduplicates_comparisons_and_licm_hoists_the_header_guard() {
    use zkcl_opt::BlockId;

    let mut func = loop_with_binding_guards();
    optimize(&mut func, &NoSpecializationInputs);

    let preheader = func.block(BlockId(0)).unwrap();
    assert_eq!(
        preheader
            .insts
            .iter()
            .filter(|i| matches!(i.op, SsaOp::CmpI64 { .. }))
            .count(),
        1,
        "loop-invariant pure CmpI64 should be hoisted to preheader and GVN-deduplicated"
    );
    assert_eq!(
        preheader
            .insts
            .iter()
            .filter(|i| matches!(i.op, SsaOp::GuardPrimitiveBinding { .. }))
            .count(),
        1,
        "loop-invariant GuardPrimitiveBinding should be hoisted to preheader"
    );

    let total_cmps: usize = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| matches!(i.op, SsaOp::CmpI64 { .. }))
        .count();
    let total_guards: usize = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| matches!(i.op, SsaOp::GuardPrimitiveBinding { .. }))
        .count();
    assert_eq!(total_cmps, 1);
    assert_eq!(total_guards, 2);
    assert!(func.block(BlockId(2)).unwrap().insts.iter().any(|inst| {
        matches!(inst.op, SsaOp::GuardPrimitiveBinding { .. }) && inst.pc == Some(2)
    }));
}

#[test]
fn licm_keeps_a_binding_guard_in_a_loop_with_a_cell_read() {
    use zkcl_opt::{BlockId, DeoptState, SsaInst, ValueId};

    let mut func = loop_with_binding_guards();
    func.constant_count = 2;
    let read = func.fresh_value();
    let body = func.block_mut(BlockId(2)).unwrap();
    body.insts.push(SsaInst {
        id: Some(read),
        op: SsaOp::ReadCell {
            symbol: 1,
            function: false,
            at: 4,
        },
        ty: TypeTag::Any,
        pc: Some(4),
        deopt: Some(DeoptState {
            pc: 4,
            registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(ValueId(6)))],
        }),
    });

    optimize(&mut func, &NoSpecializationInputs);

    let has_binding_guard = |id| {
        func.block(BlockId(id))
            .unwrap()
            .insts
            .iter()
            .any(|inst| matches!(inst.op, SsaOp::GuardPrimitiveBinding { .. }))
    };
    assert!(!has_binding_guard(0));
    assert!(has_binding_guard(1));
}

#[test]
fn calls_inside_unwind_scope_can_specialize_and_non_unwind_roots_reuse_slots() {
    use zkcl_opt::{
        BasicBlock, BlockId, BlockParam, ScopeId, ScopeTree, SsaFunction, SsaInst, ValueId,
    };

    // Part A: Protected call inside an unwind_succs block still specializes.
    let mut code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        3,
        vec![
            Instruction::EnterProtect {
                cleanup: 4,
                clear: 0,
            },
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(2),
                args: 0..2,
                at: 1,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(2..3),
            Instruction::BeginCleanup,
            Instruction::EndCleanup,
            Instruction::SetValues(2..3),
        ],
    );
    code.constants = vec![0];
    let mut inputs = SpecializationInputTables::new();
    inputs.symbols.insert(0, ("CL", "+"));
    inputs
        .call_site_profiles
        .insert(1, CallSiteProfile::FixnumArithmetic);

    let mut func = lift_code(&code, &inputs).unwrap();
    optimize(&mut func, &inputs);
    assert!(
        func.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| matches!(i.op, SsaOp::AddI64Checked(_, _))),
        "calls inside protected blocks should still be specialized"
    );

    // Part B: Root layout pins unwind-live values while reusing slots for non-unwind temporaries.
    let call_op = |args: Vec<ValueId>| SsaOp::CallNamed {
        symbol: 0,
        args,
        has_result: true,
        at: 0,
        associate: false,
        notinline: true,
        profile: CallSiteProfile::Unprofiled,
    };
    let ssa = SsaFunction {
        fixnum_bounds: (zkcl_opt::FIXNUM_MIN, FIXNUM_MAX),
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        constant_count: 1,
        entry: BlockId(0),
        entry_params: vec![ValueId(0)],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                params: vec![BlockParam {
                    value: ValueId(0),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                }],
                insts: vec![
                    SsaInst {
                        id: None,
                        op: SsaOp::EnterBlock {
                            token: ValueId(0),
                            resume: BlockId(1),
                            clear: 1,
                        },
                        ty: TypeTag::Top,
                        pc: Some(0),
                        deopt: None,
                    },
                    // v1 is live across safepoints and ALSO live at unwind block BlockId(1).
                    SsaInst {
                        id: Some(ValueId(1)),
                        op: call_op(vec![ValueId(0)]),
                        ty: TypeTag::Any,
                        pc: Some(1),
                        deopt: None,
                    },
                    // v2 is live across the next safepoint (v3), not live at BlockId(1).
                    SsaInst {
                        id: Some(ValueId(2)),
                        op: call_op(vec![ValueId(0)]),
                        ty: TypeTag::Any,
                        pc: Some(2),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(3)),
                        op: call_op(vec![ValueId(0)]),
                        ty: TypeTag::Any,
                        pc: Some(3),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(4)),
                        op: call_op(vec![ValueId(1), ValueId(2), ValueId(3)]),
                        ty: TypeTag::Any,
                        pc: Some(4),
                        deopt: None,
                    },
                    // v5 is live across the next safepoint (v6), disjoint from v2, not live at BlockId(1).
                    SsaInst {
                        id: Some(ValueId(5)),
                        op: call_op(vec![ValueId(4)]),
                        ty: TypeTag::Any,
                        pc: Some(5),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(6)),
                        op: call_op(vec![ValueId(4)]),
                        ty: TypeTag::Any,
                        pc: Some(6),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(7)),
                        op: call_op(vec![ValueId(5), ValueId(6)]),
                        ty: TypeTag::Any,
                        pc: Some(7),
                        deopt: None,
                    },
                ],
                terminator: Terminator::ReturnOne(ValueId(7)),
                terminator_pc: 8,
                entry_scope: ScopeId::ROOT,
                unwind_succs: vec![BlockId(1)],
            },
            BasicBlock {
                id: BlockId(1),
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Terminator::ReturnOne(ValueId(1)),
                terminator_pc: 9,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
        ],
        scopes: ScopeTree::new(),
        pc_to_block: FxHashMap::default(),
        next_value_id: 8,
    };

    let layout = compute_root_layout(&ssa);
    let slot_v1 = layout.value_slots[&ValueId(1)];
    let slot_v2 = layout.value_slots[&ValueId(2)];
    let slot_v5 = layout.value_slots[&ValueId(5)];
    assert_eq!(
        slot_v2, slot_v5,
        "non-unwind temporaries with disjoint safepoint intervals should share a root slot"
    );
    assert_ne!(
        slot_v1, slot_v2,
        "unwind-live value must keep its own dedicated slot"
    );
    assert_ne!(slot_v1, slot_v5);
}

#[test]
fn late_load_elimination_forwards_boxes_cells_captures_and_respects_alias_domains() {
    use zkcl_opt::{BasicBlock, BlockId, BlockParam, CellTarget, ScopeId, ScopeTree, SsaFunction, SsaInst, ValueId};

    let mut func = SsaFunction {
        fixnum_bounds: (zkcl_opt::FIXNUM_MIN, FIXNUM_MAX),
        arity: Arity {
            required: 2,
            maximum: Some(2),
        },
        constant_count: 2,
        entry: BlockId(0),
        entry_params: vec![ValueId(0), ValueId(1)],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            params: vec![
                BlockParam {
                    value: ValueId(0),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                },
                BlockParam {
                    value: ValueId(1),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(1),
                },
            ],
            insts: vec![
                // Immutable capture read duplicated across an unknown call.
                SsaInst {
                    id: Some(ValueId(2)),
                    op: SsaOp::ReadCapture(0),
                    ty: TypeTag::Any,
                    pc: Some(0),
                    deopt: None,
                },
                // Local mutable box (escaped below to Closure? No, keep non-escaping here?
                // Wait: test BOTH cell forwarding and AliasDomain invalidation on WriteCell/ReadCell!)
                SsaInst {
                    id: None,
                    op: SsaOp::WriteCell {
                        symbol: 0,
                        src: ValueId(0),
                    },
                    ty: TypeTag::Top,
                    pc: Some(1),
                    deopt: None,
                },
                // ReadCell immediately after WriteCell forwards to v0.
                SsaInst {
                    id: Some(ValueId(3)),
                    op: SsaOp::ReadCell {
                        symbol: 0,
                        function: false,
                        at: 2,
                    },
                    ty: TypeTag::Any,
                    pc: Some(2),
                    deopt: None,
                },
                // Non-escaping BoxLocal written once, read across a CallNamed, then forwarded.
                SsaInst {
                    id: Some(ValueId(4)),
                    op: SsaOp::BoxLocal {
                        value: ValueId(0),
                        at: 3,
                    },
                    ty: TypeTag::Any,
                    pc: Some(3),
                    deopt: None,
                },
                SsaInst {
                    id: None,
                    op: SsaOp::WriteBox {
                        cell: CellTarget::Local(ValueId(4)),
                        src: ValueId(1),
                    },
                    ty: TypeTag::Top,
                    pc: Some(4),
                    deopt: None,
                },
                // Escaping capture cell read -> call -> read again must NOT be forwarded across call!
                SsaInst {
                    id: Some(ValueId(5)),
                    op: SsaOp::ReadBox(CellTarget::Capture(0)),
                    ty: TypeTag::Any,
                    pc: Some(5),
                    deopt: None,
                },
                SsaInst {
                    id: Some(ValueId(6)),
                    op: SsaOp::CallNamed {
                        symbol: 1,
                        args: vec![ValueId(2), ValueId(3)],
                        has_result: true,
                        at: 6,
                        associate: false,
                        notinline: true,
                        profile: CallSiteProfile::Unprofiled,
                    },
                    ty: TypeTag::Any,
                    pc: Some(6),
                    deopt: None,
                },
                // Duplicate ReadCapture(0) MUST still be eliminated across the call.
                SsaInst {
                    id: Some(ValueId(7)),
                    op: SsaOp::ReadCapture(0),
                    ty: TypeTag::Any,
                    pc: Some(7),
                    deopt: None,
                },
                // Non-escaping BoxLocal read AFTER CallNamed STILL forwards to ValueId(1)!
                SsaInst {
                    id: Some(ValueId(8)),
                    op: SsaOp::ReadBox(CellTarget::Local(ValueId(4))),
                    ty: TypeTag::Any,
                    pc: Some(8),
                    deopt: None,
                },
                // Capture ReadBox AFTER CallNamed MUST be preserved because CallNamed writes BOX_CELL.
                SsaInst {
                    id: Some(ValueId(9)),
                    op: SsaOp::ReadBox(CellTarget::Capture(0)),
                    ty: TypeTag::Any,
                    pc: Some(9),
                    deopt: None,
                },
                // Global ReadCell AFTER CallNamed MUST be preserved because CallNamed writes SYMBOL_VALUE.
                SsaInst {
                    id: Some(ValueId(10)),
                    op: SsaOp::ReadCell {
                        symbol: 0,
                        function: false,
                        at: 10,
                    },
                    ty: TypeTag::Any,
                    pc: Some(10),
                    deopt: None,
                },
                // Observable user tying everything together.
                SsaInst {
                    id: Some(ValueId(11)),
                    op: SsaOp::CallNamed {
                        symbol: 1,
                        args: vec![ValueId(5), ValueId(6), ValueId(7), ValueId(8), ValueId(9), ValueId(10)],
                        has_result: true,
                        at: 11,
                        associate: false,
                        notinline: true,
                        profile: CallSiteProfile::Unprofiled,
                    },
                    ty: TypeTag::Any,
                    pc: Some(11),
                    deopt: None,
                },
            ],
            terminator: Terminator::ReturnOne(ValueId(11)),
            terminator_pc: 12,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }],
        scopes: ScopeTree::new(),
        pc_to_block: FxHashMap::default(),
        next_value_id: 12,
    };

    optimize(&mut func, &NoSpecializationInputs);
    let insts = &func.blocks[0].insts;

    let read_captures = insts
        .iter()
        .filter(|i| matches!(i.op, SsaOp::ReadCapture(0)))
        .count();
    assert_eq!(read_captures, 1, "immutable ReadCapture should be deduplicated");

    let capture_box_reads = insts
        .iter()
        .filter(|i| matches!(i.op, SsaOp::ReadBox(CellTarget::Capture(0))))
        .count();
    assert_eq!(
        capture_box_reads, 2,
        "ReadBox(Capture) must be reloaded after CallNamed clobbers BOX_CELL"
    );

    let cell_reads = insts
        .iter()
        .filter(|i| matches!(i.op, SsaOp::ReadCell { .. }))
        .count();
    assert_eq!(
        cell_reads, 1,
        "dominated ReadCell before call should be forwarded to WriteCell src, while post-call ReadCell stays"
    );

    assert!(
        !insts.iter().any(|i| matches!(i.op, SsaOp::BoxLocal { .. })),
        "non-escaping BoxLocal whose ReadBox was forwarded should be completely scalar-replaced"
    );
    let final_call_args = match &insts.last().unwrap().op {
        SsaOp::CallNamed { args, .. } => args,
        other => panic!("expected CallNamed, got {other:?}"),
    };
    assert_eq!(
        final_call_args[2],
        ValueId(2),
        "duplicate ReadCapture arg should be rewritten to first ReadCapture"
    );
    assert_eq!(
        final_call_args[3],
        ValueId(1),
        "non-escaping BoxLocal read after call should be forwarded to WriteBox src"
    );
}

#[test]
fn escape_analysis_scalar_replaces_non_escaping_box_local() {
    use zkcl_opt::{
        BasicBlock, BlockId, BlockParam, BranchTarget, CellTarget, ScopeId, ScopeTree, SsaFunction,
        SsaInst, ValueId,
    };

    let func_with_loop_and_escape = SsaFunction {
        fixnum_bounds: (zkcl_opt::FIXNUM_MIN, FIXNUM_MAX),
        arity: Arity {
            required: 2,
            maximum: Some(2),
        },
        constant_count: 1,
        entry: BlockId(0),
        entry_params: vec![ValueId(0), ValueId(1)],
        blocks: vec![
            BasicBlock {
                id: BlockId(0),
                params: vec![
                    BlockParam {
                        value: ValueId(0),
                        ty: TypeTag::Any,
                        repr: ValueRepr::TaggedWord,
                        origin_reg: Some(0),
                    },
                    BlockParam {
                        value: ValueId(1),
                        ty: TypeTag::Any,
                        repr: ValueRepr::TaggedWord,
                        origin_reg: Some(1),
                    },
                ],
                insts: vec![
                    // b_const: read-only inside loop -> scalar-replaced to v0!
                    SsaInst {
                        id: Some(ValueId(2)),
                        op: SsaOp::BoxLocal {
                            value: ValueId(0),
                            at: 0,
                        },
                        ty: TypeTag::Any,
                        pc: Some(0),
                        deopt: None,
                    },
                    // b_escape: passed to Closure -> MUST stay boxed!
                    SsaInst {
                        id: Some(ValueId(3)),
                        op: SsaOp::BoxLocal {
                            value: ValueId(1),
                            at: 1,
                        },
                        ty: TypeTag::Any,
                        pc: Some(1),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(4)),
                        op: SsaOp::Closure {
                            code: 0,
                            cells: vec![ValueId(3)],
                            at: 2,
                        },
                        ty: TypeTag::Function,
                        pc: Some(2),
                        deopt: None,
                    },
                ],
                terminator: Terminator::Jump(BranchTarget {
                    block: BlockId(1),
                    args: Vec::new(),
                }),
                terminator_pc: 3,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(1),
                params: Vec::new(),
                insts: vec![
                    SsaInst {
                        id: Some(ValueId(5)),
                        op: SsaOp::ReadBox(CellTarget::Local(ValueId(2))),
                        ty: TypeTag::Any,
                        pc: Some(4),
                        deopt: None,
                    },
                    SsaInst {
                        id: Some(ValueId(6)),
                        op: SsaOp::CallValue {
                            callee: ValueId(4),
                            args: vec![ValueId(5)],
                            has_result: true,
                            at: 5,
                            associate: false,
                            notinline: true,
                            profile: CallSiteProfile::Unprofiled,
                        },
                        ty: TypeTag::Any,
                        pc: Some(5),
                        deopt: None,
                    },
                ],
                terminator: Terminator::BranchTruthy {
                    cond: ValueId(6),
                    truthy: BranchTarget {
                        block: BlockId(1),
                        args: Vec::new(),
                    },
                    falsy: BranchTarget {
                        block: BlockId(2),
                        args: vec![ValueId(6)],
                    },
                },
                terminator_pc: 6,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(2),
                params: vec![BlockParam {
                    value: ValueId(7),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                }],
                insts: Vec::new(),
                terminator: Terminator::ReturnOne(ValueId(7)),
                terminator_pc: 7,
                entry_scope: ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
        ],
        scopes: ScopeTree::new(),
        pc_to_block: FxHashMap::default(),
        next_value_id: 8,
    };

    let mut func = func_with_loop_and_escape;
    optimize(&mut func, &NoSpecializationInputs);

    let box_count: usize = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| matches!(i.op, SsaOp::BoxLocal { .. }))
        .count();
    assert_eq!(
        box_count, 1,
        "only the escaping BoxLocal passed to Closure should remain allocated"
    );

    let read_box_count: usize = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|i| matches!(i.op, SsaOp::ReadBox(_)))
        .count();
    assert_eq!(read_box_count, 0, "read-only non-escaping BoxLocal read in loop should be replaced by initial value");

    let loop_block = func.block(BlockId(1)).unwrap();
    let closure_call = loop_block
        .insts
        .iter()
        .find(|i| matches!(i.op, SsaOp::CallValue { .. }))
        .unwrap();
    match &closure_call.op {
        SsaOp::CallValue { args, .. } => assert_eq!(args, &[ValueId(0)]),
        _ => unreachable!(),
    }
}

#[test]
fn bytecode_function_facts_seed_entry_param_types_and_fold_predicates_in_aot_mode() {
    use zkcl_bytecode::{FunctionFacts, ParamFact, ParamTypeFact};
    use zkcl_opt::{OptimizeConfig, optimize_with_options};

    // Function(x): (if x 111 222), with static fact `x: Fixnum`.
    // In AOT mode (`allow_speculative_deopt: false`), Fixnum is guaranteed
    // non-NIL so the falsy `222` branch folds away without any speculative guard.
    let mut code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        2,
        vec![
            Instruction::Unless(0, 3),
            Instruction::Load(1, Immediate::Fixnum(111)),
            Instruction::Jump(4),
            Instruction::Load(1, Immediate::Fixnum(222)),
            Instruction::SetValues(1..2),
        ],
    );
    code.facts = FunctionFacts {
        params: vec![ParamFact {
            ty: ParamTypeFact::Fixnum,
            dynamic_extent: false,
        }],
    };

    let mut func = lift_code(&code, &NoSpecializationInputs).unwrap();
    assert_eq!(func.blocks[0].params[0].ty, TypeTag::Fixnum);

    optimize_with_options(
        &mut func,
        &NoSpecializationInputs,
        OptimizeConfig {
            allow_speculative_deopt: false,
        },
    );

    assert_eq!(func.blocks.len(), 1, "dead falsy branch should be eliminated");
    match func.blocks[0].terminator {
        Terminator::ReturnOne(val) => {
            let def = func.blocks[0]
                .insts
                .iter()
                .find(|i| i.id == Some(val))
                .expect("returned constant 111");
            assert_eq!(def.op, SsaOp::ConstWord(Immediate::Fixnum(111)));
        }
        ref other => panic!("expected constant ReturnOne(111), got {other:?}"),
    }
}
