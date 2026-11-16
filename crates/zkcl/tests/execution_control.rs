use zkcl::{EvalErrorKind, ExecutionStop, Runtime};

#[test]
fn cancellation_unwinds_lisp_cleanups_in_their_dynamic_environment() {
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
        .load_str("(defvar *cancel-binding* :outside) (defvar *cancel-log* nil)")
        .unwrap();

    let error = runtime
        .eval(
            "(let ((*cancel-binding* :inside) (payload (list 42)))
               (handler-bind ((error (lambda (condition)
                                       (declare (ignore condition))
                                       (push :handler *cancel-log*))))
                 (unwind-protect
                     (unwind-protect (request-cancel)
                       (collect-during-cleanup)
                       (push (cons *cancel-binding* payload) *cancel-log*))
                   (push :outer *cancel-log*))))",
        )
        .err()
        .unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    let cancellation = runtime.cancellation_handle();
    assert!(cancellation.is_cancelled());
    assert_eq!(
        runtime.eval("42").err().unwrap().execution_stop(),
        Some(ExecutionStop::Cancelled)
    );
    cancellation.reset();
    assert!(
        runtime
            .eval("(equal (list *cancel-binding* *cancel-log*) '(:outside (:outer (:inside 42))))")
            .unwrap()[0]
            .is_t()
    );
}

#[test]
fn a_hard_stop_can_interrupt_a_cleanup() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("request-cancel", |runtime, _| {
            runtime.cancellation_handle().cancel_with_cleanup();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .register_function("stop-cleanup", |runtime, arguments| {
            if arguments[0].is_nil() {
                runtime.set_step_budget(Some(0));
            } else {
                let cancellation = runtime.cancellation_handle();
                cancellation.cancel();
                cancellation.cancel_with_cleanup();
            }
            Ok(Vec::new())
        })
        .unwrap();
    runtime.eval("(defvar *cleanup-log* nil)").unwrap();
    for (argument, stop) in [
        ("nil", ExecutionStop::BudgetExhausted),
        ("t", ExecutionStop::Aborted),
    ] {
        let error = runtime
            .eval(&format!(
                "(unwind-protect (request-cancel)
                   (push :started *cleanup-log*)
                   (stop-cleanup {argument})
                   (push :finished *cleanup-log*))"
            ))
            .err()
            .unwrap();
        assert_eq!(error.execution_stop(), Some(stop));
        let cancellation = runtime.cancellation_handle();
        runtime.set_step_budget(None);
        cancellation.reset();
        cancellation.cancel_with_cleanup();
        assert_eq!(
            runtime.eval("42").err().unwrap().execution_stop(),
            Some(ExecutionStop::Cancelled)
        );
        cancellation.reset();
    }
    assert!(
        runtime
            .eval("(equal *cleanup-log* '(:started :started))")
            .unwrap()[0]
            .is_t()
    );
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn a_host_request_wakes_a_sleeping_lisp_call() {
    use std::time::{Duration, Instant};

    let runtime = Runtime::start().unwrap();
    let cancellation = runtime.cancellation_handle();
    let (started, waiting) = std::sync::mpsc::channel();
    runtime
        .register_function("about-to-sleep", move |_, _| {
            started.send(())?;
            Ok(Vec::new())
        })
        .unwrap();
    let worker = std::thread::spawn(move || {
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        cancellation.cancel_with_cleanup();
    });
    runtime.eval("(defvar *slept-cleanup* nil)").unwrap();
    let start = Instant::now();
    let error = runtime
        .eval("(unwind-protect (progn (about-to-sleep) (sleep 5)) (setq *slept-cleanup* t))")
        .err()
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    worker.join().unwrap();
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    runtime.cancellation_handle().reset();
    assert!(runtime.eval("*slept-cleanup*").unwrap()[0].is_t());
}
