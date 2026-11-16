#![cfg(feature = "cranelift")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use zkcl_bytecode::{
    Arity, BytecodeFunction, Callee, CompiledFile, Handler, HandlerTable, HandlerTarget, Immediate,
    Instruction, JumpTable, LiteralNode, Test,
};
use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::Runtime;

fn code(arity: Arity, registers: usize, instructions: Vec<Instruction>) -> BytecodeFunction {
    BytecodeFunction {
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
    }
}

fn check_vm_and_native(function: BytecodeFunction, cases: &[(&str, &str)]) {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let mut entry = code(
        Arity::ZERO,
        1,
        vec![
            Instruction::Constant(0, 1),
            Instruction::DefineFunction {
                symbol: 0,
                definition: 0,
                is_macro: false,
                at: 0,
            },
        ],
    );
    entry.constants = vec![0, 1];
    let file = CompiledFile {
        target: runtime.compilation_target().clone(),
        source_files: vec![],
        nodes: vec![
            LiteralNode::InternedSymbol {
                package: "CL-USER".into(),
                name: "RECOVERY-PROBE".into(),
            },
            LiteralNode::Function(function),
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP".into(),
                name: "ERROR".into(),
            },
        ],
        entries: vec![entry],
    };
    file.validate().expect("valid external bytecode");
    runtime.load_fasl(&file.encode().unwrap()).unwrap();
    for &(form, expected) in cases {
        let result = runtime.eval(form).unwrap();
        assert_eq!(runtime.repr(&result[0]).unwrap(), expected, "VM {form}");
    }

    let backend = Arc::new(CraneliftJitBackend::new().unwrap());
    runtime.set_jit_backend(backend.clone());
    let function = runtime.eval("#'recovery-probe").unwrap().pop().unwrap();
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    runtime
        .call(&compile, &[&runtime.nil(), &function])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(
            Instant::now() < deadline,
            "native compilation did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(backend.code_cache().len(), 1);
    for &(form, expected) in cases {
        let result = runtime.eval(form).unwrap();
        assert_eq!(runtime.repr(&result[0]).unwrap(), expected, "native {form}");
    }
}

#[test]
fn ordinary_jumps_and_fallthrough_keep_registers_at_a_recovery_destination() {
    for jump in [false, true] {
        let resume = if jump { 5 } else { 4 };
        let mut instructions = vec![
            Instruction::Load(0, Immediate::Nil),
            Instruction::EnterCatch {
                tag: 0,
                resume,
                clear: 1,
            },
            Instruction::Load(1, Immediate::Fixnum(42)),
            Instruction::LeaveTarget,
        ];
        if jump {
            instructions.push(Instruction::Jump(resume));
        }
        instructions.push(Instruction::SetValues(1..2));
        check_vm_and_native(
            code(Arity::ZERO, 2, instructions),
            &[("(recovery-probe)", "42")],
        );
    }
}

#[test]
fn runtime_recovery_clears_registers_only_for_its_own_registration() {
    check_vm_and_native(
        code(
            Arity {
                required: 1,
                maximum: Some(1),
            },
            3,
            vec![
                Instruction::Load(1, Immediate::Nil),
                Instruction::Unless(0, 5),
                Instruction::EnterCatch {
                    tag: 1,
                    resume: 8,
                    clear: 2,
                },
                Instruction::Load(2, Immediate::Fixnum(42)),
                Instruction::Throw { tag: 1, at: 0 },
                Instruction::EnterCatch {
                    tag: 1,
                    resume: 8,
                    clear: 3,
                },
                Instruction::Load(2, Immediate::Fixnum(42)),
                Instruction::Throw { tag: 1, at: 0 },
                Instruction::SetValues(2..3),
            ],
        ),
        &[
            ("(recovery-probe t)", "NIL"),
            ("(recovery-probe nil)", "42"),
        ],
    );
}

