use zkcl::Runtime;

mod common;

use common::files::TempDir;
use common::lisp::{eval_reprs, lisp_path};

#[test]
fn host_file_compilation_can_muffle_warnings_from_the_debugger() {
    let runtime = Runtime::start().unwrap();
    let dir = TempDir::new("host-compiler-conditions");
    let input = dir.write(
        "warning.lisp",
        "(defun host-warning (x) (declare (unknown-host-declaration x)) x)",
    );
    runtime
        .load_str(
            "(defvar *host-warnings* nil)
        (defun host-debugger (c)
          (push (and (typep c 'warning) (pathnamep *compile-file-pathname*)
                     (not (null (find-restart 'muffle-warning c)))) *host-warnings*)
          (muffle-warning c))",
        )
        .unwrap();
    let debugger = runtime.eval("#'host-debugger").unwrap();
    runtime.set_debugger(Some(&debugger[0])).unwrap();
    runtime.eval("(setf *break-on-signals* 'warning)").unwrap();
    runtime.compile_file(&input, None).unwrap();
    runtime.compile_file_to_bytecode(&input).unwrap();
    assert_eq!(eval_reprs(&runtime, "*host-warnings*"), ["(T T)"]);
}

#[test]
fn compile_file_counts_only_warnings_from_its_runtime() {
    struct ThreadCheckedRuntime {
        runtime: std::mem::ManuallyDrop<Runtime>,
        owner: std::thread::ThreadId,
    }
    // SAFETY: Every access and drop asserts `thread::current().id() == self.owner` (temporary until Wave 4 splits `Vm`/`Domain`).
    unsafe impl Send for ThreadCheckedRuntime {}
    // SAFETY: Single-threaded test callback; `owner` check ensures `!Send`/`!Sync` inner `Runtime` is never used off-thread.
    unsafe impl Sync for ThreadCheckedRuntime {}
    impl Drop for ThreadCheckedRuntime {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.owner);
            unsafe { std::mem::ManuallyDrop::drop(&mut self.runtime) };
        }
    }
    impl ThreadCheckedRuntime {
        fn eval(&self, code: &str) -> Result<Vec<zkcl::Root<'_>>, zkcl::EvalError> {
            assert_eq!(std::thread::current().id(), self.owner);
            self.runtime.eval(code)
        }
    }

    let runtime = Runtime::start().unwrap();
    let other = ThreadCheckedRuntime {
        runtime: std::mem::ManuallyDrop::new(Runtime::start().unwrap()),
        owner: std::thread::current().id(),
    };
    runtime
        .register_function("warn-in-other-runtime", move |_, _| {
            other.eval("(handler-bind ((warning #'muffle-warning)) (warn \"other runtime\"))")?;
            Ok(Vec::new())
        })
        .unwrap();
    let dir = TempDir::new("compile-warning-runtime");
    let input = dir.write(
        "warning.lisp",
        "(eval-when (:compile-toplevel) (warn-in-other-runtime))",
    );
    let warning_flags = format!(
        "(multiple-value-bind (output warnings-p failure-p) (compile-file {} :verbose nil)
           (declare (ignore output))
           (values warnings-p failure-p))",
        lisp_path(&input)
    );
    assert_eq!(eval_reprs(&runtime, &warning_flags), ["NIL", "NIL"]);

    dir.write(
        "warning.lisp",
        "(eval-when (:compile-toplevel)
           (handler-bind ((warning #'muffle-warning)) (warn \"compiling runtime\")))",
    );
    assert_eq!(eval_reprs(&runtime, &warning_flags), ["T", "T"]);
}

#[test]
fn immediate_lambda_calls_warn_only_for_definitely_unrecognized_keys() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun call-warnings (body)
               (let ((count 0))
                 (handler-bind ((warning (lambda (c) (incf count) (muffle-warning c))))
                   (compile nil (list 'lambda '(key permit) body)))
                 count))",
        )
        .unwrap();
    let warnings = eval_reprs(
        &runtime,
        "(mapcar #'call-warnings
           '(((lambda (&optional value &key keep) value) :positional :lost nil)
             ((lambda (&key keep) keep) :keep t)
             ((lambda (&key keep) keep) :lost nil :allow-other-keys permit)
             ((lambda (&key keep) keep) :lost nil key t)
             ((lambda (&key keep) keep) :lost nil :allow-other-keys nil :allow-other-keys t)
             ((lambda (&key keep) keep) :lost nil :allow-other-keys t :allow-other-keys nil)
             ((lambda (&key ((custom value))) value) 'custom 42)
             ((lambda (&key ((custom value))) value) :custom 42)
             ((lambda (&rest args &key &allow-other-keys) args) :lost nil)))",
    );
    assert_eq!(warnings, ["(1 0 0 0 1 0 0 1 0)"]);
}

/// A warning reaches the handlers of the form that is being compiled, and an
/// unhandled warning still leaves the form compilable.
#[test]
fn compiler_warnings_reach_handlers_in_the_callers_environment() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(block declaration-result
               (handler-bind ((warning (lambda (c)
                                         (declare (ignore c))
                                         (return-from declaration-result :caught))))
                 (eval '(locally (declare (not-a-declaration)) 1))))"
        ),
        [":CAUGHT"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(eval '(locally (declare (not-a-declaration)) 2))"
        ),
        ["2"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(block declaration-result
               (handler-bind ((warning (lambda (c)
                                         (declare (ignore c))
                                         (return-from declaration-result (typep c 'warning)))))
                 (eval '(let ((x 1)) (declare (other-bogus x)) x))))"
        ),
        ["T"],
        "the condition is a WARNING"
    );
    // A standard declaration must stay silent.
    assert_eq!(
        eval_reprs(
            &runtime,
            "(block declaration-result
               (handler-bind ((warning (lambda (c)
                                         (declare (ignore c))
                                         (return-from declaration-result :warned))))
                 (eval '(let ((x 1)) (declare (ignore x)) x))))"
        ),
        ["1"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(block declaration-result
               (handler-bind ((warning (lambda (c)
                                         (declare (ignore c))
                                         (return-from declaration-result :warned))))
                 (eval '(let ((x 1)) (declare (special x)) x))))"
        ),
        ["1"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(multiple-value-bind (fn warnings-p failure-p)
                 (compile nil '(lambda (x) (+ x 1)))
               (values (funcall fn 41) warnings-p failure-p))"
        ),
        ["42", "NIL", "NIL"]
    );
    assert_compilation_outcomes_after_warnings(&runtime);
}

fn assert_compilation_outcomes_after_warnings(runtime: &Runtime) {
    // Named COMPILE catches warning via HANDLER-BIND, sets warnings-p & failure-p,
    // writes warning to *ERROR-OUTPUT*, and installs function on normal return.
    let res = eval_reprs(
        runtime,
        "(let ((seen nil)
               (err (make-string-output-stream)))
           (let ((*error-output* err))
             (multiple-value-bind (name warnings-p failure-p)
                 (handler-bind ((warning (lambda (c) (push (typep c 'warning) seen))))
                   (compile 'cw-warn-fn '(lambda (x) (declare (bogus-decl x)) (+ x 1))))
               (values name
                       warnings-p
                       failure-p
                       seen
                       (cw-warn-fn 41)
                       (get-output-stream-string err)))))",
    );
    assert_eq!(&res[..5], ["CW-WARN-FN", "T", "T", "(T)", "42"]);
    assert!(res[5].contains("Warning:"), "got {}", res[5]);

    // Non-local transfer from handler aborts installation.
    assert_eq!(
        eval_reprs(
            runtime,
            "(block abort
               (handler-bind ((warning (lambda (c)
                                         (declare (ignore c))
                                         (return-from abort (fboundp 'cw-aborted-fn)))))
                 (compile 'cw-aborted-fn '(lambda (x) (declare (bogus-decl x)) x))))"
        ),
        ["NIL"]
    );

    // COMPILE-FILE signals warnings inside *COMPILE-FILE-PATHNAME* scope and returns (values truename T T),
    // and unwind restores *COMPILE-FILE-PATHNAME* / *COMPILE-FILE-TRUENAME*.
    let dir = TempDir::new("compile-warnings");
    let src = dir.write(
        "warn.lisp",
        "(defun cf-ok (x) (declare (bogus-cf x)) (+ x 2))\n",
    );
    let fasl = dir.path().join("warn.zkclfasl");
    let src = lisp_path(&src);
    let fasl = lisp_path(&fasl);
    let cf_res = eval_reprs(
        runtime,
        &format!(
            "(let ((seen nil)
                   (bound-during-handler nil)
                   (err (make-string-output-stream)))
               (let ((*error-output* err))
                 (multiple-value-bind (out warnings-p failure-p)
                     (handler-bind ((warning (lambda (c)
                                               (push (typep c 'warning) seen)
                                               (setf bound-during-handler
                                                     (and (not (null *compile-file-pathname*))
                                                          (not (null *compile-file-truename*)))))))
                       (compile-file {src} :output-file {fasl} :verbose nil :print nil))
                   (load out :verbose nil :print nil)
                   (values (not (null out))
                           warnings-p
                           failure-p
                           seen
                           bound-during-handler
                           *compile-file-pathname*
                           *compile-file-truename*
                           (cf-ok 40)
                           (get-output-stream-string err)))))"
        ),
    );
    assert_eq!(
        &cf_res[..8],
        ["T", "T", "T", "(T)", "T", "NIL", "NIL", "42"]
    );
    assert!(cf_res[8].contains("Warning:"), "got {}", cf_res[8]);

    // Unwinding out of COMPILE-FILE via HANDLER-BIND restores dynamic compile-file bindings.
    assert_eq!(
        eval_reprs(
            runtime,
            &format!(
                "(values
                   (block escape
                     (handler-bind ((warning (lambda (c)
                                               (declare (ignore c))
                                               (return-from escape :escaped))))
                       (compile-file {src} :output-file {fasl} :verbose nil :print nil)))
                   *compile-file-pathname*
                   *compile-file-truename*)"
            )
        ),
        [":ESCAPED", "NIL", "NIL"]
    );
}
