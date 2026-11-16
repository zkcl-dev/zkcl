use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn read_time_eval_inserts_the_primary_value_in_the_enclosing_form() {
    let runtime = Runtime::start().unwrap();
    let number = |source| runtime.eval(source).unwrap()[0].as_fixnum();

    assert_eq!(number("#.(values 42 17)"), Some(42));
    assert_eq!(number("#.(values) 42"), Some(42));
    assert_eq!(number("(car '(#.(values) 42))"), Some(42));
    assert_eq!(number("#.(+ 2 #.(+ 38 2))"), Some(42));
    assert_eq!(number("#.(list '+ 40 2)"), Some(42));
    assert_eq!(number("((lambda #.(list 'x) (+ x 2)) 40)"), Some(42));
    let namestring = runtime
        .eval(r#"(namestring #P#.(progn "example.lisp"))"#)
        .unwrap();
    assert_eq!(
        runtime.string(&namestring[0]).as_deref(),
        Some("example.lisp")
    );
    assert_eq!(
        number(
            "#+#.(cl:if (cl:eq cl:*package* (cl:find-package \"KEYWORD\")) :common-lisp :not-found) 42"
        ),
        Some(42)
    );
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
}

#[test]
fn read_time_eval_obeys_suppression_without_evaluating_the_payload() {
    let runtime = Runtime::start().unwrap();
    let number = |source| runtime.eval(source).unwrap()[0].as_fixnum();
    let truth = |source| !runtime.eval(source).unwrap()[0].is_nil();

    runtime
        .load_str("(defparameter *read-dot-state* 0)")
        .unwrap();
    assert_eq!(
        number(
            "(let ((*read-eval* nil))
                   (handler-case (read-from-string \"#.#.(setq *read-dot-state* 1)\")
                     (reader-error () 42)))"
        ),
        Some(42)
    );
    assert_eq!(number("*read-dot-state*"), Some(0));
    assert_eq!(
        number(
            "(let ((*read-eval* nil))
                   (read-from-string \"#-common-lisp #.(setq *read-dot-state* 1) 42\"))"
        ),
        Some(42)
    );
    assert!(truth(
        "(let ((*read-suppress* t) (*read-eval* nil))
           (null (read-from-string \"#.(setq *read-dot-state* 1)\")))"
    ));
    assert_eq!(number("*read-dot-state*"), Some(0));
    let error = runtime
        .eval("(let ((*read-eval* nil)) (read-from-string \"#.(+ 1 2)\"))")
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ReaderError)
    );
}

#[test]
fn read_time_eval_preserves_the_identity_of_inserted_objects() {
    let runtime = Runtime::start().unwrap();
    let number = |source| runtime.eval(source).unwrap()[0].as_fixnum();
    let truth = |source| !runtime.eval(source).unwrap()[0].is_nil();

    runtime
        .register_function("collect-in-reader", |rt, _| {
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    assert!(truth(
        "(let ((pair (car '(#.(let ((shared (list 1))) (list shared shared))
                            #.(collect-in-reader)))))
           (eq (car pair) (cadr pair)))"
    ));
    assert!(truth(
        "(let ((cycle '#.(let ((cell (list 1))) (rplacd cell cell) cell)))
           (eq (cdr cycle) cycle))"
    ));
    runtime
        .load_str("(defparameter *read-dot-state* 0)")
        .unwrap();
    assert!(truth(
        "(let ((value '(head . #.(setq *read-dot-state* (list 1 2)))))
           (eq (cdr value) *read-dot-state*))"
    ));
    assert_eq!(number("(car . #.(list '(quote (42))))"), Some(42));
    assert_eq!(
        number(
            "(progn #.(setq *read-dot-state* (list '+ 1 2))
                        #.(progn (rplaca (cdr *read-dot-state*) 40) (values)))"
        ),
        Some(42)
    );
    runtime
        .load_str("(defmacro original-read-form (&whole form) (eq form *read-dot-state*))")
        .unwrap();
    assert!(truth(
        "#.(setq *read-dot-state* (list 'original-read-form))"
    ));
    let error = runtime
        .eval("#.(let ((cell (list 1))) (rplacd cell cell) cell)")
        .err()
        .unwrap();
    assert!(error.message().contains("circular executable form"));
}
