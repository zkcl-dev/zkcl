use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use zkcl::{ConditionClass, DebugMotion, EvalErrorKind, Runtime};

#[test]
fn returning_a_frame_runs_cleanup_and_preserves_multiple_values() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-debugger", |rt, _| {
            let frames = rt.debug_frames()?;
            let frame = frames.iter().find(|f| f.function == "RETURN-LEAF").unwrap();
            assert!(frame.can_return);
            let a = rt.integer_from(41).unwrap();
            let b = rt.integer_from(42).unwrap();
            rt.debug_return_frame(frame.index, &[&a, &b])?;
            panic!("frame return must transfer out of the debugger");
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    let values = runtime.eval("(progn (defparameter *debug-cleanups* nil) (defun return-leaf (x) (declare (optimize (debug 3))) (unwind-protect (progn (break \"return here\") (+ x 1)) (catch 'inner (throw 'inner 1)) (push :leaf *debug-cleanups*))) (list (multiple-value-list (return-leaf 10)) *debug-cleanups*))").unwrap();
    assert_eq!(runtime.preview(&values[0]).unwrap(), "((41 42) (:LEAF))");
}

#[test]
fn restarting_a_frame_uses_original_arguments_and_the_new_definition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-debugger", |rt, _| {
            let frames = rt.debug_frames()?;
            let frame = frames
                .iter()
                .find(|f| f.function == "RESTART-LEAF")
                .unwrap();
            assert!(frame.can_restart);
            rt.debug_eval(frame.index, "(defun restart-leaf (x) (+ x 100))")?;
            rt.debug_restart_frame(frame.index)?;
            panic!("frame restart must transfer out of the debugger");
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    let values=runtime.eval("(progn (defparameter *debug-cleanups* nil) (defun restart-leaf (x) (declare (optimize (debug 3))) (incf x) (unwind-protect (progn (break \"restart here\") x) (push :old *debug-cleanups*))) (list (restart-leaf 10) *debug-cleanups*))").unwrap();
    assert_eq!(runtime.preview(&values[0]).unwrap(), "(110 (:OLD))");
}

#[test]
fn a_pause_request_interrupts_a_running_loop_and_can_continue() {
    let runtime = Runtime::start().unwrap();
    let pause = runtime.pause_handle();
    let (started, receiver) = std::sync::mpsc::channel();
    runtime
        .register_function("loop-started", move |_, _| {
            started.send(()).unwrap();
            Ok(Vec::new())
        })
        .unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&stopped);
    runtime
        .register_function("host-debugger", move |rt, _| {
            assert!(!rt.debug_frames()?.is_empty());
            observed.store(true, Ordering::Relaxed);
            rt.debug_eval(0, "(setq *debug-loop-running* nil)")?;
            assert!(rt.debug_motion(DebugMotion::Continue)?);
            Ok(Vec::new())
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    let worker = std::thread::spawn(move || {
        receiver.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        pause.pause();
    });
    let values=runtime.eval("(progn (defparameter *debug-loop-running* t) (loop-started) (loop while *debug-loop-running*) 42)").unwrap();
    worker.join().unwrap();
    assert!(stopped.load(Ordering::Relaxed));
    assert_eq!(values[0].as_fixnum(), Some(42));
}

#[test]
fn frame_eval_uses_the_original_reader_package_and_catches_ordinary_errors() {
    let runtime = Runtime::start().unwrap();
    let enters = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&enters);
    runtime
        .register_function("host-debugger", move |rt, _| {
            let count = observed.fetch_add(1, Ordering::Relaxed) + 1;
            assert_eq!(
                count,
                1,
                "ordinary frame eval errors must not enter another debugger"
            );
            let frames = rt.debug_frames()?;
            let frame = frames
                .iter()
                .find(|f| f.function == "PACKAGE-LEAF")
                .unwrap();
            let failure = rt.debug_eval(frame.index, "(car 17)").err().unwrap();
            assert_eq!(
                failure.kind(),
                EvalErrorKind::LispCondition(ConditionClass::TypeError)
            );
            let value = rt.debug_eval(frame.index, "x")?.remove(0);
            assert_eq!(value.as_fixnum(), Some(33));
            rt.debug_return_frame(frame.index, &[&value])?;
            panic!("frame return must leave the callback");
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    runtime.load_str("(defpackage :debug-source-package (:use :cl)) (in-package :debug-source-package) (defun package-leaf (x) (declare (optimize (debug 3))) (break \"package frame\") x) (in-package :cl-user)").unwrap();
    let value = runtime
        .eval("(debug-source-package::package-leaf 33)")
        .unwrap()
        .remove(0);
    assert_eq!(value.as_fixnum(), Some(33));
    assert_eq!(enters.load(Ordering::Relaxed), 1);
}

#[test]
fn cleanup_can_supersede_a_requested_frame_return() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-debugger", |rt, _| {
            let frame = rt
                .debug_frames()?
                .into_iter()
                .find(|f| f.function == "CLEANUP-EXIT")
                .unwrap();
            let value = rt.integer_from(99).unwrap();
            rt.debug_return_frame(frame.index, &[&value])?;
            panic!("cleanup transfer must leave debugger");
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    let value=runtime.eval("(progn (defun cleanup-exit () (declare (optimize (debug 3))) (unwind-protect (break \"return\") (throw 'outer :cleanup-won))) (catch 'outer (cleanup-exit)))").unwrap().remove(0);
    assert_eq!(runtime.preview(&value).unwrap(), ":CLEANUP-WON");
}

#[test]
fn structured_debug_operations_outside_a_pause_return_errors() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("unexpected-debugger", |_, _| {
            panic!("debug API errors must be returned to the host")
        })
        .unwrap();
    let debugger = runtime.eval("#'unexpected-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    assert!(runtime.debug_frames().is_err());
    assert!(runtime.debug_variables(0).is_err());
    assert!(runtime.debug_restarts().is_err());
    assert!(runtime.debug_eval(0, "42").is_err());
    assert!(runtime.debug_motion(DebugMotion::Continue).is_err());
}

#[test]
fn pause_requests_do_not_cancel_and_cannot_clear_a_cancellation() {
    let runtime = Runtime::start().unwrap();
    let pause = runtime.pause_handle();
    let cancellation = runtime.cancellation_handle();
    pause.pause();
    assert!(pause.is_pending());
    assert!(!cancellation.is_cancelled());
    cancellation.cancel_with_cleanup();
    pause.reset();
    assert!(cancellation.is_cancelled());
    cancellation.reset();
}

#[test]
fn restarting_resolves_a_global_definition_after_old_cleanup_finishes() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("host-debugger", |rt, _| {
            let frame = rt
                .debug_frames()?
                .into_iter()
                .find(|f| f.function == "CLEANUP-REDEFINE")
                .unwrap();
            rt.debug_eval(frame.index, "(defun cleanup-redefine (x) (+ x 100))")?;
            rt.debug_restart_frame(frame.index)?;
            panic!("restart must leave the debugger");
        })
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap().remove(0);
    runtime.set_debugger(Some(&debugger)).unwrap();
    let value=runtime.eval("(progn (defun cleanup-redefine (x) (declare (optimize (debug 3))) (unwind-protect (break \"restart\") (defun cleanup-redefine (x) (+ x 200)))) (cleanup-redefine 10))").unwrap().remove(0);
    assert_eq!(value.as_fixnum(), Some(210));
}
