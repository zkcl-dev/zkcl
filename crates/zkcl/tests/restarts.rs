use zkcl::Runtime;

fn eval_numbers(runtime: &Runtime, source: &str) -> Vec<Option<isize>> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum())
        .collect()
}

#[test]
fn restart_bind_invokes_the_function_in_the_callers_dynamic_scope() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defvar *level* 1)").unwrap();

    assert_eq!(
        eval_numbers(
            &rt,
            "(restart-bind ((use (lambda (x) (values x *level*))))
           (let ((*level* 5)) (invoke-restart 'use 4)))",
        ),
        [Some(4), Some(5)]
    );
}

#[test]
fn restart_case_preserves_values_when_no_restart_is_invoked() {
    let rt = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(&rt, "(restart-case (values 3 4) (unused () 9))"),
        [Some(3), Some(4)]
    );
    assert!(
        rt.eval("(restart-case (values) (unused () 9))")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn restart_case_unwinds_before_running_the_restart_clause() {
    let rt = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &rt,
            "(let ((cleaned 0))
           (handler-bind ((error (lambda (c) (invoke-restart (find-restart 'use c) 8))))
             (restart-case (unwind-protect (error \"failure\") (setq cleaned 1))
               (use (x) (values x cleaned (find-restart 'use))))))",
        ),
        [Some(8), Some(1), None]
    );
}

#[test]
fn restart_case_limits_visibility_to_the_signaled_condition() {
    let rt = Runtime::start().unwrap();

    // A directly signaled condition is associated with the case's restarts.
    assert_eq!(
        eval_numbers(
            &rt,
            "(block fail
           (let ((other (make-condition 'simple-error)))
             (handler-bind ((error (lambda (c)
                                      (if (find-restart 'use other) (return-from fail -1))
                                      (invoke-restart (find-restart 'use c) 9))))
               (restart-case (error \"original\") (use (arg) arg)))))",
        ),
        [Some(9)]
    );

    let associated = rt
        .eval(
            "(let ((this (make-condition 'simple-error)) (other (make-condition 'simple-error)))
           (restart-bind ((use (lambda () 17)))
             (let ((r (find-restart 'use)))
               (with-condition-restarts this (list r)
                 (values (eq r (find-restart 'use this))
                         (find-restart 'use other) (eq r (find-restart 'use)))))))",
        )
        .unwrap();
    assert!(!associated[0].is_nil());
    assert!(associated[1].is_nil());
    assert!(!associated[2].is_nil());

    assert_eq!(
        eval_numbers(
            &rt,
            "(macrolet ((fail () (list 'error \"from local macro\")))
               (block missing-association
                 (let ((other (make-condition 'simple-error)))
                   (handler-bind
                       ((error (lambda (c)
                                 (if (find-restart 'use other)
                                     (return-from missing-association -1))
                                 (invoke-restart (find-restart 'use c) 18))))
                     (restart-case (fail) (use (arg) arg))))))",
        ),
        [Some(18)]
    );
}

#[test]
fn restart_objects_distinguish_same_named_or_anonymous_clauses() {
    let rt = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &rt,
            "(restart-case (invoke-restart (car (cdr (compute-restarts))))
           (same () 1) (same () 2) (nil () 3))",
        ),
        [Some(2)]
    );
    let anonymous = rt
        .eval("(restart-case (restart-name (car (compute-restarts))) (nil () 3))")
        .unwrap();
    assert!(anonymous[0].is_nil());
}

#[test]
fn restart_tests_run_in_the_dynamic_control_scope() {
    let rt = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &rt,
            "(let ((condition (make-condition 'simple-error)))
           (restart-bind ((visible (lambda (n) n)
                            :test-function (lambda (c) (eq c condition))))
             (let ((r (find-restart 'visible condition)))
               (if (find-restart 'visible) 0 (invoke-restart r 81)))))",
        ),
        [Some(81)]
    );
    assert_eq!(
        eval_numbers(
            &rt,
            "(let ((calls 0))
           (handler-bind ((condition (lambda (c) (setq calls (+ calls 1)))))
             (restart-case
                 (invoke-restart 'visible)
               (visible () :test (lambda (c) (signal \"checking\") t) calls))))",
        ),
        [Some(1)]
    );
    assert_eq!(
        eval_numbers(
            &rt,
            "(block stop
           (restart-bind ((r (lambda () 1) :test-function (lambda (c) (return-from stop 77))))
             (find-restart 'r))
           0)",
        ),
        [Some(77)]
    );
}

#[test]
fn exited_restart_remains_inspectable_but_cannot_be_invoked() {
    let rt = Runtime::start().unwrap();

    rt.load_str("(setq stale (restart-case (find-restart 'gone) (gone () nil)))")
        .unwrap();
    rt.collect();
    assert_eq!(
        rt.symbol_name(&rt.eval("(restart-name stale)").unwrap()[0])
            .as_deref(),
        Some("GONE")
    );
    assert!(rt.eval("(find-restart stale)").unwrap()[0].is_nil());
    assert_eq!(
        eval_numbers(
            &rt,
            "(handler-case (invoke-restart stale) (control-error () 42))"
        ),
        [Some(42)]
    );
}

#[test]
fn interactive_restart_gets_arguments_in_the_callers_dynamic_scope() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defvar *level* 1)").unwrap();

    assert_eq!(
        eval_numbers(
            &rt,
            "(restart-case (invoke-restart-interactively 'choose)
           (choose (a b) :interactive (lambda () (list 5 8)) (values b a)))",
        ),
        [Some(8), Some(5)]
    );
    assert_eq!(
        eval_numbers(
            &rt,
            "(restart-bind ((choose (lambda () 12))) (invoke-restart-interactively 'choose))",
        ),
        [Some(12)]
    );
    assert_eq!(
        eval_numbers(
            &rt,
            "(restart-bind ((choose (lambda (x) (values x *level*))
                         :interactive-function (lambda () (list (+ *level* 1)))))
           (let ((*level* 10)) (invoke-restart-interactively 'choose)))",
        ),
        [Some(11), Some(10)]
    );
}

#[test]
fn macroexpansion_exposes_simple_restart_to_code_walkers() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defun walk-restart-example (form)
          (cond ((atom form) form)
                ((eq (car form) 'marker) (second form))
                ((and (symbolp (car form)) (special-operator-p (car form))
                      (not (member (car form) '(function progn))))
                 form)
                (t (mapcar #'walk-restart-example form))))"#,
        )
        .unwrap();
    runtime
        .eval(
            r#"
      (let ((form '(with-simple-restart (retry "~A" (marker :report))
                     (values (marker :body) (princ-to-string (find-restart 'retry))))))
        (assert (equal (multiple-value-list (eval (walk-restart-example (macroexpand-1 form))))
                       '(:body "REPORT"))))"#,
        )
        .unwrap();
}

#[test]
fn invoking_an_outer_simple_restart_unwinds_inner_scopes() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(
            r#"
      (let ((cleanup 0) (condition (make-condition 'simple-error)))
        (assert (equal
                 (multiple-value-list
                  (with-simple-restart (retry "outer")
                    (let ((outer (find-restart 'retry)))
                      (with-condition-restarts condition (list outer)
                        (with-simple-restart (retry "inner")
                          (unwind-protect (invoke-restart outer) (incf cleanup)))))))
                 '(nil t)))
        (assert (= cleanup 1))
        (assert (null (find-restart 'retry)))
        (assert (null (multiple-value-list (with-simple-restart (nil "empty") (values))))))"#,
        )
        .unwrap();
}
