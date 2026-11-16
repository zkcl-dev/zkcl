use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Files(PathBuf);

impl Files {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("zkcl-fasl-process-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        Self(directory)
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

fn lisp_path(path: &Path) -> String {
    let path = path
        .to_str()
        .unwrap()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{path}\"")
}

fn repl(source: &str) -> Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{source}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn fasl_loads_in_fresh_process_after_source_removal() {
    let files = Files::new();
    let source = files.path("compile source.lisp");
    let compiled_file = files.path("compiled code.custom");
    fs::write(
        &source,
        r#"(defpackage :zk-fasl-process
             (:use :cl)
             (:export :answer :answer-value :add-two :make-adder :load-path
                      :structure-answer :setter-answer :condition-answer))
           (in-package :zk-fasl-process)
           (eval-when (:compile-toplevel)
             (defmacro compile-only-offset () 40))
           (defmacro add-two (value) (list '+ value 2))
           (defun make-adder (delta) (lambda (n) (+ n delta)))
           (defun answer () (add-two (compile-only-offset)))
           (defstruct answer-record (value 40))
           (defun structure-answer ()
             (let ((record (make-answer-record)))
               (incf (answer-record-value record) 2)))
           (defun setter-answer ()
             (let ((cell (list 40)))
               (setf (cell-value cell) 41)
               (funcall #'(setf cell-value) 42 cell)
               (car cell)))
           (defun (setf cell-value) (new cell)
             (rplaca cell new)
             new)
           (defun report-answer-problem (condition output)
             (format output "problem ~D" (answer-problem-answer condition)))
           (define-condition answer-problem (error)
             ((answer :initarg :answer :reader answer-problem-answer))
             (:report report-answer-problem))
           (defun condition-answer ()
             (handler-case (error 'answer-problem :answer 42)
               (answer-problem (condition)
                 (assert (string= (princ-to-string condition) "problem 42"))
                 (answer-problem-answer condition))))
           (define-symbol-macro answer-value (answer))
           (defparameter load-path (namestring *load-truename*))"#,
    )
    .unwrap();
    let source_name = lisp_path(&source);
    let compiled_file_name = lisp_path(&compiled_file);
    let compiled = repl(&format!(
        "(multiple-value-bind (output warnings failure)
             (compile-file {source_name} :output-file {compiled_file_name} :verbose nil :print nil)
           (values (pathnamep output) warnings failure (string= (package-name *package*) \"COMMON-LISP-USER\")))"
    ));
    assert_eq!(compiled.stdout, b"T\nNIL\nNIL\nT\n");
    assert!(
        fs::read(&compiled_file)
            .unwrap()
            .starts_with(b"\x89ZKCLFASL")
    );
    let cli_compiled = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("compile")
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        cli_compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_compiled.stderr)
    );
    let default_output = source.with_extension("zkclfasl").canonicalize().unwrap();
    assert_eq!(
        String::from_utf8(cli_compiled.stdout).unwrap().trim_end(),
        default_output.to_str().unwrap()
    );
    fs::remove_file(source).unwrap();
    let compiled_file = compiled_file.canonicalize().unwrap();
    let compiled_file_name = lisp_path(&compiled_file);

    let loaded = repl(&format!(
        "(values (find-package :zk-fasl-process))
         (let ((*macroexpand-hook* (lambda (&rest arguments) (error \"expanded during compiled load\"))))
           (load {compiled_file_name} :external-format :irrelevant-for-compiled-data :verbose nil :print nil))
         (values (zk-fasl-process:answer)
                 zk-fasl-process:answer-value
                 (zk-fasl-process:add-two 40)
                 (compiled-function-p #'zk-fasl-process:answer)
                 (funcall (zk-fasl-process:make-adder 7) 35)
                 (zk-fasl-process:structure-answer)
                 (zk-fasl-process:setter-answer)
                 (zk-fasl-process:condition-answer)
                 (macro-function 'zk-fasl-process::compile-only-offset)
                 (string= zk-fasl-process:load-path {compiled_file_name})
                 (string= (package-name *package*) \"COMMON-LISP-USER\"))"
    ));
    assert_eq!(
        loaded.stdout,
        b"NIL\nT\n42\n42\n42\nT\n42\n42\n42\n42\nNIL\nT\nT\n"
    );

    let mut run = Command::new(env!("CARGO_BIN_EXE_zkcl"));
    let output = run.arg("run").arg(&compiled_file).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .arg("run")
            .arg(default_output)
            .status()
            .unwrap()
            .success()
    );
}
