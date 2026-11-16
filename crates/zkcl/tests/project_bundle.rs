#![cfg(feature = "delivery")]
use std::fs;
use std::path::Path;
use zkcl::Runtime;
use zkcl::project::{
    ProjectBundle,
    delivery::{self, Delivery, StartupStep},
};

#[test]
fn prepared_bundle_loads_without_the_source_project_feature() {
    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/project-bundle");
    fs::create_dir_all(&scratch).unwrap();
    let temporary = tempfile::tempdir_in(scratch).unwrap();
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
