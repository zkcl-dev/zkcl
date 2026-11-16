use std::error::Error;

use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn debugger_entry_unwinds_with_the_original_condition() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str(r#"
      (defvar *debug-events* nil)
      (defvar *debug-condition* (make-condition 'simple-error :format-control "original"))
      (defun record-hook (condition hook)
        (push (if (and (eq condition *debug-condition*) (eq hook 'record-hook) (null *debugger-hook*))
                  :hook :wrong-hook-arguments) *debug-events*) :ignored)
      (defun record-debugger (condition)
        (push (if (eq condition *debug-condition*) :debugger :wrong-condition) *debug-events*) :ignored)
    "#).unwrap();
    let frontend = runtime.eval("#'record-debugger").unwrap();
    runtime.set_debugger(Some(&frontend[0])).unwrap();
    drop(frontend);
    runtime.collect();
    let other = Runtime::start().unwrap();
    let foreign = other.eval("#'identity").unwrap();
    assert!(runtime.set_debugger(Some(&foreign[0])).is_err());
    for entry in [
        "(invoke-debugger *debug-condition*)",
        "(funcall #'invoke-debugger *debug-condition*)",
        "(error *debug-condition*)",
    ] {
        let intercept = if entry.starts_with("(error") {
            entry.to_owned()
        } else {
            format!("(handler-case {entry} (error () :intercepted))")
        };
        let error = runtime
            .eval(&format!(
                "(let ((*debugger-hook* 'record-hook))
            (unwind-protect {intercept} (push :cleanup *debug-events*)))"
            ))
            .err()
            .unwrap();
        assert_eq!(error.message(), "original");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::SimpleError)
        );
        assert!(!runtime.eval("(prog1 (equal *debug-events* '(:cleanup :debugger :hook)) (setq *debug-events* nil))").unwrap()[0].is_nil());
    }
    runtime.set_debugger(None).unwrap();
    let error = runtime
        .eval("(ignore-errors (invoke-debugger *debug-condition*))")
        .err()
        .unwrap();
    assert_eq!(error.message(), "original");
}

#[test]
fn break_on_signals_enters_the_debugger_before_dispatching_handlers() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (defvar *events* nil)
      (defvar *seen-condition* nil)
      (defun debug-condition-p (c) (push :test *events*) (typep c 'error))
      (defun debug-before-handlers (c hook)
        (declare (ignore hook))
        (when *seen-condition* (assert (eq *seen-condition* c)))
        (setf *seen-condition* c)
        (cond ((find-restart 'continue c) (push :before *events*) (continue c))
              (t (push :unhandled *events*) (throw :debug c))))
    "#,
        )
        .unwrap();
    let values = runtime
        .eval(
            r#"
      (let* ((c (make-condition 'simple-error))
             (*break-on-signals* '(and error (satisfies debug-condition-p)))
             (*debugger-hook* #'debug-before-handlers))
        (handler-bind ((error (lambda (x) (assert (eq c x)) (push :handler *events*))))
          (signal 'warning)
          (signal c))
        (values (eq c *seen-condition*) (equal (reverse *events*) '(:test :before :handler))
                (null (find-restart 'continue))))"#,
        )
        .unwrap();
    assert!(values.iter().all(|v| !v.is_nil()));
    for form in ["(error \"unhandled\")", "(car 37)"] {
        let values = runtime
            .eval(&format!(
                "(let ((*events* nil) (*seen-condition* nil)
              (*break-on-signals* 'error) (*debugger-hook* #'debug-before-handlers))
            (let ((caught (catch :debug {form})))
              (and (eq caught *seen-condition*) (equal *events* '(:unhandled :before)))))"
            ))
            .unwrap();
        assert!(!values[0].is_nil(), "{form}");
    }
}

#[test]
fn break_returns_one_nil_when_the_debugger_continues() {
    let runtime = Runtime::start().unwrap();
    let frontend = runtime
        .eval(
            r#"(lambda (c)
      (unless (and (null *debugger-hook*) (equal (princ-to-string c) "At 7")
                   (equal (princ-to-string (find-restart 'continue c)) "Return from BREAK."))
        (throw :bad :debugger))
      (continue c))"#,
        )
        .unwrap();
    runtime.set_debugger(Some(&frontend[0])).unwrap();
    let result = runtime
        .eval(
            r#"(catch :bad
        (let ((*debugger-hook* (lambda (&rest args) (declare (ignore args)) (throw :bad :hook)))
              (*break-on-signals* t))
          (handler-bind ((condition (lambda (c) (throw :bad c))))
            (break "At ~D" 7))))"#,
        )
        .unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].is_nil());
}

#[test]
fn condition_callbacks_can_transfer_control_to_the_caller() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defun choosing-p (c) (throw :choice c))
        (defun failing-p (c) (declare (ignore c)) (error "predicate failed"))
        (define-condition reporting-warning (warning) ()
          (:report (lambda (c stream) (declare (ignore stream)) (throw :report c))))
    "#,
        )
        .unwrap();
    let values = runtime.eval(r#"(let ((c (make-condition 'reporting-warning)))
      (values
        (eq c (catch :choice (handler-case (signal c) ((satisfies choosing-p) () nil))))
        (handler-case (handler-bind (((satisfies failing-p) #'identity)) (signal c))
          (simple-error (x) (equal (princ-to-string x) "predicate failed")))
        (eq c (catch :report (with-output-to-string (*error-output*) (warn c))))
        (eq :muffled (catch :report (handler-bind ((warning #'muffle-warning)) (warn c)) :muffled))))"#).unwrap();
    assert!(values.iter().all(|v| !v.is_nil()));
}

#[test]
fn restart_callbacks_capture_their_lexical_environment() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let ((reports 0) (reads 0) (v (list :bad)))
      (values
        (equal (multiple-value-list
                 (with-simple-restart (nil "Attempt ~D" (incf reports))
                   (let ((r (car (compute-restarts))))
                     (assert (zerop reports))
                     (assert (equal "Attempt 1" (princ-to-string r)))
                     (invoke-restart r)))) '(nil t))
        (with-input-from-string (input "(+ 4 5)")
          (let ((*debug-io* (make-two-way-stream input (make-string-output-stream))))
            (handler-bind ((type-error (lambda (c)
               (assert (= reads 1))
               (assert (eq (type-error-datum c) :bad))
               (assert (search "an integer" (princ-to-string c)))
               (invoke-restart-interactively (find-restart 'store-value c)))))
              (check-type (car (progn (incf reads) v)) integer "an integer")))
          (and (= (car v) 9) (= reads 3)))))"#,
        )
        .unwrap();
    assert!(values.iter().all(|v| !v.is_nil()));
}

#[test]
fn interactive_assertion_updates_only_the_selected_place() {
    let runtime = Runtime::start().unwrap();
    let values = runtime.eval(r#"(let ((x 1) (a 0) (b 0) (reads 0))
      (with-input-from-string (answers (format nil "n~%y~%(values 7 8)~%"))
        (let ((*debug-io* (make-two-way-stream answers (make-string-output-stream))))
          (handler-bind ((error (lambda (c) (invoke-restart-interactively (find-restart 'continue c)))))
            (assert (= (+ a b) 15) ((car (progn (incf reads) (list x))) (values a b))))))
      (values x reads a b))"#).unwrap();
    let result: Vec<_> = values.iter().map(|v| v.as_fixnum().unwrap()).collect();
    assert_eq!(result, [1, 0, 7, 8]);
}

#[test]
fn handler_bind_macroexpansion_preserves_the_callers_environment() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let* ((form
                       '(handler-bind
                            ((simple-error
                               (progn
                                 (push :first-initializer calls)
                                 (lambda (condition)
                                   (declare (ignore condition))
                                   (push :first-handler calls)
                                   (signal 'simple-warning))))
                             (condition
                               (progn
                                 (push :second-initializer calls)
                                 (lambda (condition)
                                   (declare (ignore condition))
                                   (push :second-handler calls)))))
                          (locally
                            (declare (type symbol marker))
                            (signal 'simple-error)
                            (return-from exit (values :done marker)))))
                      (expanded (macroexpand-1 form))
                      (kernel (car (third (third expanded)))))
                 (flet ((run (body compile-p)
                          (let ((definition
                                  `(lambda ()
                                     (let ((calls nil) (marker :lexical))
                                       (multiple-value-bind (first second)
                                           (block exit
                                             (handler-bind
                                                 ((warning
                                                    (lambda (condition)
                                                      (declare (ignore condition))
                                                      (push :outer-handler calls))))
                                               ,body))
                                         (list first second (reverse calls)))))))
                            (funcall (if compile-p
                                         (compile nil definition)
                                         (eval definition))))))
                   (and (special-operator-p kernel)
                        (fboundp kernel)
                        (equal (run form nil)
                               (run expanded nil))
                        (equal (run expanded t)
                               '(:done :lexical
                                 (:first-initializer :second-initializer :first-handler
                                  :outer-handler :second-handler))))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn eval_executes_deferred_expansion_errors_only_on_selected_paths() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defvar *compile-count* 0)
               (defvar *compile-cleanups* 0)
               (defvar *select-invalid-branch* nil)
               (defvar *original-expansion-error*
                 (make-condition 'simple-error :format-control "Expansion diagnostic: ~D"
                                               :format-arguments '(37)))
               (defvar *evaluation-entered* nil)
               (defmacro rejected-expansion ()
                 (incf *compile-count*)
                 (unwind-protect (error *original-expansion-error*)
                   (incf *compile-cleanups*)))
               (defmacro incomplete-expansion-input () (read-from-string "("))
               (defmacro escape-expansion () (throw 'escape :escaped))"#,
        )
        .unwrap();

    let observed = runtime
        .eval(
            r#"(and (eq (eval '(if *select-invalid-branch* (rejected-expansion) :selected))
                         :selected)
                    (eq (nth-value 1 (eval '(let () (ignore-errors (rejected-expansion)))))
                        *original-expansion-error*)
                    (eq (handler-case (eval '(progn (rejected-expansion) :unreachable))
                          (simple-error (condition) condition))
                        *original-expansion-error*)
                    (eq (catch 'escape (eval '(let () (escape-expansion)))) :escaped)
                    (= *compile-count* *compile-cleanups* 3))"#,
        )
        .unwrap();
    assert!(!observed[0].is_nil());

    let protected = runtime
        .eval(
            r#"(flet ((run-with-active-handler (form)
                        (block outer-handler
                          (handler-bind ((error (lambda (condition)
                                                 (declare (ignore condition))
                                                 (return-from outer-handler :outer-handler))))
                            (eval form)))))
                 (and
                  (eq (nth-value 1 (run-with-active-handler '(ignore-errors (rejected-expansion))))
                      *original-expansion-error*)
                  (eq (run-with-active-handler
                        '(handler-case (rejected-expansion) (simple-error (condition) condition)))
                      *original-expansion-error*)
                  (eq (run-with-active-handler
                        '(block inner
                           (handler-bind ((simple-error (lambda (condition)
                                                          (return-from inner condition))))
                             (rejected-expansion))))
                      *original-expansion-error*)
                  (eq (run-with-active-handler
                        '(let ()
                           (setq *evaluation-entered* t)
                           (handler-bind ((error (rejected-expansion))) :unreachable)))
                      :outer-handler)
                  (not *evaluation-entered*)
                  (= *compile-count* *compile-cleanups* 7)))"#,
        )
        .unwrap();
    assert!(!protected[0].is_nil());

    let error = runtime
        .eval("(eval '(let () (rejected-expansion)))")
        .err()
        .expect("the rejected macro expansion must signal an error");
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::SimpleError)
    );
    assert_eq!(error.message(), "Expansion diagnostic: 37");

    let read_error = runtime
        .eval("(eval '(let () (incomplete-expansion-input)))")
        .err()
        .expect("the macro's incomplete input must signal an error");
    assert_eq!(
        read_error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::EndOfFile)
    );
    let cause = read_error
        .source()
        .unwrap()
        .downcast_ref::<zkcl::EvalError>()
        .unwrap();
    assert_eq!(cause.kind(), EvalErrorKind::Read);
    assert_eq!(cause.byte_range(), 1..1);
}

#[test]
fn compiled_type_checks_retry_after_store_value_updates_the_place() {
    let runtime = Runtime::start().unwrap();
    for (check, expected) in [
        ("(check-type value integer)", "(nil 15)"),
        ("(ctypecase value (integer :accepted))", "(:accepted 15)"),
    ] {
        let source = format!(
            "(equal (multiple-value-list
                      (funcall (compile nil
                                 '(lambda (value)
                                    (handler-bind ((type-error (lambda (c) (store-value 15 c))))
                                      (values {check} value))))
                               :invalid))
                    '{expected})"
        );
        let values = runtime
            .eval(&source)
            .unwrap_or_else(|error| panic!("{check}: {error}"));
        assert!(!values[0].is_nil(), "{check}");
    }
}

#[test]
fn simple_type_error_uses_the_first_value_for_a_repeated_initarg() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(progn
               (defparameter *saved-type-error*
                 (make-condition 'simple-type-error
                   :datum 3 :datum 4
                   :expected-type 'symbol
                   :format-control \"x\"
                   :format-arguments nil))
               (let ((condition *saved-type-error*))
                 (values
                   (typep condition 'simple-condition)
                   (typep condition 'type-error)
                   (eq (class-of condition) (find-class 'simple-type-error))
                   (type-error-datum condition)
                   (type-error-expected-type condition)
                   (simple-condition-format-control condition)
                   (simple-condition-format-arguments condition))))",
        )
        .unwrap();

    assert!(values[..3].iter().all(|value| !value.is_nil()));
    assert_eq!(values[3].as_fixnum(), Some(3));
    assert_eq!(rt.symbol_name(&values[4]).as_deref(), Some("SYMBOL"));
    assert_eq!(rt.string(&values[5]).as_deref(), Some("x"));
    assert!(values[6].is_nil());

    rt.collect();
    let after_collection = rt
        .eval(
            "(let ((condition *saved-type-error*))
               (values
                 (type-error-datum condition)
                 (type-error-expected-type condition)
                 (simple-condition-format-control condition)
                 (simple-condition-format-arguments condition)))",
        )
        .unwrap();
    assert_eq!(after_collection[0].as_fixnum(), Some(3));
    assert_eq!(
        rt.symbol_name(&after_collection[1]).as_deref(),
        Some("SYMBOL")
    );
    assert_eq!(rt.string(&after_collection[2]).as_deref(), Some("x"));
    assert!(after_collection[3].is_nil());
}

#[test]
fn cerror_handlers_can_resume_through_a_condition_restart() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(values
               (handler-bind
                   ((simple-error
                      (lambda (condition)
                        (invoke-restart (find-restart 'continue condition)))))
                 (progn (cerror \"Keep going.\" 'simple-error) 10))
               (handler-bind
                   ((simple-error
                      (lambda (condition)
                        (invoke-restart
                         (find-restart 'use-replacement condition)
                         27))))
                 (restart-case
                     (cerror \"Keep going.\" 'simple-error)
                   (use-replacement (value) value))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(10));
    assert_eq!(values[1].as_fixnum(), Some(27));
}

#[test]
fn cerror_initializes_user_defined_conditions_before_signaling() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(define-condition recoverable-problem (error)
               ((detail :initarg :detail :reader recoverable-detail)
                (context :initform (list :context) :reader recoverable-context)))",
        )
        .unwrap();
    let values = runtime
        .eval(
            "(handler-bind
                 ((recoverable-problem
                    (lambda (condition)
                      (invoke-restart
                       (find-restart 'replacement condition)
                       (recoverable-detail condition)
                       (recoverable-context condition)
                       (princ-to-string (find-restart 'continue condition))))))
               (restart-case
                   (cerror \"Retry with ~*~A.\" 'recoverable-problem :detail 31)
                 (replacement (detail context report)
                   (values detail (first context) report))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(31));
    assert_eq!(runtime.symbol_name(&values[1]).as_deref(), Some("CONTEXT"));
    assert_eq!(
        runtime.string(&values[2]).as_deref(),
        Some("Retry with 31.")
    );
}

#[test]
fn cerror_formats_the_continue_report_only_when_requested() {
    let runtime = Runtime::start().unwrap();
    for (call, other_condition_sees_outer) in [
        (
            "(cerror (progn (incf evaluations) formatter) condition 17)",
            false,
        ),
        (
            "(funcall #'cerror (progn (incf evaluations) formatter) condition 17)",
            true,
        ),
    ] {
        let values = runtime
            .eval(&format!(
                "(let ((condition (make-condition 'simple-error :format-control \"Original.\"))
                       (other (make-condition 'simple-error))
                       (evaluations 0) (reports 0) (report nil) (outer-visible nil))
                   (let ((formatter (lambda (stream value &rest remaining)
                                      (incf reports)
                                      (format stream \"Continue with ~A.\" value)
                                      remaining)))
                     (values
                       (handler-bind
                           ((simple-error
                              (lambda (caught)
                                (assert (eq caught condition))
                                (assert (zerop reports))
                                (setf outer-visible (not (null (find-restart 'outer other))))
                                (setf report (princ-to-string (find-restart 'continue caught)))
                                (invoke-restart-interactively (find-restart 'continue caught)))))
                         (restart-case {call} (outer () :outer)))
                       evaluations reports report outer-visible)))"
            ))
            .unwrap_or_else(|error| panic!("{call}: {error}"));
        assert!(values[0].is_nil());
        assert_eq!(values[1].as_fixnum(), Some(1));
        assert_eq!(values[2].as_fixnum(), Some(1));
        assert_eq!(
            runtime.string(&values[3]).as_deref(),
            Some("Continue with 17.")
        );
        assert_eq!(!values[4].is_nil(), other_condition_sees_outer);
    }
}

#[test]
fn warn_associates_outer_restarts_only_for_a_direct_restart_case_form() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(define-condition warning-with-detail (warning)
               ((detail :initarg :detail :reader warning-detail)))",
        )
        .unwrap();
    for (form, other_condition_sees_outer, detail) in [
        ("(warn \"Warning.\")", false, None),
        ("(progn (warn \"Warning.\"))", true, None),
        ("(warn 'warning-with-detail :detail 41)", false, Some(41)),
        (
            "(progn (warn 'warning-with-detail :detail 43))",
            true,
            Some(43),
        ),
    ] {
        let results = runtime
            .eval(&format!(
                "(let ((other (make-condition 'simple-warning))
                       (visible nil) (detail nil))
                   (values
                     (handler-bind
                         ((warning
                            (lambda (condition)
                              (assert (find-restart 'outer condition))
                              (setf visible (not (null (find-restart 'outer other))))
                              (when (typep condition 'warning-with-detail)
                                (setf detail (warning-detail condition)))
                              (muffle-warning condition))))
                       (restart-case {form} (outer () :outer)))
                     visible detail))"
            ))
            .unwrap_or_else(|error| panic!("{form}: {error}"));
        assert!(results[0].is_nil());
        assert_eq!(!results[1].is_nil(), other_condition_sees_outer);
        assert_eq!(results[2].as_fixnum(), detail);
    }
}

#[test]
fn signaling_accepts_functions_as_condition_format_controls() {
    let runtime = Runtime::start().unwrap();
    for (form, condition_type) in [
        ("(signal control 19)", "simple-condition"),
        ("(error control 19)", "simple-error"),
        ("(cerror \"Continue.\" control 19)", "simple-error"),
        ("(warn control 19)", "simple-warning"),
    ] {
        let values = runtime
            .eval(&format!(
                "(let ((control (formatter \"Observed ~A.\")))
                   (handler-case {form}
                     ({condition_type} (caught)
                       (values (eq control (simple-condition-format-control caught))
                               (car (simple-condition-format-arguments caught))
                               (princ-to-string caught)))))"
            ))
            .unwrap_or_else(|error| panic!("{form}: {error}"));
        assert!(!values[0].is_nil());
        assert_eq!(values[1].as_fixnum(), Some(19));
        assert_eq!(runtime.string(&values[2]).as_deref(), Some("Observed 19."));
    }
}

#[test]
fn handlers_run_in_the_signaling_environment_before_unwind() {
    let rt = Runtime::start().unwrap();
    let numbers = |source| {
        rt.eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        numbers(
            "(let ((trace 0))
           (handler-bind ((condition (lambda (c) (setq trace (+ (* trace 10) 4)))))
             (handler-bind ((condition (lambda (c)
                                         (setq trace (+ (* trace 10) 1))
                                         (signal \"nested\")))
                            (condition (lambda (c) (setq trace (+ (* trace 10) 2)))))
               (signal \"outer\")))
           trace)",
        ),
        [Some(1424)]
    );

    rt.load_str(
        "(defvar *level* 0)
        (defun raise () (error \"stop\"))",
    )
    .unwrap();
    assert_eq!(
        numbers(
            "(let ((*level* 2) (before 0) (cleanup 0))
           (handler-case
               (let ((*level* 3))
                 (handler-bind ((error (lambda (c) (setq before *level*))))
                   (unwind-protect (raise)
                     (setq cleanup *level*))))
             (simple-error (c) (values before cleanup *level*))))",
        ),
        [Some(3), Some(3), Some(2)]
    );
    assert_eq!(numbers("*level*"), [Some(0)]);

    let results = rt
        .eval(
            "(handler-case (funcall #'error 'type-error :datum (list 98) :expected-type 'symbol)
           (type-error (c) (values (car (type-error-datum c)) (type-error-expected-type c)))
           (error () nil))",
        )
        .unwrap();
    assert_eq!(results[0].as_fixnum(), Some(98));
    assert_eq!(rt.symbol_name(&results[1]).as_deref(), Some("SYMBOL"));
    assert_eq!(
        numbers("(handler-case (car 42) (type-error (c) (type-error-datum c)))"),
        [Some(42)]
    );
    assert_eq!(
        numbers("(handler-case (throw :none 0) (control-error () 91))"),
        [Some(91)]
    );
    assert_eq!(
        rt.symbol_name(
            &rt.eval("(handler-case missing-variable (unbound-variable (c) (cell-error-name c)))",)
                .unwrap()[0]
        )
        .as_deref(),
        Some("MISSING-VARIABLE")
    );
    assert_eq!(
        rt.symbol_name(
            &rt.eval(
                "(handler-case (missing-function) (undefined-function (c) (cell-error-name c)))",
            )
            .unwrap()[0]
        )
        .as_deref(),
        Some("MISSING-FUNCTION")
    );

    assert_eq!(
        numbers("(handler-case (values 7 3) (:no-error (a b) (values b a)))"),
        [Some(3), Some(7)]
    );
    assert!(
        rt.eval("(handler-case (values) (:no-error () (values)))")
            .unwrap()
            .is_empty()
    );
    assert!(
        rt.eval("(handler-case (values 1 2) (:no-error (a) a))")
            .is_err()
    );
    assert_eq!(
        numbers(
            "(block escape
        (handler-bind ((simple-error (lambda (c) (return-from escape (values 6 8)))))
          (error \"unreachable\")))"
        ),
        [Some(6), Some(8)]
    );
    assert!(
        rt.eval("(handler-bind ((error (lambda (c) 47))) (error \"declined\"))")
            .err()
            .unwrap()
            .to_string()
            .contains("declined")
    );
}

#[test]
fn simple_condition_reports_use_the_original_format_arguments() {
    let rt = Runtime::start().unwrap();

    let error = rt.eval("(error \"Problem with ~A\" 42)").err().unwrap();
    assert_eq!(error.message(), "Problem with 42");
    let warning = rt
        .eval("(with-output-to-string (*error-output*) (warn \"Problem with ~A\" 42))")
        .unwrap();
    assert!(rt.string(&warning[0]).unwrap().contains("Problem with 42"));

    let ignored = rt
        .eval("(ignore-errors (error \"你好 \\\"Lisp\\\"\" 4))")
        .unwrap();
    assert!(ignored[0].is_nil());
    assert_eq!(ignored.len(), 2);
    rt.collect();
    let reader = rt.eval("#'simple-condition-format-control").unwrap();
    let text = rt.call(&reader[0], &[&ignored[1]]).unwrap();
    assert_eq!(rt.string(&text[0]).as_deref(), Some("你好 \"Lisp\""));
    let read_args = rt.eval("#'simple-condition-format-arguments").unwrap();
    let args = rt.call(&read_args[0], &[&ignored[1]]).unwrap();
    assert_eq!(rt.cons_fields(&args[0]).unwrap().0.as_fixnum(), Some(4));
    assert!(rt.eval("(signal \"nobody handles me\")").unwrap()[0].is_nil());

    let identity = rt
        .eval(
            "(let ((c (make-condition 'simple-error :format-control \"same\")))
        (handler-case (error c) (condition (caught) (eq c caught))))",
        )
        .unwrap();
    assert!(!identity[0].is_nil());
    assert!(
        !rt.eval("(typep (make-condition 'simple-error) 'simple-condition)")
            .unwrap()[0]
            .is_nil()
    );
}

#[test]
fn error_accessors_retain_their_designators_through_collection() {
    let rt = Runtime::start().unwrap();

    let package_fault = rt
        .eval(
            r#"(handler-case (error 'package-error :package "UNAVAILABLE")
                  (package-error (caught) caught))"#,
        )
        .unwrap();
    assert!(ConditionClass::PackageError.is_a(ConditionClass::Error));
    rt.collect();
    let accessor = rt.eval("#'package-error-package").unwrap();
    let designator = rt.call(&accessor[0], &[&package_fault[0]]).unwrap();
    assert_eq!(rt.string(&designator[0]).as_deref(), Some("UNAVAILABLE"));
    let expected = rt
        .eval(
            "(handler-case (package-error-package (make-condition 'simple-error))
               (type-error (caught) (type-error-expected-type caught)))",
        )
        .unwrap();
    assert_eq!(
        rt.symbol_name(&expected[0]).as_deref(),
        Some("PACKAGE-ERROR")
    );
    let error = rt
        .eval("(error 'package-error :package :unknown)")
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::PackageError)
    );

    let file_fault = rt
        .eval(
            r#"(handler-case (error 'file-error :pathname "absent.lisp")
                  (file-error (caught) caught))"#,
        )
        .unwrap();
    assert!(ConditionClass::FileError.is_a(ConditionClass::Error));
    rt.collect();
    let accessor = rt.eval("#'file-error-pathname").unwrap();
    let pathname = rt.call(&accessor[0], &[&file_fault[0]]).unwrap();
    assert_eq!(rt.string(&pathname[0]).as_deref(), Some("absent.lisp"));
    let expected = rt
        .eval(
            "(handler-case (file-error-pathname (make-condition 'simple-error))
               (type-error (caught) (type-error-expected-type caught)))",
        )
        .unwrap();
    assert_eq!(rt.symbol_name(&expected[0]).as_deref(), Some("FILE-ERROR"));
    let error = rt
        .eval("(error 'file-error :pathname \"absent.lisp\")")
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
}

#[test]
fn style_warnings_dispatch_to_warning_handlers() {
    let rt = Runtime::start().unwrap();

    assert!(ConditionClass::StyleWarning.is_a(ConditionClass::Warning));
    assert!(!ConditionClass::StyleWarning.is_a(ConditionClass::Error));
    let warning = rt
        .eval(
            "(handler-case (signal (make-condition 'style-warning))
               (warning () 42))",
        )
        .unwrap();
    assert_eq!(warning[0].as_fixnum(), Some(42));
}

#[test]
fn unhandled_errors_can_resume_from_the_debugger() {
    let rt = Runtime::start().unwrap();

    let resumed = rt
        .eval(
            "(let ((*debugger-hook*
                 (lambda (condition hook)
                   (declare (ignore hook))
                   (assert (null *debugger-hook*))
                   (invoke-restart (find-restart 'supply-value condition) 57))))
           (restart-case (error \"choose a value\")
             (supply-value (value) value)))",
        )
        .unwrap();
    assert_eq!(resumed[0].as_fixnum(), Some(57));
    let handled = rt
        .eval(
            "(let ((*debugger-hook*
                 (lambda (condition hook)
                   (declare (ignore condition hook))
                   (error \"handled errors must not enter the debugger\"))))
           (handler-case (car 42) (type-error () 12)))",
        )
        .unwrap();
    assert_eq!(handled[0].as_fixnum(), Some(12));
}

#[test]
fn simple_type_error_preserves_both_inherited_slot_groups() {
    let rt = Runtime::start().unwrap();
    let condition = rt
        .eval(
            r#"(handler-case
                   (error (find-class 'simple-type-error)
                          :format-control "bad datum ~S" :format-arguments (list (list 5))
                          :datum (list 5) :expected-type (list 'integer 1 4))
                 (simple-type-error (c) c))"#,
        )
        .unwrap();
    rt.collect();

    let inspect = rt
        .eval(
            "(lambda (c)
               (values (typep c 'simple-condition)
                       (typep c 'type-error)
                       (simple-condition-format-control c)
                       (simple-condition-format-arguments c)
                       (type-error-datum c)
                       (type-error-expected-type c)
                       (princ-to-string c)))",
        )
        .unwrap();
    let values = rt.call(&inspect[0], &[&condition[0]]).unwrap();
    let printed: Vec<_> = values.iter().map(|value| rt.repr(value).unwrap()).collect();
    assert_eq!(
        printed,
        [
            "T",
            "T",
            "\"bad datum ~S\"",
            "((5))",
            "(5)",
            "(INTEGER 1 4)",
            "\"bad datum (5)\"",
        ]
    );

    let signal = rt.eval("#'error").unwrap();
    let error = rt.call(&signal[0], &[&condition[0]]).err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::SimpleTypeError)
    );
    assert_eq!(error.message(), "bad datum (5)");
}
