use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::lisp_path;

#[test]
fn unhandled_error_reports_innermost_source_locations() {
    let runtime = Runtime::start().unwrap();
    let source = r#"
(defun debug-leaf (value)
  (+ 1 (car value)))

(defun debug-caller (value)
  (+ 1 (debug-leaf value)))

(list (debug-caller 42))
"#;

    let error = runtime.load_str(source).unwrap_err();
    assert!(matches!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    ));

    let frames = error.stack_frames();
    assert!(frames.len() >= 3, "{frames:#?}");
    assert_eq!(frames[0].function.as_deref(), Some("DEBUG-LEAF"));
    assert_eq!(frames[1].function.as_deref(), Some("DEBUG-CALLER"));
    assert_eq!(frames[2].function, None);

    let leaf_site = source.find("(car value)").unwrap() + 1;
    assert_eq!(
        frames[0].source_range.as_ref(),
        Some(&(leaf_site..leaf_site))
    );

    let caller_site = source.find("(debug-leaf value)").unwrap() + 1;
    assert_eq!(
        frames[1].source_range.as_ref(),
        Some(&(caller_site..caller_site))
    );

    let top_site = source.find("(debug-caller 42)").unwrap() + 1;
    assert_eq!(frames[2].source_range.as_ref(), Some(&(top_site..top_site)));
    assert!(frames.iter().all(|frame| frame.bytecode_pc.is_some()));
}

#[test]
fn declined_error_keeps_its_original_source_location() {
    let rt = Runtime::start().unwrap();
    let source = "(defun fault-after-decline (x) (+ 1 (car x)))
(let ((*debugger-hook* (lambda (condition hook) (declare (ignore condition hook)))))
  (handler-bind ((type-error #'identity)) (fault-after-decline 7)))";
    let error = rt.load_str(source).unwrap_err();
    let frame = &error.stack_frames()[0];
    let at = source.find("(car x)").unwrap() + 1;
    assert_eq!(frame.function.as_deref(), Some("FAULT-AFTER-DECLINE"));
    assert_eq!(frame.source_range, Some(at..at));
}

#[test]
fn locally_declaring_debug_preserves_the_flet_call_frame() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun call-local-car (debug-call-p value)
               (flet ((add-one-to-car (x) (+ 1 (car x))))
                 (list (if debug-call-p
                           (locally (declare (optimize debug)) (add-one-to-car value))
                           (add-one-to-car value)))))",
        )
        .unwrap();

    for (call, innermost_function) in [
        ("(call-local-car t 42)", "ADD-ONE-TO-CAR"),
        ("(call-local-car nil 42)", "CALL-LOCAL-CAR"),
    ] {
        let error = runtime.eval(call).err().expect("CAR requires a list");
        assert!(matches!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError)
        ));
        assert_eq!(
            error.stack_frames()[0].function.as_deref(),
            Some(innermost_function)
        );
    }
}

#[test]
fn bytecode_frames_keep_source_paths_after_nested_load_returns() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("source-debugger");
    let inner_source = "; 雪\n(defun file-leaf (value) (+ 1 (car value)))";
    let inner = temp_dir.write("inner.lisp", inner_source);
    let outer_source = format!(
        "(load {})\n(defun file-caller (value) (+ 1 (file-leaf value)))\n(list (file-caller 42))",
        lisp_path(&inner)
    );
    let outer = temp_dir.write("outer.lisp", &outer_source);

    let error = runtime.load_file(&outer).unwrap_err();
    let inner_path = inner.canonicalize().unwrap();
    let outer_path = outer.canonicalize().unwrap();
    let leaf_at = inner_source.find("(car value)").unwrap() + 1;
    let caller_at = outer_source.find("(file-leaf value)").unwrap() + 1;
    let top_at = outer_source.find("(file-caller 42)").unwrap() + 1;
    let frames = error.stack_frames();
    assert_eq!(frames[0].function.as_deref(), Some("FILE-LEAF"));
    assert_eq!(frames[0].source_path.as_deref(), Some(inner_path.as_path()));
    assert_eq!(frames[0].source_range, Some(leaf_at..leaf_at));
    assert_eq!(frames[1].function.as_deref(), Some("FILE-CALLER"));
    assert_eq!(frames[1].source_path.as_deref(), Some(outer_path.as_path()));
    assert_eq!(frames[1].source_range, Some(caller_at..caller_at));
    assert_eq!(frames[2].source_path.as_deref(), Some(outer_path.as_path()));
    assert_eq!(frames[2].source_range, Some(top_at..top_at));

    let error = runtime
        .eval("(list (file-caller 42))")
        .err()
        .expect("loaded function should fail");
    let frames = error.stack_frames();
    assert_eq!(frames[0].source_path.as_deref(), Some(inner_path.as_path()));
    assert_eq!(frames[1].source_path.as_deref(), Some(outer_path.as_path()));
    assert_eq!(frames[2].source_path, None);
}

#[test]
fn fasl_retains_named_frames_and_utf8_source_offsets() {
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("fasl-source-debugger");
    let source = "; 雪🙂\n(defun compiled-leaf (value) (+ 1 (car value)))\n\
                  (defun compiled-caller (value) (+ 1 (compiled-leaf value)))";
    let input = directory.write("program.lisp", source);
    let fasl = runtime.compile_file(&input, None).unwrap();
    runtime.load_file(&fasl).unwrap();
    let error = runtime
        .eval("(list (compiled-caller 42))")
        .err()
        .expect("CAR requires a list");
    let path = input.canonicalize().unwrap();
    for (frame, function, expression) in [
        (&error.stack_frames()[0], "COMPILED-LEAF", "(car value)"),
        (
            &error.stack_frames()[1],
            "COMPILED-CALLER",
            "(compiled-leaf value)",
        ),
    ] {
        let at = source.find(expression).unwrap() + 1;
        assert_eq!(frame.function.as_deref(), Some(function));
        assert_eq!(frame.source_path.as_deref(), Some(path.as_path()));
        assert_eq!(frame.source_range, Some(at..at));
        assert!(frame.bytecode_pc.is_some());
    }
}
