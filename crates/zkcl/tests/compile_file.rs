use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use zkcl::{EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::{eval, eval_fixnums, lisp_path};

fn compile_file(runtime: &Runtime, input: &Path, options: &str) -> PathBuf {
    let input = lisp_path(input);
    let values = eval(
        runtime,
        &format!("(compile-file {input} :verbose nil :print nil {options})"),
    );
    assert_eq!(values.len(), 3);
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil(), "unexpected compile-file warnings");
    assert!(values[2].is_nil(), "unexpected compile-file failure");
    let namestring = eval(runtime, "#'namestring");
    let pathname = runtime.call(&namestring[0], &[&values[0]]).unwrap();
    PathBuf::from(runtime.string(&pathname[0]).unwrap())
}

#[test]
fn compile_file_preserves_the_invalid_output_pathname_cause() {
    let dir = TempDir::new("compile-invalid-output");
    let input = dir.write("source.lisp", "(defun compiled-value () 42)");
    let output = dir.path().join("*.zkclfasl");
    let runtime = Runtime::start().unwrap();
    let form = format!(
        "(compile-file {} :output-file {} :verbose nil)",
        lisp_path(&input),
        lisp_path(&output)
    );
    let Err(error) = runtime.eval(&form) else {
        panic!("compile-file accepted a wild output pathname");
    };
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(zkcl::ConditionClass::FileError)
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        "expected a concrete pathname rather than a wild pathname"
    );
}

