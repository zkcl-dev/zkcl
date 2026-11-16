use zkcl::{EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_reprs;

fn fixnum_values(runtime: &Runtime, source: &str) -> Vec<Option<isize>> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum())
        .collect()
}

fn runtime_with_collecting_callback() -> Runtime {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-and-return", |runtime, args| {
            runtime.collect();
            Ok(args.to_vec())
        })
        .unwrap();
    runtime
}

#[test]
fn multiple_value_call_splices_values_in_evaluation_order() {
    let runtime = Runtime::start().unwrap();
    let numbers = |source| fixnum_values(&runtime, source);
    assert_eq!(numbers("(multiple-value-call #'values)"), []);
    assert_eq!(numbers("(multiple-value-call #'values (values))"), []);
    assert_eq!(
        numbers("(multiple-value-call (values #'values :ignored) (values 1 2) (values) 3)"),
        [Some(1), Some(2), Some(3)]
    );
    assert_eq!(
        numbers(
            "(let ((order 0))
               (multiple-value-call
                 (progn (setq order (+ (* order 10) 1)) 'values)
                 (progn (setq order (+ (* order 10) 2)) (values 4 5))
                 (progn (setq order (+ (* order 10) 3)) (values))
                 order))",
        ),
        [Some(4), Some(5), Some(123)]
    );
    assert_eq!(
        numbers("(let ((x (multiple-value-call #'values (values) (values 6 7)))) x)"),
        [Some(6)]
    );
}

#[test]
fn multiple_value_prog1_returns_the_first_forms_values() {
    let runtime = Runtime::start().unwrap();
    let numbers = |source| fixnum_values(&runtime, source);
    assert_eq!(
        numbers("(let ((x (multiple-value-prog1 (values) (values 6 7)))) x)"),
        [None]
    );
    assert_eq!(
        numbers("(let ((x 1)) (multiple-value-prog1 (values x 2) (setq x 3) x))"),
        [Some(1), Some(2)]
    );
    assert_eq!(
        numbers("(multiple-value-prog1 (values 1 2))"),
        [Some(1), Some(2)]
    );
    assert_eq!(numbers("(multiple-value-prog1 (values) (values 1 2))"), []);
}

#[test]
fn pending_multiple_values_survive_collection() {
    let runtime = runtime_with_collecting_callback();
    let values = runtime
        .eval(
            "(let ((marker 27))
               (multiple-value-call (lambda (head &rest tail)
                                      (values head (car tail) (car (cdr tail)) marker))
                 (values (cons 42 nil))
                 (multiple-value-prog1 (values (cons 43 nil)) (collect-and-return))
                 (collect-and-return (cons 44 nil))))",
        )
        .unwrap();
    let actual: Vec<_> = values[..3]
        .iter()
        .map(|value| runtime.cons_fields(value).unwrap().0.as_fixnum())
        .collect();
    assert_eq!(actual, [Some(42), Some(43), Some(44)]);
    assert_eq!(values[3].as_fixnum(), Some(27));
}

#[test]
fn nonlocal_exits_discard_abandoned_multiple_values() {
    let runtime = runtime_with_collecting_callback();
    let numbers = |source| fixnum_values(&runtime, source);
    assert_eq!(
        numbers(
            "(multiple-value-call #'values
               (values 1 2)
               (catch :leave
                 (multiple-value-call #'values
                   (values :abandoned)
                   (unwind-protect (throw :leave (values 3 4)) (collect-and-return))))
               (block exit (multiple-value-prog1 (values :abandoned)
                             (return-from exit (values 5 6)))))",
        ),
        [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]
    );
    assert_eq!(
        numbers(
            "(handler-case
               (multiple-value-call #'values (values :abandoned) (car 1))
               (type-error () (multiple-value-prog1 (values 7 8) (collect-and-return))))",
        ),
        [Some(7), Some(8)]
    );
    assert_eq!(
        numbers(
            "(let ((n 0))
               (tagbody retry
                 (setq n (+ n 1))
                 (multiple-value-call #'values
                   (values :abandoned)
                   (if (= n 3) (go done) (go retry)))
                 done)
               n)",
        ),
        [Some(3)]
    );
}

#[test]
fn multiple_value_operators_require_an_initial_form() {
    let runtime = Runtime::start().unwrap();
    for source in ["(multiple-value-call)", "(multiple-value-prog1)"] {
        assert_eq!(
            runtime.eval(source).err().unwrap().kind(),
            EvalErrorKind::Compile
        );
    }
}

#[test]
fn compiling_multiple_value_forms_preserves_macro_assignments_to_lexical_variables() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defmacro write-hidden () '(setq x 7))
             (defmacro make-writer () '(lambda (value) (setq x value)))
             (defmacro write-with-expansion-environment (&environment env)
               (macroexpand '(write-here) env))",
        )
        .unwrap();
    let value = runtime
        .eval(
            "(funcall (compile nil
           '(lambda ()
              (let ((x 1) (y 2))
                (values (multiple-value-list (multiple-value-setq (x y) (values 3 4)))
                        x y
                        (let ((x 5))
                          (multiple-value-list (write-hidden))
                          x)
                        (macrolet ((write-here () '(setq y 8)))
                          (multiple-value-list (write-with-expansion-environment))
                          y)
                        x
                        (let ((x 10))
                          (funcall (multiple-value-prog1 (make-writer)) 12)
                          x))))))",
        )
        .unwrap();
    let actual = value
        .iter()
        .map(|v| runtime.repr(v).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, ["(3)", "3", "4", "7", "8", "3", "12"]);
}

#[test]
fn macros_in_multiple_value_forms_can_transfer_to_lexical_exit_points() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defmacro go-hidden () '(go finish))
         (defmacro leave-hidden () '(return-from outer (local-value)))",
        )
        .unwrap();
    let values = runtime
        .eval(
            "(funcall (compile nil
           '(lambda ()
              (flet ((local-value () 42))
                (block outer
                  (tagbody
                    (multiple-value-list (go-hidden))
                    (return-from outer :wrong)
                    finish (multiple-value-prog1 (leave-hidden))))))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));
}

#[test]
fn values_list_returns_each_element_as_a_separate_value() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(&runtime, "(values-list nil)"),
        Vec::<String>::new()
    );
    assert_eq!(
        eval_reprs(&runtime, "(values-list '(1 2 3))"),
        ["1", "2", "3"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (values-list '(1 . 2)) (type-error () :type-error))"
        ),
        [":TYPE-ERROR"]
    );
}
