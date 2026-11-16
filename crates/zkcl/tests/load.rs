use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::lisp_path;

#[test]
fn nested_load_restores_the_callers_dynamic_environment() {
    let temp_dir = TempDir::new("load-path");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defpackage :zk-load-test (:use :cl) (:export :answer))")
        .unwrap();

    let file = temp_dir.write(
        "answers.lisp",
        "(in-package :zk-load-test)
         (progn
           (defmacro forty-two () 42)
           (defun answer () (forty-two)))
         (setq cl-user::*observed-load-path* (namestring *load-pathname*))
         (setq cl-user::*observed-load-truename* (namestring *load-truename*))",
    );
    runtime.load_file(&file).unwrap();
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
    assert_eq!(
        runtime.eval("(zk-load-test:answer)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    for name in ["*observed-load-path*", "*observed-load-truename*"] {
        let values = runtime.eval(name).unwrap();
        let observed = PathBuf::from(runtime.string(&values[0]).unwrap());
        assert_eq!(
            observed.canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
    }
    assert!(runtime.eval("*load-pathname*").unwrap()[0].is_nil());
    assert!(runtime.eval("*load-truename*").unwrap()[0].is_nil());

    #[cfg(unix)]
    {
        let alias = temp_dir.path().join("alias.lisp");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        runtime.load_file(&alias).unwrap();
        for (variable, expected) in [
            ("*observed-load-path*", "alias.lisp"),
            ("*observed-load-truename*", "answers.lisp"),
        ] {
            let result = runtime.eval(variable).unwrap();
            let spelling = runtime.string(&result[0]).unwrap();
            assert_eq!(Path::new(&spelling).file_name().unwrap(), expected);
        }
    }

    let inner = temp_dir.write(
        "inner.lisp",
        "(in-package :cl-user)
         (unwind-protect (throw 'finished (values 17 23))
           (setq *file-cleanup* t))",
    );
    let outer = temp_dir.write(
        "outer.lisp",
        format!("(in-package :zk-load-test)\n(load {})", lisp_path(&inner)),
    );
    let result = runtime
        .eval(&format!("(catch 'finished (load {}))", lisp_path(&outer)))
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].as_fixnum(), Some(17));
    assert_eq!(result[1].as_fixnum(), Some(23));
    assert!(!runtime.eval("*file-cleanup*").unwrap()[0].is_nil());
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
    assert!(runtime.eval("*load-truename*").unwrap()[0].is_nil());

    let unfinished = "(in-package :zk-load-test)\n(list";
    let incomplete = temp_dir.write("incomplete.lisp", unfinished);
    let result = runtime
        .eval(&format!(
            "(handler-case (load {}) (end-of-file () 81))",
            lisp_path(&incomplete)
        ))
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(81));
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
    let error = runtime.load_file(&incomplete).unwrap_err();
    assert_eq!(error.kind(), EvalErrorKind::Read);
    assert_eq!(error.offset(), unfinished.len());

    let missing = temp_dir.path().join("missing.lisp");
    let result = runtime
        .eval(&format!(
            "(load {} :if-does-not-exist nil)",
            lisp_path(&missing)
        ))
        .unwrap();
    assert!(result[0].is_nil());
    let result = runtime
        .eval(&format!(
            "(handler-case (load {}) (file-error (c) (pathnamep (file-error-pathname c))))",
            lisp_path(&missing)
        ))
        .unwrap();
    assert!(!result[0].is_nil());
    let error = runtime.load_file(&missing).unwrap_err();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
    assert_eq!(
        error
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound
    );

    let printed = temp_dir.write("printed.lisp", "(values 11 42)");
    let output = runtime
        .eval(&format!(
            "(let ((*standard-output* (make-string-output-stream)))
               (let ((file (compile-file {} :verbose t :print t)))
                 (load file :verbose t :print t))
               (get-output-stream-string *standard-output*))",
            lisp_path(&printed)
        ))
        .unwrap();
    let output = runtime.string(&output[0]).unwrap();
    assert!(output.contains("; Compiling "));
    assert!(output.contains("; Compiling form at byte 0"));
    assert!(output.contains("; Loading "));
    assert!(output.ends_with("11\n42\n"));
}