#[test]
fn long_method_argument_forms_survive_loading_in_another_runtime() {
    let dir = TempDir::new("method-argument-fasl");
    let input = dir.write(
        "argument-methods.lisp",
        r#"
        (define-method-combination argument-list () ((methods ()))
          (:arguments &whole all arg &optional (value arg supplied-p))
          `(list ,all ,arg ,value ,supplied-p (call-method ,(first methods))))
        (defgeneric compiled-method (arg &optional value) (:method-combination argument-list))
        (defmethod compiled-method (arg &optional value) (cons arg value))
        "#,
    );
    let output = compile_file(&Runtime::start().unwrap(), &input, "");
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&output).unwrap();
    runtime
        .eval(
            "(progn (assert (equal (compiled-method 11) '((11) 11 11 nil (11))))
                    (assert (equal (compiled-method 11 nil) '((11 nil) 11 nil t (11)))))",
        )
        .unwrap();
}

#[test]
fn cross_compilation_uses_one_target_for_reader_macroexpansion_and_loading() {
    let dir = TempDir::new("target-reader");
    let target = "i686-pc-windows-gnu"
        .parse::<zkcl::CompilationTarget>()
        .unwrap();
    let input = dir.write(
        "target.lisp",
        r#"
        (eval-when (:compile-toplevel)
          (assert (member :windows *features*))
          (assert (equal (machine-type) "x86"))
          (assert (equal (software-type) "windows")))
        (defmacro target-word-limit ()
          (if (and (member :32-bit *features*)
                   (typep (1+ most-positive-fixnum) 'bignum)
                   (subtypep '(integer 268435456 268435456) 'bignum))
              most-positive-fixnum
              0))
        (defun compiled-target ()
          (list #+(and windows x86 32-bit) :windows-x86
                #+(or darwin unix x86-64 arm64) :wrong-platform
                (target-word-limit)
                #.(typep 268435456 'fixnum)
                '#.(type-of -268435457)))
    "#,
    );
    let output = {
        let runtime = Runtime::builder()
            .compilation_target(target.clone())
            .start()
            .unwrap();
        runtime.compile_file(&input, None).unwrap()
    };
    fs::remove_file(input).unwrap();
    let runtime = Runtime::builder()
        .compilation_target(target)
        .start()
        .unwrap();
    runtime.load_file(&output).unwrap();
    let result = eval(&runtime, "(compiled-target)");
    assert_eq!(
        runtime.repr(&result[0]).unwrap(),
        "(:WINDOWS-X86 268435455 NIL BIGNUM)"
    );
}

#[test]
fn source_free_fasl_keeps_its_reader_target_and_target_integer_layout() {
    use zkcl_bytecode::{CompiledFile, LiteralNode};

    let temp_dir = TempDir::new("target-fasl");
    let source = temp_dir.write(
        "width.lisp",
        r#"
        (defun fasl-width () #+32-bit 32 #-32-bit 64)
        (defun beyond-fixnum () (values 268435456 '(268435456 -268435457)))
    "#,
    );
    let target = "i686-unknown-linux-gnu".parse().unwrap();
    let runtime = Runtime::builder()
        .compilation_target(target)
        .start()
        .unwrap();
    let output = compile_file(&runtime, &source, "");
    fs::remove_file(source).unwrap();
    let bytes = fs::read(&output).unwrap();
    let file = CompiledFile::decode(&bytes).unwrap();
    assert_eq!(&file.target, runtime.compilation_target());
    assert!(file.nodes.contains(&LiteralNode::Bignum {
        negative: false,
        magnitude: vec![0, 0, 0, 16],
    }));
    assert!(file.nodes.contains(&LiteralNode::Bignum {
        negative: true,
        magnitude: vec![1, 0, 0, 16],
    }));
    runtime.load_file(&output).unwrap();
    assert_eq!(eval_fixnums(&runtime, "(fasl-width)"), [32]);
    assert_eq!(
        common::lisp::eval_reprs(&runtime, "(beyond-fixnum)"),
        ["268435456", "(268435456 -268435457)"],
    );
    let other = Runtime::builder()
        .compilation_target("x86_64-unknown-linux-gnu".parse().unwrap())
        .start()
        .unwrap();
    let error = other.load_file(&output).unwrap_err().to_string();
    assert!(error.contains("i686-unknown-linux-gnu"), "{error}");
    assert!(error.contains("x86_64-unknown-linux-gnu"), "{error}");
    assert!(eval(&other, "(fboundp 'fasl-width)")[0].is_nil());
}

#[test]
fn file_macroexpansion_can_transfer_control_to_its_caller() {
    let temp_dir = TempDir::new("compile-file");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (define-condition cf-expansion-notice (condition) ())
            (define-condition cf-expansion-error (error) ())
            (defparameter *cf-notice* (make-condition 'cf-expansion-notice))
            (defparameter *cf-error* (make-condition 'cf-expansion-error))
            (defparameter *cf-cleanups* nil)
            (defmacro cf-notify () (signal *cf-notice*) '(defparameter *cf-value* 41))
            (defun cf-compiler-notify (value) (1+ value))
            (define-compiler-macro cf-compiler-notify (value)
              (signal *cf-notice*)
              `(1+ ,value))
            (defmacro cf-fail () (error *cf-error*))
            (defmacro cf-continue ()
              (cerror "Continue file compilation." *cf-error*)
              '(defparameter *cf-recovered* 42))
            (defmacro cf-throw ()
              (unwind-protect (throw 'cf-done (values 53 54))
                (push (pathname-name *compile-file-truename*) *cf-cleanups*)))
            "#,
        )
        .unwrap();

    let input = temp_dir.write(
        "macro-control.lisp",
        "(eval-when (:compile-toplevel) (signal *cf-notice*))\n\
         (cf-notify)\n\
         (defun cf-file-result () (cf-compiler-notify *cf-value*))",
    );
    let result = eval_fixnums(
        &runtime,
        &format!(
            r#"(let ((seen 0))
                  (handler-bind ((cf-expansion-notice
                                   (lambda (c)
                                     (assert (eq c *cf-notice*))
                                     (assert (equal (pathname-name *compile-file-pathname*) "macro-control"))
                                     (assert (equal (pathname-name *compile-file-truename*) "macro-control"))
                                     (incf seen))))
                    (load (compile-file {} :verbose nil :print nil)))
                  (assert (null *compile-file-pathname*))
                  (assert (null *compile-file-truename*))
                  (values seen (cf-file-result)))"#,
            lisp_path(&input)
        ),
    );
    assert_eq!(result, vec![3, 42]);

    let failing = temp_dir.write("macro-fail.lisp", "(cf-fail)");
    let result = eval(
        &runtime,
        &format!(
            "(handler-case (compile-file {} :verbose nil)\n\
               (cf-expansion-error (c) (eq c *cf-error*)))",
            lisp_path(&failing)
        ),
    );
    assert_eq!(runtime.repr(&result[0]).unwrap(), "T");
    assert!(!failing.with_extension("zkclfasl").exists());

    let error = runtime.compile_file(&failing, None).unwrap_err();
    assert_eq!(error.kind(), EvalErrorKind::MacroExpansion);
    let document = error.source_document().expect("macro call source");
    assert_eq!(
        document.path(),
        Some(failing.canonicalize().unwrap().as_path())
    );
    let range = error.source_range().expect("macro call range");
    assert_eq!(document.text().unwrap().get(range), Some("(cf-fail)"));

    let continuing = temp_dir.write("macro-continue.lisp", "(cf-continue)");
    let result = eval_fixnums(
        &runtime,
        &format!(
            "(handler-bind ((cf-expansion-error #'continue))\n\
               (load (compile-file {} :verbose nil))\n\
               *cf-recovered*)",
            lisp_path(&continuing)
        ),
    );
    assert_eq!(result, vec![42]);

    let throwing = temp_dir.write("macro-throw.lisp", "(cf-throw)");
    let result = eval(
        &runtime,
        &format!(
            r#"(let ((*cf-cleanups* nil))
                  (list (multiple-value-list
                         (catch 'cf-done
                           (unwind-protect (compile-file {} :verbose nil)
                             (push (if *compile-file-truename* :bound :restored) *cf-cleanups*))))
                        *cf-cleanups*))"#,
            lisp_path(&throwing)
        ),
    );
    assert_eq!(
        runtime.repr(&result[0]).unwrap(),
        "((53 54) (:RESTORED \"macro-throw\"))"
    );
    assert!(!throwing.with_extension("zkclfasl").exists());
}

