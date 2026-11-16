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
