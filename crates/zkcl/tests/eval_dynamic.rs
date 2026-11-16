use zkcl::{Root, Runtime};

mod common;

use common::lisp::eval_fixnums;

#[test]
fn eval_sees_special_bindings_in_a_null_lexical_environment() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defvar *eval-special* 7)
             (set 'eval-global 40)",
        )
        .unwrap();

    assert_eq!(
        eval_fixnums(
            &runtime,
            "(let ((eval-global 99) (*eval-special* 3))
               (values (eval 'eval-global) (eval '*eval-special*) eval-global))",
        ),
        [40, 3, 99],
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(eval '(progn (defmacro eval-token () (list 'quote 9)) (eval-token)))",
        ),
        [9],
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(macrolet ((eval-token () 29)) (eval '(eval-token)))"
        ),
        [9],
    );
}

#[test]
fn eval_preserves_the_identity_of_quoted_objects_during_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-eval", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    let result = runtime
        .eval(
            "(let ((object (list 91)))
               (eval (list 'progn '(collect-during-eval)
                                  (list 'eq (list 'quote object) (list 'quote object)))))",
        )
        .unwrap();
    assert!(matches!(result.as_slice(), [truth] if !truth.is_nil()));
}

#[test]
fn eval_reaches_the_callers_dynamic_exit_points() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-eval", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime.load_str("(defvar *eval-cleanup* 0)").unwrap();
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(catch :done
               (eval '(unwind-protect
                          (throw :done (values 3 4))
                        (collect-during-eval)
                        (setq *eval-cleanup* 8))))",
        ),
        [3, 4],
    );
    assert_eq!(eval_fixnums(&runtime, "*eval-cleanup*"), [8]);
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(handler-case (eval '(car 42))
               (type-error (condition) (type-error-datum condition)))",
        ),
        [42],
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(restart-case (eval '(invoke-restart 'choose 12))
               (choose (value) (values value 13)))",
        ),
        [12, 13],
    );
    assert_eq!(
        eval_fixnums(&runtime, "(handler-case (eval) (program-error () 18))"),
        [18],
    );
}

#[test]
fn eval_returns_all_values_to_its_caller() {
    let runtime = Runtime::start().unwrap();
    assert!(runtime.eval("(eval '(values))").unwrap().is_empty());
    assert_eq!(
        eval_fixnums(&runtime, "(apply #'eval (list '(eval '(values 1 2))))"),
        [1, 2],
    );

    let eval = runtime.eval("#'eval").unwrap().pop().unwrap();
    let object = runtime.eval("'(values 6 7)").unwrap().pop().unwrap();
    let returned = runtime.call(&eval, &[&object]).unwrap();
    assert_eq!(
        returned
            .iter()
            .filter_map(Root::as_fixnum)
            .collect::<Vec<_>>(),
        [6, 7],
    );
}
