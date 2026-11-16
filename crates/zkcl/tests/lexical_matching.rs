use std::fs;

use zkcl::Runtime;

mod common;

use common::files::TempDir;

const CASES: &str = include_str!("patterns/lexical.lisp");

#[test]
fn lexical_rules_spans_and_streams_execute_from_source() {
    Runtime::start().unwrap().load_str(CASES).unwrap();
}

#[test]
fn compiled_lexical_rules_load_in_a_fresh_runtime() {
    let directory = TempDir::new("lexical-fasl");
    let path = directory.path().join("lexical.lisp");
    fs::write(&path, CASES).unwrap();
    let compiled = Runtime::start().unwrap().compile_file(&path, None).unwrap();
    fs::remove_file(&path).unwrap();
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&compiled).unwrap();
    assert!(
        runtime
            .eval("(fboundp (find-symbol \"COMPILE-MATCH\" :zk-pattern-compiler))")
            .unwrap()[0]
            .is_nil()
    );
}

#[test]
fn lexical_compile_diagnostics_reject_missing_cases_and_nonadvancing_rules() {
    let runtime = Runtime::start().unwrap();
    for (form, expected) in [
        (
            r#"(zk-lex:lexmatch "a" ("a" :a))"#,
            "Non-exhaustive lexical match",
        ),
        (
            r#"(zk-lex:lexscan scanner ("" :empty) (_ nil))"#,
            "advancing prefix rules",
        ),
        (
            r#"(zk-lex:lexmatch "a" ((:capture name (:re "a+") :as :unit) name) (_ nil))"#,
            "consumes one unit",
        ),
        (
            r#"(zk-lex:lexmatch "a" ("a" :when t :a) (_ nil))"#,
            "do not take guards",
        ),
        (
            r#"(zk-lex:lexmatch "a" ("a" :a) (_ :when t :miss))"#,
            "do not take guards",
        ),
        (
            r#"(zk-lex:lexscan scanner ("a" :a) ((:eof) :when t :eof))"#,
            "do not take guards",
        ),
        (
            r#"(zk-lex:lexmatch "a" ((:with) :a) (_ nil))"#,
            "Missing rule",
        ),
        (
            r#"(zk-text:snapshot '(1 2 3))"#,
            "Expected a vector or span",
        ),
        (
            r#"(zk-text:utf8-source 42)"#,
            "function or channel as UTF-8 octet source",
        ),
    ] {
        let error = runtime
            .eval(form)
            .err()
            .unwrap_or_else(|| panic!("invalid form compiled: {form}"));
        assert!(error.message().contains(expected), "{form}: {error}");
    }
}
