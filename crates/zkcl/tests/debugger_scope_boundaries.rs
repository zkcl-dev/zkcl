use zkcl::Runtime;

fn values(runtime: &Runtime, source: &str) -> Vec<isize> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum().unwrap())
        .collect()
}

#[test]
fn returning_the_outermost_frame_preserves_the_frameless_evaluator_and_abort() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defparameter *boundary-cleanups* 0)")
        .unwrap();
    runtime
        .register_function("boundary-debugger", |runtime, _| {
            let frames = runtime.debug_frames()?;
            let outermost = frames.last().unwrap();
            let first = runtime.fixnum(41).unwrap();
            let second = runtime.fixnum(42).unwrap();
            runtime.debug_return_frame(outermost.index, &[&first, &second])?;
            unreachable!("frame return leaves the callback")
        })
        .unwrap();
    let debugger = runtime.eval("#'boundary-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    assert_eq!(
        values(
            &runtime,
            "(unwind-protect (break \"outermost return\") (incf *boundary-cleanups*))"
        ),
        [41, 42]
    );
    let mut forms = Vec::new();
    runtime.eval_source::<zkcl::EvalError>(
        "(unwind-protect (break \"return and resume evaluator\") (incf *boundary-cleanups*)) (values *boundary-cleanups* (if (find-restart 'abort) 1 0))",
        |values| {
            forms.push(values.iter().map(|value| value.as_fixnum().unwrap()).collect::<Vec<_>>());
            Ok(())
        },
    ).unwrap();
    assert_eq!(forms, [vec![41, 42], vec![2, 1]]);
}

#[test]
fn restarting_the_outermost_frame_preserves_its_evaluator_boundary() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(declaim (optimize (debug 3))) (defparameter *boundary-attempts* 0) (defparameter *boundary-cleanups* 0)").unwrap();
    runtime
        .register_function("boundary-debugger", |runtime, _| {
            let frames = runtime.debug_frames()?;
            let outermost = frames.last().unwrap();
            assert!(outermost.can_restart);
            runtime.debug_restart_frame(outermost.index)?;
            unreachable!("frame restart leaves the callback")
        })
        .unwrap();
    let debugger = runtime.eval("#'boundary-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    assert_eq!(
        values(
            &runtime,
            r#"
      (let ()
        (incf *boundary-attempts*)
        (unwind-protect
            (when (= *boundary-attempts* 1) (break "outermost restart"))
          (incf *boundary-cleanups*))
        (values *boundary-attempts* *boundary-cleanups* (if (find-restart 'abort) 1 0)))"#
        ),
        [2, 2, 1]
    );
}

#[test]
fn frame_return_resumes_the_cell_recovery_continuation_after_cleanup() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (defparameter *boundary-cleanups* 0)
      (defun boundary-return-target (x)
        (declare (optimize (debug 3)))
        (unwind-protect (progn (break "recovery return") x)
          (incf *boundary-cleanups*)))"#,
        )
        .unwrap();
    runtime
        .register_function("boundary-debugger", |runtime, _| {
            let frame = runtime
                .debug_frames()?
                .into_iter()
                .find(|frame| frame.function == "BOUNDARY-RETURN-TARGET")
                .unwrap();
            let first = runtime.fixnum(41).unwrap();
            let second = runtime.fixnum(42).unwrap();
            runtime.debug_return_frame(frame.index, &[&first, &second])?;
            unreachable!("frame return leaves the callback")
        })
        .unwrap();
    let debugger = runtime.eval("#'boundary-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    assert_eq!(
        values(
            &runtime,
            r#"
      (let ((effects 0))
        (handler-bind ((undefined-function (lambda (c) (use-value #'boundary-return-target c))))
          (multiple-value-bind (a b) (missing-boundary-return (incf effects))
            (values a b effects *boundary-cleanups* (if (find-restart 'abort) 1 0)))))"#
        ),
        [41, 42, 1, 1, 1]
    );
}

#[test]
fn frame_restart_keeps_original_arguments_and_the_recovery_continuation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (defparameter *boundary-cleanups* 0)
      (defun boundary-restart-target (x)
        (declare (optimize (debug 3)))
        (incf x)
        (unwind-protect (progn (break "recovery restart") x)
          (incf *boundary-cleanups*)))"#,
        )
        .unwrap();
    runtime
        .register_function("boundary-debugger", |runtime, _| {
            let frame = runtime
                .debug_frames()?
                .into_iter()
                .find(|frame| frame.function == "BOUNDARY-RESTART-TARGET")
                .unwrap();
            runtime.debug_eval(
                frame.index,
                "(defun boundary-restart-target (x) (values (+ x 100) 42))",
            )?;
            runtime.debug_restart_frame(frame.index)?;
            unreachable!("frame restart leaves the callback")
        })
        .unwrap();
    let debugger = runtime.eval("#'boundary-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    assert_eq!(
        values(
            &runtime,
            r#"
      (let ((effects 0))
        (handler-bind ((undefined-function (lambda (c) (use-value #'boundary-restart-target c))))
          (multiple-value-bind (a b) (missing-boundary-restart (incf effects))
            (values a b effects *boundary-cleanups* (if (find-restart 'abort) 1 0)))))"#
        ),
        [101, 42, 1, 1, 1]
    );
}
