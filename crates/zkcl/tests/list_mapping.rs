use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_reprs;

#[test]
fn finite_list_bounds_mapping_over_circular_inputs() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((cycle (list 1 2)) (repeat (list 10)))
               (rplacd (cdr cycle) cycle)
               (rplacd repeat repeat)
               (values
                 (mapcar #'+ cycle repeat '(1 2 3 4))
                 (maplist (lambda (a b) (list (car a) (car b))) '(1 2 3) cycle)
                 (mapcar #'list repeat nil)
                 (handler-case (mapcar #'list cycle repeat)
                   (type-error () :unbounded))))",
        ),
        ["(12 14 14 16)", "((1 1) (2 2) (3 1))", "NIL", ":UNBOUNDED"],
    );
}

#[test]
fn list_mapping_applies_the_function_to_successive_input_cells() {
    let runtime = Runtime::start().unwrap();

    assert_eq!(
        eval_reprs(&runtime, "(mapcar '+ '(1 2 3) '(10 20))"),
        ["(11 22)"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(mapcar (lambda (x) (if x (values x 9) (values))) '(nil 3))"
        ),
        ["(NIL 3)"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((calls 0))
               (values (mapcar (lambda (x y) (setq calls (+ calls 1))) '(1) nil) calls))",
        ),
        ["NIL", "0"]
    );
    runtime
        .load_str("(defun replace-map-function (x) (defun replace-map-function (y) 9) x)")
        .unwrap();
    assert_eq!(
        eval_reprs(&runtime, "(mapcar 'replace-map-function '(1 2 3))"),
        ["(1 2 3)"]
    );

    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (list 1 2 3)) (sum 0)
                    (returned (mapc (lambda (a b) (setq sum (+ (* sum 10) (+ a b))))
                                    input '(1 2))))
               (values (eq input returned) sum))",
        ),
        ["T", "24"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((input (list 1 2 3))
                    (tails (maplist (lambda (tail) (values tail 9)) input))
                    (sum 0)
                    (returned (mapl (lambda (tail) (setq sum (+ (* 10 sum) (car tail)))) input)))
               (values (eq input (car tails)) (eq (cdr input) (cadr tails))
                       (eq input returned) sum))",
        ),
        ["T", "T", "T", "123"]
    );

    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((left (cons 1 :tail)) (right (list 2 3)) (unchanged nil)
                    (result
                      (mapcan (lambda (x)
                                (if (= x 1)
                                    (values left :ignored)
                                    (progn
                                      (setq unchanged (eq (cdr left) :tail))
                                      right)))
                              '(1 2))))
               (values result (eq result left) (eq (cdr result) right) unchanged))",
        ),
        ["(1 2 3)", "T", "T", "T"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(mapcan (lambda (x) (if (= x 1) (list x) x)) '(1 2))"
        ),
        ["(1 . 2)"]
    );
    assert_eq!(
        eval_reprs(&runtime, "(mapcon #'list '(1 2 3))"),
        ["((1 2 3) (2 3) (3))"]
    );
}

#[test]
fn mapcar_keeps_pending_inputs_live_during_host_callbacks() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("mapping-gc-step", |runtime, args| {
            runtime.collect();
            Ok(vec![runtime.cons(&args[0], &runtime.nil())?])
        })
        .unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(values (mapcar #'mapping-gc-step (list (list 1) (list 2) (list 3)))
                     (mapcan #'mapping-gc-step (list (list 1) (list 2) (list 3))))",
        ),
        ["(((1)) ((2)) ((3)))", "((1) (2) (3))"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(mapcar (lambda (x) (mapcar #'+ x '(1 2))) '((10 20) (30 40)))"
        ),
        ["((11 22) (31 42))"]
    );

    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((cleaned nil))
               (multiple-value-bind (escaped number)
                   (catch :done
                     (mapcar (lambda (x)
                               (unwind-protect (throw :done (values x 7))
                                 (setq cleaned t)))
                             (list (list 1) (list 2))))
                 (values escaped number cleaned)))",
        ),
        ["(1)", "7", "T"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (mapcar #'car '(17))
               (type-error (condition) (type-error-datum condition)))",
        ),
        ["17"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(restart-case (mapcar (lambda (x) (invoke-restart 'choose x)) '(12))
               (choose (value) (values value 13)))",
        ),
        ["12", "13"]
    );

    let mapped = runtime.eval("#'mapcar").unwrap().pop().unwrap();
    let native = runtime.eval("#'mapping-gc-step").unwrap().pop().unwrap();
    let input = runtime
        .eval("(list (list 4) (list 5))")
        .unwrap()
        .pop()
        .unwrap();
    let values = runtime.call(&mapped, &[&native, &input]).unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(((4)) ((5)))");

    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (mapcar 'unknown-map-function nil)
               (undefined-function () :undefined))",
        ),
        [":UNDEFINED"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (mapcar #'list) (program-error () :wrong-arity))",
        ),
        [":WRONG-ARITY"]
    );

    for source in [
        "(mapcar #'list 42)",
        "(mapcar #'list '(1 . 2))",
        "(let ((cycle (list 1 2))) (rplacd (cdr cycle) cycle) (mapcar #'list cycle))",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .expect("map argument must be a proper list");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError)
        );
    }
}
