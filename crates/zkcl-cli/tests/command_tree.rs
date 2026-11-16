use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/diagnostics-rework/commands");
    fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    fs::write(
        directory.path().join("zkcl.sexp"),
        r#"(project "test/commands" (version "0.1.0")
      (dependency "missing" (module "test/missing") (version "1.0.0"))
      (binary "first" (files "first.lisp") (entry "CL-USER::FIRST"))
      (binary "second" (files "second.lisp" "helper.lisp") (entry "CL-USER::SECOND")))"#,
    )
    .unwrap();
    directory
}
fn run(directory: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(directory)
        .args(args)
        .output()
        .unwrap()
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_is_available_before_project_access_at_all_legal_positions() {
    let directory = fixture();
    for args in [
        &["run", "--bin", "first", "--help"][..],
        &["build", "--bin", "first", "--help"],
        &["repl", "--locked", "--help"],
        &["test", "--doc", "--help"],
        &["owner", "claim", "test", "--help"],
    ] {
        let output = run(directory.path(), args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        assert!(text(&output.stdout).contains("Usage:"));
        assert!(output.stderr.is_empty());
    }
    assert!(!directory.path().join("zkcl.lock").exists());
    assert!(!directory.path().join("target").exists());
}

#[test]
fn list_and_invalid_target_are_read_only_before_resolution() {
    let directory = fixture();
    let list = run(directory.path(), &["run", "--list"]);
    assert!(list.status.success(), "{}", text(&list.stderr));
    let listing = text(&list.stdout);
    assert_eq!(
        listing
            .lines()
            .map(|line| line.split_whitespace().take(2).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        [vec!["first", "first.lisp"], vec!["second", "second.lisp"]],
    );
    assert!(listing.contains("(+1 file)"));
    for args in [
        &["run"][..],
        &["run", "--bin", "missing"],
        &["build", "--bin", "missing"],
        &["repl", "--bin", "missing", "--no-init"],
    ] {
        let output = run(directory.path(), args);
        assert!(!output.status.success());
        let error = text(&output.stderr);
        assert!(error.contains("Available targets:"), "{args:?}: {error}");
        assert!(error.contains("first"));
        assert!(error.contains("second"));
        assert!(
            error.contains(&format!("zkcl {} --bin first", args[0])),
            "{args:?}: {error}"
        );
    }
    assert!(!directory.path().join("zkcl.lock").exists());
    assert!(!directory.path().join("target").exists());
}

#[test]
fn dynamic_bin_completion_respects_directory_selection() {
    let directory = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(directory.path().parent().unwrap())
        .env("COMPLETE", "bash")
        .env("_CLAP_COMPLETE_INDEX", "5")
        .args(["--", "zkcl", "-C"])
        .arg(directory.path())
        .args(["run", "--bin", "fi"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("first"),
        "{}",
        text(&output.stdout)
    );
    assert!(!text(&output.stdout).contains("second"));
    assert!(!directory.path().join("zkcl.lock").exists());
    assert!(!directory.path().join("target").exists());
}

#[test]
fn usage_errors_exit_before_project_or_input_io() {
    let directory = fixture();
    for (args, expected) in [
        (&["run", "--bogus"][..], "unexpected argument '--bogus'"),
        (
            &["run", "--bin"][..],
            "a value is required for '--bin <NAME>'",
        ),
        (
            &["run", "--list", "--bin", "first"][..],
            "cannot be used with '--bin <NAME>'",
        ),
        (
            &["compile", "--target", "bad-triple-$$$", "missing.lisp"][..],
            "for '--target <TRIPLE>'",
        ),
        (
            &["build", "missing.lisp", "--jobs", "0"][..],
            "for '--jobs <JOBS>'",
        ),
        (
            &["repl", "--init", "missing.lisp", "--no-init"][..],
            "cannot be used with '--no-init'",
        ),
    ] {
        let output = run(directory.path(), args);
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}",);
        assert!(output.stdout.is_empty());
        assert!(
            stderr.contains(expected) && stderr.contains("--help"),
            "{args:?}: {stderr}"
        );
    }
    assert!(!directory.path().join("zkcl.lock").exists());
    assert!(!directory.path().join("target").exists());
}
