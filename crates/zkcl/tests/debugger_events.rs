use zkcl::{DebugMotion, EvalError, Evaluation, EvaluationPoll, ExecutionStop, Runtime};

fn next<'rt>(evaluation: &mut Evaluation<'rt>) -> Result<EvaluationPoll<'rt>, EvalError> {
    for _ in 0..1000 {
        match evaluation.resume(None)? {
            EvaluationPoll::Runnable => {}
            result => return Ok(result),
        }
    }
    panic!("evaluation did not reach an observable stop")
}

#[test]
fn conditional_pause_lends_live_locals_and_task_bindings_to_the_host() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "\
(defparameter *event-mode* :outside)
(defun event-leaf (x)
  (declare (optimize (debug 3)))
  (let ((*event-mode* :inside))
    (setq *event-mode* :inside)
    (values (1+ x) *event-mode*)))",
        )
        .unwrap();
    let function = runtime.eval("#'event-leaf").unwrap().remove(0);
    runtime
        .add_source_breakpoint_with_options(
            &function,
            5,
            zkcl::BreakpointOptions {
                condition: Some("(= x 3)".into()),
                ..Default::default()
            },
        )
        .unwrap();

    let mut without_hit = runtime
        .start_call(&function, &[&runtime.fixnum(2).unwrap()])
        .unwrap();
    without_hit.enable_debugging();
    let EvaluationPoll::Ready(values) = next(&mut without_hit).unwrap() else {
        panic!("condition should skip the stop")
    };
    assert_eq!(values[0].as_fixnum(), Some(3));

    let mut stopped = runtime
        .start_call(&function, &[&runtime.fixnum(3).unwrap()])
        .unwrap();
    stopped.enable_debugging();
    let EvaluationPoll::Paused(event) = next(&mut stopped).unwrap() else {
        panic!("expected the breakpoint")
    };
    assert!(event.message.contains("Breakpoint"), "{}", event.message);
    assert_eq!(event.frames[0].function, "EVENT-LEAF");
    assert_eq!(event.frames[0].line, Some(5));
    assert!(runtime.eval("nil").is_err());
    runtime.collect();
    assert!(matches!(
        stopped.resume(None).unwrap(),
        EvaluationPoll::Paused(_)
    ));
    stopped
        .with_debugger(|rt| -> Result<(), EvalError> {
            let x = rt.lookup_symbol("CL-USER", "X")?.unwrap();
            assert_eq!(rt.debug_local(0, &x)?.as_fixnum(), Some(3));
            assert_eq!(
                rt.preview(&rt.debug_eval(0, "*event-mode*")?[0])?,
                ":INSIDE"
            );
            rt.debug_set_local(0, &x, &rt.fixnum(41).unwrap())?;
            assert_eq!(rt.debug_local(0, &x)?.as_fixnum(), Some(41));
            assert!(rt.debug_motion(DebugMotion::Continue)?);
            Ok(())
        })
        .unwrap()
        .unwrap()
        .unwrap();
    let EvaluationPoll::Ready(values) = next(&mut stopped).unwrap() else {
        panic!("continued invocation")
    };
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(runtime.preview(&values[1]).unwrap(), ":INSIDE");
    assert_eq!(
        runtime
            .preview(&runtime.eval("*event-mode*").unwrap()[0])
            .unwrap(),
        ":OUTSIDE"
    );
    assert!(!event.condition.is_nil());
}

#[test]
fn condition_restart_exits_the_host_borrow_and_resumes_the_failing_operation() {
    let runtime = Runtime::start().unwrap();
    let mut evaluation = runtime
        .start_eval("(+ 2 missing-debug-event-value)")
        .unwrap();
    evaluation.enable_debugging();
    let EvaluationPoll::Paused(event) = next(&mut evaluation).unwrap() else {
        panic!("unbound variable should stop")
    };
    assert_eq!(event.condition_type, "UNBOUND-VARIABLE");
    assert!(
        !evaluation
            .with_debugger(|rt| rt.debug_motion(DebugMotion::Continue))
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert!(matches!(
        evaluation.resume(None).unwrap(),
        EvaluationPoll::Paused(_)
    ));
    assert!(
        evaluation
            .with_debugger(|rt| -> Result<(), EvalError> {
                let use_value = rt.lookup_symbol("CL", "USE-VALUE")?.unwrap();
                rt.debug_invoke_restart(&use_value, &[&rt.fixnum(40).unwrap()])?;
                panic!("the restart leaves the host callback")
            })
            .unwrap_or_else(|error| panic!("{error}"))
            .is_none()
    );
    let EvaluationPoll::Ready(values) = next(&mut evaluation).unwrap() else {
        panic!("expected values from the original call")
    };
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert!(!event.condition.is_nil());
}

#[test]
fn a_frameless_host_call_can_recover_a_missing_function() {
    let runtime = Runtime::start().unwrap();
    let missing = runtime.read_value("missing-incremental-function").unwrap();
    let value = runtime.eval("(list 42)").unwrap().remove(0);
    let mut evaluation = runtime.start_call(&missing, &[&value]).unwrap();
    evaluation.enable_debugging();
    let EvaluationPoll::Paused(event) = next(&mut evaluation).unwrap() else {
        panic!("undefined function should stop")
    };
    assert_eq!(event.condition_type, "UNDEFINED-FUNCTION");
    assert!(event.frames.is_empty());
    let result = evaluation
        .with_debugger(|rt| -> Result<(), EvalError> {
            let restart = rt.lookup_symbol("CL", "USE-VALUE")?.unwrap();
            let identity = rt.lookup_symbol("CL", "IDENTITY")?.unwrap();
            let function = rt.resolve_function(&identity)?;
            rt.debug_invoke_restart(&restart, &[&function])?;
            panic!("restart transferred out of the callback")
        })
        .unwrap();
    assert!(result.is_none());
    let EvaluationPoll::Ready(values) = next(&mut evaluation).unwrap() else {
        panic!("expected the invocation's result")
    };
    let eq = runtime.lookup_symbol("CL", "EQ").unwrap().unwrap();
    assert!(!runtime.call(&eq, &[&value, &values[0]]).unwrap()[0].is_nil());
}

#[test]
fn cancelling_a_paused_evaluation_runs_lisp_cleanup() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defparameter *event-cleaned* nil)")
        .unwrap();
    let mut evaluation = runtime
        .start_eval("(unwind-protect (break \"stop\") (setq *event-cleaned* t))")
        .unwrap();
    evaluation.enable_debugging();
    assert!(matches!(
        next(&mut evaluation).unwrap(),
        EvaluationPoll::Paused(_)
    ));
    let cancellation = runtime.cancellation_handle();
    cancellation.cancel_with_cleanup();
    let Err(error) = next(&mut evaluation) else {
        panic!("expected cancellation")
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    cancellation.reset();
    assert!(!runtime.eval("*event-cleaned*").unwrap()[0].is_nil());
}
