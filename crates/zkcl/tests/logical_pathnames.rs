use std::error::Error;
use std::fs;
use std::num::{IntErrorKind, ParseIntError};

use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::lisp_path;

#[test]
fn file_operations_resolve_logical_names_through_host_translations() {
    let runtime = Runtime::start().unwrap();
    // The wildcard rules used to initialize logical pathnames in universe.lsp.
    runtime
        .load_str(
            r#"(setf (logical-pathname-translations "CLTESTROOT")
                     (list (list "**;*.*.*"
                                 (merge-pathnames
                                  "sandbox/"
                                  (make-pathname :directory '(:absolute :wild-inferiors)
                                                 :name :wild :type :wild)))))
               (setf (logical-pathname-translations "CLTEST")
                     (list (list "**;*.*.*"
                                 (merge-pathnames
                                  "sandbox/"
                                  (make-pathname
                                   :directory (append (pathname-directory (truename (make-pathname)))
                                                      '(:wild-inferiors))
                                   :name :wild :type :wild)))))"#,
        )
        .unwrap();
    let values = runtime
        .eval(
            r#"(let ((root (logical-pathname "cltestroot:")))
                 (values (typep root 'logical-pathname) (pathnamep root)
                         (type-of root) (namestring root)
                         (namestring (translate-logical-pathname "cltestroot:src;File.Lisp.3"))
                         (wild-pathname-p "cltestroot:SRC;FILE-*.LISP" :name)))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert_eq!(
        runtime.symbol_name(&values[2]).as_deref(),
        Some("LOGICAL-PATHNAME")
    );
    assert_eq!(runtime.string(&values[3]).as_deref(), Some("CLTESTROOT:"));
    assert_eq!(
        runtime.string(&values[4]).as_deref(),
        Some("/src/sandbox/file.lisp")
    );
    assert!(!values[5].is_nil());

    let values = runtime
        .eval(
            r#"(multiple-value-bind (path index)
                   (parse-namestring ";Src;file.lisp.31 junk" "cltest" nil :junk-allowed t)
                 (values (namestring path) (pathname-host path)
                         (pathname-device path) (directory-namestring path)
                         (pathname-version path) index))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("CLTEST:;SRC;FILE.LISP.31")
    );
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("CLTEST"));
    assert_eq!(
        runtime.symbol_name(&values[2]).as_deref(),
        Some("UNSPECIFIC")
    );
    assert_eq!(runtime.string(&values[3]).as_deref(), Some(";SRC;"));
    assert_eq!(values[4].as_fixnum(), Some(31));
    assert_eq!(values[5].as_fixnum(), Some(17));

    let error = runtime
        .eval(r#"(logical-pathname "CLTEST:FILE.LISP.999999999999999999999999999999")"#)
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    let source = error.source().unwrap().source().unwrap();
    assert_eq!(
        source.downcast_ref::<ParseIntError>().unwrap().kind(),
        &IntErrorKind::PosOverflow
    );

    let values = runtime
        .eval(
            r#"(let* ((defaults (logical-pathname "cltest:src;base.lisp"))
                      (path ";unit.lisp")
                      (abbreviated (enough-namestring path defaults)))
                 (values abbreviated
                         (equal (merge-pathnames abbreviated defaults)
                                (merge-pathnames (parse-namestring path nil defaults) defaults))))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some(";UNIT.LISP.NEWEST")
    );
    assert!(!values[1].is_nil());

    let values = runtime
        .eval(
            r#"(let ((*default-pathname-defaults* (logical-pathname "cltest:src;base.lisp")))
                 (values (namestring (merge-pathnames ";unit"))
                         (namestring (make-pathname :name "foo" :type "lisp"))
                         (typep (pathname "/tmp/a.lisp") 'logical-pathname)))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("CLTEST:SRC;UNIT.LISP.NEWEST")
    );
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("CLTEST:FOO.LISP")
    );
    assert!(values[2].is_nil());

    let dir = TempDir::new("logical-pathnames");
    let target_pattern = lisp_path(&dir.path().join("**/*.*"));
    let test_source = dir.write(
        "test.lisp",
        "(defparameter *test-path* #P\"PATHIO:DEMO.LISP\")",
    );
    runtime
        .load_str(&format!(
            r#"(setf (logical-pathname-translations "PATHIO") '(("**;*.*.*" {target_pattern})))
               (setf (logical-pathname-translations "PATHALT") '(("**;*.*.*" "PATHIO:**;*.*.*")))
               (setf (logical-pathname-translations "CYCLE") '(("**;*.*.*" "CYCLE:**;*.*.*")))"#
        ))
        .unwrap();
    let values = runtime
        .eval(
            r#"(let ((rules (logical-pathname-translations "pathalt")))
                 (values (typep (caar rules) 'logical-pathname)
                         (namestring (cadar rules))
                         (pathname-match-p "PATHALT:TEST.LISP" "PATHALT:T*.LISP.*")
                         (pathname-match-p "PATHALT:TEST.LISP" "PATHALT:OTHER;*.LISP.*")
                         (namestring (translate-pathname "PATHALT:FILE-42.LISP"
                                                       "PATHALT:FILE-*.LISP"
                                                       "PATHIO:TEST-*.LISP"))
                         (pathnamep (probe-file "PATHALT:TEST.LISP"))
                         (length (directory "PATHALT:**;*.LISP.*"))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("PATHIO:**;*.*.*")
    );
    assert!(!values[2].is_nil());
    assert!(values[3].is_nil());
    assert_eq!(
        runtime.string(&values[4]).as_deref(),
        Some("PATHIO:TEST-42.LISP")
    );
    assert!(!values[5].is_nil());
    assert_eq!(values[6].as_fixnum(), Some(1));

    let values = runtime
        .eval(
            r#"(values
                 (namestring (translate-pathname "PATHALT:LIB-ABC;FILE.LISP"
                                                "PATHALT:LIB-*;*.LISP"
                                                "PATHIO:OUTPUT-*;*.LISP"))
                 (namestring (translate-pathname "PATHALT:LIB-A-MID-B;FILE.LISP"
                                                "PATHALT:LIB-*-MID-*;*.LISP"
                                                "PATHIO:*--*;*.LISP"))
                 (pathname-match-p "PATHALT:*.LISP" "PATHALT:FILE-*.LISP")
                 (let ((target (translate-pathname "PATHALT:TEST.LISP" "PATHALT:*.LISP"
                                                   "resource*/literal*.lisp")))
                   (and (not (wild-pathname-p target)) (namestring target)))
                 (let ((p (pathname "PATHALT\\:LOCAL.LISP")))
                   (and (equal p (read-from-string (prin1-to-string p)))
                        (equal p (pathname (namestring p))))))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("PATHIO:OUTPUT-ABC;FILE.LISP")
    );
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("PATHIO:A--B;FILE.LISP")
    );
    assert!(values[2].is_nil());
    assert_eq!(
        runtime.string(&values[3]).as_deref(),
        Some("resource*/literal*.lisp")
    );
    assert!(!values[4].is_nil());

    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::write(dir.path().join("nested/original.txt"), "file").unwrap();
    let fixed_target = lisp_path(&test_source);
    runtime
        .load_str(&format!(
            r#"(setf (logical-pathname-translations "FIXED") '(("**;*.*.*" {fixed_target})))"#,
        ))
        .unwrap();
    let values = runtime
        .eval(
            r#"(values
                (handler-case (probe-file "FIXED:*.LISP")
                  (file-error (condition) (namestring (file-error-pathname condition))))
                (handler-case (ensure-directories-exist "FIXED:*;TEST.LISP")
                  (file-error (condition) (wild-pathname-p (file-error-pathname condition) :directory)))
                (namestring (rename-file "PATHALT:NESTED;ORIGINAL.TXT" "renamed.txt")))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("FIXED:*.LISP.NEWEST")
    );
    assert!(!values[1].is_nil());
    assert_eq!(
        runtime.string(&values[2]).as_deref(),
        Some("PATHALT:NESTED;RENAMED.TXT.NEWEST")
    );
    assert!(!dir.path().join("nested/original.txt").exists());
    assert!(dir.path().join("nested/renamed.txt").is_file());

    let error = runtime
        .eval(r#"(translate-logical-pathname "cycle:loop.lisp")"#)
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );

    runtime.eval(r#"(load "PATHALT:TEST.LISP")"#).unwrap();
    let values = runtime
        .eval("(values (typep *test-path* 'logical-pathname) (namestring *test-path*))")
        .unwrap();
    assert!(!values[0].is_nil());
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("PATHIO:DEMO.LISP")
    );

    let compiled = runtime
        .eval(r#"(compile-file "PATHALT:TEST.LISP")"#)
        .unwrap();
    assert_eq!(compiled.len(), 3);
    assert!(dir.path().join("test.zkclfasl").is_file());
    fs::remove_file(test_source).unwrap();
    runtime
        .eval(r#"(progn (setq *test-path* nil) (load "PATHALT:TEST.ZKCLFASL"))"#)
        .unwrap();
    let loaded = runtime
        .eval("(typep *test-path* 'logical-pathname)")
        .unwrap();
    assert!(!loaded[0].is_nil());
}

#[test]
fn growing_logical_translation_respects_the_execution_budget() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(setf (logical-pathname-translations "GROWS")
                     '(("**;*.*.*" "GROWS:MORE;**;*.*.*")))"#,
        )
        .unwrap();
    runtime.set_step_budget(Some(128));
    let error = runtime
        .eval(r#"(translate-logical-pathname "GROWS:SOURCE.LISP")"#)
        .err()
        .unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
}
