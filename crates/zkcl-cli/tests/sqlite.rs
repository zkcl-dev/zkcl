//! Exercise the same SQLite example through the CLI and an embedded runtime.
#![cfg(feature = "ffi")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
#[cfg(any(feature = "cranelift-jit", feature = "direct-jit"))]
use std::time::{Duration, Instant};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/sqlite.lisp")
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
}

fn successful(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn assert_rows(output: Output) {
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "Rows from the SQLite callback",
            "n | 字 | optional",
            "1 | λ | NULL",
            r#"2 | 雪 | """#,
        ]
    );
}

#[test]
fn sqlite_example_prints_same_rows_across_execution_modes() {
    let source = example();
    assert_rows(successful(cli().arg("run").arg(&source)));
    #[cfg(any(feature = "cranelift-jit", feature = "direct-jit"))]
    assert_rows(successful(cli().args(["run", "--jit"]).arg(&source)));

    let fasl = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sqlite-example-{}.zkclfasl", std::process::id()));
    successful(cli().arg("compile").arg(&source).arg(&fasl));
    let output = successful(cli().arg("run").arg(&fasl));
    fs::remove_file(&fasl).unwrap();
    assert_rows(output);
}

#[test]
fn sqlite_callbacks_reenter_lisp_after_errors() {
    // SAFETY: the test supplies the C signatures and loads system SQLite or
    // the trusted library the test runner selects via ZKCL_SQLITE_LIBRARY.
    let runtime = unsafe { zkcl::Runtime::builder().allow_foreign_calls() }
        .start()
        .unwrap();
    runtime.load_file(&example()).unwrap();
    runtime
        .register_function("SQLITE-EXAMPLE::COLLECT-IN-SQLITE-CALLBACK", |rt, _| {
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(include_str!("sqlite_callbacks.lisp"))
        .unwrap();

    runtime
        .load_str(
            "(sqlite-example::test-reentrant-callbacks)
             (sqlite-example::test-sqlite-errors)
             (sqlite-example::test-callback-error)
             (sqlite-example::test-reentrant-callbacks)",
        )
        .unwrap();

    // Attach machine code to the Lisp entry called by the C trampoline.
    // Disable hot compilation so only the explicit request can complete here.
    #[cfg(any(feature = "cranelift-jit", feature = "direct-jit"))]
    {
        runtime.set_jit_threshold(u32::MAX);
        #[cfg(feature = "cranelift-jit")]
        zkcl::enable_jit(&runtime).unwrap();
        #[cfg(all(not(feature = "cranelift-jit"), feature = "direct-jit"))]
        zkcl::enable_direct_jit(&runtime).unwrap();
        runtime
            .eval("(compile nil #'sqlite-example::test-row-callback)")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.poll_jit_completions() == 0 {
            assert!(Instant::now() < deadline, "SQLite callback did not compile");
            std::thread::sleep(Duration::from_millis(2));
        }
        runtime
            .eval("(sqlite-example::test-reentrant-callbacks)")
            .unwrap();
    }
}
