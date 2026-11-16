use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn assert_continue_updates_places_before_retrying_the_test() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("assertion-collect", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(let ((checks 0) (datums 0) (arguments 0) (restarts 0))
                 (handler-bind
                     ((simple-error (lambda (c)
                                      (setq restarts (+ restarts 1))
                                      (assertion-collect)
                                      (continue c))))
                   (values
                      (assert (progn (setq checks (+ checks 1)) (= checks 3))
                              nil
                              (progn (setq datums (+ datums 1)) "attempt ~D")
                              (progn (setq arguments (+ arguments 1)) checks))
                      checks datums arguments restarts)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values[0].is_nil());
    assert_eq!(values[1].as_fixnum(), Some(3));
    for value in &values[2..] {
        assert_eq!(value.as_fixnum(), Some(2));
    }

    let values = runtime
        .eval(
            r#"(let ((condition (make-condition 'simple-error :format-control "original"))
                     (ready nil))
                 (handler-bind
                     ((error (lambda (observed)
                               (when (and (eq observed condition)
                                          (find-restart 'continue observed))
                                 (setq ready t)
                                 (continue observed)))))
                   (values (assert ready nil condition) ready)))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert!(!values[1].is_nil());

    let values = runtime
        .eval(
            "(handler-case (assert nil () 'type-error :datum 17 :expected-type 'string)
               (type-error (c)
                 (values (type-error-datum c)
                         (eq (type-error-expected-type c) 'string))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(17));
    assert!(!values[1].is_nil());

    let values = runtime
        .eval(
            r#"(let ((touches 0) (cell (cons nil nil)))
                 (values
                   (macrolet ((passed () '(values t 19)))
                     (assert (passed)
                             ((car (progn (setq touches (+ touches 1)) cell)))
                             (error "success must not evaluate datum")
                             (error "success must not evaluate arguments")))
                   touches))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert_eq!(values[1].as_fixnum(), Some(0));

    let values = runtime
        .eval(
            r#"(let ((cell (cons nil 19)) (touches 0) (before -1))
                 (handler-bind
                     ((error (lambda (c)
                               (setq before touches)
                               (assertion-collect)
                               (invoke-restart (find-restart 'continue c) t nil))))
                   (values
                     (assert (car cell)
                             ((car (progn (setq touches (+ touches 1)) cell))
                              (cdr cell)))
                     (car cell) (cdr cell) before touches)))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert!(!values[1].is_nil());
    assert!(
        values[2].is_nil(),
        "an explicit NIL replaces the second place"
    );
    assert_eq!(values[3].as_fixnum(), Some(0));
    assert_eq!(values[4].as_fixnum(), Some(1));

    let values = runtime
        .eval(
            "(handler-case (assert (= 1 2))
               (simple-error (c)
                 (equal (simple-condition-format-arguments c) '((= 1 2)))))",
        )
        .unwrap();
    assert!(!values[0].is_nil());

    let error = runtime.eval("(assert nil)").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::SimpleError)
    );
    assert!(error.message().contains("assertion"));
    assert!(runtime.eval("(assert)").is_err());
    assert!(runtime.eval("(assert t (x . y))").is_err());
    assert!(!runtime.eval("(macro-function 'assert)").unwrap()[0].is_nil());
}
