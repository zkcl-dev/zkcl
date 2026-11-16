use zkcl::{ConditionClass, EvalErrorKind, Runtime};

fn eval_fixnums(runtime: &Runtime, source: &str) -> Vec<isize> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum().unwrap())
        .collect()
}

#[test]
fn find_restart_skips_ineligible_candidates() {
    let rt = Runtime::start().unwrap();
    let result = rt
        .eval(
            r#"
      (let ((a (make-condition 'condition)) (b (make-condition 'condition))
            (other (make-condition 'condition)) (tested nil))
        (restart-bind ((pick (lambda (x) x) :test-function (lambda (c) (push :first tested) t))
                       (pick (lambda (x) x) :test-function (lambda (c) (push :second tested) t))
                       (unused #'identity :test-function (lambda (c) (push :unused tested) t)))
          (let ((r (find-restart 'pick)))
            (with-condition-restarts a (list r)
              (with-condition-restarts b (list r)
                (and (eq r (find-restart 'pick a)) (eq r (find-restart 'pick b))
                     (not (eq r (find-restart 'pick other)))
                     (equal tested '(:second :first :first :first))))))))
    "#,
        )
        .unwrap();
    assert!(!result[0].is_nil());
}

#[test]
fn invalid_restart_arguments_report_typed_conditions() {
    let rt = Runtime::start().unwrap();
    for form in [
        "(find-restart 23)",
        "(compute-restarts 23)",
        "(with-condition-restarts 23 nil :bad)",
        "(with-condition-restarts (make-condition 'condition) '(23) :bad)",
    ] {
        let result = rt
            .eval(&format!(
                "(handler-case {form} (type-error (c)
          (and (eql (type-error-datum c) 23) (not (typep 23 (type-error-expected-type c))))))"
            ))
            .unwrap();
        assert!(!result[0].is_nil(), "{form}");
    }
    for form in ["(invoke-restart nil)", "(invoke-restart-interactively 23)"] {
        let error = rt.eval(form).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ControlError)
        );
    }
}

#[test]
fn restart_test_can_query_outer_restarts_during_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-restart", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();

    assert_eq!(
        eval_fixnums(
            &runtime,
            "(let ((tests 0))
               (restart-bind ((choose (lambda () 10)))
                 (let ((outer (find-restart 'choose)))
                   (restart-bind
                       ((choose (lambda () 20)
                                :test-function
                                  (lambda (condition)
                                    (setq tests (+ tests 1))
                                    (if (= tests 1)
                                        (progn (collect-during-restart)
                                               (eq (car (compute-restarts condition)) outer))
                                        nil))))
                     (values (invoke-restart 'choose) tests)))))",
        ),
        [20, 1]
    );
}

#[test]
fn standard_restart_invokers_handle_missing_restarts() {
    let runtime = Runtime::start().unwrap();

    assert!(
        runtime
            .eval("(values (continue) (use-value 3 nil) (store-value 4))")
            .unwrap()
            .iter()
            .all(|value| value.is_nil())
    );
    for missing in ["(muffle-warning)"] {
        let error = runtime.eval(missing).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ControlError)
        );
    }
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(handler-case (restart-bind ((abort (lambda () 99))) (abort))
               (control-error () 13))",
        ),
        [13]
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(handler-bind ((error #'abort))
               (restart-case (error \"expected to abort\")
                 (abort () (values 7 8))))",
        ),
        [7, 8]
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(restart-case (muffle-warning nil) (muffle-warning () 16))"
        ),
        [16]
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(restart-bind ((continue (lambda () (values 1 2)))) (continue))"
        ),
        [1, 2]
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(restart-bind ((store-value (lambda (x) (+ x 1)))) (store-value 19))"
        ),
        [20]
    );
}

#[test]
fn use_value_selects_a_restart_for_the_given_condition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-restart", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();

    assert_eq!(
        eval_fixnums(
            &runtime,
            "(let ((wanted (make-condition 'simple-error))
                   (other (make-condition 'simple-error))
                   (tested 0))
               (restart-bind ((use-value (lambda (x) x)
                                        :test-function
                                          (lambda (c)
                                            (collect-during-restart)
                                            (setq tested (+ tested 1))
                                            (eq wanted c))))
                 (let ((r (car (compute-restarts wanted))))
                   (with-condition-restarts wanted (list r)
                     (values (if (use-value 0 other) -1 (use-value 42 wanted))
                             tested)))))",
        ),
        [42, 2]
    );
}

#[test]
fn cleanup_reenters_lisp_during_restart_unwind() {
    let runtime = Runtime::start().unwrap();

    runtime
        .load_str("(defparameter *cleanup-state* (list 1))")
        .unwrap();
    runtime
        .register_function("inspect-cleanup-state", |runtime, _| {
            runtime.collect();
            Ok(runtime.eval("(values (car *cleanup-state*) (find-restart 'observe))")?)
        })
        .unwrap();
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(let ((seen 0))
               (multiple-value-bind (first second)
                   (catch :done
                     (let ((*cleanup-state* (list 2)))
                       (restart-bind ((observe (lambda () (car *cleanup-state*))))
                         (unwind-protect
                             (progv '(*cleanup-state*) '((3))
                               (handler-bind
                                   ((type-error
                                      (lambda (c)
                                        (throw :done (values (list (car *cleanup-state*))
                                                             (list (type-error-datum c)))))))
                                 (car 4)))
                           (multiple-value-bind (dynamic restart) (inspect-cleanup-state)
                             (setq seen (if restart -1 (+ dynamic (invoke-restart 'observe)))))))))
                 (values (car first) (car second) seen (car *cleanup-state*))))",
        ),
        [3, 4, 4, 1]
    );
}
