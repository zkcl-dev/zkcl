use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn compiles_a_deeply_nested_macro_expansion_on_the_default_test_stack() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defmacro nested-lets (depth)
               (let ((form 37))
                 (dotimes (index depth form)
                   (setf form (list 'let nil form)))))",
        )
        .unwrap();

    let values = runtime.eval("(nested-lets 128)").unwrap();
    assert_eq!(values[0].as_fixnum(), Some(37));
}

#[test]
fn compile_updates_the_named_definition_without_invalidating_saved_functions() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-answer", |runtime, _| {
            Ok(vec![runtime.fixnum(42).unwrap()])
        })
        .unwrap();
    runtime
        .register_function("collect-during-compilation", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    let mut result = runtime
        .eval("(compile nil '(lambda (n &optional (offset 2)) (values (+ n offset) n)))")
        .unwrap();
    assert_eq!(result.len(), 3);
    assert!(result.pop().unwrap().is_nil());
    assert!(result.pop().unwrap().is_nil());
    let function = result.pop().unwrap();
    let argument = runtime.fixnum(40).unwrap();
    let result = runtime.call(&function, &[&argument]).unwrap();
    assert_eq!(
        result
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(42), Some(40)]
    );

    let result = runtime
        .eval(
            "(values (eq (compile nil #'host-answer) #'host-answer)
                     (compiled-function-p #'host-answer)
                     (typep #'host-answer 'compiled-function)
                     (compiled-function-p #'+)
                     (let ((n 1)) (compiled-function-p (lambda () n)))
                     (let ((n 1)) (typep (lambda () n) 'compiled-function))
                     (functionp '(lambda () nil)))",
        )
        .unwrap();
    assert!(result[..6].iter().all(|value| !value.is_nil()));
    assert!(result[6].is_nil());

    runtime
        .load_str(
            "(compile 'compiled-target '(lambda () 17))
             (setq saved-compiled-target #'compiled-target)
             (defmacro recompilable-macro (&rest forms) 17)
             (defmacro force-compile-collection () (collect-during-compilation) nil)",
        )
        .unwrap();
    let result = runtime
        .eval(
            "(values (compile 'compiled-target)
                     (eq saved-compiled-target #'compiled-target)
                     (compile 'recompilable-macro)
                     (compile 'when))",
        )
        .unwrap();
    assert_eq!(
        runtime.symbol_name(&result[0]).as_deref(),
        Some("COMPILED-TARGET")
    );
    assert!(!result[1].is_nil());
    assert_eq!(
        runtime.symbol_name(&result[2]).as_deref(),
        Some("RECOMPILABLE-MACRO")
    );
    assert_eq!(runtime.symbol_name(&result[3]).as_deref(), Some("WHEN"));

    runtime
        .load_str(
            "(compile 'compiled-target #'host-answer)
             (compile 'recompilable-macro '(lambda (form environment) 42))",
        )
        .unwrap();
    let result = runtime
        .eval("(values (compiled-target) (funcall saved-compiled-target) (recompilable-macro))")
        .unwrap();
    assert_eq!(
        result
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(42), Some(17), Some(42)]
    );

    let result = runtime
        .eval(
            "(let* ((literal (list 1 2))
                    (definition (list 'lambda nil '(force-compile-collection)
                                      (list 'quote literal))))
               (values literal (compile nil definition)))",
        )
        .unwrap();
    runtime.collect();
    let returned = runtime.call(&result[1], &[]).unwrap();
    assert!(returned[0].same_identity(&result[0]));

    assert_failed_compilation_preserves_the_installed_definition(&runtime);
    assert_global_function_definition_queries(&runtime);
}

fn assert_failed_compilation_preserves_the_installed_definition(runtime: &Runtime) {
    for (source, expected) in [
        (
            "(compile 'undefined-compile-target)",
            ConditionClass::UndefinedFunction,
        ),
        ("(compile nil)", ConditionClass::ProgramError),
        ("(compile nil 42)", ConditionClass::TypeError),
        ("(compile 42 #'+)", ConditionClass::TypeError),
        (
            "(compile '(setf missing-accessor))",
            ConditionClass::UndefinedFunction,
        ),
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(expected),
            "{source}"
        );
    }

    let result = runtime
        .eval(
            "(values (handler-case (compile 'compiled-target '(lambda (42) 0))
                       (program-error (condition) 1))
                     (compiled-target))",
        )
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(1));
    assert_eq!(result[1].as_fixnum(), Some(42));

    let result = runtime
        .eval(
            "(handler-case (compile nil 42)
               (type-error (condition)
                 (values (type-error-datum condition)
                         (equal (type-error-expected-type condition)
                                '(or function (cons (eql lambda) cons))))))",
        )
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(42));
    assert!(!result[1].is_nil());
}

fn assert_global_function_definition_queries(runtime: &Runtime) {
    let result = runtime
        .eval(
            "(values (fboundp 'compiled-target)
                     (fboundp 'recompilable-macro)
                     (fboundp 'when)
                     (fboundp 'if)
                     (fboundp 'defun)
                     (special-operator-p 'multiple-value-call)
                     (eq (fdefinition 'compiled-target) #'compiled-target)
                     (eq (symbol-function 'recompilable-macro)
                         (macro-function 'recompilable-macro))
                     (eq (fdefinition 'if) 'if)
                     (fboundp 'declare)
                     (fboundp (make-symbol \"IF\"))
                     (fboundp '(setf compiled-target))
                     (special-operator-p 'lambda)
                     (special-operator-p 'when)
                     (flet ((only-lexical () 42)) (fboundp 'only-lexical)))",
        )
        .unwrap();
    assert!(result[..9].iter().all(|value| !value.is_nil()));
    assert!(result[9..].iter().all(|value| value.is_nil()));

    let result = runtime
        .eval(
            "(flet ((compiled-target () 17))
               (values (compiled-target) (funcall (symbol-function 'compiled-target))))",
        )
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(17));
    assert_eq!(result[1].as_fixnum(), Some(42));

    let result = runtime
        .eval(
            "(handler-case (fdefinition '(setf compiled-target))
               (undefined-function (condition)
                 (equal (cell-error-name condition) '(setf compiled-target))))",
        )
        .unwrap();
    assert!(!result[0].is_nil());

    let result = runtime
        .eval(
            "(handler-case (fboundp 42)
               (type-error (condition)
                 (equal (type-error-expected-type condition)
                        '(or symbol (cons (eql setf) (cons symbol null))))))",
        )
        .unwrap();
    assert!(!result[0].is_nil());

    for source in [
        "(fboundp '(setf 42))",
        "(fdefinition '(setf compiled-target extra))",
        "(symbol-function '(setf compiled-target))",
        "(special-operator-p #'+)",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}"
        );
    }
}

#[test]
fn setf_call_resolution_respects_lexical_scope() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun compiled-accessor (cell) (car cell))
             (defun (setf compiled-accessor) (value cell)
               (rplaca cell value)
               (return-from compiled-accessor value))
             (compile '(setf added-accessor) '(lambda (new base) (+ new base)))
             (let ((offset 7))
               (setf (fdefinition '(setf captured-accessor))
                     (lambda (new) (+ new offset))))",
        )
        .unwrap();
    runtime.collect();

    let results = runtime
        .eval(
            "(let ((cell (list 0)))
               (values (setf (compiled-accessor cell) 12)
                       (funcall #'(setf compiled-accessor) 35 cell)
                       (incf (compiled-accessor cell) 7)
                       (compiled-accessor cell)
                       (funcall #'(setf added-accessor) 7 35)
                       (funcall #'(setf captured-accessor) 35)
                       (eq (fdefinition '(setf compiled-accessor))
                           #'(setf compiled-accessor))
                       (fboundp '(setf compiled-accessor))
                       (equal (compile '(setf compiled-accessor))
                              '(setf compiled-accessor))))",
        )
        .unwrap();
    assert_eq!(
        results[..6]
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(12), Some(35), Some(42), Some(42), Some(42), Some(42)]
    );
    assert!(results[6..].iter().all(|value| !value.is_nil()));

    let results = runtime
        .eval(
            "(flet ((compiled-accessor (x) (+ x 1))
                    ((setf compiled-accessor) (new x) (return-from compiled-accessor (+ new x)))
                    ((setf only-local) (new x) (+ new x)))
               (labels (((setf countdown) (new n)
                          (if (zerop n) new (funcall #'(setf countdown) (+ new 1) (- n 1)))))
                 (values (compiled-accessor 41)
                         (setf (compiled-accessor 40) 2)
                         (funcall #'(setf countdown) 39 3)
                         (fboundp '(setf only-local)))))",
        )
        .unwrap();
    assert!(
        results[..3]
            .iter()
            .all(|value| value.as_fixnum() == Some(42))
    );
    assert!(results[3].is_nil());

    let results = runtime
        .eval(
            "(let* ((name (make-symbol \"COMPILED-ACCESSOR\"))
                    (setter (list 'setf name)))
               (setf (fdefinition setter) #'list)
               (values (eq (fdefinition setter) #'list)
                       (eq (fdefinition '(setf compiled-accessor)) #'list)
                       (fboundp name)
                       (handler-case #'(setf missing-accessor)
                         (undefined-function (condition)
                           (equal (cell-error-name condition) '(setf missing-accessor))))))",
        )
        .unwrap();
    assert!(!results[0].is_nil());
    assert!(results[1].is_nil());
    assert!(results[2].is_nil());
    assert!(!results[3].is_nil());
}

#[test]
fn function_docstrings_do_not_hide_parameter_declarations() {
    let runtime = Runtime::start().unwrap();

    let results = runtime
        .eval(
            r#"(progn
                 (defun documented-binding (value)
                   (declare (ignorable value))
                   "Parameter declarations following the documentation still apply."
                   (declare (special value))
                   (symbol-value 'value))
                 (values (documented-binding 42)
                         (funcall (compile nil '(lambda () "returned string")))
                         ((lambda () "documentation" (declare (optimize safety))))))"#,
        )
        .unwrap();
    assert_eq!(results[0].as_fixnum(), Some(42));
    assert_eq!(
        runtime.string(&results[1]).as_deref(),
        Some("returned string")
    );
    assert!(results[2].is_nil());
}
