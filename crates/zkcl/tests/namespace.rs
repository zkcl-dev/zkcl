use zkcl::{NamespaceBindingKind, Runtime};

#[test]
fn snapshot_retains_exact_names_and_imported_symbol_identity() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defpackage :namespace-source (:use :cl) (:export :shared))
        (in-package :namespace-source)
        (defun shared () 1)
        (defun |Foo| () 2)
        (defun |foo| () 3)
        (defun foo () 4)
        (defpackage :namespace-client (:use :cl)
          (:import-from :namespace-source :shared))
        (in-package :namespace-client)
    "#,
        )
        .unwrap();
    let snapshot = runtime.namespace_snapshot();
    for name in ["Foo", "foo", "FOO"] {
        assert!(
            snapshot
                .symbols
                .iter()
                .any(|symbol| symbol.package == "NAMESPACE-SOURCE" && symbol.name == name)
        );
    }
    let original = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.package == "NAMESPACE-SOURCE" && symbol.name == "SHARED")
        .unwrap();
    let imported = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.package == "NAMESPACE-CLIENT" && symbol.name == "SHARED")
        .unwrap();
    assert_eq!(original.id, imported.id);
    assert_eq!(imported.home_package.as_deref(), Some("NAMESPACE-SOURCE"));
    assert!(original.external);
    assert!(!imported.external);
    assert!(original.accessible && imported.accessible);
}

#[test]
fn snapshot_keeps_each_binding_and_its_documentation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defpackage :namespace-bindings (:use :cl))
        (in-package :namespace-bindings)
        (defun both (value) "Function documentation." value)
        (defparameter both 42 "Variable documentation.")
        (defclass both () () (:documentation "Class documentation."))
        (deftype derived () 'integer)
        (defmacro expansion (value) value)
        (defparameter expansion 7)
    "#,
        )
        .unwrap();
    let snapshot = runtime.namespace_snapshot();
    let both = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.package == "NAMESPACE-BINDINGS" && symbol.name == "BOTH")
        .unwrap();
    for (kind, documentation) in [
        (NamespaceBindingKind::Function, "Function documentation."),
        (NamespaceBindingKind::Variable, "Variable documentation."),
        (NamespaceBindingKind::Type, "Class documentation."),
    ] {
        let binding = both
            .bindings
            .iter()
            .find(|binding| binding.kind == kind)
            .unwrap();
        assert_eq!(binding.documentation.as_deref(), Some(documentation));
    }
    assert_eq!(both.bindings.len(), 3);
    let derived = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.package == "NAMESPACE-BINDINGS" && symbol.name == "DERIVED")
        .unwrap();
    assert_eq!(derived.bindings[0].kind, NamespaceBindingKind::Type);
    let expansion = snapshot
        .symbols
        .iter()
        .find(|symbol| symbol.package == "NAMESPACE-BINDINGS" && symbol.name == "EXPANSION")
        .unwrap();
    assert!(
        expansion
            .bindings
            .iter()
            .any(|binding| binding.kind == NamespaceBindingKind::Macro)
    );
    assert!(
        expansion
            .bindings
            .iter()
            .any(|binding| binding.kind == NamespaceBindingKind::Variable)
    );
}

#[test]
fn snapshot_includes_live_builtin_and_keyword_bindings() {
    let runtime = Runtime::start().unwrap();
    runtime.eval(":namespace-keyword").unwrap();
    let snapshot = runtime.namespace_snapshot();
    assert!(snapshot.symbols.iter().any(|symbol| {
        symbol.package == "COMMON-LISP"
            && symbol.name == "CAR"
            && symbol
                .bindings
                .iter()
                .any(|binding| binding.kind == NamespaceBindingKind::Function)
    }));
    assert!(snapshot.symbols.iter().any(|symbol| {
        symbol.package == "KEYWORD"
            && symbol.name == "NAMESPACE-KEYWORD"
            && symbol
                .bindings
                .iter()
                .any(|binding| binding.kind == NamespaceBindingKind::Constant)
    }));
}
