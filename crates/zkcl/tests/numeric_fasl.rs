use std::fs;

use zkcl::Runtime;
use zkcl_bytecode::{Arity, BytecodeFunction, CompiledFile, Instruction, LiteralNode};

mod common;

use common::files::TempDir;
use common::lisp::eval;

#[test]
fn numeric_printer_produces_readable_representations() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (
            "(prin1-to-string 123456789012345678901234567890)",
            "123456789012345678901234567890",
        ),
        (
            "(write-to-string 18446744073709551616 :base 16 :radix t)",
            "#x10000000000000000",
        ),
        (
            r#"(format nil "~:@D" 123456789012345678901234567890)"#,
            "+123,456,789,012,345,678,901,234,567,890",
        ),
        ("(write-to-string -10/3 :base 16 :radix t)", "#x-A/3"),
        ("(write-to-string 1/3 :base 10 :radix t)", "#10r1/3"),
        (
            "(write-to-string #c(1/3 2/3) :base 2 :radix t)",
            "#C(#b1/11 #b10/11)",
        ),
        ("(prin1-to-string 1.5)", "1.5"),
        ("(prin1-to-string -0.0d0)", "-0.0d0"),
        ("(prin1-to-string 1.0e-12)", "1.0e-12"),
        ("(prin1-to-string 1.25d7)", "1.25d7"),
        (
            "(let ((*read-default-float-format* 'double-float)) (prin1-to-string 1.5d0))",
            "1.5",
        ),
        (
            "(let ((*read-default-float-format* 'double-float)) (prin1-to-string 1.5f0))",
            "1.5f0",
        ),
    ] {
        let value = eval(&runtime, source);
        assert_eq!(
            runtime.string(&value[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }

    let roundtrip = eval(
        &runtime,
        r#"(let* ((numbers '#c(18446744073709551616/3 7/5))
                  (*read-base* 16)
                  (fixed (write-to-string 42 :base 10 :readably t))
                  (printed (write-to-string numbers :base 3 :readably t))
                  (single (write-to-string 1.5f0 :readably t)))
             (let ((*read-default-float-format* 'double-float))
               (and (eql (read-from-string fixed) 42)
                    (eql (read-from-string single) 1.5f0)
                    (= (read-from-string printed) numbers))))"#,
    );
    assert!(!roundtrip[0].is_nil());
}

#[test]
fn compiled_numeric_literals_round_trip_without_source() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("numeric-fasl");
    let input = temp_dir.path().join("numbers.lisp");
    fs::write(
        &input,
        r#"(defun saved-numeric-values ()
              (values 123456789012345678901234567890
                      -18446744073709551616/3
                      -0.0f0
                      #c(1.25d7 -0.0d0)
                      #c(1/3 2/3)))"#,
    )
    .unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    let file = CompiledFile::decode(&fs::read(&compiled).unwrap()).unwrap();
    assert!(
        file.nodes
            .iter()
            .any(|node| matches!(node, LiteralNode::Bignum { .. }))
    );
    assert!(
        file.nodes
            .iter()
            .any(|node| matches!(node, LiteralNode::Ratio(..)))
    );
    assert!(
        file.nodes
            .contains(&LiteralNode::SingleFloat((-0.0_f32).to_bits()))
    );
    assert!(
        file.nodes
            .contains(&LiteralNode::DoubleFloat((-0.0_f64).to_bits()))
    );
    assert_eq!(
        file.nodes
            .iter()
            .filter(|node| matches!(node, LiteralNode::Complex(..)))
            .count(),
        2
    );
    fs::remove_file(input).unwrap();
    runtime.collect();
    runtime.load_file(&compiled).unwrap();
    runtime.collect();

    let loaded = eval(&runtime, "(saved-numeric-values)");
    for (value, expected) in loaded.iter().zip([
        "123456789012345678901234567890",
        "-18446744073709551616/3",
        "-0.0",
        "#C(1.25d7 -0.0d0)",
        "#C(1/3 2/3)",
    ]) {
        assert_eq!(runtime.repr(value).unwrap(), expected);
    }
    assert_eq!(loaded.len(), 5);
}

#[test]
fn compiled_files_reject_unreduced_ratio_literals() {
    let unreduced = CompiledFile {
        target: zkcl::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Fixnum(6),
            LiteralNode::Fixnum(9),
            LiteralNode::Ratio(0, 1),
        ],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 1,
            instructions: vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            constants: vec![2],
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
    let error = unreduced.encode().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ratio literal must be in lowest terms")
    );
}
