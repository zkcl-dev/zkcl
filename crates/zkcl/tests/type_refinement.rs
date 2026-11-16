use zkcl::Runtime;
use zkcl_bytecode::{Callee, Instruction, LiteralNode};

mod common;
use common::{files::TempDir, lisp::eval_reprs};

#[test]
fn bounded_arithmetic_selects_checked_instructions_and_preserves_dynamic_calls() {
    use zkcl_bytecode::FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
    let source = "
        (defun bounded-inc (x) (declare (type (integer 0 1000) x)) (1+ x))
        (defun bounded-ops (x y) (declare (type (integer -20 20) x y))
          (values (+ x y) (- x y) (* x y) (1- x)))
        (defun unrestricted-inc (x) (declare (fixnum x)) (1+ x))
        (defun dynamic-inc (x) (declare (type bit x) (notinline 1+)) (1+ x))";
    let dir = TempDir::new("fixnum-arithmetic");
    let input = dir.write("bounded.lisp", source);
    let rt = Runtime::start().unwrap();
    let fasl = rt.compile_file(&input, None).unwrap();
    let file = zkcl_bytecode::CompiledFile::decode(&std::fs::read(&fasl).unwrap()).unwrap();
    let selected = file
        .nodes
        .iter()
        .filter_map(|node| match node {
            LiteralNode::Function(code) => Some(&code.instructions),
            _ => None,
        })
        .flatten()
        .filter_map(|ins| match ins {
            Instruction::TryFixnumArithmetic { op, .. } => Some(*op),
            _ => None,
        })
        .collect::<Vec<_>>();
    for op in [Inc, Add, Sub, Mul, Dec] {
        assert_eq!(
            selected.iter().filter(|&&found| found == op).count(),
            1,
            "{selected:?}"
        );
    }
    for from_fasl in [false, true] {
        if from_fasl {
            rt.load_file(&fasl).unwrap();
        } else {
            rt.load_str(source).unwrap();
        }
        assert_eq!(
            eval_reprs(
                &rt,
                "
          (values (bounded-inc 1000) (multiple-value-list (bounded-ops -20 20))
                  (typep (unrestricted-inc most-positive-fixnum) 'bignum)
                  (dynamic-inc 1)
                  (handler-case (bounded-inc 1001) (type-error (c) (type-error-datum c)))
                  (handler-case (bounded-inc :x) (type-error (c) (type-error-datum c))))"
            ),
            ["1001", "(0 -40 -400 -21)", "T", "2", "1001", ":X"]
        );
        assert_eq!(
            eval_reprs(
                &rt,
                r#"
          (let ((*trace-output* (make-string-output-stream)))
            (trace 1+)
            (unwind-protect
                (values (bounded-inc 7)
                        (not (null (search "TRACE 1+: enter" (get-output-stream-string *trace-output*)))))
              (untrace 1+)))"#
            ),
            ["8", "T"]
        );
    }
}

#[test]
fn fasl_arithmetic_checks_operands_and_the_live_function_binding() {
    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("arithmetic-fasl");
    let input = dir.write(
        "unguarded.lisp",
        "(defun unguarded-inc (x) (declare (type (integer 0 1000) x)) (1+ x))",
    );
    let mut file = rt.compile_file_to_bytecode(&input).unwrap();
    let mut removed = 0;
    for node in &mut file.nodes {
        let LiteralNode::Function(code) = node else {
            continue;
        };
        for (pc, ins) in code.instructions.iter_mut().enumerate() {
            if matches!(ins, Instruction::UnlessFixnumInRange(..)) {
                *ins = Instruction::Jump(pc + 1);
                removed += 1;
            }
        }
    }
    assert_eq!(removed, 1);
    let path = dir.write("unguarded.fasl", file.encode().unwrap());
    rt.load_file(&path).unwrap();
    assert_eq!(
        eval_reprs(
            &rt,
            "(values (typep (unguarded-inc most-positive-fixnum) 'bignum)
        (unguarded-inc 1/2)
        (handler-case (unguarded-inc :bad) (type-error (c) (type-error-datum c))))"
        ),
        ["T", "3/2", ":BAD"]
    );

    // Redirect the same call to an ordinary, redefinable symbol whose first
    // binding really is 1+. The compiler's operation cannot stand in for that
    // symbol's current function after redefinition.
    let alias = file
        .nodes
        .iter_mut()
        .find(|node| matches!(node, LiteralNode::InternedSymbol { name, .. } if name == "1+"))
        .unwrap();
    *alias = LiteralNode::InternedSymbol {
        package: "COMMON-LISP-USER".into(),
        name: "MUTABLE-INCREMENT".into(),
    };
    rt.load_str("(setf (symbol-function 'mutable-increment) #'1+)")
        .unwrap();
    let path = dir.write("rebound.fasl", file.encode().unwrap());
    rt.load_file(&path).unwrap();
    assert_eq!(eval_reprs(&rt, "(unguarded-inc 7)"), ["8"]);
    rt.load_str("(defun mutable-increment (x) (values :replaced x))")
        .unwrap();
    assert_eq!(eval_reprs(&rt, "(unguarded-inc 7)"), [":REPLACED", "7"]);
}

