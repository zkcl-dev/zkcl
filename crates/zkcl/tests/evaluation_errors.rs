use std::error::Error;
use std::io;

use zkcl::{ConditionClass, EvalError, EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_error;

#[test]
fn evaluation_errors_identify_the_failing_language_phase() {
    let runtime = Runtime::start().unwrap();

    let malformed = "(list \"雪\" ))";
    let error = eval_error(&runtime, malformed);
    assert_eq!(error.kind(), EvalErrorKind::Read);
    assert_eq!(error.offset(), malformed.rfind(')').unwrap());

    let error = eval_error(&runtime, "(if)");
    assert_eq!(error.kind(), EvalErrorKind::Compile);

    for (source, class) in [
        ("(car 42)", ConditionClass::TypeError),
        ("(missing-function)", ConditionClass::UndefinedFunction),
        (
            "(error \"explicit Lisp error\")",
            ConditionClass::SimpleError,
        ),
    ] {
        let error = eval_error(&runtime, source);
        assert_eq!(error.kind(), EvalErrorKind::LispCondition(class));
    }

    for (form, class, message) in [
        (
            r#"(parse-namestring "a\\")"#,
            ConditionClass::ParseError,
            "dangling escape in namestring",
        ),
        (
            "(unexport 'cl:car :common-lisp)",
            ConditionClass::PackageError,
            "a built-in package's external symbols cannot be replaced or unexported",
        ),
    ] {
        let error = eval_error(&runtime, form);
        assert_eq!(error.kind(), EvalErrorKind::LispCondition(class), "{form}");
        assert_eq!(error.message(), message, "{form}");
        assert_eq!(error.source().unwrap().to_string(), message, "{form}");
    }
}

#[test]
fn host_callback_errors_retain_their_cause_through_macroexpansion() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-fault", |_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "backing store denied").into())
        })
        .unwrap();
    let source = "(progn 1 (host-fault))";
    let error = eval_error(&runtime, source);
    assert_eq!(error.kind(), EvalErrorKind::HostCallback);
    assert_eq!(error.offset(), source.find("host-fault").unwrap());
    assert!(error.to_string().contains("backing store denied"));
    let cause = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);

    let recovered = runtime
        .eval("(handler-case (host-fault) (simple-error (c) (simple-condition-format-control c)))")
        .unwrap();
    assert_eq!(
        runtime.string(&recovered[0]).as_deref(),
        Some("backing store denied")
    );

    runtime
        .load_str("(defmacro fail-on-expansion () (host-fault))")
        .unwrap();
    let source = "(let () (fail-on-expansion))";
    let error = eval_error(&runtime, source);
    assert_eq!(error.kind(), EvalErrorKind::MacroExpansion);
    assert_eq!(error.offset(), source.find("(fail-on-expansion)").unwrap());
    let inner = error.source().unwrap().downcast_ref::<EvalError>().unwrap();
    assert_eq!(inner.kind(), EvalErrorKind::HostCallback);
    let cause = inner.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);

    let error = eval_error(&runtime, "(list (eval '(fail-on-expansion)))");
    assert_eq!(error.kind(), EvalErrorKind::MacroExpansion);
    let inner = error.source().unwrap().downcast_ref::<EvalError>().unwrap();
    assert_eq!(inner.kind(), EvalErrorKind::HostCallback);
    let cause = inner.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn lisp_macro_expansion_uses_the_callers_dynamic_control() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (define-condition expansion-notice (condition) ())
            (define-condition expansion-failure (error) ())
            (defparameter *expansion-notice* (make-condition 'expansion-notice))
            (defparameter *expansion-failure* (make-condition 'expansion-failure))
            (defparameter *expansion-log* nil)
            (defmacro notifying-expansion () (signal *expansion-notice*) 7)
            (defun compiler-expansion-notice (value) (list value))
            (define-compiler-macro compiler-expansion-notice (value)
              (signal *expansion-notice*)
              `(list ,value))
            (defmacro failing-expansion () (error *expansion-failure*))
            (defmacro continuable-expansion ()
              (cerror "Resume expanding." *expansion-failure*)
              42)
            (defmacro throwing-expansion ()
              (unwind-protect (throw 'finished-expansion (values 1 2))
                (push :macro *expansion-log*)))
            "#,
        )
        .unwrap();

    for (source, expected) in [
        (
            r#"(let ((seen 0))
                  (handler-bind ((expansion-notice
                                   (lambda (c) (when (eq c *expansion-notice*) (incf seen)))))
                    (list (eval '(notifying-expansion))
                          (eval '(list (notifying-expansion)))
                          (funcall (compile nil '(lambda () (list (notifying-expansion)))))
                          (eval '(compiler-expansion-notice 7))
                          seen)))"#,
            "(7 (7) (7) (7) 4)",
        ),
        (
            r#"(handler-case (eval '(list (failing-expansion)))
                  (expansion-failure (c) (eq c *expansion-failure*)))"#,
            "T",
        ),
        (
            r#"(handler-bind ((expansion-failure #'continue))
                  (eval '(list (continuable-expansion))))"#,
            "(42)",
        ),
        (
            r#"(let ((*expansion-log* nil))
                  (list (multiple-value-list
                         (catch 'finished-expansion
                           (unwind-protect (eval '(list (throwing-expansion)))
                             (push :caller *expansion-log*))))
                        *expansion-log*))"#,
            "((1 2) (:CALLER :MACRO))",
        ),
        (
            r#"(eval '(handler-case (list (failing-expansion))
                        (expansion-failure (c) (eq c *expansion-failure*))))"#,
            "T",
        ),
        (
            r#"(eval '(if nil (failing-expansion) :skipped))"#,
            ":SKIPPED",
        ),
        (
            r#"(let ((seen 0))
                  (handler-bind ((expansion-failure (lambda (c) (declare (ignore c)) (incf seen))))
                    (list (eval '(handler-case (list (failing-expansion))
                                   (expansion-failure (c) (eq c *expansion-failure*))))
                          seen
                          (eval '(handler-case (if nil (failing-expansion) :skipped)
                                   (expansion-failure () :unexpected)))
                          seen)))"#,
            "(T 0 :SKIPPED 0)",
        ),
        (
            r#"(handler-bind ((expansion-failure #'continue))
                  (eval '(handler-case (list (continuable-expansion))
                           (expansion-failure () :unexpected))))"#,
            "(42)",
        ),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(runtime.repr(&result[0]).unwrap(), expected, "{source}");
    }
}
