use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

#[test]
fn loaded_functions_report_their_source_lambda_lists() {
    let directory = common::files::TempDir::new("source-lambda-lists");
    let source = directory.write(
        "parameters.lisp",
        r#"
        (defun parameter-sample (a &key (b 2) &aux c) (list a b c))
        (defmacro parameter-macro ((a &optional b) &body body)
          `(list ',a ',b ',body))
        (defun (setf parameter-sample) (new object) (cons new object))
        (defgeneric parameter-generic (a &optional b))
        (defun parameter-local (captured)
          (flet ((sample (item &optional (size 3)) (list captured item size)))
            #'sample))
        (defun parameter-empty () nil)
        "#,
    );
    let fasl = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&fasl).unwrap();
    runtime.collect();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(values
                  (multiple-value-list (zk:function-lambda-list #'parameter-sample))
                  (zk:function-lambda-list (macro-function 'parameter-macro))
                  (zk:function-lambda-list #'(setf parameter-sample))
                  (zk:function-lambda-list #'parameter-generic)
                  (zk:function-lambda-list (parameter-local 1))
                  (multiple-value-list (zk:function-lambda-list #'parameter-empty))
                  (multiple-value-list (zk:function-lambda-list #'car))
                  (let* ((parameters (list 'argument))
                         (function (compile nil (list 'lambda parameters 'argument))))
                    (rplaca parameters 'mutated)
                    (zk:function-lambda-list function)))"#,
        ),
        [
            "((A &KEY (B 2) &AUX C) T)",
            "((A &OPTIONAL B) &BODY BODY)",
            "(NEW OBJECT)",
            "(A &OPTIONAL B)",
            "(ITEM &OPTIONAL (SIZE 3))",
            "(NIL T)",
            "(NIL NIL)",
            "(ARGUMENT)",
        ]
    );
}

#[test]
fn compiled_definitions_keep_documentation_on_their_own_objects() {
    let directory = common::files::TempDir::new("documented-definitions");
    let source = directory.path().join("documented.lisp");
    std::fs::write(
        &source,
        r#"
        (defun documented (x) (declare (ignore x)) "Function." 7)
        (defmacro documented-macro () "Macro." 8)
        (defun (setf documented) (new) "Setter." new)
        (setf (documentation '*documented* 'variable) "Variable.")
        (define-symbol-macro *documented* 9)
    "#,
    )
    .unwrap();
    let fasl = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&fasl).unwrap();
    runtime.collect();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"
        (and (equal (documentation 'documented 'function) "Function.")
             (equal (documentation #'documented t) "Function.")
             (equal (documentation 'documented-macro 'function) "Macro.")
             (equal (documentation '(setf documented) 'function) "Setter.")
             (equal (documentation '*documented* 'variable) "Variable."))"#
        ),
        ["T"]
    );
    runtime
        .load_str(
            r#"
        (defparameter *old* #'documented)
        (defun documented () "value")
        (setf (documentation *old* t) "Old.")
        (defun doc-closure (n) (lambda () n))
        (defparameter *first* (doc-closure 1))
        (defparameter *second* (doc-closure 2))
        (setf (documentation *first* 'function) "First.")
    "#,
        )
        .unwrap();
    runtime.collect();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"
        (and (null (documentation 'documented 'function))
             (equal (documented) "value")
             (equal (documentation *old* t) "Old.")
             (equal (documentation *first* t) "First.")
             (null (documentation *second* t)))"#
        ),
        ["T"]
    );
}

#[test]
fn constantly_ignores_all_call_arguments() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((f (constantly :ok)))
               (values (funcall f)
                       (funcall f 1)
                       (funcall f 1 2 3 4)
                       (functionp f)))"
        ),
        [":OK", ":OK", ":OK", "T"]
    );
}

#[test]
fn complement_negates_the_result_of_the_forwarded_call() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(flet ((both (&key a b) (and a b))
                   (var (&rest xs) (evenp (length xs))))
               (let ((c-id (complement #'identity))
                     (c-both (complement #'both))
                     (c-var (complement #'var)))
                 (values (funcall c-id nil)
                         (funcall c-id 42)
                         (funcall c-both :a t :b t)
                         (funcall c-both :a t :b nil)
                         (funcall c-var)
                         (funcall c-var 1)
                         (funcall c-var 1 2))))"
        ),
        ["T", "NIL", "NIL", "T", "NIL", "T", "NIL"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (complement 123) (type-error () :type-error))"
        ),
        [":TYPE-ERROR"]
    );
}

#[test]
fn fmakunbound_removes_the_named_function_binding() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(progn
               (defun obsolete-function (x) x)
               (defmacro obsolete-macro (x) x)
               (defun (setf obsolete-accessor) (v x) (list v x))
               (let ((function-name (fmakunbound 'obsolete-function))
                     (function-bound-p (fboundp 'obsolete-function))
                     (macro-name (fmakunbound 'obsolete-macro))
                     (macro-bound-p (fboundp 'obsolete-macro))
                     (setter-name (fmakunbound '(setf obsolete-accessor)))
                     (setter-bound-p (fboundp '(setf obsolete-accessor))))
                 (values function-name function-bound-p macro-name macro-bound-p setter-name setter-bound-p)))"
        ),
        ["OBSOLETE-FUNCTION", "NIL", "OBSOLETE-MACRO", "NIL", "(SETF OBSOLETE-ACCESSOR)", "NIL"]
    );
}

#[test]
fn function_lambda_expression_reports_captured_lexicals() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((captured 10))
               (flet ((non-closure (x) x)
                      (closure (x) (+ x captured)))
                 (multiple-value-bind (primitive-expression primitive-closure-p primitive-name) (function-lambda-expression #'cons)
                   (declare (ignore primitive-expression primitive-name))
                   (multiple-value-bind (local-expression local-closure-p local-name) (function-lambda-expression #'non-closure)
                     (declare (ignore local-expression local-name))
                     (multiple-value-bind (capturing-expression capturing-closure-p capturing-name) (function-lambda-expression #'closure)
                       (declare (ignore capturing-expression capturing-name))
                       (values (length (multiple-value-list (function-lambda-expression #'cons)))
                               primitive-closure-p local-closure-p (not (null capturing-closure-p))))))))"
        ),
        ["3", "NIL", "NIL", "T"]
    );
}

#[test]
fn lambda_list_constants_cannot_be_reassigned() {
    let runtime = Runtime::start().unwrap();
    for name in [
        "call-arguments-limit",
        "lambda-parameters-limit",
        "multiple-values-limit",
        "lambda-list-keywords",
    ] {
        assert!(runtime.eval(&format!("(setq {name} 1)")).is_err(), "{name}");
    }
    assert_eq!(
        eval_reprs(
            &runtime,
            "(values
               (handler-case (set 'call-arguments-limit 1) (program-error () :const))
               (handler-case (set 'lambda-parameters-limit 1) (program-error () :const))
               (handler-case (set 'multiple-values-limit 1) (program-error () :const))
               (handler-case (set 'lambda-list-keywords nil) (program-error () :const))
               (>= call-arguments-limit 50)
               (>= lambda-parameters-limit 50)
               (>= multiple-values-limit 20)
               (every #'symbolp lambda-list-keywords)
               (every (lambda (k) (member k lambda-list-keywords))
                      '(&optional &rest &body &key &allow-other-keys &aux &whole &environment)))"
        ),
        [
            ":CONST", ":CONST", ":CONST", ":CONST", "T", "T", "T", "T", "T"
        ]
    );
}
