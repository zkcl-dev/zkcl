use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_project() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/cli-project");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn put(root: &Path, path: &str, contents: &str) -> PathBuf {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
    path
}

fn zkcl(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn runs_only_the_declared_build_script_before_compiling_project_sources() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "zkcl.sexp",
        r#"(project "example/generate" (version "1.0.0")
             (binary "app" (files "src/main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(
        root,
        "build.lisp",
        r#"(error "build.lisp must be declared")"#,
    );
    put(
        root,
        "src/main.lisp",
        r#"(defun main () (write-line "plain"))"#,
    );
    let src = root.join("src");
    assert_eq!(success(zkcl(&src, &["run", "--bin", "app"])), "plain\n");

    put(
        root,
        "zkcl.sexp",
        r#"(project "example/generate" (version "1.0.0")
             (build "tools/generate.lisp" (output "generated/value.lisp"))
             (library (files "generated/value.lisp"))
             (binary "app" (files "src/main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(root, "number.txt", "42\n");
    put(
        root,
        "tools/generate.lisp",
        r#"(assert (null (zk:command-line-arguments)))
           (defpackage #:build-only (:use #:cl))
           (write-line "Generating value.lisp")
           (with-open-file (calls "build.calls" :direction :output :if-exists :append
                                                :if-does-not-exist :create)
             (write-line "ran" calls))
           (let ((file (merge-pathnames "generated/value.lisp" (zk:getenv "ZKCL_OUT_DIR"))))
             (ensure-directories-exist file)
             (with-open-file (source "number.txt")
               (with-open-file (output file :direction :output :if-exists :error)
                 (format output "(defun generated-value () ~D)~%" (read source)))))"#,
    );
    put(
        root,
        "src/main.lisp",
        r#"(defun main ()
             (assert (null (find-package "BUILD-ONLY")))
             (format t "~D ~A~%" (generated-value) (first (zk:command-line-arguments))))"#,
    );
    success(zkcl(&src, &["lock"]));
    success(zkcl(&src, &["fetch", "--offline"]));
    success(zkcl(&src, &["check"]));
    assert!(!root.join("build.calls").exists());

    let output = zkcl(&src, &["run", "--locked", "--bin", "app", "--", "hello"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Generating value.lisp"));
    assert_eq!(success(output), "42 hello\n");
    assert_eq!(
        fs::read_to_string(root.join("build.calls")).unwrap(),
        "ran\n"
    );
    assert!(!root.join("generated/value.lisp").exists());
}

#[test]
fn published_tools_generate_sources_when_run_from_the_cache() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "tool/zkcl.sexp",
        r#"(project "example/build-tool" (version "1.0.0") (license "MIT")
             (build "build.lisp" (input "message.txt") (output "main.lisp"))
             (binary "app" (files "main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(root, "tool/LICENSE", "MIT License\n");
    put(root, "tool/message.txt", "generated\n");
    put(
        root,
        "tool/build.lisp",
        r#"(with-open-file (input "message.txt")
             (with-open-file (out (merge-pathnames "main.lisp" (zk:getenv "ZKCL_OUT_DIR"))
                                  :direction :output :if-exists :error)
               (format out "(defun main () (write-line ~S))~%" (read-line input))))"#,
    );
    let registry = root.join("registry");
    let location = registry.to_str().unwrap();
    success(zkcl(root, &["registry", "init", "build-test", location]));
    success(zkcl(
        &root.join("tool"),
        &["publish", "--registry", location],
    ));
    let tool = || {
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(root)
            .env("ZKCL_CACHE_DIR", root.join("cache"))
            .args(["x", "example/build-tool@1.0.0"])
            .args(["--registry", location])
            .output()
            .unwrap()
    };
    assert_eq!(success(tool()), "generated\n");
    assert_eq!(success(tool()), "generated\n");

    put(
        root,
        "zkcl.sexp",
        r#"(project "example/caller" (version "1.0.0")
             (registry "build-test" (path "registry")))"#,
    );
    let nested = root.join("nested");
    fs::create_dir(&nested).unwrap();
    let tool_in_project = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(&nested)
            .env("ZKCL_CACHE_DIR", root.join("project-cache"))
            .args(["x", "example/build-tool@1.0.0"])
            .args(args)
            .output()
            .unwrap()
    };
    assert_eq!(success(tool_in_project(&[])), "generated\n");
    fs::remove_dir_all(registry).unwrap();
    assert_eq!(success(tool_in_project(&["--offline"])), "generated\n");
    assert!(!root.join("zkcl.lock").exists());
    assert!(!root.join("tool/main.lisp").exists());
}

#[test]
fn source_analysis_without_paths_checks_project_sources() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "zkcl.sexp",
        r#"(project "example/lint" (version "1.0.0") (library (files "source.lisp")))"#,
    );
    put(
        root,
        "source.lisp",
        "(defstruct record data)\n(defun record-empty-p (record) (null (record-data record)))\n)",
    );

    for command in ["check", "lint"] {
        let output = zkcl(root, &[command]);
        assert_eq!(output.status.code(), Some(1), "{command}: {output:?}");
        let diagnostics = String::from_utf8(output.stderr).unwrap();
        assert!(
            diagnostics.contains("source.lisp:3:1")
                && diagnostics.contains("unexpected closing parenthesis"),
            "{diagnostics}"
        );
    }
}

#[test]
fn locks_and_runs_a_project_with_compile_time_dependencies() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "shared/zkcl.sexp",
        r#"(project "example/shared"
      (version "1.0.0") (packages "EXAMPLE.SHARED") (library (files "shared.lisp")))"#,
    );
    put(
        root,
        "shared/shared.lisp",
        r#"(defpackage #:example.shared (:use #:cl) (:export #:answer))
      (in-package #:example.shared)
      (eval-when (:compile-toplevel) (setf (get 'answer 'compile-only) 42))
      (defmacro answer () (get 'answer 'compile-only 0))"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app"
      (version "1.0.0") (packages "EXAMPLE.APP")
      (dependency "shared" (module "example/shared") (path "../shared") (version ">=1, <3"))
      (library (files "src/package.lisp"))
      (binary "app" (files "src/main.lisp") (entry "EXAMPLE.APP:MAIN"))
      (test (files "src/test.lisp") (suite "EXAMPLE.APP::RUN-TESTS")))"#,
    );
    put(
        root,
        "app/src/package.lisp",
        "(defpackage #:example.app (:use #:cl) (:export #:main))",
    );
    put(
        root,
        "app/src/main.lisp",
        r#"(in-package #:example.app)
      (defun main () (format t "~D ~A~%" (example.shared:answer) (first (zk:command-line-arguments))))"#,
    );
    put(
        root,
        "app/src/test.lisp",
        r#"(in-package #:example.app)
      (zk-test:deftest test-answer () (zk-test:is= 42 (example.shared:answer)))
      (zk-test:defsuite run-tests test-answer)"#,
    );
    let app = root.join("app");
    let lock = app.join("zkcl.lock");
    success(zkcl(&app, &["lock"]));
    let first = fs::read_to_string(&lock).unwrap();
    assert!(first.contains("../shared"));
    success(zkcl(&app, &["lock"]));
    assert_eq!(fs::read_to_string(&lock).unwrap(), first);
    let output = success(zkcl(
        &app,
        &[
            "run",
            "--bin",
            "app",
            "--locked",
            "--offline",
            "--",
            "hello",
        ],
    ));
    assert_eq!(output, "42 hello\n");
    let output = success(zkcl(&app.join("src"), &["test", "--locked"]));
    assert!(output.contains("summary: 1/1 passed"), "{output}");

    let manifest = root.join("shared/zkcl.sexp");
    fs::write(
        &manifest,
        fs::read_to_string(&manifest)
            .unwrap()
            .replace("1.0.0", "2.0.0"),
    )
    .unwrap();
    let output = zkcl(&app, &["run", "--locked"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("zkcl.lock differs"));
    assert_eq!(fs::read_to_string(lock).unwrap(), first);
}

