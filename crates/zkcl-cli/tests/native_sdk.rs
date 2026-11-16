//! Opt-in executable SDK checks; the SDK is built/installed outside these tests.
#![cfg(unix)]
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn command(sdk: &Path, traps: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zkcl"));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![traps.to_owned()];
    paths.extend(std::env::split_paths(&path));
    command
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("ZKCL_SDK_ROOT", sdk);
    command
}

#[test]
#[ignore = "requires a matching installed native SDK in ZKCL_TEST_SDK"]
fn installed_sdk_builds_and_relocates_without_cargo_or_rustc() {
    let sdk = PathBuf::from(std::env::var_os("ZKCL_TEST_SDK").expect("ZKCL_TEST_SDK"))
        .canonicalize()
        .unwrap();
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/native-sdk");
    fs::create_dir_all(&parent).unwrap();
    let temp = tempfile::tempdir_in(parent).unwrap();
    let root = temp.path();
    let source = root.join("source");
    let traps = root.join("traps");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&traps).unwrap();
    for name in ["cargo", "rustc"] {
        let path = traps.join(name);
        fs::write(
            &path,
            "#!/bin/sh\necho 'unexpected Rust toolchain invocation' >&2\nexit 99\n",
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        source.join("zkcl.sexp"),
        r#"(project "sdk/probe" (version "1.0.0")
      (packages "SDK.PROBE") (library (files "package.lisp" "left.lisp"))
      (binary "probe" (files "main.lisp") (entry "SDK.PROBE:MAIN")))"#,
    )
    .unwrap();
    fs::write(
        source.join("package.lisp"),
        r#"(defpackage #:sdk.probe (:use #:cl) (:export #:main))
      (in-package #:sdk.probe) (defparameter *order* (list :package))"#,
    )
    .unwrap();
    fs::write(
        source.join("left.lisp"),
        r#"(in-package #:sdk.probe)
      (push :left *order*) (defun add-left (x) (+ x 19))
      (defun distinct-literal () '(19 55))"#,
    )
    .unwrap();
    fs::write(
        source.join("main.lisp"),
        r#"(in-package #:sdk.probe) (push :main *order*)
      (defun main ()
        (format t "~D|~A|~A|~A~%" (add-left 23)
          (first (zk:command-line-arguments))
          (equal (distinct-literal) '(19 55))
          (equal *order* '(:main :left :package)))
        (zk:set-exit-code 7))"#,
    )
    .unwrap();
    let built = root.join("built");
    fs::create_dir(&built).unwrap();
    let output = command(&sdk, &traps)
        .current_dir(&source)
        .args(["build", "--bin", "probe", "--backend", "direct", "-o"])
        .arg(built.join("probe"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let emitted = root.join("emitted");
    let output = command(&sdk, &traps)
        .current_dir(&source)
        .args([
            "build",
            "--bin",
            "probe",
            "--backend",
            "direct",
            "--emit-dir",
        ])
        .arg(&emitted)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let link: serde_json::Value =
        serde_json::from_slice(&fs::read(emitted.join("link.json")).unwrap()).unwrap();
    let executable = link["output"].as_str().unwrap();
    assert!(emitted.join(format!("{executable}.zkcl")).is_dir());
    fs::remove_dir_all(source).unwrap();
    let installed = root.join("installed");
    fs::rename(built, &installed).unwrap();
    let run = Command::new(installed.join("probe"))
        .arg("argument")
        .output()
        .unwrap();
    assert_eq!(
        run.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, b"42|argument|T|T\n");
}
