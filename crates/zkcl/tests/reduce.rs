use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

fn check(runtime: &Runtime, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval_reprs(runtime, source), [*expected], "source: {source}");
    }
}

#[test]
fn reduce_folds_in_the_requested_direction() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(reduce #'+ '(1 2 3 4))", "10"),
            ("(reduce #'+ nil)", "0"),
            ("(reduce #'- '(1 2 3))", "-4"),
            ("(reduce #'- '(1 2 3) :from-end t)", "2"),
            ("(reduce #'- '(1 2 3) :initial-value 10)", "4"),
            ("(reduce #'- '(1 2 3) :initial-value 10 :from-end t)", "-8"),
            ("(reduce #'+ '(1 2 3 4) :start 1 :end 3)", "5"),
            ("(reduce #'list '(1 2 3))", "((1 2) 3)"),
            ("(reduce #'list '(1 2 3) :from-end t)", "(1 (2 3))"),
            ("(reduce #'max '(3 1 5 2))", "5"),
        ],
    );
}

#[test]
fn reduce_applies_the_key_only_to_sequence_elements() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(reduce #'+ '(1 2 3) :key (lambda (n) (* n 10)))", "60"),
            (
                "(reduce #'+ '(1 2 3) :from-end t :key (lambda (n) (* n 10)))",
                "60",
            ),
            (
                "(reduce #'+ '(1 2 3) :initial-value 100 :key (lambda (n) (* n 10)))",
                "160",
            ),
            ("(reduce #'max \"abc\" :key #'char-code)", "99"),
        ],
    );
}

#[test]
fn reduce_keeps_accumulator_live_during_callbacks() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-reduce-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    check(
        &runtime,
        &[
            (
                "(reduce (lambda (acc n)
                           (collect-reduce-heap)
                           (cons n acc))
                         '(1 2 3) :initial-value nil)",
                "(3 2 1)",
            ),
            (
                "(reduce (lambda (a b) (collect-reduce-heap) (+ a b))
                         '(1 2 3 4 5))",
                "15",
            ),
        ],
    );
}

#[test]
fn reduce_rejects_invalid_arguments() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(reduce #'+ 5)",
        "(reduce 5 '(1 2))",
        "(reduce #'+ '(1 2) :start 3)",
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report an error"
        );
    }
}
