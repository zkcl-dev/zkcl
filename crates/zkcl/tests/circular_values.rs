use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn printing_distinguishes_circular_lists_from_shared_lists() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(let ((tail-cycle (list 1 2)) (head-cycle (list nil)) (shared (list 3)))
               (rplacd (cdr tail-cycle) tail-cycle)
               (rplaca head-cycle head-cycle)
               (values tail-cycle head-cycle (list shared shared)))",
        )
        .unwrap();

    runtime.collect();
    assert_eq!(
        runtime.repr(&values[0]).unwrap(),
        "(1 2 . #<CIRCULAR-CONS>)"
    );
    assert_eq!(runtime.repr(&values[1]).unwrap(), "(#<CIRCULAR-CONS>)");
    assert_eq!(runtime.repr(&values[2]).unwrap(), "((3) (3))");

    let limited = runtime
        .eval(
            "(let ((x (list 1)))
               (rplacd x x)
               (write-to-string x :length 1))",
        )
        .unwrap();
    assert_eq!(runtime.string(&limited[0]).as_deref(), Some("(1 ...)"));

    let signaled = runtime
        .eval(
            "(let ((x (list 1)))
               (rplacd x x)
               (handler-case (write-to-string x :readably t :length 0)
                 (print-not-readable (condition)
                   (eq x (print-not-readable-object condition)))))",
        )
        .unwrap();
    assert!(!signaled[0].is_nil());
}

#[test]
fn eval_distinguishes_circular_literals_from_circular_forms() {
    let runtime = Runtime::start().unwrap();

    let literal = runtime
        .eval(
            "(let ((x (list 42)))
               (rplacd x x)
               (eq x (eval (list 'quote x))))",
        )
        .unwrap();
    assert!(!literal[0].is_nil());

    let shared = runtime
        .eval(
            "(let ((form (list '+ 39 3)))
               (eval (list 'values form form)))",
        )
        .unwrap();
    assert_eq!(shared.len(), 2);
    assert!(shared.iter().all(|value| value.as_fixnum() == Some(42)));

    for source in [
        "(let ((form (list nil))) (rplaca form form) (eval form))",
        "(let ((form (list 'progn))) (rplacd form form) (eval form))",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .expect("circular executable form");
        assert!(
            error.message().contains("circular executable form"),
            "{error}"
        );
    }

    let error = runtime
        .eval("(let ((form (list 'or t))) (rplacd (cdr form) form) (macroexpand form))")
        .err()
        .expect("circular macro arguments");
    assert!(error.message().contains("proper list"), "{error}");
}

#[test]
fn reader_rejects_circular_feature_lists() {
    let runtime = Runtime::start().unwrap();

    let error = runtime
        .eval(
            r##"(let ((*features* (list :present)))
                 (rplacd *features* *features*)
                 (read-from-string "#+absent 1 2"))"##,
        )
        .err()
        .expect("circular features");
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ReaderError)
    );
    assert!(error.message().contains("*FEATURES* must be a proper list"));
}
