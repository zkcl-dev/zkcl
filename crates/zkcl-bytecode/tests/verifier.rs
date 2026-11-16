use zkcl_bytecode::{
    Arity, BytecodeFunction, CaptureKind, CellLocation, CompiledFile, Handler, HandlerTable,
    HandlerTarget, Immediate, Instruction, JumpTable, KeywordTable, LiteralNode, RestartSpec,
    RestartTable, RestartTarget, Test,
};

fn code_with(instructions: Vec<Instruction>) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity::ZERO,
        registers: 2,
        instructions,
        constants: vec![0],
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

fn single_entry_file(instructions: Vec<Instruction>) -> CompiledFile {
    CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![LiteralNode::UninternedSymbol("X".into())],
        entries: vec![code_with(instructions)],
    }
}

#[test]
fn rejects_unknown_source_file_indices() {
    let mut file = single_entry_file(vec![Instruction::SetValues(0..0)]);
    file.entries[0].source_file = Some(0);
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("unknown source file index")
    );
    file.source_files.push("program.lisp".into());
    file.validate().unwrap();

    let mut nested = code_with(vec![Instruction::SetValues(0..0)]);
    nested.source_file = Some(1);
    file.nodes.push(LiteralNode::Function(nested));
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("literal node 1: unknown source file index")
    );
}

#[test]
fn accepts_handler_case_after_cleanup_completes() {
    use Instruction::*;
    let mut file = single_entry_file(vec![
        EnterProtect {
            cleanup: 3,
            clear: 1,
        },
        Load(0, Immediate::Nil),
        BeginCleanup,
        EndCleanup,
        EnterHandlers { table: 0 },
        LeaveHandlers,
        Jump(8),
        SetValues(1..2),
    ]);
    file.entries[0].handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 7,
                condition: 1,
                clear: 2,
            },
        }],
    });
    file.validate().unwrap();
}

#[test]
fn accepts_circular_pair_literals() {
    let mut file = single_entry_file(vec![Instruction::Constant(0, 0)]);
    file.nodes.push(LiteralNode::Pair(0, 1));
    file.validate().unwrap();
}

#[test]
fn checks_fixnum_arithmetic_operand_count_and_frame_bounds() {
    use zkcl_bytecode::FixnumArithmetic::{Add, Inc};
    let validate_arithmetic = |op, symbol, out, args| {
        single_entry_file(vec![Instruction::TryFixnumArithmetic {
            op,
            symbol,
            out,
            args,
        }])
        .validate()
    };
    assert!(validate_arithmetic(Add, 0, 0, 0..2).is_ok());
    assert!(validate_arithmetic(Inc, 0, 1, 0..1).is_ok());
    assert!(validate_arithmetic(Inc, 0, 0, 0..2).is_err());
    assert!(validate_arithmetic(Add, 0, 0, 0..0).is_err());
    assert!(validate_arithmetic(Add, 0, 0, 1..3).is_err());
    assert!(validate_arithmetic(Inc, 0, 2, 0..1).is_err());
    assert!(validate_arithmetic(Inc, 1, 0, 0..1).is_err());
}

#[test]
fn accepts_go_to_a_label_in_the_active_tagbody() {
    use Instruction::*;
    let mut tagbody = single_entry_file(vec![
        ExitToken(0, 0),
        EnterTagbody {
            token: 0,
            jumps: 0,
            clear: 1,
        },
        Go {
            token: 0,
            label: 0,
            at: 0,
        },
        LeaveTarget,
    ]);
    tagbody.entries[0]
        .jump_tables
        .push(JumpTable { targets: vec![3] });
    tagbody.validate().unwrap();
}

