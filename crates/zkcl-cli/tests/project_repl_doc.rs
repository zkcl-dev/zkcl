mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use common::ReplSession;

fn temp_project() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl_project_targets");
    fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    fs::write(
        directory.path().join("zkcl.sexp"),
        r#"(project "example/repl"
      (version "1.0.0") (packages "EXAMPLE.REPL")
      (library (files "library.lisp"))
      (binary "app" (files "app.lisp") (entry "EXAMPLE.REPL:MAIN")))"#,
    )
    .unwrap();
    fs::write(
        directory.path().join("library.lisp"),
        r#"(defpackage #:example.repl (:use #:cl) (:export #:answer #:main))
(in-package #:example.repl)
;;! ```lisp test
;;! (assert (= (answer) 42))
;;! ```
(defun answer () 42)"#,
    )
    .unwrap();
    fs::write(directory.path().join("app.lisp"), "(in-package #:example.repl) (defun main () (error \"entry must not run\")) (defparameter *binary-loaded* t)").unwrap();
    directory
}

#[test]
fn project_json_repl_keeps_session_when_project_reload_fails() {
    let project = temp_project();
    let mut command = common::zkcl();
    command
        .current_dir(project.path())
        .args(["repl", "--json", "--bin", "app"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let mut output = BufReader::new(stdout);
    let mut request = |message: serde_json::Value| {
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let first = request(
        serde_json::json!({"id":1,"source":"(values (example.repl:answer) example.repl::*binary-loaded*)"}),
    );
    assert_eq!(first["values"], serde_json::json!([["42", "T"]]), "{first}");
    fs::write(
        project.path().join("library.lisp"),
        "(error \"broken startup\")",
    )
    .unwrap();
    let reload = request(serde_json::json!({"id":2,"reload":true}));
    assert!(!reload["error"].is_null(), "{reload}");
    let retained = request(serde_json::json!({"id":3,"source":"(example.repl:answer)"}));
    assert_eq!(
        retained["values"],
        serde_json::json!([["42"]]),
        "{retained}"
    );
    drop(request);
    drop(input);
    assert!(session.wait().success());
}

#[test]
fn project_refresh_recompiles_macro_consumers_and_retains_session_state() {
    use serde_json::json;

    let project = temp_project();
    let root = project.path();
    let manifest = r#"(project "example/refresh"
      (version "1.0.0") (packages "Mixed.Case")
      (repl (package "Mixed.Case"))
      (library (files "library.lisp" "app.lisp")))"#;
    fs::write(root.join("zkcl.sexp"), manifest).unwrap();
    let define_macro = |value| format!(
        "(defpackage \"Mixed.Case\" (:use #:cl) (:export #:answer))
         (in-package \"Mixed.Case\")
         (defmacro value () {value})
         (defvar *state* 0)"
    );
    fs::write(root.join("library.lisp"), define_macro(41)).unwrap();
    fs::write(root.join("app.lisp"), "(in-package \"Mixed.Case\") (defun answer () (value))").unwrap();

    let mut command = common::zkcl();
    command.current_dir(root).args(["repl", "--json"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let mut output = BufReader::new(stdout);
    let mut request = |message: serde_json::Value| {
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };

    let first = request(json!({"id":1,"source":"(progn (setf *state* 77 *read-base* 16) (values (answer) (package-name *package*)))"}));
    assert_eq!(first["values"], json!([["41", "\"Mixed.Case\""]]), "{first}");

    fs::write(root.join("library.lisp"), define_macro(42)).unwrap();
    fs::write(root.join("app.lisp"), "(error \"second component failed\")").unwrap();
    let partial = request(json!({"id":2,"refresh":true}));
    assert!(!partial["error"].is_null(), "{partial}");
    assert_eq!(partial["refresh"]["files"].as_array().unwrap().len(), 1, "{partial}");
    assert_eq!(partial["refresh"]["remaining"], 1, "{partial}");

    fs::write(root.join("app.lisp"), "(in-package \"Mixed.Case\") (defun answer () (value))").unwrap();
    let refreshed = request(json!({"id":3,"refresh":true}));
    assert!(refreshed["error"].is_null(), "{refreshed}");
    assert_eq!(refreshed["refresh"]["files"].as_array().unwrap().len(), 1, "{refreshed}");
    let state = request(json!({"id":4,"source":"(values (answer) *state* *read-base*)"}));
    assert_eq!(state["values"], json!([["42", "77", "16"]]), "{state}");

    fs::write(root.join("zkcl.sexp"), format!("; comment only\n{manifest}")).unwrap();
    let unchanged = request(json!({"id":5,"refresh":true}));
    assert!(unchanged["error"].is_null(), "{unchanged}");
    assert_eq!(unchanged["refresh"]["files"], json!([]), "{unchanged}");
    fs::write(root.join("zkcl.sexp"), manifest.replace("1.0.0", "2.0.0")).unwrap();
    let changed = request(json!({"id":6,"refresh":true}));
    assert!(changed["error"].to_string().contains(":reload"), "{changed}");

    drop(request);
    drop(input);
    assert!(session.wait().success());
}

#[test]
fn new_project_repl_selects_its_single_binary_and_enters_its_package() {
    let project = temp_project();
    let root = project.path();
    let created = common::zkcl()
        .current_dir(root)
        .args(["new", "hello", "--name", "example/hello"])
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", String::from_utf8_lossy(&created.stderr));
    fs::write(root.join("hello/check.lisp"), "(assert (fboundp 'example.hello:main))").unwrap();
    let output = common::zkcl()
        .current_dir(root.join("hello"))
        .args(["repl", "--load", "check.lisp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(b"(package-name *package*)\n")?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "\"EXAMPLE.HELLO\"");
}

#[test]
fn bare_repl_skips_even_an_invalid_project_manifest() {
    let project = temp_project();
    fs::write(project.path().join("zkcl.sexp"), "invalid manifest").unwrap();
    let output = common::zkcl()
        .current_dir(project.path())
        .args(["repl", "--no-project", "--load", "library.lisp"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[cfg(feature = "asdf")]
#[test]
fn repl_can_preload_asdf_or_load_a_standalone_secondary_system() {
    let project = temp_project();
    let root = project.path();
    fs::write(root.join("zkcl.sexp"), "invalid manifest").unwrap();
    fs::write(root.join("uses-asdf.lisp"), "(assert (asdf:asdf-version))").unwrap();
    let output = common::zkcl()
        .current_dir(root)
        .args(["repl", "--no-project", "--asdf", "--load", "uses-asdf.lisp"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let sources = root.join("带 空格");
    fs::create_dir(&sources).unwrap();
    fs::write(
        sources.join("definitions.asd"),
        r#"(defvar cl-user::*definition-loads* 0)
           (incf cl-user::*definition-loads*)
           (asdf:defsystem "definitions")
           (asdf:defsystem "repl-fixture")
           (asdf:defsystem "repl-fixture/tools" :depends-on ("repl-other")
             :components ((:file "tool")))"#,
    ).unwrap();
    fs::write(sources.join("repl-other.asd"), r#"(asdf:defsystem "repl-other")"#).unwrap();
    fs::write(sources.join("repl-fixture.asd"), "(error \"the explicit definition takes priority\")").unwrap();
    fs::write(sources.join("tool.lisp"), "(defparameter cl-user::*repl-answer* 42)").unwrap();
    fs::write(root.join("answer.lisp"), "(assert (= cl-user::*repl-answer* 42)) (assert (= cl-user::*definition-loads* 1))").unwrap();
    let output = common::zkcl()
        .current_dir(root)
        .env("XDG_CACHE_HOME", root.join("asdf-cache"))
        .args(["repl", "--system", "repl-fixture/tools", "--asd", "带 空格/definitions.asd", "--load", "answer.lisp"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn project_documentation_tests_include_library_and_readme_without_test_target() {
    let project = temp_project();
    fs::write(
        project.path().join("README.md"),
        "```lisp test\n(assert (= (example.repl:answer) 42))\n```\n",
    )
    .unwrap();
    let output = common::zkcl()
        .current_dir(project.path())
        .args(["test", "--doc"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 passed, 0 failed"));
    let locked = common::zkcl()
        .current_dir(project.path())
        .arg("lock")
        .output()
        .unwrap();
    assert!(
        locked.status.success(),
        "{}",
        String::from_utf8_lossy(&locked.stderr)
    );
    fs::write(
        project.path().join("README.md"),
        "```lisp test\n(assert nil)\n```\n",
    )
    .unwrap();
    let output = common::zkcl()
        .current_dir(project.path())
        .args(["test", "--doc", "--locked", "--offline"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("README.md:2:"));
}
