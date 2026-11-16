#[cfg(feature = "jit")]
#[test]
fn a_hot_lisp_function_still_returns_double_after_background_compilation() {
    use std::time::{Duration, Instant};

    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str("(defun jit-double (n) (+ n n))").unwrap();
    let function = rt.eval("#'jit-double").unwrap().pop().unwrap();
    let argument = rt.fixnum(21).unwrap();
    let backend = zkcl::enable_jit(&rt).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.code_cache().is_empty() {
        assert_eq!(
            rt.call(&function, &[&argument]).unwrap()[0].as_fixnum(),
            Some(42)
        );
        assert!(Instant::now() < deadline, "hot function was not compiled");
        std::thread::sleep(Duration::from_millis(2));
    }
    rt.poll_jit_completions();

    assert_eq!(
        rt.eval("(jit-double 100)").unwrap()[0].as_fixnum(),
        Some(200)
    );
    rt.collect();
    assert_eq!(
        rt.eval("(jit-double -7)").unwrap()[0].as_fixnum(),
        Some(-14)
    );
}

#[cfg(feature = "jit")]
fn install_native_function(
    runtime: &zkcl::Runtime,
    function: &zkcl::Root<'_>,
) -> std::sync::Arc<zkcl::jit::CraneliftJitBackend> {
    use std::time::{Duration, Instant};

    // Submit only this function. A single hot call is allowed to skip a busy queue.
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    let backend = zkcl::enable_jit(runtime).unwrap();
    runtime.call(&compile, &[&runtime.nil(), function]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    backend
}

#[cfg(feature = "jit")]
fn drive_evaluation<'rt>(evaluation: &mut zkcl::Evaluation<'rt>) -> zkcl::EvaluationPoll<'rt> {
    for _ in 0..100 {
        match evaluation.resume(None).unwrap() {
            zkcl::EvaluationPoll::Runnable => {}
            other => return other,
        }
    }
    panic!("expected a blocked or completed evaluation")
}

#[cfg(feature = "jit")]
#[test]
fn native_callers_keep_dynamic_scopes_when_a_thunk_opens_the_first_group() {
    use std::sync::{Arc, Mutex};

    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&events);
    runtime
        .register_function("handoff-event", move |rt, args| {
            log.lock().unwrap().push(rt.repr(&args[0])?);
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defvar *handoff-context* :outside)
         (defun handoff-inner (thunk payload)
           (let ((*handoff-context* :inner))
             (unwind-protect
                  (progn (handoff-event :enter-inner) (funcall thunk payload))
               (handoff-event *handoff-context*))))
         (defun handoff-outer (thunk payload)
           (let ((*handoff-context* :outer))
             (unwind-protect
                  (progn (handoff-event :enter-outer)
                         (multiple-value-bind (kind value) (handoff-inner thunk payload)
                           (values kind value payload)))
               (handoff-event *handoff-context*))))",
        )
        .unwrap();
    for name in ["handoff-inner", "handoff-outer"] {
        let function = runtime.eval(&format!("#'{name}")).unwrap().remove(0);
        assert!(
            !install_native_function(&runtime, &function)
                .code_cache()
                .is_empty()
        );
    }
    let values = runtime
        .eval(
            "(handoff-outer
               (lambda (value)
                 (zk-async:with-task-group (group)
                   (let ((child (zk-async:spawn-task (group)
                                  (zk:gc)
                                  (values *handoff-context* value))))
                     (zk-async:await child))))
               (list 42))",
        )
        .unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(runtime.repr(&values[0]).unwrap(), ":INNER");
    assert_eq!(runtime.repr(&values[1]).unwrap(), "(42)");
    assert!(values[1].same_identity(&values[2]));
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [":ENTER-OUTER", ":ENTER-INNER", ":INNER", ":OUTER"]
    );
    assert_eq!(
        runtime
            .repr(&runtime.eval("*handoff-context*").unwrap()[0])
            .unwrap(),
        ":OUTSIDE"
    );
}

#[cfg(feature = "jit")]
#[test]
fn native_optional_binding_can_wait_on_the_host_and_throw_across_the_caller() {
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::{Arc, mpsc};
    use zkcl::EvaluationPoll;

    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("handoff-host-read", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    let cleanups = Arc::new(AtomicI32::new(0));
    let count = Arc::clone(&cleanups);
    runtime
        .register_function("handoff-cleanup", move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defun handoff-host-inner (fn tag payload &optional (ready (funcall fn payload)))
           (throw tag (values ready payload 23)))
         (defun handoff-host-outer (fn tag payload)
           (catch tag
             (unwind-protect
                  (multiple-value-bind (value other) (handoff-host-inner fn tag payload)
                    (values value other payload))
               (handoff-cleanup))))",
        )
        .unwrap();
    for name in ["handoff-host-inner", "handoff-host-outer"] {
        let function = runtime.eval(&format!("#'{name}")).unwrap().remove(0);
        assert!(
            !install_native_function(&runtime, &function)
                .code_cache()
                .is_empty()
        );
    }

    let mut evaluation = runtime
        .start_eval("(handoff-host-outer #'handoff-host-read (list 7) (list 19))")
        .unwrap();
    assert!(matches!(
        drive_evaluation(&mut evaluation),
        EvaluationPoll::Waiting(None)
    ));
    assert_eq!(cleanups.load(Ordering::Relaxed), 0);
    runtime.collect();
    let complete = recv.try_recv().unwrap();
    assert!(complete.complete(|runtime| Ok(vec![runtime.integer_from(42)?])));
    let EvaluationPoll::Ready(values) = drive_evaluation(&mut evaluation) else {
        panic!("expected the values thrown to CATCH");
    };
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(runtime.repr(&values[1]).unwrap(), "(19)");
    assert_eq!(values[2].as_fixnum(), Some(23));
    assert_eq!(cleanups.load(Ordering::Relaxed), 1);
}

