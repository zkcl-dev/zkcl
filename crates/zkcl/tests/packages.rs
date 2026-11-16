use zkcl::{Root, Runtime};

#[test]
fn package_lookup_tracks_symbol_accessibility() {
    let runtime = &Runtime::start().unwrap();
    let extensions = one(runtime, "(symbol-package 'zk:command-line-arguments)");
    let mop = one(runtime, "(symbol-package 'zk-mop:class-slots)");
    assert!(extensions.same_identity(&one(runtime, "(find-package :zk)")));
    assert!(mop.same_identity(&one(runtime, "(find-package :zk-mop)")));
    assert!(one(runtime, "(find-package :zkcl)").is_nil());
    assert!(one(runtime, "(find-package :zkcl-mop)").is_nil());

    runtime
        .load_str(
            r#"
            (make-package "ZK-ORIGIN" :nicknames '("Zk-Origin") :use nil)
            (make-package "ZK-INTERMEDIATE" :use nil)
            (make-package "ZK-LEAF" :use '("ZK-INTERMEDIATE"))
            (make-package "" :use nil)
            "#,
        )
        .unwrap();

    let [nil, status] = two(runtime, r#"(find-symbol "NIL" "CL-USER")"#);
    assert!(nil.is_nil());
    assert!(status.same_identity(&one(runtime, ":inherited")));
    assert!(!one(runtime, "'ZK-ORIGIN::NIL").is_nil());
    let [canonical, status] = two(runtime, r#"(find-symbol "BOOLE-AND" "CL")"#);
    assert!(status.same_identity(&one(runtime, ":external")));
    assert!(canonical.same_identity(&one(runtime, "'CL:BOOLE-AND")));
    let [missing, status] = two(runtime, r#"(find-symbol "Missing" "ZK-ORIGIN")"#);
    assert!(missing.is_nil() && status.is_nil());

    let [symbol, status] = two(runtime, r#"(intern "OneCase" "Zk-Origin")"#);
    assert!(status.is_nil());
    assert_eq!(runtime.symbol_name(&symbol).as_deref(), Some("OneCase"));
    let [existing, status] = two(runtime, r#"(intern "OneCase" "ZK-ORIGIN")"#);
    assert!(symbol.same_identity(&existing));
    assert!(status.same_identity(&one(runtime, ":internal")));
    assert!(symbol.same_identity(&one(runtime, "'|Zk-Origin|::|OneCase|")));
    let [missing, status] = two(runtime, r#"(find-symbol "ONECASE" "ZK-ORIGIN")"#);
    assert!(missing.is_nil() && status.is_nil());
    assert!(runtime.eval("'ZK-ORIGIN:|OneCase|").is_err());
    assert!(runtime.eval("'ZK-ORIGIN:|Absent|").is_err());
    let [missing, status] = two(runtime, r#"(find-symbol "Absent" "ZK-ORIGIN")"#);
    assert!(missing.is_nil() && status.is_nil());
    let empty = one(runtime, "'||::|OneCase|");
    assert!(!symbol.same_identity(&empty));
    assert!(
        one(runtime, "(symbol-package '||::|OneCase|)")
            .same_identity(&one(runtime, r#"(find-package "")"#))
    );

    let [keyword, status] = two(runtime, r#"(intern "ZK-PACKAGE-KEY" "KEYWORD")"#);
    assert!(status.is_nil());
    assert!(keyword.same_identity(&one(runtime, ":zk-package-key")));
    let [existing, status] = two(runtime, r#"(find-symbol "ZK-PACKAGE-KEY" "KEYWORD")"#);
    assert!(keyword.same_identity(&existing));
    assert!(status.same_identity(&one(runtime, ":external")));

    runtime
        .load_str(
            r#"
            (export 'ZK-ORIGIN::|OneCase| "ZK-ORIGIN")
            (use-package "Zk-Origin" "ZK-INTERMEDIATE")
            "#,
        )
        .unwrap();
    assert!(symbol.same_identity(&one(runtime, "'ZK-ORIGIN:|OneCase|")));
    assert!(symbol.same_identity(&one(runtime, "'ZK-INTERMEDIATE::|OneCase|")));
    assert!(runtime.eval("'ZK-INTERMEDIATE:|OneCase|").is_err());
    let [inherited, status] = two(runtime, r#"(intern "OneCase" "ZK-INTERMEDIATE")"#);
    assert!(symbol.same_identity(&inherited));
    assert!(status.same_identity(&one(runtime, ":inherited")));
    let [missing, status] = two(runtime, r#"(find-symbol "OneCase" "ZK-LEAF")"#);
    assert!(missing.is_nil() && status.is_nil());

    runtime
        .load_str(
            r#"
            (export 'ZK-ORIGIN:|OneCase| "ZK-INTERMEDIATE")
            (import 'ZK-ORIGIN:|OneCase| "ZK-INTERMEDIATE")
            (use-package "ZK-ORIGIN" "ZK-LEAF")
            (unuse-package "ZK-INTERMEDIATE" "ZK-LEAF")
            "#,
        )
        .unwrap();
    assert!(symbol.same_identity(&one(runtime, "'ZK-INTERMEDIATE:|OneCase|")));
    let [inherited, status] = two(runtime, r#"(find-symbol "OneCase" "ZK-LEAF")"#);
    assert!(symbol.same_identity(&inherited));
    assert!(status.same_identity(&one(runtime, ":inherited")));
}

#[test]
fn dynamically_bound_package_controls_symbol_lookup() {
    let runtime = &Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (make-package "ZK-SOURCE" :use '("COMMON-LISP"))
            (shadow "IF" "ZK-SOURCE")
            (in-package "ZK-SOURCE")
            (defun if (n) (+ n 1))
            (defun relative-name () (if 41))
            "#,
        )
        .unwrap();
    assert_eq!(one(runtime, "(relative-name)").as_fixnum(), Some(42));
    assert_eq!(
        runtime
            .string(&one(runtime, "(package-name *package*)"))
            .as_deref(),
        Some("ZK-SOURCE"),
    );

    runtime.load_str("(in-package #:COMMON-LISP-USER)").unwrap();
    let [read, interned] = two(
        runtime,
        r#"(let ((*package* (find-package "ZK-SOURCE")))
              (values (read-from-string "RELATIVE-NAME") (intern "RELATIVE-NAME")))"#,
    );
    assert!(read.same_identity(&interned));
    assert!(read.same_identity(&one(runtime, "'ZK-SOURCE::RELATIVE-NAME")));
    assert!(one(runtime, "*package*").same_identity(&one(runtime, r#"(find-package "CL-USER")"#)));
}

#[test]
fn compiled_code_keeps_the_symbol_read_before_reinterning() {
    let runtime = &Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (make-package "ZK-GC-OLD" :use nil)
            (defmacro replace-read-symbol ()
              (unintern (find-symbol "ANCHOR" "ZK-GC-OLD") "ZK-GC-OLD")
              (intern "ANCHOR" "ZK-GC-OLD")
              nil)
            "#,
        )
        .unwrap();
    let closure = one(
        runtime,
        "(lambda () (replace-read-symbol) 'ZK-GC-OLD::ANCHOR)",
    );
    runtime.collect();
    let values = runtime.call(&closure, &[]).unwrap();
    let [old] = values.as_slice() else {
        panic!("expected the preserved symbol as the only return value");
    };
    let new = one(runtime, "'ZK-GC-OLD::ANCHOR");
    assert_eq!(runtime.symbol_name(old).as_deref(), Some("ANCHOR"));
    assert!(!old.same_identity(&new));
    let symbol_package = one(runtime, "#'symbol-package");
    assert!(runtime.call(&symbol_package, &[old]).unwrap()[0].is_nil());
    assert!(!runtime.call(&symbol_package, &[&new]).unwrap()[0].is_nil());
}

#[test]
fn import_assigns_a_home_to_an_uninterned_symbol() {
    let runtime = &Runtime::start().unwrap();
    one(
        runtime,
        r#"(make-package "ZK-ORIGIN" :nicknames '("Zk-Origin") :use nil)"#,
    );
    let [homeless, home_after_import] = two(
        runtime,
        r#"(let ((symbol (make-symbol "FROM-HOMELESS")))
              (import symbol "ZK-ORIGIN")
              (values symbol (symbol-package symbol)))"#,
    );
    assert!(homeless.same_identity(&one(runtime, "'ZK-ORIGIN::FROM-HOMELESS")));
    assert!(home_after_import.same_identity(&one(runtime, r#"(find-package "ZK-ORIGIN")"#)));
}

#[test]
fn shadowing_import_replaces_a_conflicting_local_symbol() {
    let runtime = &Runtime::start().unwrap();
    let left = one(runtime, r#"(make-package "ZK-LEFT" :use nil)"#);
    let target = one(runtime, r#"(make-package "ZK-TARGET" :use nil)"#);
    runtime
        .load_str(
            r#"
            (export (intern "CONFLICT" "ZK-LEFT") "ZK-LEFT")
            (intern "CONFLICT" "ZK-TARGET")
            "#,
        )
        .unwrap();
    let affected = one(
        runtime,
        r#"(handler-case (use-package "ZK-LEFT" "ZK-TARGET")
              (package-error (condition)
                (find-package (package-error-package condition))))"#,
    );
    assert!(affected.same_identity(&target));
    assert!(one(runtime, r#"(package-use-list "ZK-TARGET")"#).is_nil());
    let local = one(runtime, "'ZK-TARGET::CONFLICT");
    assert!(!local.same_identity(&one(runtime, "'ZK-LEFT:CONFLICT")));

    runtime
        .load_str(
            r#"
            (shadow "CONFLICT" "ZK-TARGET")
            (use-package "ZK-LEFT" "ZK-TARGET")
            (shadowing-import 'ZK-LEFT:CONFLICT "ZK-TARGET")
            "#,
        )
        .unwrap();
    runtime.collect();
    let imported = one(runtime, "'ZK-TARGET::CONFLICT");
    assert!(imported.same_identity(&one(runtime, "'ZK-LEFT:CONFLICT")));
    assert!(one(runtime, "(symbol-package 'ZK-TARGET::CONFLICT)").same_identity(&left));
    let symbol_package = one(runtime, "#'symbol-package");
    assert!(runtime.call(&symbol_package, &[&local]).unwrap()[0].is_nil());
}

#[test]
fn deleted_package_keeps_identity_when_its_name_is_reused() {
    let runtime = &Runtime::start().unwrap();
    let package = one(
        runtime,
        r#"(make-package "ZK-OLD-NAME" :nicknames '("ZK-OLD-NICKNAME") :use nil)"#,
    );
    let symbol = one(runtime, "'ZK-OLD-NICKNAME::MEMBER");
    assert!(package.same_identity(&one(
        runtime,
        r#"(rename-package "ZK-OLD-NAME" "ZK-NEW-NAME" '("ZK-NEW-NICKNAME"))"#,
    )));
    assert!(one(runtime, r#"(find-package "ZK-OLD-NAME")"#).is_nil());
    assert!(one(runtime, r#"(find-package "ZK-OLD-NICKNAME")"#).is_nil());
    assert!(symbol.same_identity(&one(runtime, "'ZK-NEW-NICKNAME::MEMBER")));

    let deleted = one(
        runtime,
        r#"(let ((deleted (find-package "ZK-NEW-NAME")))
              (delete-package deleted)
              deleted)"#,
    );
    runtime.collect();
    assert!(package.same_identity(&deleted));
    let packagep = one(runtime, "#'packagep");
    let package_name = one(runtime, "#'package-name");
    assert!(!runtime.call(&packagep, &[&deleted]).unwrap()[0].is_nil());
    assert!(runtime.call(&package_name, &[&deleted]).unwrap()[0].is_nil());
    assert!(one(runtime, r#"(find-package "ZK-NEW-NICKNAME")"#).is_nil());
    assert!(!package.same_identity(&one(runtime, r#"(make-package "ZK-NEW-NAME" :use nil)"#)));
    assert!(!symbol.same_identity(&one(runtime, "'ZK-NEW-NAME::MEMBER")));
}

fn one<'rt>(runtime: &'rt Runtime, source: &str) -> Root<'rt> {
    let [value] = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"))
        .try_into()
        .ok()
        .unwrap_or_else(|| panic!("expected one value from {source}"));
    value
}

fn two<'rt>(runtime: &'rt Runtime, source: &str) -> [Root<'rt>; 2] {
    runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"))
        .try_into()
        .ok()
        .unwrap_or_else(|| panic!("expected two values from {source}"))
}
