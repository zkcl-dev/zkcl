use std::fs;
use std::path::Path;
use zkcl::{Runtime, SymbolStatus};

#[test]
fn instance_environment_and_pathnames_do_not_change_the_process_or_other_instances() {
    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/host-environment");
    fs::create_dir_all(&scratch).unwrap();
    let directory = tempfile::tempdir_in(scratch).unwrap();
    fs::write(directory.path().join("input.lisp"), "42").unwrap();
    let process_directory = std::env::current_dir().unwrap();
    let process_variable = std::env::var_os("ZKCL_EMBED_INSTANCE");
    let runtime = Runtime::start().unwrap();
    let other = Runtime::start().unwrap();
    runtime
        .set_environment_variable("ZKCL_EMBED_INSTANCE", Some("instance-one"))
        .unwrap();
    runtime.set_working_directory(directory.path()).unwrap();
    assert_eq!(
        runtime.eval("(zk:getenv \"ZKCL_EMBED_INSTANCE\")").unwrap()[0]
            .as_string(&runtime)
            .as_deref(),
        Some("instance-one")
    );
    assert_eq!(
        runtime
            .eval("(with-open-file (input \"input.lisp\") (read input))")
            .unwrap()[0]
            .as_i64(),
        Some(42)
    );
    assert_eq!(other.working_directory().unwrap(), process_directory);
    assert_eq!(std::env::current_dir().unwrap(), process_directory);
    assert_eq!(std::env::var_os("ZKCL_EMBED_INSTANCE"), process_variable);
    runtime
        .set_environment_variable("ZKCL_EMBED_INSTANCE", None)
        .unwrap();
    assert!(runtime.eval("(zk:getenv \"ZKCL_EMBED_INSTANCE\")").unwrap()[0].is_nil());
}

#[test]
fn module_queries_and_symbol_status_use_the_owning_instance() {
    let runtime = Runtime::start().unwrap();
    let other = Runtime::start().unwrap();
    runtime.eval("(provide \"host-module\")").unwrap();
    assert!(
        runtime
            .loaded_modules()
            .unwrap()
            .iter()
            .any(|name| name == "host-module")
    );
    assert!(
        !other
            .loaded_modules()
            .unwrap()
            .iter()
            .any(|name| name == "host-module")
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
            .find_symbol("CL-USER", "NO-SUCH-SYMBOL-HOST-QUERY")
            .unwrap()
            .is_none()
    );
    runtime.eval("(setf *modules* (list \"cycle\"))").unwrap();
    runtime.eval("(setf (cdr *modules*) *modules*)").unwrap();
    assert!(runtime.loaded_modules().is_err());
}