#[test]
fn compiled_code_elides_provable_type_checks() {
    let runtime = Runtime::start().unwrap();
    let dir = TempDir::new("type-refinement");
    for (source, expected_calls, expected_checks, expected_ranges) in [
        ("(defun classify (x) (if (not (integerp x)) (typep x 'fixnum) (rationalp x)))",
            &[("INTEGERP", 1), ("TYPEP", 0), ("RATIONALP", 0)][..], 0, 0),
        ("(defun inline-guard (x) (declare (inline typep)) (if (typep x 'fixnum) :small :other))",
            &[("TYPEP", 0)][..], 1, 0),
        ("(defun dynamic-guard (x) (declare (notinline typep)) (if (typep x 'fixnum) :small :other))",
            &[("TYPEP", 1)][..], 0, 0),
        ("(defun checked-sum (a b) (declare (type (integer (0) 128) a) (type (unsigned-byte 8) b))
            (the fixnum (+ (* a 2) b)))", &[("TYPEP", 0)][..], 0, 2),
        ("(defun split-interval (x) (declare (fixnum x))
           (if (> 127 x) (the fixnum (1+ x)) (the fixnum (1- x))))", &[(">", 1)][..], 1, 0),
        ("(defun still-real (x) (if (< x 127) (integerp x) t))", &[("INTEGERP", 1)][..], 0, 0),
        ("(defun list-complement (x) (if (listp x) :list (consp x)))", &[("CONSP", 0)][..], 0, 0),
        ("(defun bit-endpoint (x) (declare (bit x)) (values (<= x 1) (typep x '(integer 0 7))))",
            &[("<=", 0)][..], 0, 1),
        ("(defun checked-optional-default (x &optional (y (the fixnum x)))
            (declare (optimize (safety 0)) (fixnum x)) (values (integerp x) y))",
            &[("INTEGERP", 0)][..], 1, 0),
        ("(locally (declare (optimize (safety 0)))
            (defun checked-body (x &optional (y (the fixnum x)))
              (declare (optimize (safety 3))) (the fixnum y)))", &[][..], 1, 0),
        ("(defun optional-declared (x &optional (y x)) (declare (fixnum x))
            (values (the fixnum x) y))", &[][..], 1, 0),
        ("(defun maybe-overflows (a) (declare (type fixnum a)) (the fixnum (1+ a)))",
            &[][..], 2, 0),
        ("(defun speed-guard (x)
            (values (locally (declare (optimize speed)) (if (typep x 'fixnum) 1 2))
                    (locally (declare (optimize speed)) (if (typep x '(unsigned-byte 8)) 1 2))
                    (locally (declare (optimize speed debug)) (if (typep x 'fixnum) 1 2))
                    (locally (declare (notinline typep) (optimize speed)) (if (typep x 'bit) 1 2))))",
            &[("TYPEP", 2)][..], 1, 1),
    ] {
        let path = dir.write("branches.lisp", source);
        let fasl = runtime.compile_file(&path, None).unwrap();
        let file = zkcl_bytecode::CompiledFile::decode(&std::fs::read(&fasl).unwrap()).unwrap();
        let mut checks = 0;
        let mut ranges = 0;
        let mut calls = Vec::new();
        for node in &file.nodes {
            let LiteralNode::Function(code) = node else { continue };
            for instruction in &code.instructions {
                match instruction {
                    Instruction::UnlessFixnum(..) => checks += 1,
                    Instruction::UnlessFixnumInRange(..) => ranges += 1,
                    Instruction::Call { callee: Callee::Named(c), .. } => {
                        if let LiteralNode::InternedSymbol { name, .. } = &file.nodes[code.constants[*c] as usize] {
                            calls.push(name.as_str());
                        }
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(ranges, expected_ranges, "{source}: {calls:?}");
        assert_eq!(checks, expected_checks, "{source}: {calls:?}");
        for &(name, expected) in expected_calls {
            assert_eq!(calls.iter().filter(|&&s| s == name).count(), expected, "{source}: {calls:?}");
        }
        std::fs::remove_file(path).unwrap();
        runtime.load_file(&fasl).unwrap();
    }
    assert_eq!(
        eval_reprs(
            &runtime,
            "(values (classify -1) (classify 1/2) (inline-guard 0)
       (dynamic-guard :a) (checked-sum 128 255) (split-interval 126) (split-interval 127)
       (still-real 1/2) (list-complement :a) (multiple-value-list (bit-endpoint 1))
       (multiple-value-list (checked-optional-default 3)) (checked-body 4)
       (multiple-value-list (optional-declared 5)) (multiple-value-list (speed-guard 256))
       (handler-case (maybe-overflows most-positive-fixnum) (type-error () :overflow)))"
        ),
        [
            "T",
            "NIL",
            ":SMALL",
            ":OTHER",
            "511",
            "127",
            "126",
            "NIL",
            "NIL",
            "(T T)",
            "(T 3)",
            "4",
            "(5 5)",
            "(1 2 1 2)",
            ":OVERFLOW"
        ]
    );
}

#[test]
fn identity_returns_only_the_primary_value() {
    let rt = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &rt,
            "(values (multiple-value-list (identity (values))) (multiple-value-list (identity (values 1 2))))"
        ),
        ["(NIL)", "(1)"]
    );
}

#[test]
fn writes_invalidate_earlier_type_tests() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defun detach-keyword (k) (if (keywordp k) (progn (unintern k :keyword) (keywordp k)) :other))
        (defun assigned-guard (x) (if (integerp x) (progn (setq x :changed) (typep x 'fixnum)) t))").unwrap();
    assert_eq!(
        eval_reprs(
            &rt,
            r#"(values (detach-keyword (intern "REFINEMENT-TEMP" :keyword))
                      (assigned-guard 1))"#
        ),
        ["NIL", "NIL"]
    );
}

#[test]
fn locally_shadowed_typep_does_not_refine_its_argument() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defun local-guard (x) (flet ((typep (a b) (declare (ignore a b)) t)) (if (typep x 'fixnum) (integerp x) :other)))").unwrap();
    assert_eq!(eval_reprs(&rt, "(local-guard :q)"), ["NIL"]);
}

#[test]
fn satisfies_type_calls_the_user_predicate() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defun spec-callback (x) (declare (ignore x)) (incf *refinement-calls*) t)
        (defvar *refinement-calls* 0)",
    )
    .unwrap();
    assert_eq!(
        eval_reprs(
            &rt,
            "(values (typep 1 '(satisfies spec-callback)) *refinement-calls*)"
        ),
        ["T", "1"]
    );
}

#[test]
fn folded_integer_comparison_preserves_operand_order() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((seen nil))
        (values (< (the bit (progn (push :first seen) 0))
                   (the (integer 3 8) (progn (push :second seen) 4)))
                (reverse seen)))"
        ),
        ["T", "(:FIRST :SECOND)"]
    );
}

#[test]
fn invalid_byte_width_still_signals_an_error() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (typep 0 '(unsigned-byte 0)) (error () :invalid))"
        ),
        [":INVALID"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (the (unsigned-byte 0) 0) (error () :invalid))"
        ),
        [":INVALID"]
    );
}

#[test]
fn optimization_proclamations_are_runtime_local() {
    let dir = TempDir::new("proclaimed-optimize");
    let input = dir.write(
        "guard.lisp",
        "(defun fixnum-branch (x) (if (typep x 'fixnum) 1 0))",
    );
    let count_fixnum_branches = |rt: &Runtime| {
        let file = rt.compile_file_to_bytecode(&input).unwrap();
        file.nodes
            .iter()
            .filter_map(|node| match node {
                LiteralNode::Function(code) => Some(
                    code.instructions
                        .iter()
                        .filter(|op| matches!(op, Instruction::UnlessFixnum(..)))
                        .count(),
                ),
                _ => None,
            })
            .sum::<usize>()
    };
    let first = Runtime::start().unwrap();
    let second = Runtime::start().unwrap();
    first.eval("(proclaim '(optimize speed))").unwrap();
    assert_eq!(count_fixnum_branches(&first), 1);
    assert_eq!(count_fixnum_branches(&second), 0);
    first
        .eval("(if nil (declaim (optimize (speed 0))) t)")
        .unwrap();
    assert_eq!(count_fixnum_branches(&first), 1);
    first.eval("(proclaim '(optimize debug))").unwrap();
    assert_eq!(count_fixnum_branches(&first), 0);

    let decls = dir.write("decls.lisp", "(declaim (optimize (debug 0) speed))");
    let fasl = first.compile_file(&decls, None).unwrap();
    std::fs::remove_file(decls).unwrap();
    second.load_file(&fasl).unwrap();
    assert_eq!(count_fixnum_branches(&second), 1);
    let ordinary = dir.write("ordinary.lisp", "(proclaim '(optimize (speed 0)))");
    let fasl = second.compile_file(&ordinary, None).unwrap();
    assert_eq!(count_fixnum_branches(&second), 1);
    second.load_file(&fasl).unwrap();
    assert_eq!(count_fixnum_branches(&second), 0);
}