#[test]
fn load_reads_the_next_form_after_the_previous_form_returns() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defvar *load-input*)").unwrap();
    let values = runtime
        .eval(
            r#"(let ((s (make-string-input-stream "λ (throw :done 17) 42" 2)))
                   (values (catch :done (load s)) (read s)))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(17));
    assert_eq!(values[1].as_fixnum(), Some(42));

    let values = runtime
        .eval(
            r#"(let ((*load-input* (make-concatenated-stream
                               (make-string-input-stream "(throw :done (read ")
                               (make-string-input-stream "*load-input*)) (:payload) 42"))))
             (values (catch :done (load *load-input*))
                     (read *load-input*)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(:PAYLOAD)");
    assert_eq!(values[1].as_fixnum(), Some(42));

    let values = runtime.eval(
        r#"(let* ((*load-input* (make-string-input-stream
                     "(setq *load-input* (make-string-input-stream \"(throw :done 23)\")) (error \"old stream\")"))
                  (synonym (make-synonym-stream '*load-input*)))
             (catch :done (load synonym)))"#,
    ).unwrap();
    assert_eq!(values[0].as_fixnum(), Some(23));

    let values = runtime
        .eval(
            r#"(let ((s (make-string-input-stream "(")))
             (handler-case (load s)
               (end-of-file (condition) (eq s (stream-error-stream condition)))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn load_consumes_an_open_file_stream_from_its_current_position() {
    let temp_dir = TempDir::new("load-stream");
    let runtime = Runtime::start().unwrap();
    let source = temp_dir.write("source.lisp",
        "(error \"already read\") (throw :names (list (namestring *load-pathname*) (namestring *load-truename*))) 42");
    let values = runtime
        .eval(&format!(
            "(with-open-file (s {}) (read s)
           (let ((names (catch :names (load s))))
             (values (first names) (second names) (read s))))",
            lisp_path(&source)
        ))
        .unwrap();
    assert_eq!(values.len(), 3);
    assert_eq!(Path::new(&runtime.string(&values[0]).unwrap()), source);
    assert_eq!(
        Path::new(&runtime.string(&values[1]).unwrap()),
        source.canonicalize().unwrap()
    );
    assert_eq!(values[2].as_fixnum(), Some(42));

    let invalid_tail = temp_dir.path().join("invalid-tail.lisp");
    fs::write(&invalid_tail, b"(throw :done 51)\xff").unwrap();
    let values = runtime
        .eval(&format!(
            "(with-open-file (s {})
               (values (catch :done (load s))
                       (handler-case (load s)
                         (end-of-file () nil)
                         (stream-error (c) (eq (stream-error-stream c) s)))))",
            lisp_path(&invalid_tail)
        ))
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(51));
    assert!(!values[1].is_nil());

    let source = temp_dir.write("answer.lisp", "(defparameter *octet-stream-answer* 73)");
    let compiled = runtime.compile_file(&source, None).unwrap();
    let mut bytes = vec![0x7f];
    bytes.extend(fs::read(compiled).unwrap());
    let binary = temp_dir.path().join("prefixed.bin");
    fs::write(&binary, bytes).unwrap();
    let values = runtime
        .eval(&format!(
            "(with-open-file (s {} :element-type '(unsigned-byte 8))
           (values (read-byte s) (load s) *octet-stream-answer* (read-byte s nil :end)))",
            lisp_path(&binary)
        ))
        .unwrap();
    assert_eq!(values.len(), 4);
    assert_eq!(values[0].as_fixnum(), Some(0x7f));
    assert!(!values[1].is_nil());
    assert_eq!(values[2].as_fixnum(), Some(73));
    assert_eq!(runtime.repr(&values[3]).unwrap(), ":END");
}

#[test]
fn each_loaded_form_controls_how_the_next_form_is_read() {
    let temp_dir = TempDir::new("load-position");
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defvar *file-load-stream*)").unwrap();
    let prefix = r#"(set-syntax-from-char #\! #\Space)
!(file-position *file-load-stream* OFFSET00)
(error "LOAD evaluated the form before the new file position")
"#;
    let source = temp_dir.write(
        "seek.lisp",
        format!(
            "{}(throw :done (read *file-load-stream*)) 73 42",
            prefix.replace("OFFSET00", &format!("{:08}", prefix.len()))
        ),
    );
    runtime
        .eval(&format!(
            r#"(with-open-file (in {})
                 (let ((*readtable* (copy-readtable nil))
                       (*file-load-stream* in))
                   (assert (= (catch :done (load in)) 73))
                   (assert (= (read in) 42))
                   (assert (eq (read in nil :end) :end))))"#,
            lisp_path(&source)
        ))
        .unwrap();

    let source = temp_dir.write(
        "close.lisp",
        r#"(close *file-load-stream*) (error "LOAD read from a closed file")"#,
    );
    runtime
        .eval(&format!(
            r#"(with-open-file (in {})
                 (let ((*file-load-stream* in))
                   (assert (handler-case (progn (load in) nil)
                             (stream-error (condition)
                               (eq (stream-error-stream condition) in))))))"#,
            lisp_path(&source)
        ))
        .unwrap();
}

#[test]
fn extensionless_load_binds_load_truename_to_the_file_it_selects() {
    use std::time::{Duration, SystemTime};
    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("load-file-types");
    let input = dir.write(
        "entry.lisp",
        "(defparameter *selected* (list :compiled (pathname-type *load-truename*)))",
    );
    let compiled = rt.compile_file(&input, None).unwrap();
    let bare = dir.path().join("entry");
    let load = format!("(load {})", lisp_path(&bare));
    fs::write(
        &input,
        "(defparameter *selected* (list :source (pathname-type *load-truename*)))",
    )
    .unwrap();
    let stamp = |file: &Path, secs| {
        fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap()
    };
    stamp(&input, 10);
    stamp(&compiled, 10);
    rt.eval(&load).unwrap();
    rt.eval(r#"(assert (equal *selected* '(:compiled "zkclfasl")))"#)
        .unwrap();
    stamp(&input, 20);
    rt.eval(&load).unwrap();
    rt.eval(r#"(assert (equal *selected* '(:source "lisp")))"#)
        .unwrap();
    fs::write(&bare, "(setq *selected* :exact)").unwrap();
    rt.eval(&load).unwrap();
    rt.eval("(assert (eq *selected* :exact))").unwrap();
}