#[cfg(feature = "jit")]
#[test]
fn an_unbound_native_cell_can_suspend_in_a_lisp_handler() {
    use std::sync::mpsc;
    use zkcl::EvaluationPoll;

    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("read-handler-result", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    runtime
        .load_str(
            "(defvar *native-unbound*)
         (defvar *native-read-cleanups* 0)
         (defun native-read-cell ()
           (unwind-protect *native-unbound* (incf *native-read-cleanups*)))",
        )
        .unwrap();
    let function = runtime.eval("#'native-read-cell").unwrap().remove(0);
    assert!(
        !install_native_function(&runtime, &function)
            .code_cache()
            .is_empty()
    );
    let mut evaluation = runtime
        .start_eval(
            "(catch 'handled
           (handler-bind ((unbound-variable
                            (lambda (condition)
                              (assert (eq '*native-unbound* (cell-error-name condition)))
                              (throw 'handled (read-handler-result)))))
             (native-read-cell)))",
        )
        .unwrap();
    assert!(matches!(
        drive_evaluation(&mut evaluation),
        EvaluationPoll::Waiting(None)
    ));
    runtime.collect();
    assert!(
        recv.try_recv()
            .unwrap()
            .complete(|runtime| Ok(vec![runtime.integer_from(42)?]))
    );
    let EvaluationPoll::Ready(values) = drive_evaluation(&mut evaluation) else {
        panic!("expected the handler to finish");
    };
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(
        runtime.eval("*native-read-cleanups*").unwrap()[0].as_fixnum(),
        Some(1)
    );
}

#[cfg(feature = "jit")]
#[test]
fn native_argument_binding_reads_later_keywords_after_collection() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .register_function("collect-during-binding", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defun bind-native-arguments (x &optional (head 2 supplied) &rest options
                                         &key (before (collect-during-binding)) payload)
           (values x head supplied before payload options))",
        )
        .unwrap();
    let function = runtime
        .eval("#'bind-native-arguments")
        .unwrap()
        .pop()
        .unwrap();
    let backend = install_native_function(&runtime, &function);
    assert!(!backend.code_cache().is_empty());

    let result = runtime
        .eval(
            "(let ((payload (list 42)))
           (multiple-value-bind (x head supplied before value options)
               (bind-native-arguments 7 nil :payload payload :payload 10)
             (list x head supplied before (eq payload value) options)))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&result[0]).unwrap(),
        "(7 NIL T NIL T (:PAYLOAD (42) :PAYLOAD 10))"
    );
}

#[cfg(feature = "jit")]
#[test]
fn installed_native_tail_calls_cross_code_and_preserve_dynamic_lookup() {
    use zkcl::{ExecutionStop, Runtime};

    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.register_function("tail-collect", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    })
    .unwrap();
    rt.load_str(
        "(defun native-tail-even (n payload)
           (if (= n 0) (values :even payload) (native-tail-odd (1- n) payload 42)))
         (defun native-tail-odd (n payload tag)
           (when (= n 37) (tail-collect))
           (if (= n 0) (values tag payload)
               (multiple-value-call #'native-tail-even (values (1- n) payload))))
         (defun native-tail-wrapper (n payload)
           (multiple-value-bind (tag value) (native-tail-even n payload)
             (list value tag)))
         (defun native-tail-reload (n action)
           (declare (notinline native-tail-reload))
           (when (= n 4) (funcall action))
           (if (= n 0) (values :old n) (native-tail-reload (1- n) action)))
         (defun native-tail-loop () (native-tail-loop))",
    )
    .unwrap();
    for name in [
        "native-tail-even",
        "native-tail-odd",
        "native-tail-wrapper",
        "native-tail-reload",
        "native-tail-loop",
    ] {
        let function = rt.eval(&format!("#'{name}")).unwrap().pop().unwrap();
        assert!(
            !install_native_function(&rt, &function)
                .code_cache()
                .is_empty()
        );
    }

    let values = rt.eval("(native-tail-wrapper 40001 (list 9))").unwrap();
    assert_eq!(rt.repr(&values[0]).unwrap(), "((9) 42)");
    let values = rt
        .eval(
            "(native-tail-reload 10
           (lambda ()
             (setf (fdefinition 'native-tail-reload)
                   (lambda (n action) (declare (ignore action)) (values :new n)))
             (tail-collect)))",
        )
        .unwrap();
    assert_eq!(rt.repr(&values[0]).unwrap(), ":NEW");
    assert_eq!(values[1].as_fixnum(), Some(3));

    let function = rt.eval("#'native-tail-loop").unwrap().pop().unwrap();
    rt.set_step_budget(Some(10_000));
    let error = rt.call(&function, &[]).err().unwrap();
    assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));
}

#[cfg(feature = "jit")]
#[test]
fn deoptimized_tail_calls_do_not_nest_native_frames() {
    let rt = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    rt.load_str(
        "(defun deopt-tail-a (n x)
           (if (= n 0) x (deopt-tail-b (- n 1) (+ x 1))))
         (defun deopt-tail-b (n x)
           (if (= n 0) x (deopt-tail-a (- n 1) (+ x 1))))",
    )
    .unwrap();
    // Profile integer arguments so the floating input misses the native guard.
    assert_eq!(
        rt.eval("(deopt-tail-a 16 0)").unwrap()[0].as_fixnum(),
        Some(16)
    );
    for name in ["deopt-tail-a", "deopt-tail-b"] {
        let function = rt.eval(&format!("#'{name}")).unwrap().pop().unwrap();
        assert!(
            !install_native_function(&rt, &function)
                .code_cache()
                .is_empty()
        );
    }
    assert!(rt.eval("(= (deopt-tail-a 20000 0.5d0) 20000.5d0)").unwrap()[0].is_t());
}

#[cfg(feature = "jit")]
#[test]
fn native_backedge_cancellation_runs_its_cleanup_and_the_vm_callers_cleanup() {
    use zkcl::{ExecutionStop, Runtime};

    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("request-cancel", |runtime, _| {
            runtime.cancellation_handle().cancel_with_cleanup();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .register_function("collect-during-cleanup", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defvar *jit-cancel-log* nil)
             (defun hot-protected (payload nonlocal-loop)
               (unwind-protect
                   (progn (request-cancel)
                          (if nonlocal-loop
                              (let ((count 0))
                                (tagbody again
                                   (incf count)
                                   (when (< count 50) (go again))))
                              (let ((total 0))
                                (dotimes (index 100000 total)
                                  (setq total (+ total index))))))
                 (collect-during-cleanup)
                 (push payload *jit-cancel-log*)))",
        )
        .unwrap();
    let function = runtime.eval("#'hot-protected").unwrap().pop().unwrap();
    let backend = install_native_function(&runtime, &function);
    assert!(!backend.code_cache().is_empty());

    runtime.set_step_budget(Some(10_000));
    for nonlocal_loop in ["t", "nil"] {
        let error = runtime
            .eval(&format!(
                "(unwind-protect (hot-protected (list 42) {nonlocal_loop})
                   (push :vm *jit-cancel-log*))"
            ))
            .err()
            .unwrap();
        assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
        runtime.cancellation_handle().reset();
        assert!(
            runtime
                .eval("(equal *jit-cancel-log* '(:vm (42)))")
                .unwrap()[0]
                .is_t()
        );
        runtime.eval("(setq *jit-cancel-log* nil)").unwrap();
    }
}