#[test]
fn eval_when_selects_the_compilation_phase() {
    let temp_dir = TempDir::new("compile-file");
    let runtime = Runtime::start().unwrap();
    const COUNTERS: &[&str] = &[
        "*cf-plain*",
        "*cf-compile*",
        "*cf-load*",
        "*cf-execute*",
        "*cf-both*",
        "*cf-ctt-execute*",
        "*cf-ctt-load*",
        "*cf-ctt-compile*",
        "*cf-ctt-both*",
        "*cf-nct-both*",
        "*cf-nct-execute*",
        "*cf-nct-compile*",
        "*cf-macro-mode*",
        "*cf-symbol-mode*",
    ];
    for counter in COUNTERS {
        eval(&runtime, &format!("(defparameter {counter} 0)"));
    }
    runtime
        .load_str(
            "(defparameter *cf-compile-path* nil)
                   (defparameter *cf-compile-true* nil)
                   (defparameter *cf-load-true* nil)
                   (defparameter *cf-init-count* 0)
                   (defun cf-read-global () (symbol-value '*cf-local-special*))",
        )
        .unwrap();
    let phases = format!("(values {})", COUNTERS.join(" "));
    let source = temp_dir.write(
        "phases.lisp",
        r#"(incf *cf-plain*)
           (eval-when (:compile-toplevel)
             (incf *cf-compile*)
             (setq *cf-compile-path* (namestring *compile-file-pathname*)
                   *cf-compile-true* (namestring *compile-file-truename*)))
           (eval-when (:load-toplevel) (incf *cf-load*))
           (eval-when (:execute) (incf *cf-execute*))
           (eval-when (:compile-toplevel :load-toplevel)
             (incf *cf-both*)
             (eval-when (:execute) (incf *cf-ctt-execute*))
             (eval-when (:load-toplevel) (incf *cf-ctt-load*))
             (eval-when (:compile-toplevel) (incf *cf-ctt-compile*))
             (eval-when (:load-toplevel :execute) (incf *cf-ctt-both*)))
           (eval-when (:load-toplevel :execute)
             (incf *cf-nct-both*)
             (eval-when (:execute) (incf *cf-nct-execute*))
             (eval-when (:compile-toplevel) (incf *cf-nct-compile*)))
           (locally
             (declare (special *cf-macro-mode*))
             (macrolet ((advance () '(eval-when (:compile-toplevel :load-toplevel)
                                        (incf *cf-macro-mode* 3))))
               (advance)))
           (symbol-macrolet ((place *cf-symbol-mode*))
             (eval-when (:compile-toplevel :load-toplevel) (incf place 5)))
           (defun cf-non-top-level ()
             (values (eval-when (:execute) 42) (eval-when (:load-toplevel) (error "wrong phase"))))
           (defvar *cf-local-special* (incf *cf-init-count*))
           (defun cf-local-call () (let ((*cf-local-special* 42)) (cf-read-global)))
           (setq *cf-load-true* (namestring *load-truename*))"#,
    );
    let output = compile_file(&runtime, &source, "");
    assert_eq!(
        output,
        source.with_extension("zkclfasl").canonicalize().unwrap()
    );
    assert_eq!(
        eval_fixnums(&runtime, &phases),
        [0, 1, 0, 0, 1, 1, 0, 1, 1, 0, 0, 1, 3, 5]
    );
    for binding in ["*cf-compile-path*", "*cf-compile-true*"] {
        let value = eval(&runtime, binding);
        let observed = PathBuf::from(runtime.string(&value[0]).unwrap());
        assert_eq!(
            observed.canonicalize().unwrap(),
            source.canonicalize().unwrap()
        );
    }
    assert!(eval(&runtime, "(fboundp 'cf-non-top-level)")[0].is_nil());
    assert!(eval(&runtime, "(boundp '*cf-local-special*)")[0].is_nil());
    assert_eq!(eval_fixnums(&runtime, "*cf-init-count*"), [0]);
    assert!(eval(&runtime, "*compile-file-pathname*")[0].is_nil());
    assert!(eval(&runtime, "*compile-file-truename*")[0].is_nil());

    runtime.load_file(&output).unwrap();
    assert_eq!(
        eval_fixnums(&runtime, &phases),
        [1, 1, 1, 0, 2, 1, 1, 1, 2, 1, 0, 1, 6, 10]
    );
    let result = eval(&runtime, "(cf-non-top-level)");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].as_fixnum(), Some(42));
    assert!(result[1].is_nil());
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(values (cf-local-call) *cf-local-special* *cf-init-count*)"
        ),
        [42, 1, 1]
    );
    let load_truename = eval(&runtime, "*cf-load-true*");
    assert_eq!(
        Path::new(&runtime.string(&load_truename[0]).unwrap()),
        output
    );
    assert!(eval(&runtime, "*load-truename*")[0].is_nil());
}