#[test]
fn local_go_clears_registers_while_a_plain_jump_to_the_same_label_preserves_them() {
    for go in [false, true] {
        let transfer = if go {
            Instruction::Go {
                token: 0,
                label: 0,
                at: 0,
            }
        } else {
            Instruction::Jump(4)
        };
        let mut function = code(
            Arity::ZERO,
            2,
            vec![
                Instruction::ExitToken(0, 0),
                Instruction::EnterTagbody {
                    token: 0,
                    jumps: 0,
                    clear: 1,
                },
                Instruction::Load(1, Immediate::Fixnum(42)),
                transfer,
                Instruction::LeaveTarget,
                Instruction::SetValues(1..2),
            ],
        );
        function.jump_tables.push(JumpTable { targets: vec![4] });
        check_vm_and_native(
            function,
            &[("(recovery-probe)", if go { "NIL" } else { "42" })],
        );
    }
}

#[test]
fn normal_cleanup_clears_abandoned_registers_before_resuming_bytecode() {
    check_vm_and_native(
        code(
            Arity::ZERO,
            2,
            vec![
                Instruction::EnterProtect {
                    cleanup: 4,
                    clear: 1,
                },
                Instruction::Load(1, Immediate::Fixnum(42)),
                Instruction::SetValues(1..2),
                Instruction::BeginCleanup,
                Instruction::EndCleanup,
                Instruction::SetValues(1..2),
            ],
        ),
        &[("(recovery-probe)", "NIL")],
    );
}

#[test]
fn ordinary_handler_destination_keeps_its_register_value() {
    let mut function = code(
        Arity::ZERO,
        1,
        vec![
            Instruction::EnterHandlers { table: 0 },
            Instruction::Load(0, Immediate::Fixnum(42)),
            Instruction::LeaveHandlers,
            Instruction::Jump(4),
            Instruction::SetValues(0..1),
        ],
    );
    function.handlers.push(HandlerTable {
        handlers: vec![Handler {
            test: Test::Any,
            target: HandlerTarget::Case {
                pc: 4,
                condition: 0,
                clear: 1,
            },
        }],
    });
    check_vm_and_native(function, &[("(recovery-probe)", "42")]);
}

#[test]
fn handler_recovery_supplies_the_condition_register_for_each_registration() {
    let error = || Instruction::Call {
        callee: Callee::Named(0),
        out: None,
        args: 0..0,
        at: 0,
        associate: false,
        notinline: false,
    };
    let mut function = code(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        3,
        vec![
            Instruction::Load(1, Immediate::Nil),
            Instruction::Load(2, Immediate::Nil),
            Instruction::Unless(0, 7),
            Instruction::EnterHandlers { table: 0 },
            error(),
            Instruction::LeaveHandlers,
            Instruction::Jump(11),
            Instruction::EnterHandlers { table: 1 },
            error(),
            Instruction::LeaveHandlers,
            Instruction::Jump(11),
            Instruction::Unless(1, 14),
            Instruction::Load(0, Immediate::Fixnum(41)),
            Instruction::Jump(15),
            Instruction::Load(0, Immediate::Fixnum(42)),
            Instruction::SetValues(0..1),
        ],
    );
    function.constants.push(2);
    function.handlers = [1, 2]
        .map(|condition| HandlerTable {
            handlers: vec![Handler {
                test: Test::Any,
                target: HandlerTarget::Case {
                    pc: 11,
                    condition,
                    clear: 3,
                },
            }],
        })
        .into();
    check_vm_and_native(
        function,
        &[("(recovery-probe t)", "41"), ("(recovery-probe nil)", "42")],
    );
}

#[test]
fn backedges_to_bytecode_entry_keep_nonargument_registers() {
    check_vm_and_native(
        code(
            Arity {
                required: 1,
                maximum: Some(1),
            },
            2,
            vec![
                Instruction::Unless(0, 4),
                Instruction::Load(1, Immediate::Fixnum(42)),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Jump(0),
                Instruction::SetValues(1..2),
            ],
        ),
        &[
            ("(recovery-probe nil)", "NIL"),
            ("(recovery-probe t)", "42"),
        ],
    );
}
