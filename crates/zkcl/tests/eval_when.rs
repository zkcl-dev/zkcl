use zkcl::{EvalErrorKind, Runtime};

#[test]
fn eval_when_runs_selected_toplevel_forms_in_source_order() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval(
            "(eval-when (:compile-toplevel :load-toplevel :execute)
               (defmacro added-by-eval-when (form) (list '+ 2 form))
               (values (added-by-eval-when 40) 7))",
        )
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].as_fixnum(), Some(42));
    assert_eq!(result[1].as_fixnum(), Some(7));

    runtime
        .load_str(
            "(defmacro never-expanded () (error \"must not expand\"))
             (eval-when (compile load) (never-expanded))
             (eval-when (eval) (defparameter *eval-when-result* 11))",
        )
        .unwrap();
    let result = runtime
        .eval(
            "(let ((n *eval-when-result*))
               (eval-when (:load-toplevel) (setq n 0))
               (eval-when (:execute) (values n (+ n 1))))",
        )
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].as_fixnum(), Some(11));
    assert_eq!(result[1].as_fixnum(), Some(12));

    assert!(runtime.eval("(eval-when ())").unwrap()[0].is_nil());
    assert!(
        runtime
            .eval("(eval-when (:execute) (values))")
            .unwrap()
            .is_empty()
    );
    let error = runtime
        .eval("(eval-when (:execute :unknown) 1)")
        .err()
        .unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Compile);
}