#[test]
fn file_compilation_honors_local_readtable_changes() {
    let temp_dir = TempDir::new("compile-file");
    let runtime = Runtime::start().unwrap();
    let source = temp_dir.write(
        "reader-state.lisp",
        r#"(eval-when (:compile-toplevel)
             (setq *readtable* (copy-readtable nil))
             (set-macro-character #\?
               (lambda (stream character)
                 (declare (ignore stream character))
                 (set-macro-character #\!
                   (lambda (stream character)
                     (declare (ignore stream character))
                     17))
                 (values))))
           (defun cf-readtable-value ()
             (+ ? !
                #.(progn (setq *readtable* (copy-readtable nil))
                         (set-macro-character #\@
                           (lambda (stream character)
                             (declare (ignore stream character))
                             25))
                         (values))
                @))"#,
    );
    let lisp_output = compile_file(&runtime, &source, "");
    let host_output = runtime
        .compile_file(
            &source,
            Some(&temp_dir.path().join("reader-state-host.zkclfasl")),
        )
        .unwrap();
    for output in [lisp_output, host_output] {
        runtime.load_file(&output).unwrap();
        assert_eq!(eval_fixnums(&runtime, "(cf-readtable-value)"), [42]);
    }
    assert!(eval(&runtime, r"(get-macro-character #\@)")[0].is_nil());
}

