use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use zkcl::Runtime;

fn text(runtime: &Runtime, form: &str) -> String {
    runtime.repr(&runtime.eval(form).unwrap()[0]).unwrap()
}

#[test]
fn trace_follows_defun_and_setter_replacement_and_untrace_restores_latest_definition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defun traced-value (x) (values x 2)) (trace traced-value)")
        .unwrap();
    let old = runtime.eval("#'traced-value").unwrap().pop().unwrap();
    runtime
        .load_str("(defun traced-value (x) (values (+ x 10) 3))")
        .unwrap();
    let trace = text(
        &runtime,
        "(with-output-to-string (s) (let ((*trace-output* s)) (traced-value 7)))",
    );
    assert!(trace.contains("enter (7)"), "{trace}");
    assert!(trace.contains("exit (17 3)"), "{trace}");
    let input = runtime.fixnum(7).unwrap();
    assert_eq!(
        runtime.call(&old, &[&input]).unwrap()[0].as_fixnum(),
        Some(7)
    );
    runtime
        .eval("(setf (symbol-function 'traced-value) (lambda (x) (+ x 100)))")
        .unwrap();
    let trace = text(
        &runtime,
        "(with-output-to-string (s) (let ((*trace-output* s)) (traced-value 7)))",
    );
    assert!(trace.contains("exit (107)"), "{trace}");
    runtime.eval("(untrace traced-value)").unwrap();
    assert_eq!(text(&runtime, "(traced-value 7)"), "107");
    assert_eq!(text(&runtime, "(trace)"), "NIL");
}

#[test]
fn trace_options_gate_output_preserve_values_and_break_with_continue() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defun option-target (x) (values (+ x 1) x))")
        .unwrap();
    runtime.eval("(trace option-target :condition (> (car zk::*trace-arguments*) 1) :report nil :print (list :input zk::*trace-arguments*) :print-after (list :output zk::*trace-values*))").unwrap();
    assert_eq!(
        text(
            &runtime,
            "(with-output-to-string (s) (let ((*trace-output* s)) (option-target 1)))"
        ),
        "\"\""
    );
    let trace = text(
        &runtime,
        "(with-output-to-string (s) (let ((*trace-output* s)) (option-target 3)))",
    );
    assert!(trace.contains(":INPUT (3)"), "{trace}");
    assert!(trace.contains(":OUTPUT (4 3)"), "{trace}");
    assert!(!trace.contains("TRACE"), "{trace}");
    assert_eq!(
        text(&runtime, "(multiple-value-list (option-target 3))"),
        "(4 3)"
    );
    runtime
        .eval("(trace option-target :report nil :break-after t)")
        .unwrap();
    let frontend = runtime
        .eval("(lambda (condition) (declare (ignore condition)) (continue))")
        .unwrap()
        .remove(0);
    runtime.set_debugger(Some(&frontend)).unwrap();
    assert_eq!(
        text(&runtime, "(multiple-value-list (option-target 4))"),
        "(5 4)"
    );
    assert!(runtime.eval("(trace option-target :unknown t)").is_err());
    assert!(runtime.eval("(trace option-target :print)").is_err());
}

#[test]
fn trace_options_keep_symbol_identity_across_redefinition_from_another_package() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defparameter *trace-threshold* 2) (defun package-traced (x) x)")
        .unwrap();
    runtime
        .eval("(trace package-traced :condition (> (car zk::*trace-arguments*) *trace-threshold*))")
        .unwrap();
    runtime.load_str("(defpackage :trace-other (:use :cl)) (in-package :trace-other) (defun cl-user::package-traced (x) (+ x 10)) (in-package :cl-user)").unwrap();
    assert_eq!(
        text(
            &runtime,
            "(with-output-to-string (s) (let ((*trace-output* s)) (package-traced 1)))"
        ),
        "\"\""
    );
    let trace = text(
        &runtime,
        "(with-output-to-string (s) (let ((*trace-output* s)) (package-traced 4)))",
    );
    assert!(trace.contains("exit (14)"), "{trace}");
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn dribble_records_host_interaction_once_without_changing_dynamic_streams() {
    let mut runtime = Runtime::start().unwrap();
    runtime.set_standard_input(io::Cursor::new(b"abc\n"));
    runtime.set_standard_output(Sink::default());
    runtime.set_error_output(Sink::default());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let directory = root.join(".agents/tmp/zkcl/repl-completion/env");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!(
        "dribble-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let quoted = format!("{:?}", path.to_string_lossy());
    runtime.eval(&format!("(dribble {quoted})")).unwrap();
    runtime.eval("(progn (peek-char) (read-char) (unread-char #\\a) (read-char) (read-line) (write-string \"out\") (write-string \"err\" *error-output*) (let ((*standard-output* (make-string-output-stream))) (write-string \"private\")) (dribble) (write-string \"after\"))").unwrap();
    let transcript = std::fs::read_to_string(&path).unwrap();
    assert_eq!(transcript, "abc\nouterr");
    assert!(runtime.eval(&format!("(dribble {quoted})")).is_err());
    std::fs::remove_file(path).unwrap();
}
