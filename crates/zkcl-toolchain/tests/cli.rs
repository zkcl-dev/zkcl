use std::process::{Command, Output};

fn launcher(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl-launcher"))
        .env_remove("ZKCL_TOOLCHAIN")
        .env_remove("NO_COLOR")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn management_help_precedes_installation_and_directory_access() {
    for args in [
        vec!["toolchain", "--help"],
        vec!["toolchain", "install", "stable", "--help"],
        vec!["toolchain", "component", "add", "sdk", "--help"],
        vec![
            "-C",
            "/nonexistent-zkcl-launcher-help-test",
            "toolchain",
            "pin",
            "stable",
            "--help",
        ],
        vec!["__bootstrap", "--prefix", "unused", "--help"],
    ] {
        let output = launcher(&args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage:"), "{args:?}: {stdout}");
        assert!(!stdout.contains('\u{1b}'), "{args:?}: {stdout:?}");
    }
}

#[test]
fn malformed_management_is_a_usage_error_without_installation() {
    for args in [
        vec!["toolchain", "install", "stable", "extra"],
        vec!["toolchain", "pin"],
        vec!["toolchain", "list", "--bogus"],
        vec!["toolchain", "self-update", "0.1.0"],
        vec!["toolchain", "component", "add", "unknown"],
        vec!["-C"],
        vec!["+stable", "+beta", "toolchain", "list"],
        vec!["__bootstrap", "--prefix", "unused"],
    ] {
        let output = launcher(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("error:"), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("unmanaged installation"),
            "{args:?}: {stderr}"
        );
    }
}

#[test]
fn directory_access_errors_are_operational_failures() {
    let output = launcher(&[
        "-C",
        "/nonexistent-zkcl-launcher-directory-test",
        "toolchain",
        "list",
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("change directory"), "{stderr}");
    assert!(!stderr.contains("Usage:"), "{stderr}");
    let output = launcher(&[
        "--color=never",
        "-C",
        "/nonexistent-zkcl-launcher-\u{1b}[2J-test",
        "toolchain",
        "list",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!output.stderr.contains(&0x1b), "{output:?}");
}

#[test]
fn explicit_color_controls_help_usage_and_operational_errors() {
    for (args, stdout) in [
        (
            vec!["toolchain", "install", "stable", "--help", "--color=always"],
            true,
        ),
        (
            vec!["--color=always", "toolchain", "list", "--bogus"],
            false,
        ),
        (
            vec![
                "--color=always",
                "-C",
                "/nonexistent-zkcl-launcher-color-test",
                "toolchain",
                "list",
            ],
            false,
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_zkcl-launcher"))
            .env("NO_COLOR", "1")
            .args(&args)
            .output()
            .unwrap();
        let text = if stdout { output.stdout } else { output.stderr };
        assert!(text.contains(&0x1b), "{args:?}: {text:?}");
    }
    let output = launcher(&["toolchain", "--color=never", "--help"]);
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn installed_launcher_forwards_opaque_arguments_after_applying_directories_once() {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/diagnostics-rework/launcher/forwarding-tests");
    fs::create_dir_all(&scratch).unwrap();
    let fixture = tempfile::tempdir_in(scratch.canonicalize().unwrap()).unwrap();
    let prefix = fixture.path().join("installation");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    let launcher = prefix.join("bin/zkcl");
    fs::copy(env!("CARGO_BIN_EXE_zkcl-launcher"), &launcher).unwrap();
    symlink("zkcl", prefix.join("bin/zkclx")).unwrap();
    fs::write(prefix.join("lock"), "").unwrap();
    fs::write(
        prefix.join("state.json"),
        serde_json::json!({
            "source": "https://example.invalid/releases",
            "default": "stable",
            "channels": { "stable": "1.0.0" }
        })
        .to_string(),
    )
    .unwrap();
    for id in ["1.0.0", "2.0.0"] {
        let release = prefix.join("toolchains").join(id);
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::write(
            release.join("release.json"),
            serde_json::json!({
                "id": id,
                "version": id,
                "commit": "fixture",
                "runtime": { "sha256": "unused", "size": 0 },
                "launcher": { "sha256": "unused", "size": 0 }
            })
            .to_string(),
        )
        .unwrap();
        let binary = release.join("bin/zkcl");
        fs::write(
            &binary,
            b"#!/bin/sh\nprintf '%s\\000' \"$PWD\" \"$ZKCL_TOOLCHAIN_ROOT\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(binary, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let nested = fixture.path().join("project/child/inner");
    fs::create_dir_all(&nested).unwrap();
    fs::write(fixture.path().join("project/zkcl-toolchain"), "1.0.0\n").unwrap();
    let opaque = OsString::from_vec(b"file-\xff.lisp".to_vec());
    let arguments: Vec<OsString> = [
        "--color=never",
        "run",
        "--future-option",
        "future value",
        "--",
        "--help",
        "+beta",
        "-C",
        "never-apply",
    ]
    .map(OsString::from)
    .into_iter()
    .chain([opaque])
    .collect();
    let mut expected: Vec<OsString> = vec![
        nested.as_os_str().to_owned(),
        prefix.join("toolchains/2.0.0").into_os_string(),
    ];
    expected.extend(arguments.clone());
    let expected: Vec<u8> = expected
        .iter()
        .flat_map(|arg| arg.as_bytes().iter().copied().chain([0]))
        .collect();
    for name in ["zkcl", "zkclx"] {
        let output = Command::new(prefix.join("bin").join(name))
            .current_dir(fixture.path())
            .env_remove("ZKCL_TOOLCHAIN")
            .args([
                "-C",
                "project",
                "+2.0.0",
                "-C",
                "child",
                "--directory=inner",
            ])
            .args(&arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stderr.is_empty(), "{name}: {output:?}");
        // /var may be a platform symlink; shells canonicalize PWD when started.
        let fields: Vec<_> = output.stdout.split(|byte| *byte == 0).collect();
        assert_eq!(
            PathBuf::from(OsString::from_vec(fields[0].to_vec()))
                .canonicalize()
                .unwrap(),
            nested.canonicalize().unwrap()
        );
        let expected_fields: Vec<_> = expected.split(|byte| *byte == 0).collect();
        assert_eq!(&fields[1..], &expected_fields[1..], "{name}");
    }
}