#[test]
fn native_project_can_compile_against_an_explicit_asdf_system() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "external/example-asdf.asd",
        r#"(asdf:defsystem "example-asdf" :components ((:file "external")))"#,
    );
    put(
        root,
        "external/external.lisp",
        r#"(defpackage #:example.asdf (:use #:cl) (:export #:answer))
      (in-package #:example.asdf) (defmacro answer () 42)"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/mixed"
      (version "1.0.0") (packages "EXAMPLE.MIXED")
      (dependency "external" (module "external/example") (path "../external")
        (system "example-asdf") (asd "example-asdf.asd"))
      (binary "mixed" (files "main.lisp") (entry "EXAMPLE.MIXED:MAIN")))"#,
    );
    put(
        root,
        "app/main.lisp",
        r#"(defpackage #:example.mixed (:use #:cl) (:export #:main))
      (in-package #:example.mixed)
      (defun main () (format t "~D~%" (example.asdf:answer)))"#,
    );
    let app = root.join("app");
    success(zkcl(&app, &["lock"]));
    let output = success(zkcl(&app, &["run", "--locked"]));
    assert_eq!(output, "42\n");
}

#[test]
fn publishes_namespaced_asdf_and_native_libraries_and_rebuilds_offline() {
    let temp = temp_project();
    let root = temp.path();
    let registry = root.join("registry");
    let registry_arg = registry.to_str().unwrap();
    success(zkcl(root, &["registry", "init", "private", registry_arg]));
    put(
        root,
        "native/zkcl.sexp",
        r#"(project "example/text" (version "1.4.0") (license "MIT")
          (packages "EXAMPLE.TEXT") (library (files "text.lisp")))"#,
    );
    put(root, "native/LICENSE", "MIT");
    put(
        root,
        "native/text.lisp",
        r#"(defpackage #:example.text (:use #:cl) (:export #:answer))
        (in-package #:example.text) (defmacro answer () 40)"#,
    );
    put(
        root,
        "compat/zkcl.sexp",
        r#"(project "zk/example" (version "2.1.0") (license "MIT")
          (packages "ZK-EXAMPLE") (asdf-system "zk-example" "zk-example.asd")
          (include "example.lisp"))"#,
    );
    put(root, "compat/LICENSE", "MIT");
    put(
        root,
        "compat/zk-example.asd",
        r#"(asdf:defsystem "zk-example" :components ((:file "example")))"#,
    );
    put(
        root,
        "compat/example.lisp",
        r#"(defpackage #:zk-example (:use #:cl) (:export #:extra))
        (in-package #:zk-example) (defmacro extra () 2)"#,
    );
    for module in ["native", "compat"] {
        success(zkcl(
            &root.join(module),
            &["publish", "--registry", registry_arg],
        ));
    }
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0") (packages "EXAMPLE.APP")
          (dependency "text" (module "example/text") (version "^1") (registry "private"))
          (dependency "compat" (module "zk/example") (version "^2")
            (system "zk-example") (registry "private"))
          (binary "app" (files "app.lisp") (entry "EXAMPLE.APP:MAIN")))"#,
    );
    put(
        root,
        "app/app.lisp",
        r#"(defpackage #:example.app (:use #:cl) (:export #:main))
        (in-package #:example.app) (defun main () (format t "~D~%" (+ (example.text:answer) (zk-example:extra))))"#,
    );
    let app = root.join("app");
    success(zkcl(&app, &["lock", "--registry", registry_arg]));
    let lock = fs::read_to_string(app.join("zkcl.lock")).unwrap();
    assert!(lock.contains("(registry \"private\")"));
    assert!(lock.contains("(checksum "));
    success(zkcl(&app, &["fetch", "--registry", registry_arg]));
    fs::remove_dir_all(registry).unwrap();
    fs::remove_dir_all(root.join("native")).unwrap();
    fs::remove_dir_all(root.join("compat")).unwrap();
    let output = success(zkcl(&app, &["run", "--locked", "--offline"]));
    assert_eq!(output, "42\n");
}

