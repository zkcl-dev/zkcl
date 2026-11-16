use std::fs;

use zkcl::{ConditionClass, EvalErrorKind, Runtime};
use zkcl_bytecode::{CompiledFile, LiteralNode};

mod common;

use common::files::TempDir;
use common::lisp::eval;

fn check(runtime: &Runtime, source: &str) {
    let values = eval(runtime, source);
    assert_eq!(values.len(), 1, "{source}");
    assert!(!values[0].is_nil(), "{source}");
}

#[test]
fn random_state_snapshots_resume_the_same_sequence() {
    let runtime = Runtime::start().unwrap();

    check(
        &runtime,
        "(let ((fresh (make-random-state t)))
           (and (random-state-p *random-state*)
                (random-state-p fresh)
                (typep fresh 'random-state)
                (eq (type-of fresh) 'random-state)
                (eq (class-of fresh) (find-class 'random-state))
                (not (random-state-p nil))
                (not (eq fresh *random-state*))))",
    );
    runtime
        .load_str(
            "(defparameter *original-state* *random-state*)
             (defparameter *random-sequence* (make-random-state))
             (defparameter *random-copy* (make-random-state *random-sequence*))
             (defparameter *first-sample* (random (ash 1 137) *random-sequence*))",
        )
        .unwrap();

    runtime.collect();

    check(
        &runtime,
        "(and (not (eq *random-sequence* *random-copy*))
              (= *first-sample* (random (ash 1 137) *random-copy*))
              (= (random 37 *random-sequence*) (random 37 *random-copy*))
              (= (random 2.5f0 *random-sequence*) (random 2.5f0 *random-copy*))
              (= (random 2.5d0 *random-sequence*) (random 2.5d0 *random-copy*)))",
    );
    check(
        &runtime,
        "(and (let ((*random-state* *random-sequence*))
                (let ((copy (make-random-state nil)))
                  (and (not (eq *random-state* copy))
                       (= (random 1000) (random 1000 copy) (random 1000 *random-copy*))
                       (= (random 2.0d0) (random 2.0d0 *random-copy*)))))
              (eq *random-state* *original-state*))",
    );
    check(
        &runtime,
        "(let* ((printed (write-to-string *random-sequence* :readably t))
                (*read-eval* nil)
                (*read-base* 36)
                (restored (read-from-string printed)))
           (let ((*read-base* 10))
             (and (random-state-p restored)
                  (not (eq *random-sequence* restored))
                  (= (random (ash 1 137) *random-sequence*)
                     (random (ash 1 137) restored)))))",
    );
    let dir = TempDir::new("random-state");
    let input = dir.path().join("state.lisp");
    let state = runtime.eval("*random-sequence*").unwrap();
    let literal = runtime.repr(&state[0]).unwrap();
    fs::write(&input, format!("(defun saved-random-state () {literal})")).unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    let encoded = fs::read(&compiled).unwrap();
    let mut file = CompiledFile::decode(&encoded).unwrap();
    let node = file
        .nodes
        .iter_mut()
        .find(|node| matches!(node, LiteralNode::RandomState(_)))
        .unwrap();
    *node = LiteralNode::RandomState([0; 4]);
    assert!(file.encode().is_err());
    runtime.load_file(&compiled).unwrap();
    drop(dir);
    runtime.collect();
    check(
        &runtime,
        "(let ((compiled (saved-random-state))
               (copy (make-random-state (saved-random-state))))
           (and (random-state-p compiled)
                (not (eq compiled *random-sequence*))
                (= (random 137 (make-random-state *random-sequence*))
                   (random 137 copy))
                (eq compiled (saved-random-state))))",
    );
}

#[test]
fn random_returns_values_below_the_exclusive_bound() {
    let runtime = Runtime::start().unwrap();

    check(
        &runtime,
        "(let ((big-bound (1+ (ash 1 137))))
           (dotimes (i 128 t)
             (let ((small (random 37))
                   (big (random big-bound))
                   (single (random most-positive-single-float))
                   (double (random most-positive-double-float)))
               (unless (and (typep small 'fixnum) (<= 0 small) (< small 37)
                            (integerp big) (<= 0 big) (< big big-bound)
                            (typep single 'single-float) (<= 0 single)
                            (< single most-positive-single-float)
                            (typep double 'double-float) (<= 0 double)
                            (< double most-positive-double-float))
                 (return nil)))))",
    );
    check(
        &runtime,
        "(let ((single (random least-positive-single-float))
               (double (random least-positive-double-float)))
           (and (= (random 1) 0)
                (eql single 0f0)
                (eql double 0d0)))",
    );
}

#[test]
fn random_signals_type_errors_for_invalid_arguments() {
    let runtime = Runtime::start().unwrap();

    for source in [
        "(random 1 nil)",
        "(random 0)",
        "(random -1.0)",
        "(random 1/2)",
        "(make-random-state :invalid)",
    ] {
        assert_eq!(
            runtime.eval(source).err().unwrap().kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}",
        );
    }
    check(
        &runtime,
        "(handler-case (random 1/2)
           (type-error (condition)
             (let ((expected (type-error-expected-type condition)))
               (and (= (type-error-datum condition) 1/2)
                    (typep 37 expected)
                    (typep 1.0f0 expected)
                    (typep 1.0d0 expected)
                    (not (typep 0 expected))
                    (not (typep 0.0f0 expected))
                    (not (typep 1/2 expected))))))",
    );
    check(
        &runtime,
        "(handler-case (make-random-state :invalid)
           (type-error (condition)
             (let ((expected (type-error-expected-type condition)))
               (and (eq (type-error-datum condition) :invalid)
                    (typep *random-state* expected)
                    (typep t expected)
                    (typep nil expected)
                    (not (typep :invalid expected))))))",
    );
}

#[test]
fn random_state_reader_rejects_invalid_literals() {
    let runtime = Runtime::start().unwrap();
    for literal in [
        r#"#Y"short""#,
        r#"#Y"0000000000000000000000000000000000000000000000000000000000000000""#,
    ] {
        assert!(
            runtime.eval(literal).is_err(),
            "expected invalid random-state syntax: {literal}",
        );
    }
}
