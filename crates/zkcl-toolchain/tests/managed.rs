#![cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use std::env::consts::{ARCH, OS};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LICENSE: &[u8] = b"Fixture license\n";

fn host() -> &'static str {
    match (OS, ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => unreachable!(),
    }
}

struct Archive {
    path: PathBuf,
    sha256: String,
    size: u64,
}

impl Archive {
    fn write(directory: &Path, stem: &str, entries: &[(&str, u32, &[u8])]) -> Self {
        let path = directory.join(format!("{stem}.tar.gz"));
        let gzip = GzEncoder::new(File::create(&path).unwrap(), Compression::none());
        let mut archive = tar::Builder::new(gzip);
        for &(name, mode, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(mode);
            header.set_size(body.len() as u64);
            archive
                .append_data(&mut header, format!("{stem}/{name}"), body)
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();

        let mut sha256 = Sha256::new();
        let size = io::copy(&mut File::open(&path).unwrap(), &mut sha256).unwrap();
        let archive = Self {
            path,
            sha256: format!("{:x}", sha256.finalize()),
            size,
        };
        archive.write_checksum(directory);
        archive
    }

    fn write_checksum(&self, directory: &Path) {
        let name = self.path.file_name().unwrap().to_str().unwrap();
        fs::write(
            directory.join(format!("{name}.sha256")),
            format!("{}  {name}\n", self.sha256),
        )
        .unwrap();
    }

    fn manifest_entry(&self) -> Value {
        json!({ "sha256": self.sha256, "size": self.size })
    }
}

fn launcher_archive(directory: &Path, executable: &[u8]) -> Archive {
    Archive::write(
        directory,
        &format!("zkcl-launcher-{}", host()),
        &[("bin/zkcl", 0o755, executable), ("LICENSE", 0o644, LICENSE)],
    )
}

fn publish_release(mirror: &Path, id: &str, version: &str, launcher: &Archive) -> PathBuf {
    let tag = if id.starts_with("nightly-") {
        id.to_owned()
    } else {
        format!("v{id}")
    };
    let directory = mirror.join("download").join(tag);
    fs::create_dir_all(&directory).unwrap();

    let executable = format!(
        "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 64\nprintf '%s\\n' 'zkcl {version}'\n"
    );
    let runtime = Archive::write(
        &directory,
        &format!("zkcl-{id}-{}", host()),
        &[
            ("bin/zkcl", 0o755, executable.as_bytes()),
            ("LICENSE", 0o644, LICENSE),
            ("LICENSE-ASDF", 0o644, LICENSE),
        ],
    );
    fs::hard_link(
        &launcher.path,
        directory.join(launcher.path.file_name().unwrap()),
    )
    .unwrap();
    launcher.write_checksum(&directory);

    let sdk = Archive::write(
        &directory,
        &format!("zkcl-sdk-{id}-{}", host()),
        &[
            (
                &format!("sdk/{}/lib/libzkcl_capi.a", host()),
                0o644,
                b"static SDK fixture",
            ),
            (
                &format!("sdk/{}/include/zkcl.h", host()),
                0o644,
                b"header fixture",
            ),
            (&format!("sdk/{}/runtime.json", host()), 0o644, b"{}"),
        ],
    );
    let wasm_metadata =
        json!({"kind": "wasm", "target": "wasm32-unknown-unknown", "version": version}).to_string();
    let wasm_sdk = Archive::write(
        &directory,
        &format!("zkcl-sdk-{id}-wasm32-unknown-unknown"),
        &[
            (
                "sdk/wasm32-unknown-unknown/runtime.wasm",
                0o644,
                b"\0asm\x01\0\0\0",
            ),
            (
                "sdk/wasm32-unknown-unknown/runtime.json",
                0o644,
                wasm_metadata.as_bytes(),
            ),
        ],
    );
    let manifest = json!({
        "format": 1,
        "toolchain": id,
        "version": version,
        "commit": "0123456789012345678901234567890123456789",
        "targets": {
            (host()): {
                "runtime": runtime.manifest_entry(),
                "launcher": launcher.manifest_entry(),
                "sdk": { (host()): sdk.manifest_entry(), "wasm32-unknown-unknown": wasm_sdk.manifest_entry() },
            }
        }
    });
    fs::write(
        directory.join("zkcl-toolchain-v1.json"),
        manifest.to_string(),
    )
    .unwrap();
    runtime.path
}

fn point_channel(mirror: &Path, channel: &str, id: &str) {
    let directory = if channel == "stable" {
        mirror.join("latest/download")
    } else {
        mirror.join("download").join(channel)
    };
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("zkcl-channel-v1.txt"), format!("{id}\n")).unwrap();
}

