#![cfg(all(unix, feature = "ffi"))]

use std::env;
use std::path::Path;
use std::process::Command;
use zkcl::Runtime;

#[test]
fn foreign_aggregate_values_round_trip_through_the_c_abi() {
    let library = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "libffi_aggregate_{}.{}",
        std::process::id(),
        env::consts::DLL_EXTENSION
    ));
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = Command::new(compiler)
        .args(["-std=c11", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        })
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/ffi_aggregate.c"))
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // SAFETY: the Lisp declarations match the types and function signatures in ffi_aggregate.c.
    let rt = unsafe { Runtime::builder().allow_foreign_calls() }
        .start()
        .unwrap();
    let path = library.to_str().unwrap().to_owned();
    rt.register_function("aggregate-library-path", move |rt, _| {
        Ok(vec![rt.make_string(&path)?])
    })
    .unwrap();
    rt.register_function("collect-in-aggregate-test", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    })
    .unwrap();
    rt.load_str(include_str!("common/ffi_aggregate.lisp"))
        .unwrap();
    rt.collect();
    rt.load_str("(check-aggregate-calls) (check-aggregate-callbacks) (release-aggregate-bindings)")
        .unwrap();
    rt.collect();
    // The struct retains the library after its binding dies. An extracted
    // pointer then retains that library after the struct itself is released.
    rt.load_str(
        "(defvar *retained-text* (check-message-owner))
                 (defvar *retained-strings* (extract-captured-strings))",
    )
    .unwrap();
    rt.collect();
    rt.load_str(
        r#"(assert (string= "native" (zk-ffi:read-c-string *retained-text*)))
            (assert (equal '("first answer" "second answer" "Lisp input" "bcd" "xyz")
                           (mapcar #'zk-ffi:read-c-string *retained-strings*)))
            (dolist (p *retained-strings*)
              (let ((length (length (zk-ffi:read-c-string p))))
                (assert (nth-value 1 (ignore-errors (zk-ffi:inc-pointer p (+ length 2)))))))"#,
    )
    .unwrap();
    drop(rt);
    std::fs::remove_file(library).unwrap();
}