#[test]
fn fasl_loading_preserves_literal_graph_identity() {
    let temp_dir = TempDir::new("compile-file");
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("cf-collect", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    let source = temp_dir.write(
        "literals.lisp",
        r#"(eval-when (:compile-toplevel)
             (defparameter *cf-shared-source* (cons 42 (make-list 530)))
             (defparameter *cf-identity-source* (make-symbol "SAME"))
             (defparameter *cf-circular-source* (list 17))
             (rplacd *cf-circular-source* *cf-circular-source*)
             (defmacro cf-literal ()
               (cf-collect)
               (list 'quote *cf-shared-source*))
             (defmacro cf-identity () (list 'quote *cf-identity-source*))
             (defmacro cf-circular () (list 'quote *cf-circular-source*)))
           (defparameter *cf-shared-loaded* (cf-literal))
           (defparameter *cf-identity-loaded* (cf-identity))
           (eval-when (:load-toplevel) (cf-collect))
           (defparameter *cf-original-name* 'cf-reused-name)
           (eval-when (:compile-toplevel) (unintern 'cf-reused-name))
           (defparameter *cf-replacement-name* 'cf-reused-name)
           (defun cf-read-literals ()
             (values (cf-literal) (lambda () (cf-literal))
                     (cf-identity) '#:same (cf-circular)))"#,
    );
    let compiled_file = compile_file(&runtime, &source, "");
    fs::remove_file(source).unwrap();
    runtime.collect();
    runtime.load_file(&compiled_file).unwrap();
    runtime.collect();
    let values = eval(&runtime, "(cf-read-literals)");
    assert_eq!(values.len(), 5);
    let literal = &values[0];
    assert_eq!(
        runtime.cons_fields(literal).unwrap().0.as_fixnum(),
        Some(42)
    );
    assert!(literal.same_identity(&runtime.call(&values[1], &[]).unwrap()[0]));
    assert!(literal.same_identity(&eval(&runtime, "*cf-shared-loaded*")[0]));
    assert_eq!(eval_fixnums(&runtime, "(length *cf-shared-loaded*)"), [531]);
    assert_eq!(runtime.symbol_name(&values[2]).as_deref(), Some("SAME"));
    assert_eq!(runtime.symbol_name(&values[3]).as_deref(), Some("SAME"));
    assert!(values[2].same_identity(&eval(&runtime, "*cf-identity-loaded*")[0]));
    assert!(!values[2].same_identity(&values[3]));
    let (head, rest) = runtime.cons_fields(&values[4]).unwrap();
    assert_eq!(head.as_fixnum(), Some(17));
    assert!(rest.same_identity(&values[4]));

    let symbols = eval(
        &runtime,
        "(values *cf-original-name* *cf-replacement-name*)",
    );
    assert_eq!(
        runtime.symbol_name(&symbols[0]).as_deref(),
        Some("CF-REUSED-NAME")
    );
    assert_eq!(
        runtime.symbol_name(&symbols[1]).as_deref(),
        Some("CF-REUSED-NAME")
    );
    assert!(!symbols[0].same_identity(&symbols[1]));
    let homes = eval(
        &runtime,
        "(values (symbol-package *cf-original-name*)
                 (symbol-package *cf-replacement-name*) (find-package :cl-user))",
    );
    assert!(homes[0].is_nil());
    assert!(homes[1].same_identity(&homes[2]));
}

#[test]
fn failed_recompilation_leaves_the_existing_fasl_loadable() {
    let temp_dir = TempDir::new("compile-file");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defparameter *cf-before-transfer* 0) (defparameter *cf-cleanup* 0)")
        .unwrap();
    let source = temp_dir.write(
        "nonlocal.lisp",
        "(setq *cf-before-transfer* 23)
         (unwind-protect (throw :cf-leave (values 17 42))
           (setq *cf-cleanup* 3))
         (setq *cf-before-transfer* 0)",
    );
    let explicit = temp_dir.path().join("saved.custom");
    let compiled_file = compile_file(
        &runtime,
        &source,
        &format!(":output-file {}", lisp_path(&explicit)),
    );
    assert_eq!(compiled_file, explicit.canonicalize().unwrap());
    let compile_pathname = eval(
        &runtime,
        &format!(
            "(namestring (compile-file-pathname {} :output-file {}))",
            lisp_path(&source),
            lisp_path(&explicit)
        ),
    );
    // COMPILE-FILE-PATHNAME defaults the namestring as given, while
    // COMPILE-FILE returns the written file's truename. Compare the files
    // those spellings name; macOS temp directories are symlinked.
    assert_eq!(
        Path::new(&runtime.string(&compile_pathname[0]).unwrap())
            .canonicalize()
            .unwrap(),
        compiled_file
    );
    let host_path = temp_dir.path().join("host-copy.zkclfasl");
    let host_output = runtime.compile_file(&source, Some(&host_path)).unwrap();
    assert_eq!(host_output, host_path.canonicalize().unwrap());
    let load = format!(
        "(catch :cf-leave (load {} :verbose nil :print nil))",
        lisp_path(&compiled_file)
    );
    assert_eq!(eval_fixnums(&runtime, &load), [17, 42]);
    assert_eq!(
        eval_fixnums(&runtime, "(values *cf-before-transfer* *cf-cleanup*)"),
        [23, 3]
    );
    assert!(eval(&runtime, "*load-truename*")[0].is_nil());

    let saved = fs::read(&compiled_file).unwrap();
    let decoded = zkcl_bytecode::CompiledFile::decode(&saved).unwrap();
    assert!(
        decoded
            .entries
            .iter()
            .any(|code| !code.instructions.is_empty())
    );
    fs::write(&source, "(in-package :keyword) (defun unfinished ()").unwrap();
    let rejected = runtime.eval(&format!(
        "(compile-file {} :output-file {} :verbose nil :print nil)",
        lisp_path(&source),
        lisp_path(&compiled_file)
    ));
    if let Ok(values) = rejected {
        assert_eq!(values.len(), 3);
        assert!(values[0].is_nil());
        assert!(!values[2].is_nil());
    }
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
    assert!(eval(&runtime, "*compile-file-truename*")[0].is_nil());
    assert_eq!(fs::read(&compiled_file).unwrap(), saved);
    assert_eq!(eval_fixnums(&runtime, &load), [17, 42]);

    let truncated = temp_dir.path().join("truncated.zkclfasl");
    fs::write(&truncated, &saved[..saved.len() - 1]).unwrap();
    eval(&runtime, "(setq *cf-before-transfer* 91)");
    assert!(runtime.load_file(&truncated).is_err());
    let corrupted_path = temp_dir.path().join("corrupted.zkclfasl");
    let mut corrupted = saved;
    let fasl_magic = b"\x89ZKCLFASL";
    assert!(corrupted.starts_with(fasl_magic));
    corrupted[fasl_magic.len()..].fill(0xff);
    fs::write(&corrupted_path, corrupted).unwrap();
    assert!(runtime.load_file(&corrupted_path).is_err());
    assert_eq!(eval_fixnums(&runtime, "*cf-before-transfer*"), [91]);

    let signal = temp_dir.write(
        "signal.lisp",
        "(incf *cf-before-transfer*) (error \"compiled failure\") (setq *cf-before-transfer* 0)",
    );
    let signal = compile_file(&runtime, &signal, "");
    let caught = eval(
        &runtime,
        &format!(
            "(handler-case (load {} :verbose nil :print nil) (simple-error () t))",
            lisp_path(&signal),
        ),
    );
    assert!(!caught[0].is_nil());
    assert_eq!(eval_fixnums(&runtime, "*cf-before-transfer*"), [92]);
    assert!(eval(&runtime, "*load-pathname*")[0].is_nil());
}

#[test]
fn core_range_proofs_follow_the_targets_fixnum_limits() {
    use zkcl_bytecode::Instruction;

    let dir = TempDir::new("target-range");
    let path = dir.write(
        "ranges.lisp",
        r#"
        (defun word-limit-check (x)
          (declare (optimize (speed 3) (safety 3)))
          (if (typep x '(unsigned-byte 29)) (the (unsigned-byte 29) x) nil))
        (defun word-limit-increment (x)
          (declare (optimize (speed 3) (safety 3)))
          (if (typep x '(integer 0 268435455)) (1+ x) nil))
        (defun word-limit-types ()
          (list (typep 268435456 'fixnum) (typep '(268435456) 'cons)
                (typep (1+ 268435455) 'bignum)))
    "#,
    );
    for (target, narrow) in [
        ("i686-unknown-linux-gnu", true),
        ("x86_64-unknown-linux-gnu", false),
    ] {
        let rt = Runtime::builder()
            .compilation_target(target.parse().unwrap())
            .start()
            .unwrap();
        let file = rt.compile_file_to_bytecode(&path).unwrap();
        let insns: Vec<_> = file
            .nested_function_nodes()
            .flat_map(|(_, code)| &code.instructions)
            .collect();
        assert!(
            insns
                .iter()
                .any(|op| matches!(op, Instruction::UnlessFixnumInRange(_, 0, 268435455, _)))
        );
        assert_eq!(
            insns
                .iter()
                .any(|op| matches!(op, Instruction::UnlessFixnumInRange(_, 0, 536870911, _))),
            !narrow
        );
        assert_eq!(
            insns
                .iter()
                .any(|op| matches!(op, Instruction::TryFixnumArithmetic { .. })),
            !narrow
        );
        let output = rt.compile_file(&path, None).unwrap();
        rt.load_file(&output).unwrap();
        assert_eq!(
            common::lisp::eval_reprs(&rt, "(word-limit-check 268435456)"),
            ["268435456"]
        );
        assert_eq!(
            common::lisp::eval_reprs(&rt, "(word-limit-increment 268435455)"),
            ["268435456"]
        );
        assert_eq!(
            common::lisp::eval_reprs(&rt, "(word-limit-types)"),
            [if narrow { "(NIL T T)" } else { "(T T NIL)" }]
        );
    }
}
