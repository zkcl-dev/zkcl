use std::fs;
use std::path::{Path, PathBuf};

use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::{lisp_path, lisp_string};

fn configure_asdf(runtime: &Runtime, source_root: &Path, cache: &Path) {
    let source_root = format!("{}/", source_root.display());
    let cache = format!("{}/", cache.display());
    let values = runtime
        .eval(&format!(
            r#"(progn
                 (asdf:initialize-source-registry
                   '(:source-registry (:directory {})
                     :ignore-inherited-configuration))
                 (asdf:initialize-output-translations
                   '(:output-translations (t {})
                     :ignore-inherited-configuration))
                 (asdf:asdf-version))"#,
            lisp_string(&source_root),
            lisp_string(&cache),
        ))
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("3.3.7"));
}

#[test]
fn asdf_can_select_raw_strings_per_system() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = Runtime::start().unwrap();
            let dir = TempDir::new("asdf-raw-strings");
            dir.write(
                "raw-dependency.asd",
                r#"(asdf:defsystem "raw-dependency" :components ((:file "dependency")))"#,
            );
            dir.write(
                "dependency.lisp",
                r#"(eval-when (:compile-toplevel :load-toplevel)
          (assert (null (get-dispatch-macro-character #\# #\>))))"#,
            );
            dir.write(
                "raw-project.asd",
                r#"(asdf:defsystem "raw-project"
          :depends-on ("raw-dependency") :around-compile "zk:call-with-raw-strings"
          :components ((:file "main")))"#,
            );
            dir.write(
                "main.lisp",
                "(defun raw-project-text ()\n#>file\n#>  text\n)",
            );
            runtime.eval("(require \"asdf\")").unwrap();
            configure_asdf(&runtime, dir.path(), &dir.path().join("cache"));
            runtime.eval("(asdf:load-system \"raw-project\")").unwrap();
            assert!(!runtime.raw_strings_enabled());
            let values = runtime.eval("(raw-project-text)").unwrap();
            assert_eq!(runtime.string(&values[0]).as_deref(), Some("file\n  text"));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_file_readtable_selection_survives_compilation_and_restores_the_caller() {
    let dir = TempDir::new("raw-string-fasl");
    let source = dir.write(
        "raw.lisp",
        concat!(
            "(zk:in-readtable :raw-strings)\n",
            "(defun raw-example ()\n  #>a \"string\"\n  #>  #\\B\n)\n",
        ),
    );
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&source).unwrap();
    assert!(!runtime.raw_strings_enabled());
    let fasl = runtime.compile_file(&source, None).unwrap();
    assert!(!runtime.raw_strings_enabled());

    let fresh = Runtime::start().unwrap();
    fresh.load_file(&fasl).unwrap();
    assert!(!fresh.raw_strings_enabled());
    let values = fresh.eval("(raw-example)").unwrap();
    assert_eq!(
        fresh.string(&values[0]).as_deref(),
        Some("a \"string\"\n  #\\B")
    );

    let fixture = dir.write("fixture.lisp", "#>雪\r\n  #>  😀\r\n#|comment|#\n#x2a");
    let values = fresh
        .eval(&format!(
            "(let ((*readtable* (zk:make-raw-strings-readtable)))
               (with-open-file (stream {}) (values (read stream) (read stream))))",
            lisp_path(&fixture)
        ))
        .unwrap();
    assert_eq!(fresh.string(&values[0]).as_deref(), Some("雪\n  😀"));
    assert_eq!(values[1].as_fixnum(), Some(42));
}

