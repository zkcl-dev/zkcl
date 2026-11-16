use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "requires ZKCL_WASM_RUNTIME or an installed Wasm SDK in ZKCL_TEST_WASM_SDK"]
fn project_wasm_preserves_units_native_tables_target_and_arguments() {
    let sdk = std::env::var_os("ZKCL_TEST_WASM_SDK").map(PathBuf::from);
    let runtime = std::env::var_os("ZKCL_WASM_RUNTIME").map(PathBuf::from);
    assert!(
        sdk.is_some() || runtime.is_some(),
        "provide a Wasm SDK or runtime template"
    );
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/native-sdk");
    fs::create_dir_all(&parent).unwrap();
    let temp = tempfile::tempdir_in(parent).unwrap();
    let root = temp.path();
    fs::write(
        root.join("zkcl.sexp"),
        r#"(project "wasm/probe" (version "1.0.0")
      (packages "WASM.PROBE") (library (files "package.lisp" "left.lisp"))
      (binary "probe" (files "main.lisp") (entry "WASM.PROBE:MAIN")))"#,
    )
    .unwrap();
    fs::write(
        root.join("package.lisp"),
        r#"(defpackage #:wasm.probe (:use #:cl) (:export #:main))
      (in-package #:wasm.probe) (defparameter *order* (list :package))
      (defparameter *target* #+wasm32 :wasm #-wasm32 :other)"#,
    )
    .unwrap();
    fs::write(
        root.join("left.lisp"),
        r#"(in-package #:wasm.probe) (push :left *order*)
      (defun add-two (x) (+ x 2))
      (defun add-left (x &optional (extra 0)) (+ x 19 extra))
      (defun distinct-literal () '(19 55))"#,
    )
    .unwrap();
    fs::write(
        root.join("main.lisp"),
        r#"(in-package #:wasm.probe) (push :main *order*)
      (defun own-leaf (x) (+ x 3))
      (defun main ()
        (format t "~D|~D|~A|~A|~A|~A~%" (add-left (add-two 21)) (own-leaf 39)
          (first (zk:command-line-arguments))
          (equal (distinct-literal) '(19 55))
          (equal *order* '(:main :left :package)) *target*)
        (zk:set-exit-code 7))"#,
    )
    .unwrap();
    let output = root.join("probe.wasm");
    let mut build = Command::new(env!("CARGO_BIN_EXE_zkcl"));
    build.current_dir(root).args([
        "build",
        "--bin",
        "probe",
        "--target",
        "wasm32-unknown-unknown",
    ]);
    if let Some(sdk) = sdk {
        build
            .env("ZKCL_SDK_ROOT", sdk.canonicalize().unwrap())
            .env_remove("ZKCL_WASM_RUNTIME");
    } else {
        build
            .arg("--wasm-runtime")
            .arg(runtime.unwrap().canonicalize().unwrap());
    }
    let built = build.arg("-o").arg(&output).output().unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .arg("run")
        .arg(output)
        .args(["--", "argument"])
        .output()
        .unwrap();
    assert_eq!(
        run.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"42|42|argument|T|T|WASM\n");
}
