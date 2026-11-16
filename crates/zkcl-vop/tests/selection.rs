use zkcl_bytecode::native_abi::{FixnumComparison, LeafGuard, LeafPrimitive};
use zkcl_bytecode::{
    Arity, BytecodeFunction, Callee, CompiledFile, FixnumArithmetic, Immediate, Instruction,
    LiteralNode, WORD_NIL,
};
use zkcl_vop::leaf::{Exit, Operand, Operation, Step, Test};
use zkcl_vop::{LeafFunction, SelectionError, select, select_compiled};

fn exact(required: usize) -> Arity {
    Arity {
        required,
        maximum: Some(required),
    }
}

fn select_plain(
    argc: usize,
    regs: usize,
    code: &[Instruction],
    width: u8,
) -> Result<LeafFunction, SelectionError> {
    select(exact(argc), regs, code, width, |_| None)
}

#[test]
fn value_snapshots_survive_register_reuse_and_control_flow() {
    use Instruction::*;
    let ops = [
        Unless(0, 4),
        SetValues(0..2),
        Clear(0..2),
        Jump(5),
        SetValues(0..1),
        BindValues(0..2),
        SetValues(1..2),
    ];
    let leaf = select_plain(2, 2, &ops, 64).unwrap();
    let Exit::Branch {
        value: Operand::Arg(0),
        test: Test::Nil,
        yes,
        no,
    } = leaf.blocks()[0].exit
    else {
        panic!("expected a NIL branch");
    };
    let [yes, no] = [yes, no].map(|index| &leaf.blocks()[index]);
    let [
        Step::Copy {
            destination,
            value: Operand::Immediate(WORD_NIL),
        },
    ] = &yes.steps[..]
    else {
        panic!("missing value is padded with NIL on the taken edge");
    };
    assert_eq!(
        no.steps,
        [Step::Copy {
            destination: *destination,
            value: Operand::Arg(1)
        }]
    );
    assert_eq!(yes.exit, no.exit);
    let Exit::Jump(join) = yes.exit else {
        panic!("expected a shared successor")
    };
    assert_eq!(
        leaf.blocks()[join].exit,
        Exit::Return(Operand::Local(*destination))
    );
}

#[test]
fn eighteen_value_joins_have_linear_code_growth() {
    use Instruction::*;
    let mut ops = Vec::new();
    let diamond_count = 18;
    for _ in 0..diamond_count {
        let pc = ops.len();
        ops.extend([
            Unless(0, pc + 3),
            TryFixnumArithmetic {
                op: FixnumArithmetic::Inc,
                symbol: 0,
                out: 1,
                args: 1..2,
            },
            Jump(pc + 4),
            TryFixnumArithmetic {
                op: FixnumArithmetic::Dec,
                symbol: 1,
                out: 1,
                args: 1..2,
            },
        ]);
    }
    ops.push(SetValues(1..2));
    for bits in [32, 64] {
        let leaf = select_plain(2, 2, &ops, bits).unwrap();
        assert_eq!(leaf.locals(), 1);
        let count = leaf
            .blocks()
            .iter()
            .flat_map(|block| &block.steps)
            .filter(|step| matches!(step, Step::Compute { .. }))
            .count();
        assert_eq!(count, 2 * diamond_count);
        assert_eq!(leaf.blocks().len(), 3 * diamond_count + 1);
    }
}

#[test]
fn a_branch_can_enter_a_value_join_directly() {
    use Instruction::*;
    let ops = [
        Load(1, Immediate::Fixnum(10)),
        Unless(0, 3),
        Load(1, Immediate::Fixnum(20)),
        SetValues(1..2),
    ];
    let leaf = select_plain(1, 2, &ops, 64).unwrap();
    let Exit::Branch { yes, no, .. } = leaf.blocks()[0].exit else {
        panic!("expected a branch")
    };
    let [taken, other] = [yes, no].map(|index| &leaf.blocks()[index]);
    for (block, n) in [(taken, 10), (other, 20)] {
        assert_eq!(
            block.steps,
            [Step::Copy {
                destination: 0,
                value: Operand::Immediate(Immediate::Fixnum(n).encode_word(64).unwrap()),
            }]
        );
    }
    assert_eq!(taken.exit, other.exit);
}