#[test]
fn accepts_restart_case_landing_outside_its_scope() {
    use Instruction::*;
    let mut restarts = single_entry_file(vec![
        EnterRestarts { table: 0, at: 0 },
        LeaveRestarts,
        Jump(4),
        SetValues(0..1),
    ]);
    restarts.entries[0].restarts.push(RestartTable {
        restarts: vec![RestartSpec {
            name: 0,
            object: 0,
            test: None,
            interactive: None,
            report: None,
            target: RestartTarget::Case { pc: 3, clear: 1 },
        }],
    });
    restarts.validate().unwrap();
}

#[test]
fn lambda_list_metadata_requires_a_list_literal() {
    use Instruction::*;
    let mut lambda_list = single_entry_file(vec![Constant(0, 0)]);
    lambda_list.entries[0].lambda_list = Some(1);
    assert!(lambda_list.validate().is_err());
    lambda_list.entries[0].lambda_list = Some(0);
    assert!(lambda_list.validate().is_err());
    lambda_list.nodes[0] = LiteralNode::Nil;
    assert!(lambda_list.validate().is_ok());
}

#[test]
fn pathname_components_must_reference_existing_literals() {
    let mut file = single_entry_file(vec![Instruction::Constant(0, 0)]);
    file.nodes[0] = LiteralNode::Pathname([2; 6]);
    assert!(file.validate().is_err());
}

#[test]
fn literal_function_maximum_arity_must_cover_required_arguments() {
    let mut file = single_entry_file(vec![Instruction::Constant(0, 0)]);
    file.nodes[0] = LiteralNode::Function(BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity {
            required: 3,
            maximum: Some(2),
        },
        registers: 3,
        instructions: Vec::new(),
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
    });
    assert!(file.validate().is_err());
}

#[test]
fn rejects_invalid_instruction_operands() {
    use Instruction::*;
    for instructions in [
        vec![Clear(std::ops::Range { start: 2, end: 1 })],
        vec![Copy(0, 2)],
        vec![Closure {
            out: 0,
            code: 0,
            cells: 0..0,
            at: 0,
        }],
        vec![EnterRestarts {
            table: usize::MAX,
            at: 0,
        }],
        vec![EnterTagbody {
            token: 0,
            jumps: 0,
            clear: 1,
        }],
    ] {
        assert!(single_entry_file(instructions).validate().is_err());
    }
    assert_eq!(
        single_entry_file(vec![Constant(0, 1)])
            .validate()
            .unwrap_err()
            .to_string(),
        "entry 0, instruction 0: constant index is out of bounds"
    );
}

#[test]
fn fixnum_range_branches_require_valid_operands() {
    use Instruction::*;
    single_entry_file(vec![
        Load(0, Immediate::Fixnum(-128)),
        UnlessFixnumInRange(0, -128, 127, 3),
        SetValues(0..1),
    ])
    .validate()
    .unwrap();

    for branch in [
        UnlessFixnumInRange(0, 1, 0, 1),
        UnlessFixnumInRange(2, -1, 1, 1),
        UnlessFixnumInRange(0, -1, 1, 2),
    ] {
        assert!(single_entry_file(vec![branch]).validate().is_err());
    }
}

#[test]
fn rejects_unbinding_without_a_dynamic_binding() {
    assert!(
        single_entry_file(vec![Instruction::Unbind(1)])
            .validate()
            .is_err()
    );
}

#[test]
fn branch_join_requires_matching_dynamic_scopes() {
    use Instruction::*;
    assert!(
        single_entry_file(vec![
            Unless(0, 2),
            BindSpecial {
                symbol: 0,
                value: 1
            },
            Unbind(1),
        ])
        .validate()
        .is_err()
    );
}

#[test]
fn handler_case_cannot_close_its_discarded_scope() {
    use Instruction::*;
    let mut bad_handler_landing = single_entry_file(vec![
        EnterHandlers { table: 0 },
        LeaveHandlers,
        LeaveHandlers,
    ]);
    bad_handler_landing.entries[0].handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 2,
                condition: 1,
                clear: 2,
            },
        }],
    });
    assert!(bad_handler_landing.validate().is_err());
}