#[cfg(feature = "jit")]
#[test]
fn native_named_calls_report_the_violated_ftype_component() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defun jit-ftype-target (value result)
           (values value result :extra))
         (defun jit-ftype-caller (value result)
           (declare (optimize (safety 3))
                    (notinline jit-ftype-target)
                    (ftype (function ((integer 0 10) t) (values fixnum symbol))
                           jit-ftype-target))
           (jit-ftype-target value result))",
    )
    .unwrap();
    let function = rt.eval("#'jit-ftype-caller").unwrap().pop().unwrap();
    let check = || {
        let values = rt.eval("(jit-ftype-caller 7 :ok)").unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].as_fixnum(), Some(7));
        assert_eq!(rt.repr(&values[1]).unwrap(), ":OK");
        assert_eq!(rt.repr(&values[2]).unwrap(), ":EXTRA");
        for (arg, result, datum, expected) in [
            ("-1", ":ok", "-1", "(integer 0 10)"),
            ("7", "42", "42", "symbol"),
        ] {
            let form = format!(
                "(handler-case (jit-ftype-caller {arg} {result})
                   (type-error (c)
                     (and (eql {datum} (type-error-datum c))
                          (equal '{expected} (type-error-expected-type c)))))"
            );
            assert!(rt.eval(&form).unwrap()[0].is_t(), "{form}");
        }
    };
    check();
    let backend = install_native_function(&rt, &function);
    assert!(!backend.code_cache().is_empty());
    check();
}

#[cfg(feature = "jit")]
#[test]
fn native_the_checks_keep_the_original_values() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defun jit-the-values (items)
           (the (values fixnum symbol) (values-list items)))",
    )
    .unwrap();
    let function = rt.eval("#'jit-the-values").unwrap().pop().unwrap();
    let check = || {
        let values = rt.eval("(jit-the-values '(7 :ok :extra))").unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].as_fixnum(), Some(7));
        assert_eq!(rt.repr(&values[1]).unwrap(), ":OK");
        assert_eq!(rt.repr(&values[2]).unwrap(), ":EXTRA");
        assert_eq!(rt.eval("(jit-the-values '(7))").unwrap().len(), 1);
        assert!(
            rt.eval(
                "(handler-case (jit-the-values '())
                        (type-error (c) (and (null (type-error-datum c))
                                             (eq (type-error-expected-type c) 'fixnum))))"
            )
            .unwrap()[0]
                .is_t()
        );
    };
    check();
    let backend = install_native_function(&rt, &function);
    assert!(!backend.code_cache().is_empty());
    check();
}

#[cfg(feature = "jit")]
#[test]
fn native_range_errors_report_the_rejected_datum_against_the_declared_bounds() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defun jit-range (items)
           (the (values (integer (-129) 127)) (values-list items)))",
    )
    .unwrap();
    let function = rt.eval("#'jit-range").unwrap().pop().unwrap();
    let check = || {
        for value in [-128, 127] {
            let values = rt.eval(&format!("(jit-range '({value} :extra))")).unwrap();
            assert_eq!(values.len(), 2);
            assert_eq!(values[0].as_fixnum(), Some(value));
            assert_eq!(rt.repr(&values[1]).unwrap(), ":EXTRA");
        }
        for value in ["-129", "128", "(ash 1 80)", "(list 5)"] {
            let form = format!(
                "(let ((value {value}))
                   (handler-case (jit-range (list value))
                     (type-error (c)
                       (and (eql value (type-error-datum c))
                            (equal '(integer (-129) 127) (type-error-expected-type c))))))"
            );
            assert!(rt.eval(&form).unwrap()[0].is_t(), "{form}");
        }
    };
    check();
    let backend = install_native_function(&rt, &function);
    assert!(!backend.code_cache().is_empty());
    rt.collect();
    check();
}

#[cfg(feature = "jit")]
#[test]
fn native_numeric_calls_preserve_each_float_operation() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str("(defun fp-step (a b c) (values (+ (* a b) c) (< b a)))")
        .unwrap();
    let function = rt.eval("#'fp-step").unwrap().pop().unwrap();
    let check = || {
        let values = rt
            .eval("(fp-step (+ 1d0 (scale-float 1d0 -27)) (- 1d0 (scale-float 1d0 -27)) -1d0)")
            .unwrap();
        assert_eq!(values[0].as_double_float().unwrap().to_bits(), 0);
        assert!(values[1].is_t());
        let values = rt.eval("(fp-step -0d0 1f0 -0d0)").unwrap();
        assert_eq!(
            values[0].as_double_float().unwrap().to_bits(),
            (-0.0_f64).to_bits()
        );
        assert!(values[1].is_nil());
        assert!(
            rt.eval(
                "(handler-case (fp-step most-positive-single-float 2f0 0f0)
                           (floating-point-overflow (c) (eq '* (arithmetic-error-operation c))))"
            )
            .unwrap()[0]
                .is_t()
        );
    };
    check();
    install_native_function(&rt, &function);
    check();
}

#[cfg(feature = "jit")]
#[test]
fn native_less_than_orders_mixed_numbers_without_rounding_them_to_one_format() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str("(defun cmp-probe (a b) (values (< a b) :kept))")
        .unwrap();
    let function = rt.eval("#'cmp-probe").unwrap().pop().unwrap();
    let bad = rt.eval(":bad").unwrap().pop().unwrap();
    let zero = rt.fixnum(0).unwrap();
    let error = || rt.call(&function, &[&zero, &bad]).err().unwrap();
    let expected = error();
    install_native_function(&rt, &function);

    for (source, expected) in [
        ("(cmp-probe 1f0 1.0000000000000002d0)", true),
        ("(cmp-probe 9007199254740993 9007199254740992d0)", false),
        ("(cmp-probe 9007199254740992d0 9007199254740993)", true),
        ("(cmp-probe 1/3 3/8)", true),
    ] {
        let values = rt.eval(source).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].is_t(), expected);
        assert_eq!(rt.repr(&values[1]).unwrap(), ":KEPT");
    }
    let actual = error();
    assert_eq!(actual.kind(), expected.kind());
    assert_eq!(actual.offset(), expected.offset());

    let traced = rt
        .eval(
            "(let ((*trace-output* (make-string-output-stream)))
               (trace <)
               (unwind-protect
                   (and (cmp-probe 1f0 1.0000000000000002d0)
                        (search \"TRACE <: enter\" (get-output-stream-string *trace-output*)))
                 (untrace <)))",
        )
        .unwrap();
    assert!(traced[0].as_fixnum().is_some());
}

#[cfg(feature = "jit")]
#[test]
fn profiled_float_loops_round_each_operation_in_the_observed_format() {
    for (suffix, halfway, expected_bits, read_bits) in [
        (
            "f0",
            13,
            (0..32).fold(0.75_f32, |x, _| x * 1.25 - 0.125).to_bits() as u64,
            (|v: &zkcl::Root<'_>| v.as_single_float().unwrap().to_bits() as u64)
                as fn(&zkcl::Root<'_>) -> u64,
        ),
        (
            "d0",
            27,
            (0..32).fold(0.75_f64, |x, _| x * 1.25 - 0.125).to_bits(),
            |v: &zkcl::Root<'_>| v.as_double_float().unwrap().to_bits(),
        ),
    ] {
        let rt = zkcl::Runtime::start().unwrap();
        rt.load_str(
            "(defvar *float-iterations* 0)
             (defun profiled-float-loop (n x a b)
               (dotimes (i n x)
                 (incf *float-iterations*)
                 (setf x (+ (* x a) b))))",
        )
        .unwrap();
        let function = rt.eval("#'profiled-float-loop").unwrap().pop().unwrap();
        let format_call =
            format!("(profiled-float-loop 32 0.75{suffix} 1.25{suffix} -0.125{suffix})");
        assert_eq!(read_bits(&rt.eval(&format_call).unwrap()[0]), expected_bits);
        let backend = install_native_function(&rt, &function);
        assert_eq!(backend.code_cache().len(), 1);
        assert_eq!(read_bits(&rt.eval(&format_call).unwrap()[0]), expected_bits);
        assert_eq!(
            rt.eval("*float-iterations*").unwrap()[0].as_fixnum(),
            Some(64)
        );

        let source = format!("(profiled-float-loop 192 0.75{suffix} 1.25{suffix} -0.125{suffix})");
        let expected = read_bits(&rt.eval(&source).unwrap()[0]);
        rt.eval("(setq *float-iterations* 0)").unwrap();
        let inputs = rt
            .eval(&format!(
                "(values 192 0.75{suffix} 1.25{suffix} -0.125{suffix})"
            ))
            .unwrap();
        let mut evaluation = rt
            .start_call(&function, &inputs.iter().collect::<Vec<_>>())
            .unwrap();
        assert!(matches!(
            evaluation.resume(None).unwrap(),
            zkcl::EvaluationPoll::Runnable
        ));
        rt.collect();
        let zkcl::EvaluationPoll::Ready(values) = drive_evaluation(&mut evaluation) else {
            panic!("expected a completed float loop")
        };
        assert_eq!(read_bits(&values[0]), expected);
        assert_eq!(
            rt.eval("*float-iterations*").unwrap()[0].as_fixnum(),
            Some(192)
        );

        assert!(
            rt.eval("(eq (profiled-float-loop 0 :seed :unused :unused) :seed)")
                .unwrap()[0]
                .is_t()
        );
        let contract = format!(
            "(eql (profiled-float-loop 1 1
                       (+ 1{suffix} (scale-float 1{suffix} -{halfway}))
                       -1{suffix})
                  (scale-float 1{suffix} -{halfway}))"
        );
        assert!(rt.eval(&contract).unwrap()[0].is_t());
        let round_twice = format!(
            "(let ((e (scale-float 1{suffix} -{halfway})))
               (profiled-float-loop 1 (- 1{suffix} e) (+ 1{suffix} e) -1{suffix}))"
        );
        assert_eq!(read_bits(&rt.eval(&round_twice).unwrap()[0]), 0);
        let signed_zero = format!("(profiled-float-loop 1 -0{suffix} 1{suffix} -0{suffix})");
        let zero_sign = if suffix == "f0" {
            (-0.0_f32).to_bits() as u64
        } else {
            (-0.0_f64).to_bits()
        };
        assert_eq!(read_bits(&rt.eval(&signed_zero).unwrap()[0]), zero_sign);

        let max = if suffix == "f0" {
            "most-positive-single-float"
        } else {
            "most-positive-double-float"
        };
        let overflow = format!(
            "(let ((x (* {max} 0.25{suffix})) (b (* {max} 0.4{suffix})))
               (setq *float-iterations* 0)
               (handler-case (profiled-float-loop 3 x 1{suffix} b)
                 (floating-point-overflow (c)
                   (and (eq (arithmetic-error-operation c) '+)
                        (equal (arithmetic-error-operands c) (list (+ x b) b))
                        (= *float-iterations* 2)))))"
        );
        assert!(rt.eval(&overflow).unwrap()[0].is_t());
    }
}

#[cfg(feature = "jit")]
#[test]
fn profiled_float_binding_changes_during_a_call_resume_at_that_call_site() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defvar *trace-next-add* nil)
         (defvar *traced-operations* 0)
         (defun maybe-trace-add ()
           (incf *traced-operations*)
           (when *trace-next-add* (trace cl:+)))
         (defun profiled-float-chain (a b c)
           (let ((product (* a b)))
             (maybe-trace-add)
             (+ product c)))",
    )
    .unwrap();
    let function = rt.eval("#'profiled-float-chain").unwrap().pop().unwrap();
    assert_eq!(
        rt.eval("(profiled-float-chain 2d0 3d0 1d0)").unwrap()[0].as_double_float(),
        Some(7.0),
    );
    install_native_function(&rt, &function);
    assert!(
        rt.eval(
            "(let ((*trace-output* (make-string-output-stream)) (*trace-next-add* t))
               (let ((answer (profiled-float-chain 2d0 3d0 1d0)))
                 (untrace cl:+)
                 (and (eql answer 7d0)
                      (= *traced-operations* 2)
                      (search \"+\" (get-output-stream-string *trace-output*)))))"
        )
        .unwrap()[0]
            .as_fixnum()
            .is_some()
    );
}

#[cfg(feature = "jit")]
#[test]
fn native_single_float_assignments_check_the_value_after_evaluating_it_once() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defvar *float-check-effects* 0)
         (defvar *trace-float-check* nil)
         (defun before-float-check (value)
           (incf *float-check-effects*)
           (when *trace-float-check* (trace cl:typep))
           value)
         (defun declared-float-recurrence (n a b x)
           (declare (type single-float a x))
           (dotimes (i n (values x i))
             (setq x (+ (* x a) (before-float-check b)))))",
    )
    .unwrap();
    let function = rt
        .eval("#'declared-float-recurrence")
        .unwrap()
        .pop()
        .unwrap();
    let check = || {
        let values = rt
            .eval("(declared-float-recurrence 3 1f0 0.25f0 1f0)")
            .unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_single_float(), Some(1.75));
        assert_eq!(values[1].as_fixnum(), Some(3));
    };
    check();
    install_native_function(&rt, &function);
    check();
    rt.load_str(
        "(setq *float-check-effects* 0)
         (assert (handler-case (declared-float-recurrence 1 1d0 1f0 1f0)
                   (type-error (c)
                     (and (eql (type-error-datum c) 1d0)
                          (eq (type-error-expected-type c) 'single-float)
                          (zerop *float-check-effects*)))))
         (assert (handler-case (declared-float-recurrence 0 1f0 1f0 1d0)
                   (type-error (c)
                     (and (eql (type-error-datum c) 1d0)
                          (eq (type-error-expected-type c) 'single-float)
                          (zerop *float-check-effects*)))))
         (assert (handler-case (declared-float-recurrence 1 1f0 1d0 1f0)
                   (type-error (c)
                     (and (eql (type-error-datum c) 2d0)
                          (eq (type-error-expected-type c) 'single-float)
                          (= *float-check-effects* 1)))))
         (let ((*trace-output* (make-string-output-stream)) (*trace-float-check* t))
           (multiple-value-bind (x n) (declared-float-recurrence 1 1f0 0.25f0 1f0)
             (untrace cl:typep)
             (assert (and (eql x 1.25f0) (= n 1)
                          (= *float-check-effects* 2)
                          (search \"TRACE TYPEP: enter\" (get-output-stream-string *trace-output*))))))",
    )
    .unwrap();
}

#[cfg(feature = "jit")]
#[test]
fn native_arithmetic_handlers_preserve_live_arguments() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defun guarded-quotient (x y marker)
           (let ((result (handler-case (/ x y)
                           (division-by-zero (c) (arithmetic-error-operation c))
                           (floating-point-overflow (c) (arithmetic-error-operands c)))))
             (values marker x y result)))",
    )
    .unwrap();
    let function = rt.eval("#'guarded-quotient").unwrap().pop().unwrap();
    let check = || {
        rt.load_str(
            "(let ((marker (list :live)))
               (multiple-value-bind (kept x y result) (guarded-quotient 1d0 -0f0 marker)
                 (assert (and (eq marker kept) (eql x 1d0) (eql y -0f0) (eq result '/))))
               (multiple-value-bind (kept x y result)
                   (guarded-quotient most-positive-single-float 0.5f0 marker)
                 (assert (and (eq marker kept) (eql x most-positive-single-float) (eql y 0.5f0)
                              (equal result (list x y))))))",
        )
        .unwrap();
    };
    check();
    install_native_function(&rt, &function);
    check();
}

#[cfg(feature = "jit")]
#[test]
fn typed_setf_signals_type_errors_after_native_installation() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str(
        "(defvar *add-effects* 0)
         (defun typed-add (n)
           (declare (type fixnum n))
           (let ((sum 0))
             (declare (type fixnum sum))
             (setf sum (the fixnum (+ n (progn (incf *add-effects*) 1))))))",
    )
    .unwrap();
    let function = rt.eval("#'typed-add").unwrap().pop().unwrap();
    let arg = rt.fixnum(41).unwrap();
    let check_errors = || {
        rt.load_str(
            "(assert
               (handler-case (typed-add :invalid)
                 (type-error (c)
                   (and (eq (type-error-datum c) :invalid)
                        (eq (type-error-expected-type c) 'fixnum)))))
             (assert
               (let ((*add-effects* 0))
                 (restart-case
                     (handler-bind
                         ((type-error
                            (lambda (c)
                              (when (and (= (type-error-datum c) (1+ most-positive-fixnum))
                                         (eq (type-error-expected-type c) 'fixnum))
                                (invoke-restart (find-restart 'recover c))))))
                       (typed-add most-positive-fixnum))
                   (recover () (= *add-effects* 1)))))",
        )
        .unwrap();
    };
    check_errors();

    install_native_function(&rt, &function);
    assert_eq!(
        rt.call(&function, &[&arg]).unwrap()[0].as_fixnum(),
        Some(42)
    );
    check_errors();
}

#[cfg(feature = "jit")]
#[test]
fn replacing_jit_backend_keeps_installed_machine_code_alive() {
    use std::sync::Arc;

    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str("(defun jit-double (n) (+ n n))").unwrap();
    let function = rt.eval("#'jit-double").unwrap().pop().unwrap();
    let input = rt.fixnum(21).unwrap();

    let backend = install_native_function(&rt, &function);
    let weak_backend = Arc::downgrade(&backend);
    drop(backend);
    assert_eq!(
        rt.call(&function, &[&input]).unwrap()[0].as_fixnum(),
        Some(42)
    );

    zkcl::enable_jit(&rt).unwrap();
    assert!(
        weak_backend.upgrade().is_some(),
        "installed code lost its owner"
    );
    assert_eq!(
        rt.call(&function, &[&input]).unwrap()[0].as_fixnum(),
        Some(42)
    );

    drop(function);
    drop(input);
    drop(rt);
    assert!(weak_backend.upgrade().is_none());
}

#[cfg(not(feature = "jit"))]
#[test]
fn default_runtime_executes_lisp_without_jit() {
    let rt = zkcl::Runtime::start().unwrap();
    rt.load_str("(defun vm-double (n) (+ n n))").unwrap();
    assert_eq!(rt.eval("(vm-double 21)").unwrap()[0].as_fixnum(), Some(42));
}

#[cfg(feature = "jit")]
#[test]
fn native_tagbody_preserves_locals_across_a_special_binding() {
    let runtime = zkcl::Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *jit-tagbody-marker* 0)
             (defun jit-anchored-add (x)
               (let ((result 0))
                 (tagbody start
                    (setq result (+ x 1))
                    (let ((*jit-tagbody-marker* 1)) (go done))
                  done)
                 (values result (+ result 1) *jit-tagbody-marker*)))",
        )
        .unwrap();
    let function = runtime.eval("#'jit-anchored-add").unwrap().pop().unwrap();
    // Record fixnum feedback before requesting native compilation.
    let input = runtime.fixnum(7).unwrap();
    let warm = runtime.call(&function, &[&input]).unwrap();
    assert_eq!(warm.len(), 3);
    assert_eq!(warm[0].as_fixnum(), Some(8));
    assert_eq!(warm[1].as_fixnum(), Some(9));
    assert_eq!(warm[2].as_fixnum(), Some(0));
    install_native_function(&runtime, &function);

    let input = runtime.eval("1.5f0").unwrap().pop().unwrap();
    let values = runtime.call(&function, &[&input]).unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].as_single_float(), Some(2.5));
    assert_eq!(values[1].as_single_float(), Some(3.5));
    assert_eq!(values[2].as_fixnum(), Some(0));
}

#[cfg(feature = "jit")]
#[test]
fn native_progv_restores_the_outer_binding_before_cleanup() {
    let runtime = zkcl::Runtime::start().unwrap();
    runtime
        .register_function("collect-with-progv", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defvar *progv-first* :global)
             (defvar *progv-second* (list :default))
             (defvar *progv-cleanup*)
             (defconstant +progv-fixed+ 17)
             (defun invalid-progv-body () :body-ran)
             (defun native-progv (names vals body)
               (let ((*progv-first* :outer))
                 (handler-case
                     (unwind-protect
                         (progv names vals (funcall body))
                       (setq *progv-cleanup* *progv-first*))
                   (error () (values :caught *progv-first*)))))",
        )
        .unwrap();

    let cases = [
        (
            "(native-progv '(*progv-first* *progv-second*) (list (list 12))
               (lambda ()
                 (collect-with-progv)
                 (values *progv-first* (boundp '*progv-second*))))",
            "(((12) NIL) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(native-progv '(*progv-first*) '(:bound :unused)
               (lambda () (values *progv-first* *progv-second*)))",
            "((:BOUND (:DEFAULT)) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(catch :exit
               (native-progv '(*progv-first*) '(42)
                 (lambda () (throw :exit (values *progv-first* 43)))))",
            "((42 43) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(native-progv '(*progv-first* . :bad-tail) '(1 2) #'invalid-progv-body)",
            "((:CAUGHT :OUTER) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(native-progv '(*progv-first*) '(1 . :bad-tail) #'invalid-progv-body)",
            "((:CAUGHT :OUTER) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(native-progv '(*progv-first* +progv-fixed+) '(42 99) #'invalid-progv-body)",
            "((:CAUGHT :OUTER) :OUTER :GLOBAL (:DEFAULT))",
        ),
        (
            "(native-progv '(*progv-first* :keyword) '(42 99) #'invalid-progv-body)",
            "((:CAUGHT :OUTER) :OUTER :GLOBAL (:DEFAULT))",
        ),
    ];
    let check = || {
        for (source, expected) in cases {
            let source = format!(
                "(let ((*progv-cleanup* :not-run))
                   (list (multiple-value-list {source}) *progv-cleanup* *progv-first* *progv-second*))"
            );
            let values = runtime.eval(&source).unwrap();
            assert_eq!(runtime.repr(&values[0]).unwrap(), expected, "{source}");
        }
    };
    check();
    let function = runtime.eval("#'native-progv").unwrap().pop().unwrap();
    install_native_function(&runtime, &function);
    check();
}

#[cfg(feature = "jit")]
#[test]
fn resumable_restart_returns_to_its_caller_across_native_frames() {
    let runtime = zkcl::Runtime::start().unwrap();
    runtime
        .load_str(
            "(defvar *restart-cleanups*)
             (defun restart-cleanup (warning)
               (unwind-protect
                   (muffle-warning warning)
                 (incf *restart-cleanups*)))
             (defun restart-intermediary (warning)
               (restart-cleanup warning)
               (incf *restart-cleanups* 100))
             (defun restart-direct (warning)
               (muffle-warning warning)
               (incf *restart-cleanups* 100))
             (defun signal-warning ()
               (warn \"muffled warning\"))",
        )
        .unwrap();

    let check = |handler, expected| {
        let source = format!(
            "(let ((*restart-cleanups* 0))
               (block target
                 (let ((escape (lambda (values)
                                 (return-from target (list values *restart-cleanups*)))))
                   (handler-bind ((warning #'{handler}))
                     (funcall escape (multiple-value-list (signal-warning)))))))"
        );
        let values = runtime.eval(&source).unwrap();
        assert_eq!(runtime.repr(&values[0]).unwrap(), expected, "{handler}");
    };

    let install = |name| {
        let function = runtime.eval(&format!("#'{name}")).unwrap().pop().unwrap();
        install_native_function(&runtime, &function);
    };

    check("restart-cleanup", "((NIL) 1)");
    install("signal-warning");
    install("restart-intermediary");
    check("restart-intermediary", "((NIL) 1)");
    install("restart-cleanup");
    check("restart-cleanup", "((NIL) 1)");
    install("restart-direct");
    check("restart-direct", "((NIL) 0)");
}

#[cfg(feature = "jit")]
#[test]
fn installed_closure_code_keeps_each_environment_across_collection() {
    let runtime = zkcl::Runtime::start().unwrap();
    runtime
        .register_function("collect-closure-roots", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "(defun make-native-readers (value label)
           (values (lambda (new)
                     (setq value new)
                     (collect-closure-roots)
                     (values value label))
                   (lambda () (values value label))))",
        )
        .unwrap();
    let factory = runtime
        .eval("#'make-native-readers")
        .unwrap()
        .pop()
        .unwrap();
    install_native_function(&runtime, &factory);
    let list = |n| runtime.eval(&format!("(list {n})")).unwrap().pop().unwrap();
    let first = runtime.call(&factory, &[&list(11), &list(12)]).unwrap();
    let other = runtime.call(&factory, &[&list(21), &list(22)]).unwrap();
    install_native_function(&runtime, &first[0]);
    install_native_function(&runtime, &first[1]);
    let check = |values: Vec<zkcl::Root<'_>>, x, label| {
        assert_eq!(values.len(), 2);
        assert_eq!(runtime.repr(&values[0]).unwrap(), x);
        assert_eq!(runtime.repr(&values[1]).unwrap(), label);
    };
    check(
        runtime.call(&first[0], &[&list(31)]).unwrap(),
        "(31)",
        "(12)",
    );
    check(runtime.call(&first[1], &[]).unwrap(), "(31)", "(12)");
    check(runtime.call(&other[1], &[]).unwrap(), "(21)", "(22)");
}

#[cfg(feature = "jit")]
#[test]
fn retained_native_source_points_keep_writable_locals_rooted() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            "(defvar *native-debug-observed* nil)
        (defun retained-native-locals (x initial)
          (declare (optimize (debug 3)))
          (let ((y initial))
            (progn *native-debug-observed* (+ (car y) 1))))
        (defun native-locals-frontend (condition)
          (declare (ignore condition))
          (setq *native-debug-observed*
                (list (seventh (first (zk::%debug-frames)))
                      (zk::%debug-local 0 'x) (zk::%debug-local 0 'y)))
          (zk::%set-debug-local (list 40) 0 'y)
          (zk:gc)
          (zk::%debug-motion :continue))",
        )
        .unwrap();
    let function = runtime.eval("#'retained-native-locals").unwrap().remove(0);
    let backend = install_native_function(&runtime, &function);
    assert!(
        !backend.code_cache().is_empty(),
        "the target must have installed machine code"
    );
    runtime.add_source_breakpoint(&function, 5).unwrap();
    let frontend = runtime.eval("#'native-locals-frontend").unwrap().remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    let initial = runtime.eval("(list 3)").unwrap().remove(0);
    assert_eq!(
        runtime
            .call(&function, &[&runtime.fixnum(3).unwrap(), &initial])
            .unwrap()[0]
            .as_fixnum(),
        Some(41)
    );
    assert_eq!(
        runtime
            .repr(&runtime.eval("*native-debug-observed*").unwrap()[0])
            .unwrap(),
        "(T 3 (3))"
    );
}

#[cfg(feature = "jit")]
#[test]
fn incremental_debugger_materializes_native_registers_and_resumes_at_the_same_point() {
    use zkcl::{DebugMotion, EvalError, EvaluationPoll};
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            "\
(defparameter *native-debug-effects* 0)
(defun native-event-leaf (x)
  (declare (optimize (debug 3)))
  (incf *native-debug-effects*)
  (let ((y x))
    (setq *native-debug-effects* *native-debug-effects*)
    (values (+ (car y) 1) *native-debug-effects*)))",
        )
        .unwrap();
    let function = runtime.eval("#'native-event-leaf").unwrap().remove(0);
    let backend = install_native_function(&runtime, &function);
    assert!(!backend.code_cache().is_empty());
    runtime.add_source_breakpoint(&function, 6).unwrap();
    // Supply a movable local without taking a generic Lisp call before the breakpoint.
    let argument = runtime.eval("(list 5)").unwrap().remove(0);
    let mut evaluation = runtime.start_call(&function, &[&argument]).unwrap();
    drop(argument);
    evaluation.enable_debugging();
    let EvaluationPoll::Paused(event) = drive_evaluation(&mut evaluation) else {
        panic!("expected a native breakpoint")
    };
    assert!(event.frames[0].native, "{:?}", event.frames);
    assert_eq!(event.frames[0].line, Some(6));
    runtime.collect();
    evaluation
        .with_debugger(|rt| -> Result<(), EvalError> {
            let y = rt.lookup_symbol("CL-USER", "Y")?.unwrap();
            assert_eq!(rt.preview(&rt.debug_local(0, &y)?)?, "(5)");
            let value = rt.debug_eval(0, "(list 41)")?.remove(0);
            rt.debug_set_local(0, &y, &value)?;
            assert!(rt.debug_motion(DebugMotion::Continue)?);
            Ok(())
        })
        .unwrap()
        .unwrap()
        .unwrap();
    let EvaluationPoll::Ready(values) = drive_evaluation(&mut evaluation) else {
        panic!("native continuation did not finish")
    };
    assert_eq!(
        values.iter().map(|v| v.as_fixnum()).collect::<Vec<_>>(),
        [Some(42), Some(1)]
    );
}

#[cfg(feature = "jit")]
#[test]
fn native_source_step_stays_native_and_preserves_multiple_values() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            "(defvar *native-step-events* nil)
        (defun retained-native-step (x)
          (declare (optimize (debug 3)))
          (let ((y (+ x 1)))
            (values y x)))
        (defun native-step-frontend (condition)
          (declare (ignore condition))
          (push (seventh (first (zk::%debug-frames))) *native-step-events*)
          (zk::%debug-motion (if (= (length *native-step-events*) 1) :next :continue)))",
        )
        .unwrap();
    let function = runtime.eval("#'retained-native-step").unwrap().remove(0);
    install_native_function(&runtime, &function);
    runtime.add_breakpoint(&function, None).unwrap();
    let frontend = runtime.eval("#'native-step-frontend").unwrap().remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    let values = runtime
        .call(&function, &[&runtime.fixnum(9).unwrap()])
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(10), Some(9)]
    );
    assert_eq!(
        runtime
            .repr(&runtime.eval("*native-step-events*").unwrap()[0])
            .unwrap(),
        "(T T)"
    );
}

#[cfg(feature = "jit")]
#[test]
fn edits_to_a_suspended_native_caller_reach_its_next_instruction() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            r#"
      (defvar *native-caller-flags* nil)
      (defun native-debug-callee () (declare (optimize (debug 3))) nil)
      (defun native-debug-caller (x)
        (declare (optimize (debug 3)))
        (native-debug-callee)
        (+ x 2))
      (defun native-caller-frontend (condition)
        (declare (ignore condition))
        (setq *native-caller-flags*
              (mapcar (lambda (frame) (seventh frame))
                      (subseq (zk::%debug-frames) 0 2)))
        (zk::%set-debug-local 40 1 'x)
        (zk:gc)
        (zk::%debug-motion :continue))
    "#,
        )
        .unwrap();
    let caller = runtime.eval("#'native-debug-caller").unwrap().remove(0);
    install_native_function(&runtime, &caller);
    let callee = runtime.eval("#'native-debug-callee").unwrap().remove(0);
    install_native_function(&runtime, &callee);
    runtime.add_breakpoint(&callee, None).unwrap();
    let frontend = runtime.eval("#'native-caller-frontend").unwrap().remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    assert_eq!(
        runtime
            .call(&caller, &[&runtime.fixnum(9).unwrap()])
            .unwrap()[0]
            .as_fixnum(),
        Some(42)
    );
    assert_eq!(
        runtime
            .repr(&runtime.eval("*native-caller-flags*").unwrap()[0])
            .unwrap(),
        "(T T)"
    );
}

#[cfg(feature = "jit")]
#[test]
fn native_frame_return_runs_cleanup_once_and_returns_multiple_or_zero_values() {
    for (zero, recover_cleanup) in [(false, false), (true, false), (false, true), (true, true)] {
        let runtime = zkcl::Runtime::builder()
            .jit_threshold(u32::MAX)
            .start()
            .unwrap();
        let cleanup = if recover_cleanup {
            "(record-native-return-cleanup)"
        } else {
            "(incf *native-return-cleanups*)"
        };
        runtime
            .load_str(&format!(
                r#"(defvar *native-return-cleanups* 0)
(defun native-return-target (x)
  (declare (optimize (debug 3)))
  (unwind-protect
       (+ x 1)
    {cleanup}))
(defvar *native-return-was-native* nil)
(defvar *native-return-caller-returns* 0)
(defun record-native-return-cleanup ()
  (zk:gc)
  (incf *native-return-cleanups*))
"#,
            ))
            .unwrap();
        let result = if zero {
            "nil"
        } else {
            "(list :returned (list (zk::%debug-local 0 'x)))"
        };
        runtime
            .load_str(&format!(
                r#"
(defun native-return-frontend (condition)
  (declare (ignore condition))
  (setq *native-return-was-native* (seventh (first (zk::%debug-frames))))
  (zk:gc)
  (zk::%debug-return 0 {result}))"#
            ))
            .unwrap();
        let function = runtime.eval("#'native-return-target").unwrap().remove(0);
        install_native_function(&runtime, &function);
        runtime.add_source_breakpoint(&function, 5).unwrap();
        let frontend = runtime.eval("#'native-return-frontend").unwrap().remove(0);
        runtime.set_debugger(Some(&frontend)).unwrap();
        let values = if recover_cleanup {
            // The uncompiled cleanup helper returns this activation to the VM.
            let values = runtime
                .eval(
                    "(multiple-value-prog1 (native-return-target 9)
                       (incf *native-return-caller-returns*))",
                )
                .unwrap();
            assert_eq!(
                runtime.eval("*native-return-caller-returns*").unwrap()[0].as_fixnum(),
                Some(1)
            );
            values
        } else {
            runtime
                .call(&function, &[&runtime.fixnum(9).unwrap()])
                .unwrap()
        };
        if zero {
            assert!(values.is_empty());
        } else {
            assert_eq!(values.len(), 2);
            assert_eq!(runtime.repr(&values[0]).unwrap(), ":RETURNED");
            assert_eq!(runtime.repr(&values[1]).unwrap(), "(9)");
        }
        assert_eq!(
            runtime
                .repr(
                    &runtime
                        .eval("(list *native-return-was-native* *native-return-cleanups*)")
                        .unwrap()[0]
                )
                .unwrap(),
            "(T 1)"
        );
    }
}

#[cfg(feature = "jit")]
#[test]
fn native_frame_restart_preserves_all_incoming_arguments_through_gc_and_cleanup() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            r#"(defvar *native-restart-entries* 0)
(defvar *native-restart-cleanups* 0)
(defvar *native-restart-flags* nil)
(defun native-restart-target (x &optional y &rest tail &key z &allow-other-keys)
  (declare (optimize (debug 3)))
  (unwind-protect
       (progn
         (incf *native-restart-entries*)
         (values x y tail z))
    (incf *native-restart-cleanups*)))
(defun native-restart-frontend (condition)
  (declare (ignore condition))
  (push (seventh (first (zk::%debug-frames))) *native-restart-flags*)
  (zk:gc)
  (if (= *native-restart-entries* 1)
      (zk::%debug-restart-frame 0)
      (zk::%debug-motion :continue)))
"#,
        )
        .unwrap();
    let function = runtime.eval("#'native-restart-target").unwrap().remove(0);
    install_native_function(&runtime, &function);
    runtime.add_source_breakpoint(&function, 9).unwrap();
    let frontend = runtime.eval("#'native-restart-frontend").unwrap().remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    let values = runtime
        .eval("(native-restart-target (list 30) (list 40) :z 50 :extra (list 60))")
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|value| runtime.repr(value).unwrap())
            .collect::<Vec<_>>(),
        ["(30)", "(40)", "(:Z 50 :EXTRA (60))", "50"]
    );
    assert_eq!(runtime.repr(&runtime.eval("(list *native-restart-flags* *native-restart-entries* *native-restart-cleanups*)").unwrap()[0]).unwrap(), "((T T) 2 2)");
}

#[cfg(feature = "jit")]
#[test]
fn native_caller_edits_are_visible_to_cleanup_during_nonlocal_exit() {
    let runtime = zkcl::Runtime::builder()
        .jit_threshold(u32::MAX)
        .start()
        .unwrap();
    runtime
        .load_str(
            r#"
      (defvar *native-edited-cleanup* nil)
      (defvar *native-unwind-flags* nil)
      (defun native-edit-thrower ()
        (declare (optimize (debug 3)))
        (throw 'native-edited-exit :escaped))
      (defun native-edited-unwind (x)
        (declare (optimize (debug 3)))
        (unwind-protect (native-edit-thrower)
          (setq *native-edited-cleanup* x)))
      (defun native-edit-unwind-frontend (condition)
        (declare (ignore condition))
        (setq *native-unwind-flags*
              (mapcar (lambda (frame) (seventh frame))
                      (subseq (zk::%debug-frames) 0 2)))
        (zk::%set-debug-local 40 1 'x)
        (zk:gc)
        (zk::%debug-motion :continue))
    "#,
        )
        .unwrap();
    let caller = runtime.eval("#'native-edited-unwind").unwrap().remove(0);
    install_native_function(&runtime, &caller);
    let callee = runtime.eval("#'native-edit-thrower").unwrap().remove(0);
    install_native_function(&runtime, &callee);
    runtime.add_breakpoint(&callee, None).unwrap();
    let frontend = runtime
        .eval("#'native-edit-unwind-frontend")
        .unwrap()
        .remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    let values = runtime
        .eval("(catch 'native-edited-exit (native-edited-unwind 9))")
        .unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), ":ESCAPED");
    assert_eq!(
        runtime
            .repr(
                &runtime
                    .eval("(list *native-unwind-flags* *native-edited-cleanup*)")
                    .unwrap()[0]
            )
            .unwrap(),
        "((T T) 40)"
    );
}

#[cfg(feature = "jit")]
#[test]
fn unhandled_native_cell_recovery_enters_the_frontend_once() {
    for (operation, expected, effects) in [
        (
            "(native-unhandled-missing (incf *native-unhandled-effects*))",
            zkcl::ConditionClass::UndefinedFunction,
            1,
        ),
        (
            "(funcall 'native-unhandled-missing (incf *native-unhandled-effects*))",
            zkcl::ConditionClass::UndefinedFunction,
            1,
        ),
        (
            "(multiple-value-call 'native-unhandled-missing (values (incf *native-unhandled-effects*) 9))",
            zkcl::ConditionClass::UndefinedFunction,
            1,
        ),
        (
            "(symbol-value 'native-unhandled-value)",
            zkcl::ConditionClass::UnboundVariable,
            0,
        ),
        (
            "(fdefinition 'native-unhandled-missing)",
            zkcl::ConditionClass::UndefinedFunction,
            0,
        ),
    ] {
        let runtime = zkcl::Runtime::builder()
            .jit_threshold(u32::MAX)
            .start()
            .unwrap();
        runtime
            .load_str(&format!(
                r#"
          (defvar *native-unhandled-frontend-count* 0)
          (defvar *native-unhandled-effects* 0)
          (defvar *native-unhandled-was-native* nil)
          (defun native-unhandled-target ()
            (declare (optimize (debug 3)))
            {operation})
          (defun native-unhandled-frontend (condition)
            (declare (ignore condition))
            (incf *native-unhandled-frontend-count*)
            (setq *native-unhandled-was-native*
                  (seventh (first (zk::%debug-frames)))))
        "#
            ))
            .unwrap();
        let function = runtime.eval("#'native-unhandled-target").unwrap().remove(0);
        install_native_function(&runtime, &function);
        let frontend = runtime
            .eval("#'native-unhandled-frontend")
            .unwrap()
            .remove(0);
        runtime.set_debugger(Some(&frontend)).unwrap();
        // Reenter through a synchronous callback to keep the failing native
        // activation on the stack while the frontend observes it.
        runtime
            .register_function("run-unhandled-in-native-test", move |rt, args| {
                let error = rt.call(&args[0], &[]).err().expect("unhandled condition");
                assert!(
                    matches!(error.kind(), zkcl::EvalErrorKind::LispCondition(class) if class == expected),
                    "{operation}: {error:?}"
                );
                Ok(vec![])
            })
            .unwrap();
        let driver = runtime
            .eval("#'run-unhandled-in-native-test")
            .unwrap()
            .remove(0);
        assert!(runtime.call(&driver, &[&function]).unwrap().is_empty());
        let state = runtime.eval("(list *native-unhandled-frontend-count* *native-unhandled-was-native* *native-unhandled-effects*)").unwrap();
        assert_eq!(
            runtime.repr(&state[0]).unwrap(),
            format!("(1 T {effects})"),
            "{operation}"
        );
    }
}
