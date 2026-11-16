use std::fs;

use zkcl::{ConditionClass, EvalErrorKind, Runtime};
use zkcl_bytecode::{Arity, BytecodeFunction, CellLocation, CompiledFile, Instruction};

mod common;

use common::files::TempDir;

#[test]
fn a_failed_fasl_load_leaves_the_runtime_usable() {
    use Instruction::*;

    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("invalid-operands");
    let path = directory.path().join("invalid-operands.zkclfasl");
    for (instructions, class) in [
        (vec![Capture(0, 0)], ConditionClass::FileError),
        (
            vec![ReadBox {
                out: 0,
                cell: CellLocation::Local(0),
            }],
            ConditionClass::ProgramError,
        ),
        (vec![KeywordArgument(1, 0)], ConditionClass::FileError),
        (
            vec![
                EnterBlock {
                    token: 0,
                    resume: 2,
                    clear: 1,
                },
                LeaveTarget,
            ],
            ConditionClass::ProgramError,
        ),
        (
            vec![
                ExitToken(0, 0),
                EnterTagbody {
                    token: 0,
                    jumps: 0,
                    clear: 1,
                },
                Go {
                    token: 0,
                    label: usize::MAX,
                    at: 0,
                },
                LeaveTarget,
            ],
            ConditionClass::ControlError,
        ),
    ] {
        let jump_tables = if instructions
            .iter()
            .any(|ins| matches!(ins, EnterTagbody { .. }))
        {
            vec![zkcl_bytecode::JumpTable {
                targets: vec![2, 3],
            }]
        } else {
            Vec::new()
        };
        let file = CompiledFile {
            target: zkcl::CompilationTarget::native(),
            source_files: Vec::new(),
            nodes: Vec::new(),
            entries: vec![BytecodeFunction {
                name: None,
                source_file: None,
                debug: Default::default(),
                arity: Arity::ZERO,
                registers: 1,
                instructions,
                constants: Vec::new(),
                lambda_list: None,
                captures: Vec::new(),
                notinline: Vec::new(),
                vop_calls: Vec::new(),
                handlers: Vec::new(),
                restarts: Vec::new(),
                keywords: Vec::new(),
                jump_tables,
                facts: Default::default(),
            }],
        };
        let bytes = postcard::to_extend(&file, zkcl_bytecode::FASL_MAGIC.to_vec()).unwrap();
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            runtime.load_file(&path).unwrap_err().kind(),
            EvalErrorKind::LispCondition(class)
        );
        assert_eq!(runtime.eval("(+ 21 21)").unwrap()[0].as_fixnum(), Some(42));
    }
}