#[test]
fn restart_case_cannot_close_its_discarded_scope() {
    use Instruction::*;
    let mut bad_restart_landing = single_entry_file(vec![
        EnterRestarts { table: 0, at: 0 },
        LeaveRestarts,
        LeaveRestarts,
    ]);
    bad_restart_landing.entries[0].restarts.push(RestartTable {
        restarts: vec![RestartSpec {
            name: 0,
            object: 0,
            test: None,
            interactive: None,
            report: None,
            target: RestartTarget::Case { pc: 2, clear: 2 },
        }],
    });
    assert!(bad_restart_landing.validate().is_err());
}

#[test]
fn reading_a_boxed_capture_requires_an_existing_slot() {
    let mut bad_capture = single_entry_file(vec![Instruction::ReadBox {
        out: 0,
        cell: CellLocation::Capture(0),
    }]);
    assert!(bad_capture.validate().is_err());
    bad_capture.entries[0].captures = vec![CaptureKind::MutableCell];
    bad_capture.validate().unwrap();
}

#[test]
fn writing_a_boxed_capture_requires_a_mutable_cell() {
    let mut bad_capture = single_entry_file(vec![Instruction::WriteBox {
        cell: CellLocation::Capture(0),
        src: 0,
    }]);
    bad_capture.entries[0].captures = vec![CaptureKind::MutableCell];
    bad_capture.validate().unwrap();
    for kind in [CaptureKind::Value, CaptureKind::ExitToken] {
        bad_capture.entries[0].captures[0] = kind;
        assert!(
            bad_capture
                .validate()
                .unwrap_err()
                .to_string()
                .contains("immutable closure capture")
        );
    }
}

#[test]
fn closure_requires_one_operand_per_target_capture() {
    let mut bad_closure = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::UninternedSymbol("X".into()),
            LiteralNode::Function(BytecodeFunction {
                name: None,
                source_file: None,
                debug: Default::default(),
                arity: Arity::ZERO,
                registers: 1,
                instructions: vec![Instruction::ReadBox {
                    out: 0,
                    cell: CellLocation::Capture(0),
                }],
                constants: Vec::new(),
                lambda_list: None,
                captures: vec![CaptureKind::MutableCell],
                notinline: Vec::new(),
                vop_calls: Vec::new(),
                handlers: Vec::new(),
                restarts: Vec::new(),
                keywords: Vec::new(),
                jump_tables: Vec::new(),
                facts: Default::default(),
            }),
        ],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 2,
            instructions: vec![Instruction::Closure {
                out: 0,
                code: 0,
                cells: 0..0,
                at: 0,
            }],
            constants: vec![1],
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        }],
    };
    assert!(
        bad_closure
            .validate()
            .unwrap_err()
            .to_string()
            .contains("closure capture count does not match target code")
    );
    bad_closure.entries[0].instructions[0] = Instruction::Closure {
        out: 0,
        code: 0,
        cells: 0..1,
        at: 0,
    };
    bad_closure.validate().unwrap();
}

#[test]
fn notinline_declarations_must_reference_symbol_constants() {
    let mut file = single_entry_file(vec![]);
    file.entries[0].notinline = vec![0];
    file.validate().unwrap();

    file.nodes[0] = LiteralNode::Function(code_with(vec![]));
    assert!(file.validate().is_err());
}

#[test]
fn rejects_invalid_tagbody_jump_tables() {
    use Instruction::*;

    let mut empty_jumps = single_entry_file(vec![
        ExitToken(0, 0),
        EnterTagbody {
            token: 0,
            jumps: 0,
            clear: 1,
        },
        LeaveTarget,
    ]);
    empty_jumps.entries[0]
        .jump_tables
        .push(JumpTable { targets: vec![] });
    assert!(empty_jumps.validate().is_err());

    let mut oob_jump_target = single_entry_file(vec![
        ExitToken(0, 0),
        EnterTagbody {
            token: 0,
            jumps: 0,
            clear: 1,
        },
        LeaveTarget,
    ]);
    oob_jump_target.entries[0]
        .jump_tables
        .push(JumpTable { targets: vec![4] });
    assert!(oob_jump_target.validate().is_err());
}

