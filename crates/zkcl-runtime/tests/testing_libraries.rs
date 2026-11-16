use zkcl_runtime::Runtime;

#[test]
fn loading_a_test_system_leaves_other_runtimes_unaffected() {
    let first = Runtime::start().unwrap();
    let second = Runtime::start().unwrap();
    first.eval("(require :asdf)").unwrap();
    first.eval("(asdf:load-system \"zk-pbt\")").unwrap();
    first
        .load_str("(zk-test:deftest local-case () (zk-test:is t))")
        .unwrap();
    assert!(
        !first
            .eval("(member 'local-case (zk-test:list-tests))")
            .unwrap()[0]
            .is_nil()
    );
    assert!(second.eval("(find-package :zk-test)").unwrap()[0].is_nil());
    second.eval("(require :zk-test)").unwrap();
    assert!(second.eval("(find-package :zk-pbt)").unwrap()[0].is_nil());
    assert!(second.eval("(zk-test:list-tests)").unwrap()[0].is_nil());
}

#[test]
fn asdf_loaded_test_libraries_run_the_bundled_suites() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(require :asdf)").unwrap();
    runtime.eval("(asdf:load-system \"zk-pbt\")").unwrap();
    runtime
        .load_str(include_str!("../../../tests/lisp/zk-pbt/test-runner.lisp"))
        .unwrap();
}
