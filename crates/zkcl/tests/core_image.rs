use std::path::PathBuf;
use std::process::Command;
use zkcl::{NamedImageHooks, Runtime};

const MODE: &str = "ZKCL_CORE_IMAGE_TEST_MODE";
const PATH: &str = "ZKCL_CORE_IMAGE_TEST_PATH";

#[test]
fn complete_core_image_roundtrip_uses_independent_processes() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/image")
        .join(format!("roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("state.core");
    for mode in [
        "save",
        "restore",
        "resources",
        "restore-resources",
        "corrupt",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "core_image_child", "--nocapture"])
            .env(MODE, mode)
            .env(PATH, &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn core_image_child() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let path = PathBuf::from(std::env::var_os(PATH).unwrap());
    let mut runtime = Runtime::start().unwrap();
    match mode.as_str() {
        "save" => {
            runtime.load_str(SETUP).unwrap();
            runtime.collect();
            runtime.save_image(&path).unwrap();
        }
        "restore" => {
            runtime.restore_image(&path).unwrap();
            runtime.collect();
            runtime.load_str(VERIFY).unwrap();
            runtime.load_str("(defun image-inline (x) (+ x 100)) (defun image-new-call (x) (image-inline x)) (assert (= (image-new-call 1) 101))").unwrap();
        }
        "resources" => {
            let active = runtime.start_eval("(loop)").unwrap();
            assert!(
                runtime
                    .save_image(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("active")
            );
            drop(active);
            let file = path.with_extension("open");
            runtime.load_str(&format!("(defparameter *open-image-file* (open {:?} :direction :output :if-exists :supersede))",file.to_str().unwrap())).unwrap();
            let error = runtime.save_image(&path).unwrap_err().to_string();
            assert!(
                error.contains("external") && error.contains("hook"),
                "{error}"
            );
            runtime.load_str("(close *open-image-file*)").unwrap();
            runtime
                .register_function("image-host-function", |rt, _| {
                    Ok(vec![rt.integer_from(42).unwrap()])
                })
                .unwrap();
            let error = runtime.save_image(&path).unwrap_err().to_string();
            assert!(error.contains("external"), "{error}");
            runtime
                .save_image_with_hooks(
                    &path,
                    &NamedImageHooks::new(&[("COMMON-LISP-USER", "IMAGE-HOST-FUNCTION")]),
                )
                .unwrap();
            std::fs::remove_file(file).unwrap();
        }
        "restore-resources" => {
            assert!(
                runtime
                    .restore_image(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("restore hook")
            );
            runtime
                .register_function("image-host-function", |rt, _| {
                    Ok(vec![rt.integer_from(43).unwrap()])
                })
                .unwrap();
            runtime
                .restore_image_with_hooks(
                    &path,
                    &NamedImageHooks::new(&[("COMMON-LISP-USER", "IMAGE-HOST-FUNCTION")]),
                )
                .unwrap();
            runtime.load_str("(assert (= (image-host-function) 43)) (assert (not (open-stream-p *open-image-file*)))").unwrap();
        }
        "corrupt" => {
            let bytes = std::fs::read(&path).unwrap();
            let corrupt = path.with_extension("bad-length");
            let mut wrong_length = bytes.clone();
            wrong_length[10..18].copy_from_slice(&u64::MAX.to_le_bytes());
            std::fs::write(&corrupt, &wrong_length).unwrap();
            assert!(runtime.restore_image(&corrupt).is_err());
            std::fs::remove_file(corrupt).unwrap();
            for len in [0, 1, 9, 16, bytes.len() - 1] {
                let corrupt = path.with_extension("corrupt");
                std::fs::write(&corrupt, &bytes[..len]).unwrap();
                assert!(runtime.restore_image(&corrupt).is_err());
                runtime.load_str("(assert (= (+ 20 22) 42))").unwrap();
                std::fs::remove_file(corrupt).unwrap();
            }
        }
        _ => unreachable!(),
    }
}

const SETUP: &str = r#"
    (defpackage "IMAGE-P" (:use "CL") (:nicknames "IMAGE-N") (:shadow "CAR") (:export "SHARED"))
    (defpackage "IMAGE-Q" (:use "CL") (:import-from "IMAGE-P" "SHARED"))
    (setf (documentation (find-package "IMAGE-P") t) "package documentation")
    (defparameter image-p:shared (list :shared))
    (defparameter *image-cycle* (list image-p:shared))
    (setf (cdr *image-cycle*) *image-cycle*)
    (setf (get 'image-p:shared :property) *image-cycle*)
    (defparameter *image-gensym* (gensym "IMAGE"))
    (setf (symbol-value *image-gensym*) *image-cycle*)
    (setf (symbol-function *image-gensym*) (lambda () :gensym-function))
    (defparameter *image-closure* nil)
    (defparameter *image-closure-alias* nil)
    (defparameter *image-read-closure* nil)
    (let ((counter 40))
      (setf *image-closure* (lambda () (incf counter))
            *image-closure-alias* *image-closure*
            *image-read-closure* (lambda () counter)))
    (funcall *image-closure*)
    (declaim (inline image-inline))
    (defun image-inline (x) (+ x 1))
    (defun image-old-call (x) (image-inline x))
    (defmacro image-macro (x) `(+ ,x 2))
    (define-symbol-macro image-symbol-macro (car image-p:shared))
    (deftype image-type () '(integer 40 44))
    (defun image-compiler (x) (+ x 100))
    (define-compiler-macro image-compiler (x) `(+ ,x 3))
    (defun image-place (cell) (car cell))
    (defsetf image-place (cell) (value) `(setf (car ,cell) ,value))
    (declaim (special *image-special*))
    (defparameter *image-special* 42 "variable documentation")
    (declaim (type integer *image-special*))
    (defconstant +image-constant+ 44 "constant documentation")
    (defparameter *image-table* (make-hash-table :test 'eq))
    (setf (gethash *image-table* *image-table*) *image-cycle*)
    (defparameter *image-array* (make-array 3 :initial-contents '(10 20 30) :adjustable t))
    (defparameter *image-view* (make-array 2 :displaced-to *image-array* :displaced-index-offset 1))
    (defparameter *image-input* (make-string-input-stream "abc"))
    (read-char *image-input*)
    (defparameter *image-output* (make-string-output-stream))
    (write-string "before" *image-output*)
    (defun image-reader (stream char) (declare (ignore stream char)) (list 'quote :reader))
    (set-macro-character #\! #'image-reader)
"#;

const VERIFY: &str = r#"
    (assert (eq (find-package "IMAGE-N") (find-package "IMAGE-P")))
    (assert (equal (documentation (find-package "IMAGE-P") t) "package documentation"))
    (assert (eq (find-symbol "SHARED" "IMAGE-P") (find-symbol "SHARED" "IMAGE-Q")))
    (assert (eq (nth-value 1 (find-symbol "CAR" "IMAGE-P")) :internal))
    (assert (eq (cdr *image-cycle*) *image-cycle*))
    (assert (eq (car *image-cycle*) image-p:shared))
    (assert (eq (get 'image-p:shared :property) *image-cycle*))
    (assert (null (symbol-package *image-gensym*)))
    (assert (eq (symbol-value *image-gensym*) *image-cycle*))
    (assert (eq (funcall *image-gensym*) :gensym-function))
    (assert (eq *image-closure* *image-closure-alias*))
    (assert (= (funcall *image-read-closure*) 41))
    (assert (= (funcall *image-closure*) 42))
    (assert (= (funcall *image-read-closure*) 42))
    (assert (= (image-old-call 41) 42))
    (assert (= (image-macro 40) 42))
    (assert (typep 42 'image-type))
    (assert (not (typep 45 'image-type)))
    (assert (= (eval '(image-compiler 39)) 42))
    (let ((value (list 0))) (setf (image-place value) 42) (assert (= (car value) 42)))
    (assert (= *image-special* 42))
    (assert (equal (documentation '*image-special* 'variable) "variable documentation"))
    (assert (constantp '+image-constant+))
    (assert (eq (gethash *image-table* *image-table*) *image-cycle*))
    (setf (aref *image-view* 0) 42)
    (assert (= (aref *image-array* 1) 42))
    (assert (char= (read-char *image-input*) #\b))
    (write-string "after" *image-output*)
    (assert (equal (get-output-stream-string *image-output*) "beforeafter"))
    (assert (equal (read-from-string "!") '(quote :reader)))
    (setf image-symbol-macro :changed)
    (assert (eq (car image-p:shared) :changed))
"#;
