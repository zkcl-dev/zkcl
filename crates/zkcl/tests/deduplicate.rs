use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_reprs;

#[test]
fn deduplication_keeps_the_selected_occurrence_within_the_requested_range() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(remove-duplicates nil)", "NIL"),
        ("(remove-duplicates '(a b c b d d e))", "(A C B D E)"),
        (
            "(remove-duplicates '(a b c b d d e) :from-end t)",
            "(A B C D E)",
        ),
        (
            "(remove-duplicates '(0 1 1 2 1 3 1) :start 2 :end 5)",
            "(0 1 2 1 3 1)",
        ),
        (
            "(remove-duplicates '(0 1 1 2 1 3 1) :start 2 :end 5 :from-end t)",
            "(0 1 1 2 3 1)",
        ),
        (
            r#"(remove-duplicates "雪a雪Aβ" :test #'char-equal :start 1 :end 4)"#,
            r#""雪雪Aβ""#,
        ),
        (
            r#"(delete-duplicates "雪a雪Aβ" :test #'char-equal :start 1 :end 4 :from-end t)"#,
            r#""雪a雪β""#,
        ),
        (
            "(remove-duplicates '(:outside (1 a) (1 b) (2 c) :outside)
               :key #'car :start 1 :end 4)",
            "(:OUTSIDE (1 B) (2 C) :OUTSIDE)",
        ),
        (
            "(remove-duplicates '(1 2 3) :test (lambda (x y) (= (+ x 1) y)))",
            "(3)",
        ),
        ("(remove-duplicates '(1 2 3) :test-not #'<)", "(1 2 3)"),
        (
            "(delete-duplicates (list 1 2 3) :test-not #'< :from-end t)",
            "(1)",
        ),
        (
            "(remove-duplicates '(1 1 1 2) :start 1 :start 99 :end nil)",
            "(1 1 2)",
        ),
        (
            "(remove-duplicates '(1 1) :unknown t :allow-other-keys t)",
            "(1)",
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), [expected], "{source}");
    }
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((input (list 1 2 1 3)))
               (let ((copy (remove-duplicates input)))
                 (values input copy (eq input copy))))"
        ),
        ["(1 2 1 3)", "(2 1 3)", "NIL"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (list 1 2 1 3))
                    (first-retained (cdr input))
                    (last-retained (cdddr input))
                    (output (delete-duplicates input)))
               (values output (eq output first-retained) (eq (last output) last-retained)))"
        ),
        ["(2 1 3)", "T", "T"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (make-array 4 :element-type 'bit
                                  :initial-contents '(1 0 1 0) :fill-pointer 3))
                    (output (remove-duplicates input)))
               (values (array-element-type output) (array-dimension output 0)
                       (aref output 0) (aref output 1)))"
        ),
        ["BIT", "2", "0", "1"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (make-array 7 :initial-contents '(x 0 1 0 2 0 y)))
                    (output (delete-duplicates input :start 1 :end 5)))
               (values (array-element-type output) (array-dimension output 0)
                       (aref output 0) (aref output 1) (aref output 2)
                       (aref output 3) (aref output 4) (aref output 5)))"
        ),
        ["T", "6", "X", "1", "0", "2", "0", "Y"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((large (ash 1 75)))
               (values (length (remove-duplicates (list large (ash 1 75) 0)))
                       (length (remove-duplicates '(1 1.0 1d0 1.0)))
                       (length (remove-duplicates '(4/2 2)))
                       (length (remove-duplicates '(1/2 2/4)))))"
        ),
        ["2", "3", "1", "1"]
    );
}

#[test]
fn deduplication_keeps_selected_elements_alive_during_key_callbacks() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("deduplicate-gc-key", |runtime, args| {
            runtime.collect();
            Ok(vec![runtime.cons(&args[0], &runtime.nil())?])
        })
        .unwrap();
    runtime
        .register_function("deduplicate-gc-value", |runtime, args| {
            runtime.collect();
            Ok(vec![args[0].clone()])
        })
        .unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (list (list 1 :first) (list 2 :second) (list 1 :third)))
                    (last-element (caddr input))
                    (output
                      (remove-duplicates input
                        :key (lambda (item) (deduplicate-gc-key (car item)))
                        :test (lambda (left right)
                                (= (car (deduplicate-gc-value left)) (car right))))))
               (values output (eq (cadr output) last-element)))"
        ),
        ["((2 :SECOND) (1 :THIRD))", "T"]
    );
}

#[test]
fn deduplication_callbacks_can_exit_to_the_enclosing_dynamic_context() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((input (list 1 2 2)) (cleaned nil))
               (multiple-value-bind (escaped other)
                   (catch :done
                     (delete-duplicates input
                       :key (lambda (value)
                              (unwind-protect (throw :done (values value 42))
                                (setq cleaned t)))))
                 (values escaped other cleaned input)))"
        ),
        ["1", "42", "T", "(1 2 2)"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(restart-case
                 (remove-duplicates '(4 5)
                   :test (lambda (a b) (invoke-restart 'choose (+ a b))))
               (choose (result) (values result 42)))"
        ),
        ["9", "42"]
    );
}

#[test]
fn deduplication_rejects_invalid_sequence_arguments() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(remove-duplicates 42)",
        "(remove-duplicates (make-array '(2 2) :initial-element 0))",
        "(delete-duplicates '(1 . 2))",
        "(let ((cycle (list 1 2))) (rplacd (cdr cycle) cycle) (remove-duplicates cycle))",
        "(remove-duplicates '(1 1) :end -1)",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .expect("invalid sequence or bound");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}"
        );
    }
    assert!(runtime.eval("(remove-duplicates '(1 1) :start 3)").is_err());
    assert!(runtime.eval("(remove-duplicates \"雪\" :end 2)").is_err());
}