#[test]
fn rejects_effects_invalid_branches_and_dynamic_result_counts() {
    use Instruction::*;
    assert_eq!(
        select_plain(1, 1, &[SetValues(0..1), Jump(3)], 64),
        Err(SelectionError::Branch { pc: 1 })
    );
    assert_eq!(
        select_plain(1, 1, &[SetValues(0..1), Jump(0)], 64),
        Err(SelectionError::ResultCount)
    );
    assert_eq!(
        select_plain(1, 1, &[WriteCell { symbol: 0, src: 0 }], 64),
        Err(SelectionError::Instruction { pc: 0 })
    );
    assert_eq!(
        select_plain(
            2,
            2,
            &[Unless(0, 3), SetValues(0..1), Jump(4), SetValues(0..2)],
            64
        ),
        Err(SelectionError::ResultCount)
    );
    assert_eq!(
        select(
            Arity {
                required: 0,
                maximum: Some(1)
            },
            1,
            &[SetValues(0..1)],
            64,
            |_| None
        ),
        Err(SelectionError::Arity)
    );
}

#[test]
fn backward_edges_budget_and_update_the_same_loop_inputs() {
    use Instruction::*;
    let call = |out, args| Call {
        callee: Callee::Named(0),
        out,
        args,
        at: 0,
        associate: false,
        notinline: false,
    };
    let ops = [
        Load(1, Immediate::Fixnum(0)),
        Copy(2, 0),
        Copy(3, 1),
        call(Some(4), 2..4),
        Unless(4, 7),
        SetValues(0..1),
        Jump(9),
        Copy(0, 1),
        Jump(1),
    ];
    for bits in [32, 64] {
        let leaf = select(exact(1), 5, &ops, bits, |_| LeafPrimitive::named("=")).unwrap();
        assert!(leaf.loop_budget().is_some_and(|budget| budget > 0));
        assert!(leaf.may_miss());
        assert!(leaf.needs_dispatch());
        let checked = leaf
            .blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| block.steps.contains(&Step::CheckLoopBudget))
            .collect::<Vec<_>>();
        assert_eq!(checked.len(), 1);
        let (index, block) = checked[0];
        assert!(matches!(block.exit, Exit::Jump(target) if target <= index));
    }
}

#[test]
fn word_width_controls_signed_immediates_and_range_limits() {
    use Instruction::*;
    let ops = [
        Load(0, Immediate::Fixnum(-1)),
        UnlessFixnumInRange(0, i32::MIN, 0, 3),
        SetValues(0..1),
    ];
    let leaf = select_plain(0, 1, &ops, 32).unwrap();
    assert_eq!(
        leaf.blocks()[0].exit,
        Exit::Return(Operand::Immediate(0xffff_fff9))
    );
    let wide = [Load(0, Immediate::Fixnum(1 << 29)), SetValues(0..1)];
    assert_eq!(
        select_plain(0, 1, &wide, 32),
        Err(SelectionError::Operand { pc: 0 })
    );
    assert!(select_plain(0, 1, &wide, 64).is_ok());
}