#[test]
fn locked_features_select_the_full_registry_name_and_survive_offline() {
    let temp = temp_project();
    let root = temp.path();
    let registry = root.join("registry");
    let registry_arg = registry.to_str().unwrap();
    success(zkcl(root, &["registry", "init", "zkcl", registry_arg]));
    put(
        root,
        "llvm/zkcl.sexp",
        r#"(project "zk/llvm" (version "1.2.3") (license "MIT")
          (packages "ZK-LLVM") (library (files "llvm.lisp")))"#,
    );
    put(root, "llvm/LICENSE", "MIT");
    put(
        root,
        "llvm/llvm.lisp",
        r#"(defpackage #:zk-llvm (:use #:cl) (:export #:host-triple))
        (in-package #:zk-llvm)
        (defun host-triple () "test-host")"#,
    );
    success(zkcl(
        &root.join("llvm"),
        &["publish", "--registry", registry_arg],
    ));
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/compiler" (version "1.0.0") (packages "EXAMPLE.COMPILER")
          (features "llvm" "report")
          (dependency "llvm" (module "zk/llvm") (version "^1")
            (when (all (feature "llvm") (feature "report"))))
          (binary "compiler" (files "app.lisp") (entry "EXAMPLE.COMPILER:MAIN")))"#,
    );
    put(
        root,
        "app/app.lisp",
        r#"(defpackage #:example.compiler (:use #:cl) (:export #:main))
        (in-package #:example.compiler)
        (defun main () (write-line (zk-llvm:host-triple)))"#,
    );
    let app = root.join("app");
    let lock = app.join("zkcl.lock");
    success(zkcl(&app, &["lock"]));
    assert!(!fs::read_to_string(&lock).unwrap().contains("zk/llvm"));
    success(zkcl(
        &app,
        &[
            "lock",
            "--features",
            "report",
            "--features",
            "llvm",
            "--registry",
            registry_arg,
        ],
    ));
    let locked = fs::read_to_string(&lock).unwrap();
    assert!(locked.contains("(features \"llvm\" \"report\")"));
    assert!(locked.contains("(module \"zk/llvm\""));
    let mismatch = zkcl(&app, &["run", "--locked", "--features", "llvm"]);
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("with features"));
    assert_eq!(fs::read_to_string(&lock).unwrap(), locked);
    success(zkcl(&app, &["tree", "--locked"]));
    success(zkcl(
        &app,
        &["fetch", "--all-configurations", "--registry", registry_arg],
    ));
    fs::remove_dir_all(registry).unwrap();
    success(zkcl(&app, &["fetch", "--offline", "--all-configurations"]));
    assert_eq!(
        success(zkcl(
            &app,
            &[
                "run",
                "--locked",
                "--offline",
                "--features",
                "llvm",
                "--features",
                "report",
            ]
        )),
        "test-host\n"
    );
}

