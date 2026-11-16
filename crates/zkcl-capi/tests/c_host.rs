//! Compile an independent client against the shipped header and shared library.
#![cfg(unix)]

use std::env::{self, consts::DLL_EXTENSION};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn build_library(build_dir: &Path) -> PathBuf {
    // An integration-test build alone does not produce a distributable cdylib.
    let mut cargo = Command::new(env!("CARGO"));
    cargo
        .args([
            "build",
            "--locked",
            "-p",
            "zkcl-capi",
            "--message-format=json-render-diagnostics",
        ])
        .arg("--target-dir")
        .arg(build_dir.parent().unwrap());
    if !cfg!(feature = "ffi") {
        cargo.arg("--no-default-features");
    }
    if !cfg!(debug_assertions) {
        cargo.arg("--release");
    }
    if cfg!(feature = "project") {
        cargo.args(["--features", "project"]);
    } else if cfg!(feature = "delivery") {
        cargo.args(["--features", "delivery"]);
    }
    let output = run(&mut cargo);
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            let msg: serde_json::Value = serde_json::from_str(line).ok()?;
            if msg["target"]["name"] != "zkcl_capi" {
                return None;
            }
            msg["filenames"]
                .as_array()?
                .iter()
                .filter_map(|f| f.as_str().map(Path::new))
                .find(|p| p.extension().is_some_and(|s| s == DLL_EXTENSION))
                .map(Path::to_owned)
        })
        .expect("Cargo reported a zkcl-capi shared library")
}

#[test]
fn c_clients_run_against_shipped_abi() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let library = build_library(Path::new(env!("CARGO_TARGET_TMPDIR")));
    let scratch = crate_dir.join("../../../.agents/tmp/zkcl/c-host");
    std::fs::create_dir_all(&scratch).unwrap();
    let temporary = tempfile::tempdir_in(scratch).unwrap();
    let build_dir = temporary.path();
    let source = build_dir.join(format!("compiled-asset-{}.lisp", std::process::id()));
    std::fs::write(
        &source,
        "(defun asset-value () (values 42 nil))\n(assert (null *load-pathname*))",
    )
    .unwrap();
    let compiled_asset = zkcl::Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    std::fs::remove_file(source).unwrap();
    #[cfg(target_os = "macos")]
    {
        let id = run(Command::new("otool").arg("-D").arg(&library));
        assert_eq!(
            String::from_utf8(id.stdout).unwrap().lines().nth(1),
            Some("@rpath/libzkcl_capi.dylib")
        );
    }
    let libs = library.parent().unwrap();
    let cc = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let foreign_library_path = build_dir.join(format!("libzkcl_ffi_test.{DLL_EXTENSION}"));
    if cfg!(feature = "ffi") {
        run(Command::new(&cc)
            .args(["-std=c11", "-fPIC"])
            .arg(if cfg!(target_os = "macos") {
                "-dynamiclib"
            } else {
                "-shared"
            })
            .arg(crate_dir.join("tests/library.c"))
            .arg("-o")
            .arg(&foreign_library_path));
    }
    let mut clients = vec![
        ("examples/embed.c", "embed"),
        ("tests/abi.c", "abi"),
        ("tests/async.c", "async"),
        ("tests/tooling.c", "tooling"),
        ("tests/debug_event.c", "debug_event"),
    ];
    if cfg!(feature = "project") {
        clients.push(("tests/project.c", "project"));
        copy_tree(
            &crate_dir.join("../../tests/fixtures/embedding-project"),
            &build_dir.join("project-source"),
        );
    }
    for (source, name) in clients {
        let program = build_dir.join(name);
        run(Command::new(&cc)
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread", "-I"])
            .arg(crate_dir.join("include"))
            .arg(crate_dir.join(source))
            .arg("-L")
            .arg(libs)
            .arg("-lzkcl_capi")
            .arg(format!("-Wl,-rpath,{}", libs.display()))
            .arg("-o")
            .arg(&program));
        let mut command = Command::new(program);
        if name == "abi" {
            command.env("ZKCL_TEST_COMPILED_ASSET", &compiled_asset);
        }
        if name == "project" {
            command
                .env("ZKCL_EMBED_PROJECT", build_dir.join("project-source/app"))
                .env("ZKCL_EMBED_BUNDLE", build_dir.join("project-bundle"))
                .env("ZKCL_EMBED_CACHE", build_dir.join("project-cache"));
        }
        if name == "abi" && cfg!(feature = "ffi") {
            command
                .env("ZKCL_TEST_LIBRARY", &foreign_library_path)
                .arg(crate_dir.join("tests/foreign-calls.lisp"));
        }
        let output = run(&mut command);
        if name == "embed" {
            assert_eq!(output.stdout, b"42 40\n");
        }
    }
    std::fs::remove_file(compiled_asset).unwrap();
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}