#[test]
fn arithmetic_and_comparison_reuse_locals_and_check_each_argument_once() {
    use Instruction::*;
    let ops = [
        TryFixnumArithmetic {
            op: FixnumArithmetic::Mul,
            out: 0,
            symbol: 0,
            args: 0..2,
        },
        TryFixnumArithmetic {
            op: FixnumArithmetic::Inc,
            out: 0,
            symbol: 1,
            args: 0..1,
        },
        Call {
            callee: Callee::Named(2),
            out: None,
            args: 0..2,
            at: 0,
            associate: false,
            notinline: false,
        },
    ];
    let leaf = select(exact(2), 2, &ops, 64, |index| {
        (index == 2).then_some(LeafPrimitive::Compare(FixnumComparison::Lt))
    })
    .unwrap();
    assert_eq!(leaf.locals(), 1);
    assert!(leaf.needs_dispatch());
    assert_eq!(leaf.guards().len(), 3);
    let block = &leaf.blocks()[0];
    assert_eq!(
        block.steps[..2],
        [
            Step::CheckFixnum(Operand::Arg(0)),
            Step::CheckFixnum(Operand::Arg(1))
        ]
    );
    assert_eq!(block.steps.len(), 5);
    for step in &block.steps[2..4] {
        assert!(matches!(
            step,
            Step::Compute {
                destination: 0,
                op: Operation::Arithmetic { checked: true, .. }
            }
        ));
    }
    assert_eq!(
        leaf.guards()[2],
        LeafGuard {
            symbol: 2,
            primitive: LeafPrimitive::Compare(FixnumComparison::Lt)
        }
    );
    assert_eq!(block.exit, Exit::Return(Operand::Local(0)));
}

#[test]
fn named_calls_must_resolve_and_permit_inlining() {
    let call = |notinline| Instruction::Call {
        callee: Callee::Named(0),
        out: None,
        args: 0..2,
        at: 0,
        associate: false,
        notinline,
    };
    let resolve = |_| LeafPrimitive::named("+");
    assert!(select(exact(2), 2, &[call(false)], 64, resolve).is_ok());
    assert_eq!(
        select(exact(2), 2, &[call(true)], 64, resolve),
        Err(SelectionError::NotinlineCall { pc: 0, symbol: 0 })
    );
    assert_eq!(
        select_plain(2, 2, &[call(false)], 64),
        Err(SelectionError::UnrecognizedNamedCall { pc: 0, symbol: 0 })
    );
}

#[test]
fn compiled_function_respects_notinline_metadata_on_named_calls() {
    let mut file = CompiledFile {
        target: "aarch64-apple-darwin".parse().unwrap(),
        source_files: Vec::new(),
        nodes: vec![LiteralNode::InternedSymbol {
            package: "COMMON-LISP".into(),
            name: "+".into(),
        }],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 2,
            instructions: vec![
                Instruction::Load(0, Immediate::Fixnum(20)),
                Instruction::Load(1, Immediate::Fixnum(22)),
                Instruction::Call {
                    callee: Callee::Named(0),
                    args: 0..2,
                    out: None,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
            ],
            constants: vec![0],
            lambda_list: None,
            captures: Vec::new(),
            notinline: vec![0],
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        }],
    };
    file.validate().unwrap();
    assert_eq!(
        select_compiled(&file.entries[0], &file.nodes, 64),
        Err(SelectionError::NotinlineCall { pc: 2, symbol: 0 })
    );

    file.entries[0].notinline.clear();
    file.validate().unwrap();
    assert!(select_compiled(&file.entries[0], &file.nodes, 64).is_ok());
}

#[test]
fn range_proofs_are_path_local_and_preserve_the_bytecode_failure_arm() {
    use Instruction::*;
    let ops = [
        UnlessFixnumInRange(0, -100, 100, 4),
        TryFixnumArithmetic {
            op: FixnumArithmetic::Inc,
            out: 0,
            symbol: 0,
            args: 0..1,
        },
        SetValues(0..1),
        Jump(5),
        WriteCell { symbol: 1, src: 0 },
    ];
    for bits in [32, 64] {
        let leaf = select_plain(1, 1, &ops, bits).unwrap();
        assert!(leaf.may_miss());
        let Exit::Branch { yes, no, .. } = leaf.blocks()[0].exit else {
            panic!("expected a type test")
        };
        assert!(matches!(
            &leaf.blocks()[yes].steps[..],
            [Step::Compute {
                op: Operation::Arithmetic { checked: false, .. },
                ..
            }]
        ));
        assert_eq!(leaf.blocks()[no].exit, Exit::Miss);
        let mut ops = ops.clone();
        ops[4] = SetValues(0..1);
        let leaf = select_plain(1, 1, &ops, bits).unwrap();
        assert!(!leaf.may_miss());
        let Exit::Branch { no, .. } = leaf.blocks()[0].exit else {
            panic!("expected a type test")
        };
        assert_eq!(leaf.blocks()[no].exit, Exit::Return(Operand::Arg(0)));
    }
}

