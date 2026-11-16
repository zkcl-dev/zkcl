use zkcl::{BreakpointFollow, BreakpointOptions, BreakpointStatus, Runtime};
mod common;
use common::files::TempDir;

#[test]
fn image_retains_breakpoint_options_counters_names_and_original_function_objects() {
    let directory = TempDir::new("image-breakpoints");
    let path = directory.path().join("debugger.core");
    let (object_id, name_id, pending_id, options) = {
        let runtime = Runtime::start().unwrap();
        runtime.load_str("(defparameter *image-break-stops* 0) (defparameter *image-break-logs* nil) (defun image-break-target (x) (declare (optimize (debug 3))) (+ x 1)) (defparameter *image-old-function* #'image-break-target)").unwrap();
        let debugger = runtime
            .eval("(lambda (condition) (declare (ignore condition)) (incf *image-break-stops*))")
            .unwrap()
            .remove(0);
        runtime.set_debugger(Some(&debugger)).unwrap();
        let function = runtime.eval("*image-old-function*").unwrap().remove(0);
        let object = runtime.add_breakpoint(&function, None).unwrap();
        let symbol = runtime.eval("'image-break-target").unwrap().remove(0);
        let options = BreakpointOptions {
            condition: Some("(> x 1)".into()),
            hit_count: Some(2),
            hit_every: Some(2),
            log: Some("(push x *image-break-logs*)".into()),
            follow: BreakpointFollow::Name,
            ..Default::default()
        };
        let named = runtime
            .add_breakpoint_with_options(&symbol, None, options.clone())
            .unwrap();
        runtime.eval("(image-break-target 1)").unwrap();
        runtime
            .eval("(defun image-break-target (x) (declare (optimize (debug 3))) (+ x 10))")
            .unwrap();
        runtime.enable_breakpoint(object.id, false).unwrap();
        let pending = runtime
            .add_file_breakpoint(
                directory.path().join("not-loaded.lisp"),
                5,
                BreakpointOptions::default(),
            )
            .unwrap();
        runtime.save_image(&path).unwrap();
        (object.id, named.id, pending.id, options)
    };
    let mut runtime = Runtime::start().unwrap();
    runtime.restore_image(&path).unwrap();
    let sites = runtime.breakpoints();
    let object = sites.iter().find(|site| site.id == object_id).unwrap();
    assert!(!object.options.enabled);
    assert_eq!(object.hits, 1);
    let named = sites.iter().find(|site| site.id == name_id).unwrap();
    assert_eq!(named.options, options);
    assert_eq!(named.hits, 1);
    assert_eq!(named.status, BreakpointStatus::Verified);
    let pending = sites.iter().find(|site| site.id == pending_id).unwrap();
    assert_eq!(pending.requested_line, Some(5));
    assert!(matches!(pending.status, BreakpointStatus::Pending(_)));
    assert_eq!(
        runtime.eval("(image-break-target 3)").unwrap()[0].as_fixnum(),
        Some(13)
    );
    assert_eq!(
        runtime.eval("*image-break-stops*").unwrap()[0].as_fixnum(),
        Some(2)
    );
    assert_eq!(
        runtime
            .preview(&runtime.eval("*image-break-logs*").unwrap()[0])
            .unwrap(),
        "(3)"
    );
    runtime.enable_breakpoint(object_id, true).unwrap();
    assert_eq!(
        runtime.eval("(funcall *image-old-function* 4)").unwrap()[0].as_fixnum(),
        Some(5)
    );
    assert_eq!(
        runtime.eval("*image-break-stops*").unwrap()[0].as_fixnum(),
        Some(3)
    );
    let function = runtime.eval("#'image-break-target").unwrap().remove(0);
    let added = runtime.add_breakpoint(&function, None).unwrap();
    assert!(added.id > pending_id);
}

#[test]
fn pending_pause_must_be_cleared_before_an_image_is_saved() {
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("image-pending-pause");
    let path = directory.path().join("pause.core");
    let pause = runtime.pause_handle();
    pause.pause();
    let error = runtime.save_image(&path).unwrap_err();
    assert!(error.message().contains("pause"));
    pause.reset();
    runtime.save_image(&path).unwrap();
}