#[test]
fn rejects_invalid_handler_table_references() {
    use Instruction::*;
    assert!(
        single_entry_file(vec![EnterHandlers { table: 0 }])
            .validate()
            .is_err()
    );

    let mut oob_handler_pc = single_entry_file(vec![EnterHandlers { table: 0 }, LeaveHandlers]);
    oob_handler_pc.entries[0].handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 3,
                condition: 0,
                clear: 1,
            },
        }],
    });
    assert!(oob_handler_pc.validate().is_err());

    let mut bad_handler_test = single_entry_file(vec![EnterHandlers { table: 0 }, LeaveHandlers]);
    bad_handler_test.entries[0].handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Named(1),
            target: HandlerTarget::Function(0),
        }],
    });
    assert!(bad_handler_test.validate().is_err());
    bad_handler_test.entries[0].handlers[0].handlers[0].test = Test::Named(0);
    bad_handler_test.nodes[0] = LiteralNode::Fixnum(12);
    assert!(bad_handler_test.validate().is_err());
}

#[test]
fn rejects_invalid_restart_table_entries() {
    use Instruction::*;
    let mut oob_restart_pc =
        single_entry_file(vec![EnterRestarts { table: 0, at: 0 }, LeaveRestarts]);
    oob_restart_pc.entries[0].restarts.push(RestartTable {
        restarts: vec![RestartSpec {
            name: 0,
            object: 0,
            test: None,
            interactive: None,
            report: None,
            target: RestartTarget::Case { pc: 3, clear: 1 },
        }],
    });
    assert!(oob_restart_pc.validate().is_err());

    let mut bad_restart_name =
        single_entry_file(vec![EnterRestarts { table: 0, at: 0 }, LeaveRestarts]);
    bad_restart_name.entries[0].restarts.push(RestartTable {
        restarts: vec![RestartSpec {
            name: 1,
            object: 0,
            test: None,
            interactive: None,
            report: None,
            target: RestartTarget::Function(0),
        }],
    });
    assert!(bad_restart_name.validate().is_err());
}

#[test]
fn keyword_table_operands_must_match_the_table_schema() {
    use Instruction::*;
    let kw = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::UninternedSymbol("ALLOW".into()),
            LiteralNode::UninternedSymbol("K1".into()),
            LiteralNode::Fixnum(42),
        ],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 3,
            instructions: vec![
                CheckKeywords { start: 1, table: 0 },
                DestructureKeywords {
                    cursor: 0,
                    table: 0,
                    out: 1..2,
                    macro_call: false,
                    at: 0,
                },
            ],
            constants: vec![0, 1, 2],
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: vec![KeywordTable {
                names: vec![1],
                allow_other_keys: false,
                override_key: 0,
            }],
            jump_tables: Vec::new(),
            facts: Default::default(),
        }],
    };
    kw.validate().unwrap();

    // CheckKeywords out-of-bounds start register bound.
    let mut bad_check_start = kw.clone();
    bad_check_start.entries[0].instructions[0] = CheckKeywords { start: 4, table: 0 };
    assert!(bad_check_start.validate().is_err());

    // CheckKeywords out-of-bounds table index.
    let mut bad_check_table = kw.clone();
    bad_check_table.entries[0].instructions[0] = CheckKeywords { start: 1, table: 1 };
    assert!(bad_check_table.validate().is_err());

    // DestructureKeywords out-of-bounds table index.
    let mut bad_destructure_table = kw.clone();
    bad_destructure_table.entries[0].instructions[1] = DestructureKeywords {
        cursor: 0,
        table: 1,
        out: 1..2,
        macro_call: false,
        at: 0,
    };
    assert!(bad_destructure_table.validate().is_err());

    // DestructureKeywords out.len() mismatch.
    let mut bad_destructure_len = kw.clone();
    bad_destructure_len.entries[0].instructions[1] = DestructureKeywords {
        cursor: 0,
        table: 0,
        out: 1..3,
        macro_call: false,
        at: 0,
    };
    assert!(bad_destructure_len.validate().is_err());

    // KeywordTable invalid override_key constant index.
    let mut bad_override = kw.clone();
    bad_override.entries[0].keywords[0].override_key = 99;
    assert!(bad_override.validate().is_err());

    // KeywordTable non-symbol constant in names.
    let mut bad_name_const = kw.clone();
    bad_name_const.entries[0].keywords[0].names = vec![2];
    assert!(bad_name_const.validate().is_err());

    // KeywordTable out-of-bounds constant index in names.
    let mut oob_name_const = kw;
    oob_name_const.entries[0].keywords[0].names = vec![99];
    assert!(oob_name_const.validate().is_err());
}