#[cfg(feature = "native")]
#[test]
fn native_targets_respect_word_width_endianness_and_abi() {
    use zkcl_vop::Target;
    for triple in [
        "x86_64-unknown-linux-gnux32",
        "aarch64-unknown-linux-gnu_ilp32",
        "aarch64_be-unknown-linux-gnu",
        "wasm32-unknown-unknown",
    ] {
        assert_eq!(
            Target::from_triple(&triple.parse().unwrap()),
            None,
            "{triple}"
        );
    }
    for triple in [
        "armv7-unknown-linux-gnueabi",
        "armv7-unknown-linux-gnueabihf",
        "armv7-linux-androideabi",
    ] {
        assert_eq!(
            Target::from_triple(&triple.parse().unwrap()),
            Some(Target::Armv7),
            "{triple}"
        );
    }
    let rv64 = Target::from_triple(&"riscv64gc-unknown-linux-gnu".parse().unwrap()).unwrap();
    assert_eq!(rv64, Target::Riscv64);
    let leaf = select_plain(1, 1, &[Instruction::SetValues(0..1)], 32).unwrap();
    assert!(leaf.emit(rv64).is_none());
}

/// `UNLESS-FIXNUM` jumps when the register is *not* a fixnum, so the fallthrough
/// edge is the success edge: it carries the fixnum range and the taken edge keeps
/// the type-failure flag.
#[test]
fn a_fixnum_test_refines_the_fallthrough_edge_and_flags_the_taken_one() {
    use Instruction::*;
    let ops = [
        UnlessFixnum(0, 3),
        TryFixnumArithmetic {
            op: FixnumArithmetic::Inc,
            out: 0,
            symbol: 0,
            args: 0..1,
        },
        Jump(4),
        WriteCell { symbol: 1, src: 0 },
        SetValues(0..1),
    ];
    let leaf = select_plain(1, 1, &ops, 64).unwrap();
    let Exit::Branch {
        value,
        test,
        yes,
        no,
    } = leaf.blocks()[0].exit
    else {
        panic!("expected a fixnum test");
    };
    assert_eq!(value, Operand::Arg(0));
    assert_eq!(test, Test::Fixnum);
    // The refined range removes the per-operand fixnum check on this edge; the
    // instruction folding into `YES` is exactly the increment.
    assert!(
        matches!(
            &leaf.blocks()[yes].steps[..],
            [Step::Compute {
                op: Operation::Arithmetic {
                    op: FixnumArithmetic::Inc,
                    ..
                },
                ..
            }]
        ),
        "success edge: {:?}",
        leaf.blocks()[yes].steps
    );
    // The taken edge is only reachable through a type failure, which is what
    // turns the unsupported `WRITE-CELL` behind it into a leaf miss.
    assert_eq!(leaf.blocks()[no].exit, Exit::Miss);
}

/// An empty bind range can arrive without a value snapshot. It is allowed, but it
/// still has to name registers that exist.
#[test]
fn an_empty_bind_range_still_validates_its_register_window() {
    use Instruction::*;
    assert_eq!(
        select_plain(1, 1, &[BindValues(9..9), SetValues(0..1)], 64),
        Err(SelectionError::Operand { pc: 0 })
    );
    let leaf = select_plain(1, 1, &[BindValues(0..0), SetValues(0..1)], 64).unwrap();
    assert_eq!(leaf.blocks()[0].exit, Exit::Return(Operand::Arg(0)));
}
