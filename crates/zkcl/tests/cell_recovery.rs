use zkcl::{EvalErrorKind, Runtime};

fn numbers(runtime: &Runtime, source: &str) -> Vec<isize> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum().unwrap())
        .collect()
}

#[test]
fn continue_retries_only_the_undefined_call_and_keeps_multiple_values() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((effects 0))
        (handler-bind ((undefined-function
                        (lambda (c)
                          (assert (find-restart 'continue c))
                          (assert (find-restart 'use-value c))
                          (assert (find-restart 'store-value c))
                          (defun repaired-call (x) (values x (+ x 1)))
                          (continue c))))
          (multiple-value-bind (a b) (repaired-call (incf effects))
            (values a b effects))))"#
        ),
        [1, 2, 1]
    );
}

#[test]
fn use_value_preserves_argument_identity_through_collection_and_cleanup() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-in-recovery", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((effects 0) (cleaned 0) (argument (list 42)))
        (handler-bind ((undefined-function
                        (lambda (c)
                          (collect-in-recovery)
                          (unwind-protect
                              (use-value (lambda (x) (values (car x) (if (eq x argument) 1 0))) c)
                            (incf cleaned)))))
          (multiple-value-bind (a b) (missing-with-value (progn (incf effects) argument))
            (values a b effects cleaned (if (fboundp 'missing-with-value) 1 0)))))"#
        ),
        [42, 1, 1, 1, 0]
    );
}

#[test]
fn funcall_and_apply_recover_without_repeating_arguments() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((effects 0))
        (handler-bind ((undefined-function (lambda (c) (use-value #'+ c))))
          (values (funcall 'missing-funcall (incf effects) 40)
                  (apply 'missing-apply (incf effects) '(40)) effects)))"#
        ),
        [41, 42, 2]
    );
}

#[test]
fn store_value_updates_the_original_dynamic_binding_after_handler_cleanup() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defvar *recovery-unbound*)").unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((cleaned 0))
        (let ((answer
                (progv '(*recovery-unbound*) nil
                  (handler-bind ((unbound-variable
                                  (lambda (c)
                                    (let ((*recovery-unbound* 99))
                                      (unwind-protect (store-value 42 c)
                                        (setq cleaned *recovery-unbound*))))))
                    (list *recovery-unbound* (symbol-value '*recovery-unbound*))))))
          (values (first answer) (second answer) cleaned
                  (if (boundp '*recovery-unbound*) 1 0))))"#
        ),
        [42, 42, 99, 0]
    );
}

#[test]
fn variable_use_value_is_local_to_one_read_and_restarts_are_condition_scoped() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((calls 0))
        (handler-bind ((unbound-variable
                        (lambda (c)
                          (assert (null (find-restart 'use-value (make-condition 'simple-error))))
                          (use-value (incf calls) c))))
          (values recovery-only-once (symbol-value 'recovery-only-once) calls
                  (if (boundp 'recovery-only-once) 1 0))))"#
        ),
        [1, 2, 2, 0]
    );
}

#[test]
fn store_value_installs_a_function_for_later_calls() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((calls 0))
        (handler-bind ((undefined-function
                        (lambda (c) (incf calls) (store-value (lambda (x) (values x 9)) c))))
          (multiple-value-bind (a b) (stored-function 42)
            (values a b (stored-function 7) calls))))"#
        ),
        [42, 9, 7, 1]
    );
}

#[test]
fn debugger_hook_can_repair_and_continue_the_same_activation() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((effects 0)
            (*debugger-hook* (lambda (c old-hook)
                              (declare (ignore old-hook))
                              (defun debugger-repaired (x) (values x 42))
                              (continue c))))
        (multiple-value-bind (a b) (debugger-repaired (incf effects))
          (values a b effects)))"#
        ),
        [1, 42, 1]
    );
}

#[test]
fn toplevel_abort_is_visible_runs_cleanup_and_expires() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defparameter *abort-cleanup* 0) (defvar *saved-abort*)")
        .unwrap();
    let error = runtime
        .eval(
            r#"
      (unwind-protect
          (progn (setq *saved-abort* (find-restart 'abort))
                 (assert *saved-abort*) (abort) (setq *abort-cleanup* -1))
        (setq *abort-cleanup* 1))"#,
        )
        .err()
        .expect("the invocation aborts");
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    assert_eq!(
        numbers(
            &runtime,
            "(values *abort-cleanup* (if (find-restart *saved-abort*) 1 0))"
        ),
        [1, 0]
    );
}

#[test]
fn interactive_value_restart_reads_and_evaluates_one_argument() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((*query-io* (make-two-way-stream (make-string-input-stream "(+ 40 2)")
                                             (make-string-output-stream))))
        (handler-bind ((unbound-variable (lambda (c) (invoke-restart-interactively (find-restart 'use-value c)))))
          recovery-interactive-value))"#
        ),
        [42]
    );
}

#[test]
fn multiple_value_call_preserves_already_evaluated_argument_groups() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((effects 0))
        (handler-bind ((undefined-function (lambda (c) (use-value #'list c))))
          (let ((answer (multiple-value-call 'missing-values-call
                          (progn (incf effects) (values 1 2))
                          (progn (incf effects) (values 3)))))
            (values (first answer) (second answer) (third answer) effects))))"#
        ),
        [1, 2, 3, 2]
    );
    assert!(
        runtime
            .eval(
                r#"
      (handler-bind ((undefined-function (lambda (c) (use-value (lambda () (values)) c))))
        (missing-zero-values))"#
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn function_store_value_reinstalls_trace_and_preserves_saved_functions() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defun recovery-traced (x) (values x 1))")
        .unwrap();
    assert_eq!(
        numbers(
            &runtime,
            r#"
      (let ((old #'recovery-traced)
            (replacement (lambda (x) (values (+ x 10) 2)))
            (*trace-output* (make-string-output-stream)))
        (trace recovery-traced)
        (fmakunbound 'recovery-traced)
        (handler-bind ((undefined-function (lambda (c) (store-value replacement c))))
          (multiple-value-bind (first second) (recovery-traced 1)
            (let* ((saved #'recovery-traced)
                   (named (recovery-traced 2))
                   (saved-result (funcall saved 3))
                   (old-result (funcall old 4))
                   (reported (if (member 'recovery-traced (trace)) 1 0))
                   (printed (if (search "TRACE RECOVERY-TRACED: enter"
                                       (get-output-stream-string *trace-output*)) 1 0)))
              (untrace recovery-traced)
              (values first second named saved-result old-result reported printed
                      (if (eq replacement #'recovery-traced) 1 0))))))"#
        ),
        [11, 2, 12, 13, 4, 1, 1, 1]
    );
}
