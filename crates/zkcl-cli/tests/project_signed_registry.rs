#![cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use url::Url;
use zkcl_registry::sparse_export::{SparseSigningOptions, export_signed_sparse};
use zkcl_registry::{Digest, FileRegistry};

const IDENTITY: &str = "signed-test";

fn publish(registry: &FileRegistry, root: &Path, body: &str) -> Digest {
    let manifest = r#"(project "acme/value" (version "1.0.0")
      (library (files "value.lisp")) (packages "ACME.VALUE"))"#;
    fs::create_dir(root).unwrap();
    fs::write(root.join("zkcl.sexp"), manifest).unwrap();
    fs::write(
        root.join("value.lisp"),
        format!(
            "(defpackage #:acme.value (:use #:cl) (:export #:value))\n\
             (in-package #:acme.value)\n(defun value () {body})\n"
        ),
    )
    .unwrap();
    registry
        .publish(
            "acme/value",
            "1.0.0",
            root,
            &["zkcl.sexp", "value.lisp"],
            manifest,
        )
        .unwrap()
}

fn mirror(registry: &FileRegistry, root: &Path, version: u64) -> PathBuf {
    let keys = Path::new(env!("CARGO_MANIFEST_DIR")).join("../zkcl-registry/src/signed/test_keys");
    fs::create_dir(root).unwrap();
    let trusted_root = root.join("root.json");
    fs::copy(keys.join("root.json"), &trusted_root).unwrap();
    let repository = root.join("mirror");
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(export_signed_sparse(
            registry,
            &repository,
            SparseSigningOptions {
                trusted_root,
                signing_keys: vec![keys.join("test-only-private.pem")],
                version: NonZeroU64::new(version).unwrap(),
                expires: "2099-01-01T00:00:00Z".into(),
            },
        ))
        .unwrap();
    let descriptor = root.join("registry.json");
    let metadata_url = Url::from_directory_path(repository.join("metadata")).unwrap();
    let targets_url = Url::from_directory_path(repository.join("targets")).unwrap();
    fs::write(
        &descriptor,
        serde_json::json!({
            "format": 1,
            "identity": IDENTITY,
            "metadata_url": metadata_url.as_str(),
            "targets_url": targets_url.as_str(),
            "trusted_root": "root.json",
        })
        .to_string(),
    )
    .unwrap();
    descriptor
}

fn zkcl(project: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(project)
        .env("ZKCL_CACHE_DIR", cache)
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn locks_fetches_and_runs_signed_sources_with_exact_cached_digests() {
    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/registry_cli");
    fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::tempdir_in(scratch.canonicalize().unwrap()).unwrap();
    let root = temp.path();
    let registry = FileRegistry::init(root.join("source-registry"), IDENTITY).unwrap();
    let digest = publish(&registry, &root.join("source"), "42");
    let descriptor = mirror(&registry, &root.join("signed"), 1);
    let descriptor = descriptor.to_str().unwrap();
    fs::write(
        root.join("zkcl.sexp"),
        r#"(workspace (members "app")
            (registry "signed-test" (path "signed/registry.json")))"#,
    )
    .unwrap();
    let cache = root.join("cache");
    let app = root.join("app");
    fs::create_dir(&app).unwrap();
    fs::write(
        app.join("zkcl.sexp"),
        r#"(project "example/app" (version "1.0.0")
          (features "optional")
          (dependency "value" (module "acme/value") (version "^1")
             (registry "signed-test") (when (feature "optional")))
          (binary "app" (files "app.lisp") (entry "CL-USER::MAIN")))"#,
    )
    .unwrap();
    fs::write(
        app.join("app.lisp"),
        "(in-package #:cl-user)\n(defun main () (format t \"~D~%\" (acme.value:value)))",
    )
    .unwrap();
    success(zkcl(&app, &cache, &["lock"]));
    success(zkcl(&app, &cache, &["lock", "--features", "optional"]));
    let lock_path = root.join("zkcl.lock");
    let lock = fs::read_to_string(&lock_path).unwrap();
    assert!(lock.contains("(registry \"signed-test\")"));
    assert!(lock.contains(&digest.to_string()));

    let archive = cache.join("store").join(format!("{digest}.tar"));
    assert!(!archive.exists());
    let mirror_archive = root
        .join("signed/mirror/targets/sources")
        .join(format!("{digest}.tar"));
    let bytes = fs::read(&mirror_archive).unwrap();
    fs::write(&mirror_archive, b"changed source archive").unwrap();
    let bad_archive = zkcl(&app, &cache, &["fetch", "--features", "optional"]);
    assert!(!bad_archive.status.success(), "{bad_archive:?}");
    assert!(!archive.exists());
    fs::write(&mirror_archive, bytes).unwrap();
    success(zkcl(root, &cache, &["fetch", "--all-configurations"]));
    assert!(archive.is_file());

    fs::remove_dir_all(root.join("signed")).unwrap();
    success(zkcl(
        &app,
        &cache,
        &[
            "fetch",
            "--offline",
            "--all-configurations",
            "--registry",
            descriptor,
        ],
    ));
    success(zkcl(&app, &cache, &["fetch", "--all-configurations"]));
    let output = success(zkcl(
        &app,
        &cache,
        &["run", "--locked", "--features", "optional"],
    ));
    assert_eq!(output, "42\n");

    fs::remove_file(archive).unwrap();
    let replacement = FileRegistry::init(root.join("replaced-registry"), IDENTITY).unwrap();
    let replacement_digest = publish(&replacement, &root.join("replacement"), "99");
    assert_ne!(digest, replacement_digest);
    let replacement_descriptor = mirror(&replacement, &root.join("signed-new"), 2);
    let mismatch = zkcl(
        &app,
        &cache,
        &[
            "fetch",
            "--all-configurations",
            "--registry",
            replacement_descriptor.to_str().unwrap(),
        ],
    );
    assert!(!mismatch.status.success(), "{mismatch:?}");
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("differs from its locked digest"));
    assert!(
        !cache
            .join("store")
            .join(format!("{replacement_digest}.tar"))
            .exists()
    );
    assert_eq!(fs::read_to_string(lock_path).unwrap(), lock);
}