fn write_asdf_component_graph(temp_dir: &TempDir) -> PathBuf {
    for directory in ["support", "src", "cache"] {
        fs::create_dir(temp_dir.path().join(directory)).unwrap();
    }
    temp_dir.write(
        "zk-asdf-support.asd",
        r#"(asdf:defsystem "zk-asdf-support"
             :serial t
             :components ((:module "support"
                            :serial t
                            :components ((:file "package")
                                         (:file "macros")
                                         (:file "value")))))"#,
    );
    temp_dir.write(
        "zk-asdf-example.asd",
        r#"(asdf:defsystem "zk-asdf-example"
             :depends-on ("zk-asdf-support")
             :components ((:module "src"
                            :serial t
                            :components ((:file "package")
                                         (:file "main")))))"#,
    );
    temp_dir.write(
        "support/package.lisp",
        r#"(push :support-package cl-user::*asdf-load-order*)
           (defpackage :zk-asdf-support
             (:use :cl)
             (:export #:answer #:answer-form))"#,
    );
    temp_dir.write(
        "support/macros.lisp",
        r#"(in-package :zk-asdf-support)
           (push :support-macros cl-user::*asdf-load-order*)
           (defmacro answer-form () '(answer))"#,
    );
    temp_dir.write(
        "support/value.lisp",
        r#"(in-package :zk-asdf-support)
           (push :support-value cl-user::*asdf-load-order*)
           (defun answer () 42)"#,
    );
    temp_dir.write(
        "src/package.lisp",
        r#"(push :example-package cl-user::*asdf-load-order*)
           (defpackage :zk-asdf-example
             (:use :cl)
             (:export #:answer #:component-load-path))"#,
    );
    temp_dir.write(
        "src/main.lisp",
        r#"(in-package :zk-asdf-example)
           (push :example-main cl-user::*asdf-load-order*)
           (eval-when (:compile-toplevel)
             (unless (equal (macroexpand-1 '(zk-asdf-support:answer-form))
                            '(zk-asdf-support:answer))
               (error "support macro was unavailable while compiling the caller")))
           (defun answer () (zk-asdf-support:answer-form))
           (defparameter *component-load-path* (namestring *load-truename*))
           (defun component-load-path () *component-load-path*)
           (error "intentional first ASDF load failure")"#,
    );
    temp_dir.path().to_path_buf()
}

#[test]
fn provide_records_names_in_the_current_modules_binding() {
    let runtime = Runtime::start().unwrap();
    let initial_modules = runtime.eval("*modules*").unwrap().remove(0);
    let values = runtime
        .eval(
            r#"(let ((*modules* nil))
                 (provide #\A)
                 (provide :a)
                 (provide "a")
                 (require "a" 43)
                 (values (length *modules*) (first *modules*) (second *modules*)))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(2));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("a"));
    assert_eq!(runtime.string(&values[2]).as_deref(), Some("A"));
    assert!(runtime.eval("*modules*").unwrap()[0].same_identity(&initial_modules));
}

#[test]
fn require_visits_each_explicit_path_in_order_on_the_first_request() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    runtime
        .load_str("(defparameter *module-order* nil) (provide \"outside\")")
        .unwrap();
    let first = temp_dir.write(
        "first module.lisp",
        r#"(push :first *module-order*)
           (provide "joined")"#,
    );
    let second_source = temp_dir.write("second.lisp", "(push :second *module-order*)");
    let second = runtime.compile_file(&second_source, None).unwrap();
    fs::remove_file(&second_source).unwrap();
    let loaded = runtime
        .eval(&format!(
            r#"(let ((*modules* nil))
                 (require "joined" (list {} (pathname {})))
                 (require "joined")
                 (require "joined" {})
                 (values (equal *module-order* '(:second :first))
                         (equal *modules* '("joined"))))"#,
            lisp_path(&first),
            lisp_path(&second),
            lisp_path(&temp_dir.path().join("must-not-load.lisp")),
        ))
        .unwrap();
    assert!(loaded.iter().all(|value| !value.is_nil()));
    let outside = runtime.eval("(first *modules*)").unwrap();
    assert_eq!(runtime.string(&outside[0]).as_deref(), Some("outside"));
}

#[test]
fn require_reloads_an_explicit_file_that_does_not_provide_its_module() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    runtime
        .load_str("(defparameter *unprovided-count* 0)")
        .unwrap();
    let unprovided = temp_dir.write("unprovided.lisp", "(incf *unprovided-count*)");
    runtime
        .eval(&format!(
            r#"(progn (require "explicit only" {})
                      (require "explicit only" {}))"#,
            lisp_path(&unprovided),
            lisp_path(&unprovided),
        ))
        .unwrap();
    assert_eq!(
        runtime.eval("*unprovided-count*").unwrap()[0].as_fixnum(),
        Some(2)
    );
}

#[test]
fn require_uses_pathname_defaults_updated_by_an_earlier_file() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    runtime
        .load_str("(defparameter *module-order* nil)")
        .unwrap();
    let directory = temp_dir.path().join("next");
    fs::create_dir(&directory).unwrap();
    let defaults = temp_dir.write(
        "defaults.lisp",
        format!(
            "(setq *default-pathname-defaults* (pathname {}))",
            lisp_string(&format!("{}/", directory.display())),
        ),
    );
    temp_dir.write("next/relative.lisp", "(push :relative *module-order*)");
    runtime
        .eval(&format!(
            r#"(let ((*default-pathname-defaults* *default-pathname-defaults*))
                 (require "relative" (list {} "relative.lisp")))"#,
            lisp_path(&defaults),
        ))
        .unwrap();
    assert_eq!(
        runtime
            .symbol_name(&runtime.eval("(first *module-order*)").unwrap()[0])
            .as_deref(),
        Some("RELATIVE")
    );
}

#[test]
fn require_stops_loading_files_after_a_throw() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    let transfer = temp_dir.write("transfer.lisp", "(throw 'transfer 42)");
    let got = runtime
        .eval(&format!(
            "(catch 'transfer (require :transfer (list {} {})))",
            lisp_path(&transfer),
            lisp_path(&temp_dir.path().join("must-not-load.lisp")),
        ))
        .unwrap();
    assert_eq!(got[0].as_fixnum(), Some(42));
}

#[test]
fn module_loading_signals_specific_conditions_for_invalid_requests() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    for (source, condition) in [
        ("(provide 7)", ConditionClass::TypeError),
        ("(require 7)", ConditionClass::TypeError),
        ("(require :unknown)", ConditionClass::Error),
        (
            "(require :unknown '(\"missing\" . 3))",
            ConditionClass::TypeError,
        ),
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(error.kind(), EvalErrorKind::LispCondition(condition));
    }
    let missing = runtime
        .eval(&format!(
            r#"(require "absent file" {})"#,
            lisp_path(&temp_dir.path().join("missing.lisp")),
        ))
        .err()
        .unwrap();
    assert_eq!(
        missing.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
}

#[test]
fn lisp_implementation_identity_matches_the_package_version() {
    let runtime = Runtime::start().unwrap();
    let identity = runtime
        .eval("(values (lisp-implementation-type) (lisp-implementation-version))")
        .unwrap();
    assert_eq!(runtime.string(&identity[0]).as_deref(), Some("zkcl"));
    assert_eq!(
        runtime.string(&identity[1]).as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn asdf_output_cache_separates_ios_device_and_simulator_fasls() {
    let dir = TempDir::new("asdf-compilation-target");
    let input = dir.write("component.lisp", "");
    let output_path = |triple: &str| {
        let runtime = Runtime::builder()
            .compilation_target(triple.parse().unwrap())
            .start()
            .unwrap();
        runtime.eval("(require :asdf)").unwrap();
        let values = runtime
            .eval(&format!(
                "(progn
               (asdf:initialize-output-translations
                 '(:output-translations :enable-user-cache :ignore-inherited-configuration))
               (namestring (asdf:apply-output-translations (compile-file-pathname {}))))",
                lisp_path(&input)
            ))
            .unwrap();
        let path = runtime.string(&values[0]).unwrap();
        assert!(path.contains(triple), "{triple}: {path}");
        path
    };
    assert_ne!(
        output_path("aarch64-apple-ios"),
        output_path("aarch64-apple-ios-sim")
    );
}

#[cfg(unix)]
#[test]
fn module_search_treats_host_paths_literally() {
    let temp_dir = TempDir::new("modules");
    let directory = temp_dir.path().join("*");
    fs::create_dir(&directory).unwrap();
    temp_dir.write("*/script.lisp", "(provide :script)");
    let runtime = Runtime::start().unwrap();
    runtime.add_module_directory(&directory).unwrap();
    let values = runtime
        .eval("(let ((*modules* nil)) (require :script) *modules*)")
        .unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(\"SCRIPT\")");
}

#[test]
fn require_stops_at_the_first_provider_that_provides_the_module() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    temp_dir.write("local.lisp", "(provide :local)");
    runtime.add_module_directory(temp_dir.path()).unwrap();

    let source = r#"
        (let ((*modules* nil) (seen nil))
          (let ((zk:*module-provider-functions*
                  (list (lambda (name) (push name seen) (values nil t))
                        (lambda (name) (provide name))
                        (lambda (name) (error "unexpected provider")))))
            (require :local)
            (require "system/component")
            (require "system/component" 42)
            (and (equal seen '("system/component"))
                 (equal *modules* '("system/component" "LOCAL")))))"#;
    assert!(!runtime.eval(source).unwrap()[0].is_nil());
}

#[test]
fn require_allows_a_new_provider_after_an_aborted_load() {
    let runtime = Runtime::start().unwrap();
    let source = r#"
        (let ((*modules* nil))
          (let ((zk:*module-provider-functions*
                  (list (lambda (name) (require name)))))
            (assert (handler-case (require :retry) (error () t))))
          (let ((zk:*module-provider-functions*
                  (list (lambda (name) (throw 'outside name)))))
            (assert (eq :retry (catch 'outside (require :retry)))))
          (let ((zk:*module-provider-functions* '(provide)))
            (require :retry))
          (equal *modules* '("RETRY")))"#;
    assert!(!runtime.eval(source).unwrap()[0].is_nil());
}

#[test]
fn require_rejects_a_dotted_list_of_module_providers() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval(
            r#"(let ((zk:*module-provider-functions* '(nil . t)))
                                 (require :missing))"#,
        )
        .err()
        .unwrap();
    assert_eq!(
        result.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
}

#[test]
fn compiled_only_modules_are_discovered_after_source_removal() {
    let temp_dir = TempDir::new("modules");
    let compiler = Runtime::start().unwrap();
    let source = temp_dir.write(
        "compiled-only.lisp",
        r#"(provide :compiled-only)
           (defparameter cl-user::*compiled-only-value* 41)"#,
    );
    let fasl = compiler.compile_file(&source, None).unwrap();
    assert!(fasl.is_file());
    assert_eq!(
        fasl.extension().and_then(|value| value.to_str()),
        Some("zkclfasl")
    );

    fs::write(
        &source,
        r#"(provide :compiled-only)
           (defparameter cl-user::*compiled-only-value* 42)"#,
    )
    .unwrap();
    let source_runtime = Runtime::start().unwrap();
    source_runtime
        .add_module_directory(temp_dir.path())
        .unwrap();
    let values = source_runtime
        .eval(r#"(progn (require "compiled-only") cl-user::*compiled-only-value*)"#)
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));

    fs::remove_file(&source).unwrap();
    let compiled_runtime = Runtime::start().unwrap();
    compiled_runtime
        .add_module_directory(temp_dir.path())
        .unwrap();
    let values = compiled_runtime
        .eval(r#"(progn (require "compiled-only") cl-user::*compiled-only-value*)"#)
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(41));
}

#[test]
fn embedded_asdf_reuses_translated_fasls_after_retrying_a_component_load() {
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("modules");
    let source_root = write_asdf_component_graph(&temp_dir);

    runtime
        .load_str(
            r#"(defpackage :zk-asdf-caller (:use :cl))
               (in-package :zk-asdf-caller)
               (defparameter cl-user::*asdf-load-order* nil)
               (defparameter cl-user::*asdf-first-error* nil)"#,
        )
        .unwrap();
    runtime
        .load_str(
            r#"(let ((table (copy-readtable nil)))
                 (set-dispatch-macro-character
                  #\# #\+
                  (lambda (stream sub-character number)
                    (declare (ignore stream sub-character number))
                    (error "caller readtable dispatch was used"))
                  table)
                 (setf *readtable* table))"#,
        )
        .unwrap();
    let caller_readtable = runtime.eval("*readtable*").unwrap();
    runtime
        .load_str(
            r#"(let ((*read-base* 2)
                     (*read-eval* nil)
                     (*read-suppress* t))
                 (require "asdf")
                 (assert (= *read-base* 2))
                 (assert (not *read-eval*))
                 (assert *read-suppress*))"#,
        )
        .unwrap();
    let values = runtime.eval("(asdf:asdf-version)").unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("3.3.7"));
    assert_eq!(runtime.current_package().as_deref(), Some("ZK-ASDF-CALLER"));
    let restored_readtable = runtime.eval("*readtable*").unwrap();
    assert!(caller_readtable[0].same_identity(&restored_readtable[0]));
    runtime
        .eval(r#"(assert (get-dispatch-macro-character #\# #\+))"#)
        .unwrap();
    runtime
        .eval("(setf *readtable* (copy-readtable nil))")
        .unwrap();
    runtime.eval("(in-package :cl-user)").unwrap();

    temp_dir.write(
        "zk-cycle-a.asd",
        r#"(asdf:defsystem "zk-cycle-a" :depends-on ("zk-cycle-b"))"#,
    );
    temp_dir.write(
        "zk-cycle-b.asd",
        r#"(asdf:defsystem "zk-cycle-b" :depends-on ("zk-cycle-a"))"#,
    );
    configure_asdf(&runtime, &source_root, &source_root.join("cache"));
    runtime
        .eval(
            r#"(assert (handler-case (progn (asdf:load-system "zk-cycle-a") nil)
                        (asdf:circular-dependency () t)))"#,
        )
        .unwrap();

    let values = runtime
        .eval(
            r#"(handler-case
                 (progn (require "zk-asdf-example") t)
               (error (condition)
                 (setq cl-user::*asdf-first-error* (princ-to-string condition))
                 nil))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());
    let message = runtime
        .eval("cl-user::*asdf-first-error*")
        .unwrap()
        .into_iter()
        .next()
        .and_then(|value| runtime.string(&value))
        .unwrap();
    assert!(
        message.contains("intentional first ASDF load failure"),
        "{message}"
    );

    temp_dir.write(
        "src/main.lisp",
        r#"(in-package :zk-asdf-example)
           (push :example-main cl-user::*asdf-load-order*)
           (eval-when (:compile-toplevel)
             (unless (equal (macroexpand-1 '(zk-asdf-support:answer-form))
                            '(zk-asdf-support:answer))
               (error "support macro was unavailable while compiling the caller")))
           (defun answer () (zk-asdf-support:answer-form))
           (defparameter *component-load-path* (namestring *load-truename*))
           (defun component-load-path () *component-load-path*)"#,
    );
    runtime
        .eval("(asdf:load-system \"zk-asdf-example\" :force t)")
        .unwrap();
    assert_eq!(
        runtime.eval("(zk-asdf-example:answer)").unwrap()[0].as_fixnum(),
        Some(42)
    );

    let paths = runtime
        .eval(
            r#"(let* ((source (asdf:system-relative-pathname
                                "zk-asdf-example" "src/main.lisp"))
                      (output (asdf:apply-output-translations
                               (compile-file-pathname source))))
                 (values (namestring source) (namestring output)))"#,
        )
        .unwrap();
    let source = runtime.string(&paths[0]).unwrap();
    let output = PathBuf::from(runtime.string(&paths[1]).unwrap());
    let loaded = runtime
        .eval("(zk-asdf-example:component-load-path)")
        .unwrap();
    let loaded = PathBuf::from(runtime.string(&loaded[0]).unwrap());
    assert_eq!(
        output.canonicalize().unwrap(),
        loaded.canonicalize().unwrap()
    );
    assert_eq!(
        output.extension().and_then(|value| value.to_str()),
        Some("zkclfasl")
    );
    assert!(!Path::new(&source).with_extension("zkclfasl").exists());
    assert!(output.starts_with(source_root.join("cache")));
    let compiled_modified = fs::metadata(&output).unwrap().modified().unwrap();

    drop(runtime);
    let fresh = Runtime::start().unwrap();
    fresh
        .load_str("(defparameter cl-user::*asdf-load-order* nil)")
        .unwrap();
    fresh.eval("(require \"uiop\")").unwrap();
    let modules_after_uiop = fresh.eval("(length *modules*)").unwrap()[0]
        .as_fixnum()
        .unwrap();
    fresh.eval("(require \"asdf\")").unwrap();
    assert_eq!(
        fresh.eval("(length *modules*)").unwrap()[0]
            .as_fixnum()
            .unwrap(),
        modules_after_uiop
    );
    configure_asdf(&fresh, &source_root, &source_root.join("cache"));
    fresh.eval("(require \"zk-asdf-example\")").unwrap();
    assert_eq!(
        fresh.eval("(zk-asdf-example:answer)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    let fresh_loaded = fresh.eval("(zk-asdf-example:component-load-path)").unwrap();
    let fresh_loaded = PathBuf::from(fresh.string(&fresh_loaded[0]).unwrap());
    assert_eq!(
        fresh_loaded.canonicalize().unwrap(),
        output.canonicalize().unwrap()
    );
    assert_eq!(
        fs::metadata(&output).unwrap().modified().unwrap(),
        compiled_modified
    );
    let order = fresh.eval("cl-user::*asdf-load-order*").unwrap();
    assert_eq!(
        fresh.repr(&order[0]).unwrap(),
        "(:EXAMPLE-MAIN :EXAMPLE-PACKAGE :SUPPORT-VALUE :SUPPORT-MACROS :SUPPORT-PACKAGE)"
    );
}

#[test]
fn native_platform_system_defers_to_the_users_asdf_definition() {
    // Embedded hosts start without foreign calls, including ffi-enabled builds.
    let runtime = Runtime::start().unwrap();
    let temp_dir = TempDir::new("asdf-platform");
    runtime.eval("(require :asdf)").unwrap();
    configure_asdf(&runtime, temp_dir.path(), &temp_dir.path().join("cache"));
    runtime
        .load_str(
            r#"(setf asdf:*central-registry* nil)
               (assert (not (find-package "ZK-FFI")))
               (let ((zk:*use-asdf-ports* nil))
                 (assert (null (asdf:find-system "trivial-features" nil))))
               (asdf:load-system "trivial-features")
               (assert (= 1 (count-if (lambda (f) (member f '(:little-endian :big-endian)))
                                      *features*)))
               (assert (null (find-package "CFFI")))
               (assert (null (asdf:system-source-file (asdf:find-system "trivial-features"))))"#,
        )
        .unwrap();

    let definition = temp_dir.write(
        "trivial-features.asd",
        r#"(asdf:defsystem "trivial-features" :version "7.0")"#,
    );
    runtime
        .eval(&format!(
            r#"(let ((asdf:*central-registry* (list #p{})))
                 (asdf:load-system "trivial-features")
                 (let ((system (asdf:find-system "trivial-features")))
                   (assert (equal "7.0" (asdf:component-version system)))
                   (assert (equal (truename {}) (asdf:system-source-file system)))))"#,
            lisp_string(&format!("{}/", temp_dir.path().display())),
            lisp_path(&definition)
        ))
        .unwrap();
}

#[test]
fn asdf_load_source_requires_native_module_dependencies_before_reading_clients() {
    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("asdf-require-source");
    rt.add_module_directory(dir.path()).unwrap();
    dir.write(
        "zk-native-example.lisp",
        r#"(defpackage :zk-native-example (:use :cl) (:export #:value))
           (defun zk-native-example:value () 42)
           (incf cl-user::*native-initializations*)
           (provide "zk-native-example")"#,
    );
    dir.write(
        "zk-source-client.asd",
        r#"(asdf:defsystem "zk-source-client"
             :depends-on ((:require "zk-native-example"))
             :components ((:file "client")))"#,
    );
    dir.write(
        "client.lisp",
        r#"(assert (= 42 (zk-native-example:value)))
           (push (pathname-type *load-truename*) cl-user::*client-loads*)"#,
    );
    rt.eval("(require :asdf)").unwrap();
    configure_asdf(&rt, dir.path(), &dir.path().join("cache"));
    rt.load_str(
        r#"(defvar *native-initializations* 0)
           (defvar *client-loads* nil)
           (asdf:operate 'asdf:load-source-op "zk-source-client")
           (assert (equal *client-loads* '("lisp")))
           (asdf:load-system "zk-source-client")
           (assert (equal *client-loads* '("zkclfasl" "lisp")))
           (assert (= *native-initializations* 1))"#,
    )
    .unwrap();
}
