#![cfg(feature = "delivery")]
use std::fs;
use std::path::Path;
use zkcl::Runtime;
use zkcl::project::{
    BundleError, ProjectBundle,
    delivery::{self, Delivery, StartupStep},
};

fn scratch() -> tempfile::TempDir {
    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/project-bundle");
    fs::create_dir_all(&scratch).unwrap();
    tempfile::tempdir_in(scratch).unwrap()
}

#[test]
fn prepared_bundle_loads_without_the_source_project_feature() {
    let temporary = scratch();
    let source = temporary.path().join("input.lisp");
    fs::write(&source, "(defpackage #:bundle-test (:use #:cl) (:export #:main)) (in-package #:bundle-test) (defparameter *origin* *load-truename*) (defun main () (values 42 (namestring *origin*)))").unwrap();
    let runtime = Runtime::start().unwrap();
    let units = delivery::compile_files(
        &runtime,
        &[source.clone()],
        &temporary.path().join("compiled"),
    )
    .unwrap();
    let delivery = Delivery {
        units,
        steps: vec![StartupStep::LoadFasl(0)],
        ..Delivery::default()
    };
    let bundle = ProjectBundle::write(
        &delivery,
        &temporary.path().join("bundle"),
        &runtime.compilation_target().to_string(),
        Some("BUNDLE-TEST:MAIN".into()),
    )
    .unwrap();
    fs::remove_file(source).unwrap();
    let other = Runtime::start().unwrap();
    bundle
        .load(&other, &temporary.path().join("cache"))
        .unwrap();
    let values = bundle.call_entry(&other).unwrap();
    assert_eq!(values[0].as_i64(), Some(42));
    assert_eq!(
        other.string(&values[1]),
        Some(
            bundle
                .root()
                .join("0000.zkclfasl")
                .to_str()
                .unwrap()
                .to_owned()
        )
    );
}

#[test]
fn invalid_startup_plan_does_not_publish_bundle() {
    let temporary = scratch();
    let destination = temporary.path().join("bundle");
    let delivery = Delivery {
        steps: vec![StartupStep::LoadFasl(0)],
        ..Delivery::default()
    };
    let result = ProjectBundle::write(&delivery, &destination, "wasm32-unknown-unknown", None);
    assert!(
        matches!(result, Err(BundleError::Invalid(message)) if message.contains("missing initializer"))
    );
    assert!(!destination.exists());
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);

    ProjectBundle::write(
        &Delivery::default(),
        &destination,
        "wasm32-unknown-unknown",
        None,
    )
    .unwrap();
}
