use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Files(PathBuf);

impl Files {
    fn new(label: &str) -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let path = workspace
            .join(".agents/tmp/zkcl/js-backend/integration")
            .join(format!("application-{}-{label}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn source_builds_to_an_independent_javascript_application() {
    // Node is a test dependency: a missing engine must fail this integration
    // test, rather than turn an unexecuted backend into a passing result.
    success(Command::new("node").arg("--version").output().unwrap());
    let files = Files::new("standalone");
    let source = files.path("application.lisp");
    let module = files.path("application.mjs");
    fs::write(
        &source,
        r#"(defpackage :js-application
             (:use :cl)
             (:export :answer :make-counter :original :current :numbers
                      :cleanup-values :host-roundtrip :identity :arity
                      :object-answer :structure-answer :target-feature
                      :compile-phase-answer :condition-answer :restart-answer))
           (in-package :js-application)
           (defmacro add-two (form) (list '+ form 2))
           (defun answer () "Return the application answer." (add-two 40))
           (defun target-feature () #+javascript 42 #-javascript :wrong-target)
           (eval-when (:compile-toplevel)
             (defmacro compile-only-answer () 41))
           (defun compile-phase-answer () (1+ (compile-only-answer)))
           (defun make-counter (value)
             (values (lambda (delta) (incf value delta))
                     (lambda () value)))
           (declaim (notinline current))
           (defun current () 11)
           (defparameter original #'current)
           (defun original () (funcall original))
           (defun numbers ()
             (values 9007199254740993 3/7 1.25f0 1.25d0 -0.0d0))
           (defparameter *scope* :outer)
           (defparameter *trace* nil)
           (defun cleanup-values ()
             (setq *trace* nil)
             (multiple-value-bind (a b)
                 (block done
                   (let ((*scope* :inner))
                     (unwind-protect
                         (funcall (lambda () (return-from done (values 41 42))))
                       (push *scope* *trace*))))
               (values a b *trace* *scope*)))
           (defun host-roundtrip (callback value)
             (funcall callback (lambda (delta) (+ value delta)) value))
           (defun identity (value) (values value (eq value value)))
           (defun arity (required &optional (optional 5 supplied) &rest rest)
             (values required optional supplied rest))
           (defclass answer-object ()
             ((value :initarg :value :accessor answer-object-value)))
           (defgeneric object-value (object)
             (:method ((object (eql :quick-answer))) 42))
           (defmethod object-value ((object answer-object))
             (answer-object-value object))
           (defmethod object-value :around ((object answer-object))
             (if (next-method-p) (call-next-method) 0))
           (defun object-answer ()
             (values (object-value (make-instance 'answer-object :value 42))
                     (object-value :quick-answer)))
           (define-condition application-error (type-error)
             ((code :reader application-error-code :initarg :code))
             (:default-initargs :datum :application :expected-type 'string)
             (:report (lambda (condition stream)
                        (format stream "application ~D" (application-error-code condition)))))
           (define-condition recoverable-error (application-error) ())
           (defun condition-answer ()
             (handler-case (error 'recoverable-error :code 42)
               ((or unbound-variable application-error) (condition)
                (values (application-error-code condition)
                        (type-error-datum condition)
                        (princ-to-string condition)))))
           (defun restart-answer ()
             (let ((other (make-condition 'application-error)))
               (handler-bind ((application-error
                               (lambda (condition)
                                 (unless (find-restart 'recover other)
                                   (invoke-restart (find-restart 'recover condition) 42)))))
                 (restart-case (error 'application-error :code 0)
                   (recover (result) result)))))
           (defstruct answer-record (value 40))
           (defun structure-answer ()
             (let ((record (make-answer-record)))
               (incf (answer-record-value record) 2)))"#,
    )
    .unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("build")
            .arg(&source)
            .args(["--target", "js", "-o"])
            .arg(&module)
            .output()
            .unwrap(),
    );
    let javascript = fs::read_to_string(&module).unwrap();
    assert!(javascript.contains("export function instantiate"));
    assert!(javascript.contains("(task, frame) {"));
    assert!(javascript.contains("step: "));
    assert!(
        !javascript
            .lines()
            .any(|line| line.trim_start().starts_with("import ")),
        "the emitted application must contain its runtime"
    );
    assert!(!javascript.contains("WebAssembly"));
    assert!(!javascript.contains("node:child_process"));
    let compiled_file = files.path("application.zkclfasl");
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("compile")
            .arg(&source)
            .args(["--target", "js"])
            .arg(&compiled_file)
            .output()
            .unwrap(),
    );
    let from_compiled_file = files.path("from-compiled-file.mjs");
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("build")
            .arg(&compiled_file)
            .args(["--target", "js", "-o"])
            .arg(&from_compiled_file)
            .output()
            .unwrap(),
    );
    assert_eq!(javascript, fs::read_to_string(from_compiled_file).unwrap());
    fs::remove_file(source).unwrap();
    success(
        Command::new("node")
            .args(["--test", "--test-concurrency=1"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../zkcl-js/tests/language-semantics.test.mjs"),
            )
            .env("ZKCL_JS_APPLICATION", &module)
            .output()
            .unwrap(),
    );
}

#[test]
fn independently_built_modules_share_one_runtime_and_support_async_startup() {
    let files = Files::new("shared");
    for (name, source) in [
        (
            "first",
            "(defun shared-value () 41) (host-await (shared-value))",
        ),
        ("second", "(defun shared-answer () (1+ (shared-value)))"),
    ] {
        let input = files.path(&format!("{name}.lisp"));
        fs::write(&input, source).unwrap();
        success(
            Command::new(env!("CARGO_BIN_EXE_zkcl"))
                .arg("build")
                .arg(&input)
                .args(["--target", "js", "--split-runtime", "-o"])
                .arg(files.path(&format!("{name}.mjs")))
                .output()
                .unwrap(),
        );
    }
    let runtimes = fs::read_dir(&files.0)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("zkcl-runtime-"))
        .collect::<Vec<_>>();
    assert_eq!(runtimes.len(), 1);
    let first = fs::read_to_string(files.path("first.mjs")).unwrap();
    let second = fs::read_to_string(files.path("second.mjs")).unwrap();
    for app in [&first, &second] {
        assert!(app.contains(&format!("\"./{}\"", runtimes[0].to_string_lossy())));
        assert!(!app.contains("const __zkcl_modules ="));
    }
    success(
        Command::new("node")
            .args([
                "--disallow-code-generation-from-strings",
                "--input-type=module",
                "-e",
            ])
            .arg(
                r#"
                import assert from 'node:assert/strict';
                import * as first from './first.mjs';
                import * as second from './second.mjs';
                assert.strictEqual(first.createRuntime, second.createRuntime);
                const options = { configure(rt) {
                  rt.registerFunction('CL-USER::HOST-AWAIT', async ([value]) => value + 1);
                } };
                const execution = { slice: 1 };
                assert.deepEqual((await first.runAsync(options, execution)).values, [42]);
                const runtime = await first.instantiateAsync(options, execution);
                await runtime.loadAsync(second.program, execution);
                assert.deepEqual(runtime.call('CL-USER::SHARED-ANSWER').values, [42]);
            "#,
            )
            .current_dir(&files.0)
            .output()
            .unwrap(),
    );
}
