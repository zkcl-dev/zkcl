#![cfg(feature = "project")]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use zkcl::project::{
    LoadOptions, Project, ProjectBundle, ProjectSession, TargetSelection, Workspace, asdf,
};
use zkcl::{Runtime, SymbolStatus};

fn fixture() -> tempfile::TempDir {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/project-embedding");
    fs::create_dir_all(&root).unwrap();
    let temporary = tempfile::tempdir_in(root).unwrap();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/embedding-project"),
        &temporary.path().join("source"),
    );
    temporary
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}

fn options() -> LoadOptions {
    LoadOptions {
        target: TargetSelection::Binary(Some("app".into())),
        ..LoadOptions::default()
    }
}

fn host() -> (Runtime, Arc<AtomicU32>) {
    let mut runtime = Runtime::start().unwrap();
    runtime.set_standard_output(std::io::sink());
    runtime
        .load_str("(defpackage #:embed.host (:use #:cl) (:export #:offset))")
        .unwrap();
    let count = Arc::new(AtomicU32::new(0));
    let calls = count.clone();
    runtime
        .register_function("EMBED.HOST:OFFSET", move |runtime, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![runtime.integer_from(0_i32)?])
        })
        .unwrap();
    (runtime, count)
}

fn call_host(runtime: &Runtime) {
    let function = runtime
        .lookup_symbol("EMBED.APP", "CALL-HOST")
        .unwrap()
        .unwrap();
    assert_eq!(runtime.call(&function, &[]).unwrap()[0].as_i64(), Some(42));
}

#[test]
fn rust_host_loads_mixed_project_and_exports_relocatable_bundle() {
    let temporary = fixture();
    let source = temporary.path().join("source");
    let workspace = Workspace::open(&source).unwrap();
    assert_eq!(workspace.members.len(), 3);
    let project = Project::discover(&source.join("app/main.lisp"))
        .unwrap()
        .unwrap();
    let process_directory = std::env::current_dir().unwrap();
    let process_output = std::env::var_os("ZKCL_OUT_DIR");
    let bundle = temporary.path().join("bundle");
    {
        let (runtime, calls) = host();
        let loaded = project.load(&runtime, &options()).unwrap();
        assert_eq!(loaded.resolved().resolution.modules.len(), 3);
        let result = loaded.call_entry().unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].as_i64(), Some(42));
        assert_eq!(
            runtime.string(&result[1]).as_deref(),
            Some("embedded resources")
        );
        assert_eq!(result[2].as_i64(), Some(1));
        call_host(&runtime);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        asdf::load(&runtime, "embed-native").unwrap();
        assert_eq!(loaded.call_entry().unwrap()[2].as_i64(), Some(1));
        let info = asdf::system_info(&runtime, "embed-asdf").unwrap().unwrap();
        assert!(info.loaded);
        assert_eq!(info.version.as_deref(), Some("1.0.0"));
        assert_eq!(info.component.children.len(), 2);
        assert_eq!(info.component.children[1].children[0].name, "api");
        assert!(
            asdf::registered_systems(&runtime)
                .unwrap()
                .iter()
                .any(|name| name == "embed-asdf")
        );
        assert_eq!(
            runtime
                .find_symbol("EMBED.ASDF", "ANSWER")
                .unwrap()
                .unwrap()
                .status,
            SymbolStatus::External
        );
        assert_eq!(
            runtime
                .find_symbol("CL-USER", "CAR")
                .unwrap()
                .unwrap()
                .status,
            SymbolStatus::Inherited
        );
        assert!(
            runtime
                .namespace_snapshot()
                .packages
                .iter()
                .find(|p| p.name == "EMBED.ASDF")
                .unwrap()
                .local_nicknames
                .iter()
                .any(|(name, package)| name == "NATIVE" && package == "EMBED.NATIVE")
        );
        loaded.resolved().write_lock().unwrap();
        loaded.write_bundle(&bundle).unwrap();
    }
    assert_eq!(std::env::current_dir().unwrap(), process_directory);
    assert_eq!(std::env::var_os("ZKCL_OUT_DIR"), process_output);
    fs::remove_dir_all(&source).unwrap();
    let relocated = temporary.path().join("installed");
    fs::rename(&bundle, &relocated).unwrap();
    // Model an installer replacing source mtimes after extracting compiled files.
    let source_file = relocated.join("asdf-sources/0/api.lisp");
    fs::File::options()
        .write(true)
        .open(source_file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .unwrap();
    let bundle = ProjectBundle::open(&relocated).unwrap();
    let (runtime, calls) = host();
    bundle
        .load(&runtime, &temporary.path().join("cache"))
        .unwrap();
    let values = bundle.call_entry(&runtime).unwrap();
    assert_eq!(values[0].as_i64(), Some(42));
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("embedded resources")
    );
    assert_eq!(values[2].as_i64(), Some(1));
    call_host(&runtime);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let other = Runtime::start().unwrap();
    assert!(other.lookup_symbol("EMBED.APP", "MAIN").is_err());
}

#[test]
fn bundle_verification_precedes_all_initializers() {
    let temporary = fixture();
    let project = Project::open(&temporary.path().join("source/app")).unwrap();
    let runtime = Runtime::start().unwrap();
    let loaded = project.load(&runtime, &options()).unwrap();
    let bundle = loaded
        .write_bundle(&temporary.path().join("bundle"))
        .unwrap();
    fs::write(bundle.root().join("0000.zkclfasl"), b"corrupt").unwrap();
    let candidate = Runtime::start().unwrap();
    let error = bundle
        .load(&candidate, &temporary.path().join("output"))
        .unwrap_err();
    assert!(error.to_string().contains("verification"));
    assert!(candidate.lookup_symbol("EMBED.NATIVE", "ANSWER").is_err());
}

#[test]
fn missing_asdf_dependency_returns_context_to_host() {
    let temporary = fixture();
    let path = temporary.path().join("source/asdf-system/embed-asdf.asd");
    fs::write(
        &path,
        fs::read_to_string(&path).unwrap().replace(
            ":depends-on (\"embed-native\")",
            ":depends-on (\"missing-from-lock\")",
        ),
    )
    .unwrap();
    let project = Project::open(&temporary.path().join("source/app")).unwrap();
    let runtime = Runtime::start().unwrap();
    let error = match project.load(&runtime, &options()) {
        Ok(_) => panic!("missing dependency was accepted"),
        Err(error) => error,
    };
    let context = error
        .downcast_ref::<asdf::AsdfError>()
        .expect("typed ASDF operation failure");
    assert_eq!(context.system, "embed-asdf");
    assert!(context.source.to_string().contains("MISSING-DEPENDENCY"));
    assert_eq!(runtime.eval("(+ 20 22)").unwrap()[0].as_i64(), Some(42));
}