#[test]
fn capture_instruction_requires_an_existing_slot() {
    use Instruction::*;

    let mut cap_file = single_entry_file(vec![Capture(0, 0)]);
    assert!(cap_file.validate().is_err());
    cap_file.entries[0].captures = vec![CaptureKind::Value];
    cap_file.validate().unwrap();
    cap_file.entries[0].instructions[0] = Capture(0, 1);
    assert!(cap_file.validate().is_err());
}

#[test]
fn variable_operations_require_mutable_symbols() {
    use Instruction::*;

    for constant_node in [
        LiteralNode::Nil,
        LiteralNode::True,
        LiteralNode::InternedSymbol {
            package: "KEYWORD".into(),
            name: "FOO".into(),
        },
        LiteralNode::InternedSymbol {
            package: "COMMON-LISP".into(),
            name: "PI".into(),
        },
        LiteralNode::InternedSymbol {
            package: "CL".into(),
            name: "MOST-POSITIVE-FIXNUM".into(),
        },
    ] {
        for ins in [
            WriteCell { symbol: 0, src: 0 },
            BindSpecial {
                symbol: 0,
                value: 0,
            },
            ProclaimSpecial { symbol: 0, at: 0 },
        ] {
            let mut file = single_entry_file(match ins {
                BindSpecial { .. } => vec![ins, Unbind(1)],
                _ => vec![ins],
            });
            file.nodes[0] = constant_node.clone();
            assert!(file.validate().is_err());
        }
    }

    let mut mutable = single_entry_file(vec![WriteCell { symbol: 0, src: 0 }]);
    mutable.nodes[0] = LiteralNode::InternedSymbol {
        package: "COMMON-LISP-USER".into(),
        name: "*X*".into(),
    };
    mutable.validate().unwrap();
}

fn function_file(
    arity: Arity,
    instructions: Vec<Instruction>,
    keywords: Vec<KeywordTable>,
) -> CompiledFile {
    CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::InternedSymbol {
                package: "KEYWORD".into(),
                name: "ALLOW-OTHER-KEYS".into(),
            },
            LiteralNode::InternedSymbol {
                package: "KEYWORD".into(),
                name: "K1".into(),
            },
            LiteralNode::Function(BytecodeFunction {
                name: None,
                source_file: None,
                debug: Default::default(),
                arity,
                registers: 4,
                instructions,
                constants: vec![0, 1],
                lambda_list: None,
                captures: Vec::new(),
                notinline: Vec::new(),
                vop_calls: Vec::new(),
                handlers: Vec::new(),
                restarts: Vec::new(),
                keywords,
                jump_tables: Vec::new(),
                facts: Default::default(),
            }),
        ],
        entries: vec![],
    }
}