fn command(executable: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .env_remove("ZKCL_TOOLCHAIN")
        .args(args);
    command
}

#[track_caller]
fn success(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} exited with {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[track_caller]
fn failure(command: &mut Command, message: &str) {
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(message),
        "{command:?} exited with {}, expected {message:?}\n{stderr}",
        output.status
    );
}

#[test]
fn manages_toolchains_installed_by_the_stdin_bootstrap() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch =
        crate_dir.join("../../../.agents/tmp/zkcl/diagnostics-rework/launcher/managed-tests");
    fs::create_dir_all(&scratch).unwrap();
    let fixture = tempfile::tempdir_in(scratch.canonicalize().unwrap()).unwrap();
    let root = fixture.path();
    let mirror = root.join("release mirror");
    let prefix = root.join("install prefix");
    let launcher = launcher_archive(
        root,
        &fs::read(env!("CARGO_BIN_EXE_zkcl-launcher")).unwrap(),
    );

    publish_release(&mirror, "1.2.0", "1.2.0", &launcher);
    point_channel(&mirror, "stable", "1.2.0");
    let script = File::open(crate_dir.join("../../scripts/install-toolchain.sh")).unwrap();
    success(
        Command::new("sh")
            .current_dir(root)
            .env("TMPDIR", root)
            .args(["-s", "--", "--toolchain", "stable", "--from"])
            .arg(&mirror)
            .arg("--prefix")
            .arg(&prefix)
            .stdin(Stdio::from(script)),
    );

    let zkcl = prefix.join("bin/zkcl");
    let zkclx = prefix.join("bin/zkclx");
    let run = |args: &[&str]| success(&mut command(&zkcl, root, args));
    assert_eq!(run(&["--version"]), "zkcl 1.2.0\n");
    assert_eq!(
        success(&mut command(&zkclx, root, &["--version"])),
        "zkcl 1.2.0\n"
    );

    let mut available_targets = [host(), "wasm32-unknown-unknown"];
    available_targets.sort();
    assert_eq!(
        run(&["toolchain", "target", "list"]),
        format!("{}\n", available_targets.join("\n"))
    );
    run(&["toolchain", "target", "add", "wasm32-unknown-unknown"]);
    let installed_wasm = prefix.join("toolchains/1.2.0/sdk/wasm32-unknown-unknown");
    assert_eq!(
        fs::read(installed_wasm.join("runtime.wasm")).unwrap(),
        b"\0asm\x01\0\0\0"
    );
    run(&["toolchain", "target", "remove", "wasm32-unknown-unknown"]);
    assert!(!installed_wasm.exists());
    run(&["toolchain", "target", "add", "wasm32-unknown-unknown"]);
    assert!(installed_wasm.join("runtime.json").is_file());
    let release_directory = mirror.join("download/v1.2.0");
    let manifest_path = release_directory.join("zkcl-toolchain-v1.json");
    let original_manifest = fs::read(&manifest_path).unwrap();
    let wasm_archive = release_directory.join("zkcl-sdk-1.2.0-wasm32-unknown-unknown.tar.gz");
    let original_wasm_archive = fs::read(&wasm_archive).unwrap();
    for (magic, version, error) in [
        (
            b"invalid!".as_slice(),
            "1.2.0",
            "invalid Wasm SDK runtime header",
        ),
        (
            b"\0asm\x01\0\0\0".as_slice(),
            "9.0.0",
            "Wasm SDK metadata does not match",
        ),
    ] {
        run(&["toolchain", "target", "remove", "wasm32-unknown-unknown"]);
        let metadata =
            json!({"kind": "wasm", "target": "wasm32-unknown-unknown", "version": version})
                .to_string();
        let archive = Archive::write(
            &release_directory,
            "zkcl-sdk-1.2.0-wasm32-unknown-unknown",
            &[
                ("sdk/wasm32-unknown-unknown/runtime.wasm", 0o644, magic),
                (
                    "sdk/wasm32-unknown-unknown/runtime.json",
                    0o644,
                    metadata.as_bytes(),
                ),
            ],
        );
        let mut manifest: Value = serde_json::from_slice(&original_manifest).unwrap();
        manifest["targets"][host()]["sdk"]["wasm32-unknown-unknown"] = archive.manifest_entry();
        fs::write(&manifest_path, manifest.to_string()).unwrap();
        failure(
            &mut command(
                &zkcl,
                root,
                &["toolchain", "target", "add", "wasm32-unknown-unknown"],
            ),
            error,
        );
        assert!(!installed_wasm.exists());
        fs::write(&manifest_path, &original_manifest).unwrap();
        fs::write(&wasm_archive, &original_wasm_archive).unwrap();
        run(&["toolchain", "target", "add", "wasm32-unknown-unknown"]);
    }

    run(&["toolchain", "target", "add", host()]);
    let installed_sdk = prefix.join("toolchains/1.2.0/sdk").join(host());
    assert_eq!(
        fs::read(installed_sdk.join("lib/libzkcl_capi.a")).unwrap(),
        b"static SDK fixture"
    );
    assert_eq!(
        run(&["toolchain", "component", "list"]),
        "sdk (installed)\n"
    );
    run(&["toolchain", "component", "remove", "sdk"]);
    assert!(!installed_sdk.exists());
    let sdk_archive = mirror
        .join("download/v1.2.0")
        .join(format!("zkcl-sdk-1.2.0-{}.tar.gz", host()));
    let valid_archive = fs::read(&sdk_archive).unwrap();
    let mut corrupted = valid_archive.clone();
    corrupted[0] ^= 1;
    fs::write(&sdk_archive, corrupted).unwrap();
    failure(
        &mut command(&zkcl, root, &["toolchain", "component", "add", "sdk"]),
        "archive SHA-256",
    );
    assert!(!installed_sdk.exists());
    fs::write(&sdk_archive, valid_archive).unwrap();
    run(&["toolchain", "component", "add", "sdk"]);
    failure(
        &mut command(
            &zkcl,
            root,
            &["toolchain", "target", "add", "unknown-target"],
        ),
        "release does not provide",
    );

    let project = root.join("project");
    let nested = project.join("src/nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(project.join("zkcl.sexp"), "(project \"example/pinned\")\n").unwrap();
    let pin = project.join("zkcl-toolchain");
    let project_run = |args: &[&str]| success(&mut command(&zkcl, &nested, args));
    project_run(&["toolchain", "pin", "stable"]);
    assert_eq!(fs::read_to_string(&pin).unwrap(), "1.2.0\n");

    publish_release(&mirror, "1.1.0", "1.1.0", &launcher);
    run(&["toolchain", "install", "1.1.0"]);
    assert_eq!(run(&["+1.1.0", "--version"]), "zkcl 1.1.0\n");
    publish_release(&mirror, "1.3.0", "1.3.0", &launcher);
    point_channel(&mirror, "stable", "1.3.0");
    project_run(&["toolchain", "upgrade"]);
    assert_eq!(run(&["--version"]), "zkcl 1.3.0\n");
    assert_eq!(project_run(&["--version"]), "zkcl 1.2.0\n");
    let nested_text = nested.to_str().unwrap();
    assert_eq!(run(&["-C", nested_text, "--version"]), "zkcl 1.2.0\n");
    assert_eq!(
        run(&["+1.3.0", "-C", nested_text, "--version"]),
        "zkcl 1.3.0\n"
    );
    assert_eq!(
        run(&["--directory", nested_text, "+1.3.0", "--version"]),
        "zkcl 1.3.0\n"
    );
    assert_eq!(
        run(&["-C", nested_text, "toolchain", "which"]),
        format!("{}\n", prefix.join("toolchains/1.2.0/bin/zkcl").display())
    );
    failure(
        &mut command(&zkcl, root, &["-C"]),
        "--directory <DIRECTORY>",
    );

    assert_eq!(fs::read_to_string(&pin).unwrap(), "1.2.0\n");

    let updated_launcher = b"#!/bin/sh\nprintf '%s\\n' 'updated launcher'\n";
    let update_dir = root.join("launcher update");
    fs::create_dir(&update_dir).unwrap();
    let next_launcher = launcher_archive(&update_dir, updated_launcher);
    publish_release(&mirror, "1.4.0-beta.1", "1.4.0-beta.1", &next_launcher);
    point_channel(&mirror, "beta", "1.4.0-beta.1");
    run(&["toolchain", "install", "beta"]);

    let previous_nightly = "nightly-2026-11-22";
    let current_nightly = "nightly-2026-11-23";
    publish_release(&mirror, previous_nightly, "1.4.0", &launcher);
    point_channel(&mirror, "nightly", previous_nightly);
    run(&["toolchain", "install", "nightly"]);
    let nightly_project = root.join("nightly project");
    fs::create_dir(&nightly_project).unwrap();
    success(&mut command(
        &zkcl,
        &nightly_project,
        &["toolchain", "pin", "nightly"],
    ));
    assert_eq!(
        fs::read_to_string(nightly_project.join("zkcl-toolchain")).unwrap(),
        "nightly-2026-11-22\n"
    );
    publish_release(&mirror, current_nightly, "1.4.0", &launcher);
    point_channel(&mirror, "nightly", current_nightly);
    run(&["toolchain", "install", "nightly"]);
    assert_eq!(
        run(&["+nightly", "--version"]),
        "zkcl 1.4.0 (nightly-2026-11-23)\n"
    );
    assert_eq!(
        success(&mut command(&zkcl, &nightly_project, &["--version"])),
        "zkcl 1.4.0 (nightly-2026-11-22)\n"
    );
    point_channel(&mirror, "nightly", previous_nightly);
    failure(
        &mut command(&zkcl, root, &["toolchain", "install", "nightly"]),
        "older than the installed",
    );

    assert_eq!(
        success(command(&zkcl, &nested, &["--version"]).env("ZKCL_TOOLCHAIN", "beta")),
        "zkcl 1.4.0-beta.1\n"
    );
    assert_eq!(
        success(
            command(&zkcl, &nested, &["+nightly-2026-11-22", "--version"])
                .env("ZKCL_TOOLCHAIN", "beta"),
        ),
        "zkcl 1.4.0 (nightly-2026-11-22)\n"
    );
    let listed = project_run(&["toolchain", "list"]);
    for entry in [
        "1.1.0",
        "1.2.0",
        "1.3.0",
        "1.4.0-beta.1",
        previous_nightly,
        current_nightly,
        "Default: stable (1.3.0)",
        "Active: 1.2.0",
    ] {
        assert!(
            listed.lines().any(|line| line == entry),
            "missing {entry:?}\n{listed}"
        );
    }

    let corrupt = publish_release(&mirror, "1.4.0", "1.4.0", &launcher);
    let mut archive_bytes = fs::read(&corrupt).unwrap();
    archive_bytes[0] ^= 1;
    fs::write(corrupt, archive_bytes).unwrap();
    point_channel(&mirror, "stable", "1.4.0");
    failure(
        &mut command(&zkcl, root, &["toolchain", "upgrade"]),
        "SHA-256",
    );
    assert_eq!(run(&["--version"]), "zkcl 1.3.0\n");
    failure(
        &mut command(&zkcl, root, &["+1.4.0", "--version"]),
        "not installed",
    );

    let offline = root.join("offline mirror");
    fs::rename(&mirror, &offline).unwrap();
    project_run(&["toolchain", "install"]);
    assert_eq!(project_run(&["--version"]), "zkcl 1.2.0\n");
    let nested_text = nested.to_str().unwrap();
    assert_eq!(run(&["-C", nested_text, "--version"]), "zkcl 1.2.0\n");
    assert_eq!(
        run(&["+1.3.0", "-C", nested_text, "--version"]),
        "zkcl 1.3.0\n"
    );
    assert_eq!(
        run(&["--directory", nested_text, "+1.3.0", "--version"]),
        "zkcl 1.3.0\n"
    );
    assert_eq!(
        run(&["-C", nested_text, "toolchain", "which"]),
        format!("{}\n", prefix.join("toolchains/1.2.0/bin/zkcl").display())
    );
    failure(
        &mut command(&zkcl, root, &["-C"]),
        "--directory <DIRECTORY>",
    );

    run(&["toolchain", "default", "1.2.0"]);
    assert_eq!(run(&["--version"]), "zkcl 1.2.0\n");
    run(&["toolchain", "default", "stable"]);
    project_run(&["toolchain", "unpin"]);
    assert!(!pin.exists());
    assert_eq!(project_run(&["--version"]), "zkcl 1.3.0\n");

    failure(
        &mut command(&zkcl, root, &["toolchain", "uninstall", "1.3.0"]),
        "default toolchain",
    );
    run(&["toolchain", "uninstall", "1.1.0"]);
    failure(
        &mut command(&zkcl, root, &["+1.1.0", "--version"]),
        "not installed",
    );

    fs::rename(&offline, &mirror).unwrap();
    run(&["toolchain", "self-update", "beta"]);
    assert_eq!(fs::read(&zkcl).unwrap(), updated_launcher);
    assert_eq!(
        success(&mut command(&zkclx, root, &["--version"])),
        "updated launcher\n"
    );
}