#[test]
fn native_alias_and_asdf_consumer_share_the_loaded_library() {
    let temp = temp_project();
    let root = temp.path();
    put(
        root,
        "llvm/zkcl.sexp",
        r#"(project "zk/llvm" (version "1.2.3") (packages "ZK-LLVM")
          (asdf-alias "zk-llvm" (version "1.2.3"))
          (library (files "llvm.lisp")))"#,
    );
    put(
        root,
        "llvm/llvm.lisp",
        r#"(defpackage #:zk-llvm (:use #:cl) (:export #:answer #:*load-count*))
        (in-package #:zk-llvm)
        (defvar *load-count* 0)
        (incf *load-count*)
        (eval-when (:compile-toplevel) (setf (get 'answer 'compile-only) 42))
        (defmacro answer () (get 'answer 'compile-only 0))"#,
    );
    put(
        root,
        "legacy/zkcl.sexp",
        r#"(project "example/legacy" (version "1.0.0") (packages "EXAMPLE.LEGACY")
          (asdf-system "legacy" "legacy.asd")
          (dependency "llvm" (module "zk/llvm") (path "../llvm") (system "zk-llvm")))"#,
    );
    put(
        root,
        "legacy/legacy.asd",
        r#"(asdf:defsystem "legacy" :depends-on ((:version "zk-llvm" "1.2.3"))
          :components ((:file "legacy")))"#,
    );
    put(
        root,
        "legacy/legacy.lisp",
        r#"(defpackage #:example.legacy (:use #:cl) (:export #:answer #:answer-symbol))
        (in-package #:example.legacy)
        (defun answer () (zk-llvm:answer))
        (defun answer-symbol () 'zk-llvm:answer)"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0") (packages "EXAMPLE.APP")
          (dependency "llvm" (module "zk/llvm") (path "../llvm"))
          (dependency "legacy" (module "example/legacy") (path "../legacy") (system "legacy"))
          (binary "app" (files "app.lisp") (entry "EXAMPLE.APP:MAIN")))"#,
    );
    put(
        root,
        "app/app.lisp",
        r#"(defpackage #:example.app (:use #:cl) (:export #:main))
        (in-package #:example.app)
        (defun main ()
          (format t "~D ~A ~D~%" (example.legacy:answer)
                  (eq 'zk-llvm:answer (example.legacy:answer-symbol))
                  zk-llvm:*load-count*))"#,
    );
    let app = root.join("app");
    success(zkcl(&app, &["lock"]));
    let lock = fs::read_to_string(app.join("zkcl.lock")).unwrap();
    assert!(lock.contains("(asdf-alias \"zk-llvm\" \"1.2.3\")"));
    assert_eq!(success(zkcl(&app, &["run", "--locked"])), "42 T 1\n");
}