#[test]
fn optional_argument_indices_respect_bounded_arity() {
    use Instruction::*;

    // OptionalArgument bounded by maximum - required.
    assert!(
        function_file(
            Arity {
                required: 1,
                maximum: Some(3),
            },
            vec![OptionalArgument(0, 1), OptionalArgument(1, 2)],
            Vec::new(),
        )
        .validate()
        .is_ok()
    );
    assert!(
        function_file(
            Arity {
                required: 1,
                maximum: Some(3),
            },
            vec![OptionalArgument(2, 1)],
            Vec::new(),
        )
        .validate()
        .is_err()
    );
    // Unbounded arity allows OptionalArgument.
    assert!(
        function_file(
            Arity {
                required: 1,
                maximum: None,
            },
            vec![OptionalArgument(3, 1)],
            Vec::new(),
        )
        .validate()
        .is_ok()
    );
}

fn keyword_argument_table() -> KeywordTable {
    KeywordTable {
        names: vec![1],
        allow_other_keys: false,
        override_key: 0,
    }
}

#[test]
fn keyword_arguments_require_unbounded_arity() {
    use Instruction::*;
    let instructions = vec![
        CheckKeywords { start: 0, table: 0 },
        KeywordArgument(0, 0),
        ReleaseArguments,
    ];
    assert!(
        function_file(
            Arity {
                required: 0,
                maximum: None,
            },
            instructions.clone(),
            vec![keyword_argument_table()],
        )
        .validate()
        .is_ok()
    );
    assert!(
        function_file(
            Arity {
                required: 0,
                maximum: Some(2),
            },
            instructions,
            vec![keyword_argument_table()],
        )
        .validate()
        .is_err()
    );
}

#[test]
fn keyword_argument_indices_must_fit_a_checked_table() {
    use Instruction::*;
    assert!(
        function_file(
            Arity {
                required: 0,
                maximum: None,
            },
            vec![KeywordArgument(0, 0)],
            vec![keyword_argument_table()],
        )
        .validate()
        .is_err()
    );
    assert!(
        function_file(
            Arity {
                required: 0,
                maximum: None,
            },
            vec![
                CheckKeywords { start: 0, table: 0 },
                KeywordArgument(1, 0),
                ReleaseArguments,
            ],
            vec![keyword_argument_table()],
        )
        .validate()
        .is_err()
    );
}

#[test]
fn accepts_lexical_scope_markers_after_return_from() {
    use Instruction::*;

    // Compiler-style dead LeaveTarget after ReturnFrom is valid.
    single_entry_file(vec![
        ExitToken(0, 0),
        EnterBlock {
            token: 0,
            resume: 4,
            clear: 1,
        },
        ReturnFrom { token: 0, at: 0 },
        LeaveTarget,
        Load(0, Immediate::Nil),
    ])
    .validate()
    .unwrap();

    // Compiler-style dead BeginCleanup after ReturnFrom inside EnterProtect is valid.
    single_entry_file(vec![
        EnterProtect {
            cleanup: 3,
            clear: 0,
        },
        ReturnFrom { token: 0, at: 0 },
        BeginCleanup,
        EndCleanup,
    ])
    .validate()
    .unwrap();
}

#[test]
fn rejects_invalid_scope_transitions_in_unreachable_blocks() {
    use Instruction::*;

    // Unreachable scope close must still match the active scope.
    assert!(
        single_entry_file(vec![Jump(2), Unbind(1)])
            .validate()
            .is_err()
    );
    assert!(
        single_entry_file(vec![Jump(2), BeginCleanup])
            .validate()
            .is_err()
    );

    // Unreachable block jumping into reachable block with mismatched scopes must be rejected.
    assert!(
        single_entry_file(vec![
            Jump(3),
            BindSpecial {
                symbol: 0,
                value: 1,
            },
            Jump(3),
            Load(0, Immediate::Nil),
        ])
        .validate()
        .is_err()
    );

    // Unreachable loop leaking a dynamic binding must be rejected.
    assert!(
        single_entry_file(vec![
            Jump(3),
            BindSpecial {
                symbol: 0,
                value: 1,
            },
            Jump(1),
            Load(0, Immediate::Nil),
        ])
        .validate()
        .is_err()
    );
}
