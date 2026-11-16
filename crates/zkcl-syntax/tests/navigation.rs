#![cfg(feature = "analysis")]

use zkcl_syntax::{
    CompletionKind, DefinitionLocation, LintConfig, LintPreset, OutlineKind, SemanticTokenKind,
    SourceWorkspace,
};

#[test]
fn defstruct_indexes_generated_functions_and_explicit_name_sites() {
    let library = r#"(defpackage #:ir (:use #:cl) (:export #:function-p #:make-entry #:entry-data))
(in-package #:ir)
(defstruct (entry (:predicate function-p) (:copier nil)) data (id 0 :read-only t))
(defstruct (other (:type list) (:conc-name nil) (:constructor construct (label))) label)"#;
    let caller = r#"(in-package :cl-user)
(let ((entry (ir:make-entry :data (lambda (x) x))))
  (list (ir:function-p entry) (ir:entry-data entry) #'(setf ir:entry-data)
        (ir::entry-id entry) #'(setf ir::entry-id) (ir::copy-entry entry)
        (ir::construct :tag) (ir::label entry) (ir::other-p entry)))"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("lib.lisp", library);
    workspace.set_document("caller.lisp", caller);
    for (usage, definition) in [
        ("ir:make-entry", "entry (:predicate"),
        ("ir:function-p", "function-p)"),
        ("ir:entry-data", "data (id"),
        ("ir::entry-id", "id 0"),
        ("ir::construct", "construct (label)"),
        ("ir::label", "label)"),
    ] {
        let location = workspace
            .definition("caller.lisp", caller.find(usage).unwrap())
            .unwrap_or_else(|| panic!("{usage}"));
        assert_eq!(location.document, "lib.lisp");
        let offset = usize::from(location.selection_range.start());
        assert_eq!(offset, library.rfind(definition).unwrap(), "{usage}");
    }
    for usage in ["ir::copy-entry", "ir::other-p", "setf ir::entry-id"] {
        let offset = caller.find(usage).unwrap() + if usage.starts_with("setf ") { 5 } else { 0 };
        assert!(
            workspace.definition("caller.lisp", offset).is_none(),
            "{usage}"
        );
    }
    let setter = caller.find("setf ir:entry-data").unwrap() + 5;
    assert!(workspace.definition("caller.lisp", setter).is_some());
    assert!(
        workspace
            .prepare_rename("caller.lisp", caller.find("ir:entry-data").unwrap())
            .is_none()
    );
    let predicate = caller.find("ir:function-p").unwrap();
    assert!(workspace.prepare_rename("caller.lisp", predicate).is_some());
    assert_eq!(
        workspace.references("caller.lisp", predicate, true).len(),
        2
    );

    workspace.set_document(
        "inside.lisp",
        "(in-package :ir) (function-p (make-entry)) (function-p)",
    );
    let diagnostics =
        workspace.lint_document("inside.lisp", &LintConfig::from_preset(LintPreset::Strict));
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.rule_id == "suspicious/likely-misspelled-operator")
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d.rule_id == "correctness/arity-mismatch"),
        "{diagnostics:?}"
    );
}

#[test]
fn function_designators_resolve_in_binding_macro_operands() {
    let source = "(in-package :cl-user)
(defun predicate (condition) condition)
(flet ((predicate (x) x))
  (restart-bind ((retry #'predicate :test-function #'predicate))
    (handler-case (signal 'error) ((satisfies predicate) () nil))))";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("condition", source);
    for (reference, declaration) in [
        ("#'predicate", "(flet ((predicate"),
        (":test-function #'predicate", "(flet ((predicate"),
        ("satisfies predicate", "(defun predicate"),
    ] {
        let offset = source.find(reference).unwrap() + reference.rfind("predicate").unwrap() + 1;
        let found = workspace
            .definition("condition", offset)
            .unwrap_or_else(|| panic!("unresolved {reference}"));
        assert_eq!(
            usize::from(found.selection_range.start()),
            source.find(declaration).unwrap() + declaration.rfind("predicate").unwrap()
        );
    }
}

#[test]
fn references_preserve_symbol_identity_in_each_namespace() {
    let mut source = SourceWorkspace::new();
    let library = r#"(defpackage #:|Two:Pkg|
  (:use #:cl) (:nicknames #:dm) (:export #:|Do:it| #:same))
(in-package "Two:Pkg")
(CL:|DEFUN| |Do:it| () 42)
(defmacro same () nil)
(defparameter same 22)
(defun internal () nil)
(defgeneric dispatch (object))
(defmethod dispatch ((object t)) object)
(defclass box () ())
(define-condition trouble (error) ())"#;
    let caller = r#"(in-package :caller)
(defun call-library ()
  (values (dm:|Do:it|) (|Two:Pkg|:same) dm:same
          #'dm::dispatch (the dm::box nil) (the dm::trouble nil)
          (dm::internal) (dm:internal) (DM:|do:it|)))"#;
    source.set_document("lib", library);
    source.set_document("caller", caller);

    for (reference, definition) in [
        ("dm:|Do:it|", "|DEFUN| |Do:it|"),
        ("|Two:Pkg|:same", "defmacro same"),
        ("dm:same", "defparameter same"),
        ("dm::dispatch", "defgeneric dispatch"),
        ("dm::box", "defclass box"),
        ("dm::trouble", "define-condition trouble"),
        ("dm::internal", "defun internal"),
    ] {
        let range = target(&source, "caller", caller, reference);
        let start = library.find(definition).unwrap() + definition.find(' ').unwrap() + 1;
        let end = library[start..].find(' ').unwrap() + start;
        assert_eq!(range.document, "lib", "{reference}");
        assert_eq!(usize::from(range.selection_range.start()), start);
        assert_eq!(usize::from(range.selection_range.end()), end);
    }
    for unresolved in ["dm:internal", "DM:|do:it|"] {
        assert!(
            source
                .definition("caller", caller.find(unresolved).unwrap())
                .is_none()
        );
    }
    let lib = source.snapshot("lib").unwrap();
    let outline: Vec<_> = lib
        .outline()
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind))
        .collect();
    for entry in [
        ("dispatch", OutlineKind::Function),
        ("box", OutlineKind::Type),
        ("trouble", OutlineKind::Type),
    ] {
        assert!(outline.contains(&entry));
    }
    assert_eq!(
        outline
            .iter()
            .filter(|(name, _)| *name == "dispatch")
            .count(),
        2
    );
}

#[test]
fn document_highlights_exclude_occurrences_in_other_documents() {
    use zkcl_syntax::DocumentHighlightKind::{Read, Write};

    let mut workspace = SourceWorkspace::new();
    let library = "(in-package :demo) (defun |Mixed| () nil) (|Mixed|)";
    let caller = "(in-package :demo) (list (demo::|Mixed|) (|Mixed|) (|MIXED|))";
    workspace.set_document("library", library);
    workspace.set_document("caller", caller);

    let highlights = |document, offset| {
        workspace
            .document_highlights(document, offset)
            .into_iter()
            .map(|entry| (usize::from(entry.range.start()), entry.kind))
            .collect::<Vec<_>>()
    };
    let definition = library.find("|Mixed|").unwrap();
    let call = library.rfind("|Mixed|").unwrap();
    assert_eq!(
        highlights("library", call),
        [(definition, Write), (call, Read)]
    );
    let qualified = caller.find("demo::|Mixed|").unwrap();
    let unqualified = caller.rfind("|Mixed|").unwrap();
    assert_eq!(
        highlights("caller", qualified),
        [(qualified, Read), (unqualified, Read)]
    );

    workspace.set_document("duplicate", "(in-package :demo) (defun |Mixed| () nil)");
    assert!(
        workspace
            .document_highlights("caller", qualified)
            .is_empty()
    );
}

#[test]
fn reexported_imports_resolve_to_the_source_package() {
    let origin = "(defpackage :origin (:use :cl) (:nicknames :source))\n\
                  (in-package :origin)\n\
                  (defun shared () 42)";
    let reexporting_package = "(defpackage :reexports (:use :cl)\n\
                    (:import-from :source :shared) (:export :shared))\n\
                  (in-package :reexports)\n\
                  (defun call-shared () (shared))";
    let caller = "(defpackage :caller (:use :cl)\n\
                    (:shadowing-import-from :reexports :shared))\n\
                  (in-package :caller)\n\
                  (list (shared) (reexports:shared) (origin:shared))";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("origin", origin);
    workspace.set_document("reexports", reexporting_package);
    workspace.set_document("caller", caller);

    for (document, source, reference) in [
        ("reexports", reexporting_package, "shared))"),
        ("caller", caller, "shared) (reexports"),
        ("caller", caller, "reexports:shared"),
    ] {
        let definition = workspace
            .definition(document, source.rfind(reference).unwrap())
            .unwrap_or_else(|| panic!("missing target for {reference} in {document}"));
        assert_eq!(definition.document, "origin");
        assert_eq!(
            usize::from(definition.selection_range.start()),
            origin.find("shared").unwrap()
        );
    }
    assert!(
        workspace
            .definition("caller", caller.find("origin:shared").unwrap())
            .is_none()
    );
}

const ORIGIN_PACKAGE_SOURCE: &str = "(defpackage :origin (:use :cl) (:nicknames :source)\n\
                    (:export :shared :inherited))\n\
                  (in-package :origin)\n\
                  (defun shared () 42)\n\
                  (defun inherited () 43)\n\
                  (defun internal () 44)";
const REEXPORTING_PACKAGE_SOURCE: &str = "(defpackage :reexports (:use :cl :origin)\n\
                    (:import-from :source :shared)\n\
                    (:export :shared :inherited))\n\
                  (in-package :reexports)\n\
                  (list (shared) (inherited)\n\
                        (reexports:shared) (reexports:inherited) (reexports::inherited)\n\
                        (origin::internal))";

fn workspace_with_reexported_symbols() -> SourceWorkspace {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("origin", ORIGIN_PACKAGE_SOURCE);
    workspace.set_document("reexports", REEXPORTING_PACKAGE_SOURCE);
    workspace
}

#[test]
fn definition_resolves_reexported_symbols_to_the_source_package() {
    let workspace = workspace_with_reexported_symbols();
    for (reference, definition) in [
        ("shared) (inherited", "(defun shared"),
        ("inherited)\n", "(defun inherited"),
        ("reexports:shared", "(defun shared"),
        ("reexports:inherited", "(defun inherited"),
        ("reexports::inherited", "(defun inherited"),
        ("origin::internal", "(defun internal"),
    ] {
        let found = target(
            &workspace,
            "reexports",
            REEXPORTING_PACKAGE_SOURCE,
            reference,
        );
        assert_eq!(found.document, "origin", "{reference}");
        assert_eq!(
            usize::from(found.selection_range.start()),
            ORIGIN_PACKAGE_SOURCE.find(definition).unwrap() + "(defun ".len(),
            "{reference}"
        );
    }
}

#[test]
fn completion_offers_reexported_symbols_visible_through_the_prefix() {
    let mut workspace = workspace_with_reexported_symbols();
    for (document, text, expected) in [
        (
            "import-completion",
            "(in-package :reexports)\n(sh",
            "shared",
        ),
        (
            "use-export-completion",
            "(in-package :reexports)\n(inh",
            "inherited",
        ),
        (
            "qualified-import-completion",
            "(in-package :reexports)\n(reexports:sh",
            "reexports:shared",
        ),
        (
            "qualified-use-export-completion",
            "(in-package :reexports)\n(reexports:inh",
            "reexports:inherited",
        ),
        (
            "internal-qualified-completion",
            "(in-package :reexports)\n(origin::int",
            "origin::internal",
        ),
    ] {
        workspace.set_document(document, text);
        let candidates = workspace.completions(document, text.len());
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.label == expected),
            "missing {expected:?} for {document}: {candidates:?}"
        );
    }

    for (text, unexpected) in [
        ("(in-package :reexports)\n(int", "internal"),
        (
            "(in-package :reexports)\n(reexports:int",
            "reexports:internal",
        ),
        (
            "(in-package :reexports)\n(reexports::int",
            "reexports::internal",
        ),
    ] {
        workspace.set_document("not-accessible-completion", text);
        let candidates = workspace.completions("not-accessible-completion", text.len());
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.label != unexpected),
            "unexpected {unexpected:?} for {text:?}: {candidates:?}"
        );
    }
}

#[test]
fn definition_resolves_calls_only_in_known_evaluation_positions() {
    let text = r#"(in-package :demo)
(defun shared () nil)
(defvar shared 1)
(defmacro obscure (form) nil)
(defun run (shared)
   (values (shared) shared
           (flet ((shared () 2)) (demo::shared))
           (obscure (demo::shared))
           '(demo::shared)))
(setq *readtable* (copy-readtable))
(demo::shared)"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("demo", text);
    let definition = target(&workspace, "demo", text, "shared) shared");
    assert_eq!(definition.document, "demo");
    assert_eq!(
        usize::from(definition.selection_range.start()),
        text.find("shared () nil").unwrap()
    );
    let value = text.find("shared) shared").unwrap() + "shared) ".len();
    assert!(workspace.definition("demo", value).is_none());
    let mut qualified = text.match_indices("demo::shared");
    let (local, _) = qualified.next().unwrap();
    let definition = workspace.definition("demo", local).unwrap();
    assert_eq!(definition.document, "demo");
    assert_eq!(
        usize::from(definition.selection_range.start()),
        text.find("shared () 2").unwrap()
    );
    for (offset, _) in qualified {
        assert!(workspace.definition("demo", offset).is_none(), "{offset}");
    }
}

#[test]
fn reader_changes_bound_static_analysis_even_within_top_level_wrappers() {
    for mutation in [
        "#+maybe (set-macro-character #\\[ #'read-list)",
        "(setq *package* (find-package :other) *readtable* (copy-readtable))",
    ] {
        let source = format!(
            "(in-package #:demo)\n(progn\n  (defun target () nil)\n  (demo::target)\n  {mutation}\n  (demo::target)\n  (defun later () (cons 1 2 3)))\n"
        );
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("source", &source);
        let before = source.find("demo::target").unwrap();
        let after = source.rfind("demo::target").unwrap();
        assert!(workspace.definition("source", before).is_some());
        assert!(workspace.definition("source", after).is_none());

        let snapshot = workspace.snapshot("source").unwrap();
        let names: Vec<_> = snapshot
            .outline()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["target"]);
        assert!(workspace.semantic_warnings("source").is_empty());
        assert!(
            workspace
                .completions("source", after + "demo::tar".len())
                .is_empty()
        );
        assert!(
            workspace
                .semantic_tokens("source")
                .iter()
                .all(|token| usize::from(token.range.start()) < after),
            "{mutation}"
        );

        if mutation.starts_with("#+") {
            workspace.set_features(Some(Vec::new()));
            assert!(workspace.definition("source", after).is_some());
            assert_eq!(workspace.semantic_warnings("source").len(), 1);
        }
    }
}

#[test]
fn semantic_tokens_distinguish_matching_ranges_in_different_documents() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", "(defun demo::work () nil)");
    workspace.set_document("caller", "      (demo::work)");
    let function_token = |document| {
        workspace
            .semantic_tokens(document)
            .into_iter()
            .find(|token| {
                token.kind == SemanticTokenKind::Function && usize::from(token.range.start()) == 7
            })
            .unwrap()
    };
    let declaration = function_token("library");
    let reference = function_token("caller");
    assert_eq!(declaration.range, reference.range);
    assert!(declaration.is_definition);
    assert!(!reference.is_definition);
}

#[test]
fn definition_requires_a_unique_declaration_in_open_revisions() {
    let initial = "(in-package :unknown)\n(defun work () 1)";
    let caller = "(unknown::work)";
    let offset = caller.find("work").unwrap();
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", initial);
    workspace.set_document("caller", caller);
    assert_eq!(
        workspace.definition("caller", offset).unwrap().document,
        "library"
    );

    workspace.set_document("other", initial);
    assert!(workspace.definition("caller", offset).is_none());
    workspace.remove_document("other");
    assert!(workspace.definition("caller", offset).is_some());

    let start = initial.find("work").unwrap();
    workspace
        .apply_edit("library", start..start + "work".len(), "changed")
        .unwrap();
    assert!(workspace.definition("caller", offset).is_none());
    workspace.set_document("library", "(defun unknown::work () 2)");
    assert!(workspace.definition("caller", offset).is_some());
    workspace.remove_document("library");
    assert!(workspace.definition("caller", offset).is_none());
}

#[test]
fn known_package_shadows_do_not_have_standard_operator_semantics() {
    let text = r#"(defpackage #:demo (:use #:cl) (:shadow #:values #:defun))
(in-package #:demo)
(cl:defun real () nil)
(defun phantom () nil)
(values (demo::real))
(cl:values (demo::real))
(demo::phantom)"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("demo", text);
    let mut real_references = text.match_indices("demo::real").map(|(offset, _)| offset);
    assert!(
        workspace
            .definition("demo", real_references.next().unwrap())
            .is_none()
    );
    assert!(
        workspace
            .definition("demo", real_references.next().unwrap())
            .is_some()
    );
    assert!(
        workspace
            .definition("demo", text.find("demo::phantom").unwrap())
            .is_none()
    );
}

fn target(workspace: &SourceWorkspace, file: &str, text: &str, needle: &str) -> DefinitionLocation {
    let byte = text.find(needle).unwrap();
    workspace
        .definition(file, byte)
        .unwrap_or_else(|| panic!("missing target for {needle}"))
}

fn cursor(source: &str) -> (String, usize) {
    let (before, after) = source.split_once('^').unwrap();
    (format!("{before}{after}"), before.len())
}

#[test]
fn completion_lists_only_bindings_visible_at_the_cursor() {
    let mut workspace = SourceWorkspace::new();
    for (source, visible, hidden) in [
        (
            "(defun sample (outer-value) (restart-case nil (retry (clause-local) :report (lambda (stream-local) (list ^)) clause-local)))",
            &["outer-value", "stream-local"][..],
            &["clause-local"][..],
        ),
        (
            "(defun sample (outer-value) (let ((first-local 1) (second-local (list ^))))",
            &["outer-value"][..],
            &["first-local", "second-local"][..],
        ),
        (
            "(defun sample (outer-value) (let* ((first-local 1) (second-local (list ^",
            &["outer-value", "first-local"][..],
            &["second-local"][..],
        ),
        (
            "(prog ((first-local 1) (second-local (list ^))))",
            &[][..],
            &["first-local", "second-local"][..],
        ),
        (
            "(prog* ((first-local 1) (second-local (list ^))))",
            &["first-local"][..],
            &["second-local"][..],
        ),
        (
            "(do ((first-local 1) (second-local (list ^))))",
            &[][..],
            &["first-local", "second-local"][..],
        ),
        (
            "(do* ((first-local 1) (second-local (list ^))))",
            &["first-local"][..],
            &["second-local"][..],
        ),
        (
            "(do ((first-local nil (let ((step-local 1)) (list ^))) (second-local 1)) (nil))",
            &["first-local", "second-local", "step-local"][..],
            &[][..],
        ),
        (
            "(do ((first-local 1)) ((let ((test-local 1)) (list ^))))",
            &["first-local", "test-local"][..],
            &[][..],
        ),
        (
            "(dolist (item-local (let ((source-local 1)) (list ^))))",
            &["source-local"][..],
            &["item-local"][..],
        ),
        (
            "(dotimes (item-local 1 (let ((result-local 1)) (list ^))))",
            &["item-local", "result-local"][..],
            &[][..],
        ),
        (
            "(multiple-value-bind (body-local) (let ((source-local 1)) (values ^)))",
            &["source-local"][..],
            &["body-local"][..],
        ),
        (
            "(destructuring-bind (body-local) (let ((source-local 1)) (list ^)))",
            &["source-local"][..],
            &["body-local"][..],
        ),
        (
            "(with-slots (slot-local) (let ((source-local 1)) (list ^)))",
            &["source-local"][..],
            &["slot-local"][..],
        ),
        (
            "(let ((body-local 1)) body-local)\n^",
            &[][..],
            &["body-local"][..],
        ),
        (
            "(let ((body-local 1)) body-local)^",
            &[][..],
            &["body-local"][..],
        ),
    ] {
        let (text, offset) = cursor(source);
        workspace.set_document("scope", &text);
        let completions = workspace.completions("scope", offset);
        for name in visible {
            assert!(
                completions.iter().any(|candidate| candidate.label == *name),
                "missing {name} in {source}"
            );
        }
        for name in hidden {
            assert!(
                completions.iter().all(|candidate| candidate.label != *name),
                "out-of-scope {name} in {source}"
            );
        }
    }
}

#[test]
fn completion_hides_lambda_list_markers_until_ampersand_is_typed() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("source", "");
    assert!(
        workspace
            .completions("source", 0)
            .iter()
            .all(|candidate| !candidate.label.starts_with('&'))
    );

    workspace.set_document("source", "(lambda (&");
    assert!(
        workspace
            .completions("source", "(lambda (&".len())
            .iter()
            .any(|candidate| candidate.label == "&optional")
    );
}

#[test]
fn signature_help_respects_local_callable_visibility() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "definition",
        "(in-package #:demo) (defun transform (global given) (list global given))",
    );
    for (source, params) in [
        (
            "(flet ((transform (local) (transform ^local 1))))",
            &["global", "given"][..],
        ),
        (
            "(labels ((transform (local) (transform ^local))))",
            &["local"][..],
        ),
        (
            "(macrolet ((transform (local) (transform ^local 1))))",
            &["global", "given"][..],
        ),
        (
            "(flet ((transform (local) local)) (transform ^1))",
            &["local"][..],
        ),
        (
            "(macrolet ((transform (local) local)) (transform ^1))",
            &["local"][..],
        ),
    ] {
        let (text, offset) = cursor(&format!("(in-package #:demo) {source}"));
        workspace.set_document("caller", &text);
        assert_eq!(
            workspace
                .signature_help("caller", offset)
                .unwrap_or_else(|| panic!("missing signature in {source}"))
                .parameters,
            params,
            "{source}"
        );
        assert!(
            workspace.semantic_warnings("caller").is_empty(),
            "unexpected warnings in {source}: {:?}",
            workspace.semantic_warnings("caller")
        );
    }
}

#[test]
fn lexical_function_references_match_imported_symbol_identity_across_spellings() {
    let mut workspace = SourceWorkspace::new();
    let library = "(defpackage #:origin (:use #:cl) (:nicknames #:orig) (:export #:transform))\n\
                   (in-package #:origin) (defun transform (global first extra) (list global first extra))";
    let caller = "(defpackage #:demo (:use #:cl) (:import-from #:origin #:transform))\n\
                  (in-package #:demo)\n\
                  (flet ((transform (local) local))\n\
                    (transform 1) (orig:transform 2) (demo::transform 3))\n\
                  (origin:transform 1 2 3)";
    workspace.set_document("library", library);
    workspace.set_document("caller", caller);

    let local = caller.find("transform (local)").unwrap();
    for needle in ["transform 1)", "orig:transform 2", "demo::transform 3"] {
        let offset = caller.find(needle).unwrap();
        let found = workspace.definition("caller", offset).unwrap();
        assert_eq!(found.document, "caller", "{needle}");
        assert_eq!(
            usize::from(found.selection_range.start()),
            local,
            "{needle}"
        );
        assert_eq!(
            workspace
                .signature_help("caller", offset)
                .unwrap()
                .parameters,
            ["local"],
            "{needle}"
        );
    }
    let global = target(&workspace, "caller", caller, "origin:transform 1 2 3");
    assert_eq!(global.document, "library");
    assert_eq!(workspace.references("caller", local, true).len(), 4);
    assert_eq!(
        workspace
            .references("library", library.find("transform (global ").unwrap(), true)
            .len(),
        2
    );
    assert!(workspace.semantic_warnings("caller").is_empty());
    assert!(workspace.rename("caller", local, "replacement").is_none());

    let local_source = "(in-package #:demo) (flet ((transform (x) x)) (demo::transform 1))";
    workspace.set_document("rename", local_source);
    let edits = workspace
        .rename(
            "rename",
            local_source.find("demo::transform").unwrap(),
            "replacement",
        )
        .unwrap();
    assert_eq!(
        edits
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["replacement", "demo::replacement"]
    );
}

#[test]
fn double_colon_accesses_inherited_external_symbols_without_reexport() {
    let mut workspace = SourceWorkspace::new();
    let library = "(defpackage #:origin (:use #:cl) (:export #:transform))\n\
                   (in-package #:origin) (defun transform () nil)";
    let caller = "(defpackage #:demo (:use #:cl #:origin))\n\
                  (in-package #:demo)\n\
                  (list (demo::transform) (demo:transform))";
    workspace.set_document("library", library);
    workspace.set_document("caller", caller);
    let inherited = target(&workspace, "caller", caller, "demo::transform");
    assert_eq!(inherited.document, "library");
    assert!(
        workspace
            .definition("caller", caller.find("demo:transform").unwrap())
            .is_none()
    );
    assert_eq!(
        workspace
            .references("library", library.find("transform ()").unwrap(), true)
            .len(),
        2
    );
}

#[test]
fn global_rename_updates_package_symbol_designators() {
    let library = "(defpackage #:lib (:nicknames #:lib-alias) (:use #:cl) (:export \"OLD-NAME\"))\n\
                   (in-package #:lib) (defun old-name () (return-from old-name 1))";
    let reexporting_package = "(defpackage #:reexports (:use #:cl)\n\
                  (:import-from #:lib-alias #:old-name) (:export :old-name))";
    let client = "(in-package :reexports)\n\
                  (list (old-name) (reexports:old-name) (lib-alias:old-name))";
    let mut workspace = SourceWorkspace::new();
    for (name, text) in [
        ("library", library),
        ("reexports", reexporting_package),
        ("client", client),
    ] {
        workspace.set_document(name, text);
    }
    let call = client.find("reexports:old-name").unwrap();
    let (stem, placeholder) = workspace.prepare_rename("client", call).unwrap();
    assert_eq!(
        &client[usize::from(stem.start())..usize::from(stem.end())],
        "old-name"
    );
    assert_eq!(placeholder, "old-name");

    let edits = workspace.rename("client", call, "|new name|").unwrap();
    for (document, before, expected) in [
        (
            "library",
            library,
            &[
                "\"new name\"",
                "(defun |new name|",
                "(return-from |new name|",
            ][..],
        ),
        (
            "reexports",
            reexporting_package,
            &["#:lib-alias #:|new name|", "(:export :|new name|)"][..],
        ),
        (
            "client",
            client,
            &[
                "(|new name|)",
                "(reexports:|new name|)",
                "(lib-alias:|new name|)",
            ][..],
        ),
    ] {
        let mut changed = before.to_owned();
        let mut doc_edits: Vec<_> = edits
            .iter()
            .filter(|(loc, _)| loc.document == document)
            .collect();
        doc_edits.sort_by_key(|(loc, _)| std::cmp::Reverse(loc.range.start()));
        assert_eq!(doc_edits.len(), expected.len(), "{document}");
        for (loc, replacement) in doc_edits {
            changed.replace_range(
                usize::from(loc.range.start())..usize::from(loc.range.end()),
                replacement,
            );
        }
        for substring in expected {
            assert!(
                changed.contains(substring),
                "missing {substring:?} in {changed:?}"
            );
        }
        workspace.set_document(document, changed);
    }
    let client_after = workspace.snapshot("client").unwrap();
    for spelling in [
        "(|new name|)",
        "(reexports:|new name|)",
        "(lib-alias:|new name|)",
    ] {
        let call = client_after.text().find(spelling).unwrap() + 1;
        assert_eq!(
            workspace.definition("client", call).unwrap().document,
            "library"
        );
    }
}

#[test]
fn lexical_rename_requires_references_to_keep_their_binding() {
    let mut workspace = SourceWorkspace::new();
    for (source, accepted) in [
        ("(let ((old^ 1)) (let ((new 2)) old))", false),
        ("(let ((new 1)) (let ((old^ 2)) (list old new)))", false),
        ("(let ((old^ 1)) old) (let ((new 2)) new)", true),
        (
            "(defun sample (old^) (flet ((new () 1)) (list old (new))))",
            true,
        ),
        ("(let ((old^ 1)) (let* ((new old)) new))", true),
    ] {
        let (text, offset) = cursor(source);
        workspace.set_document("lexical", &text);
        assert_eq!(
            workspace.rename("lexical", offset - 1, "new").is_some(),
            accepted,
            "{source}"
        );
    }
}

#[test]
fn global_rename_rejects_capture_in_another_document() {
    let mut workspace = SourceWorkspace::new();
    let library = "(in-package #:lib) (defun old-name () 1)";
    let client = "(in-package #:lib) (flet ((new-name () 2)) (old-name))";
    workspace.set_document("library", library);
    workspace.set_document("client", client);
    assert!(
        workspace
            .rename("library", library.find("old-name").unwrap(), "new-name")
            .is_none()
    );
}

#[test]
fn rename_respects_the_namespace_of_standard_common_lisp_names() {
    let mut workspace = SourceWorkspace::new();
    for (source, name, accepted) in [
        ("(let ((old^ 1)) old)", "t", false),
        ("(let ((old^ 1)) old)", "*package*", false),
        ("(let ((old^ 1)) old)", "car", true),
        ("(flet ((old^ () 1)) (old))", "car", false),
        ("(flet ((old^ () 1)) (old))", "pi", true),
    ] {
        let (text, offset) = cursor(source);
        workspace.set_document("lexical", &text);
        workspace.set_initial_package("lexical", Some("CL-USER".into()));
        assert_eq!(
            workspace.rename("lexical", offset - 1, name).is_some(),
            accepted,
            "{source} -> {name}"
        );
    }
}

#[test]
fn global_rename_rejects_changes_that_break_unedited_package_users() {
    let library = "(defpackage #:lib (:use #:cl) (:export #:old-name))\n\
                   (in-package #:lib) (defun old-name () 1)";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", library);
    workspace.set_document("other", "(defpackage #:other (:export #:new-name))");
    let declaration = library.find("defun old-name").unwrap() + "defun ".len();

    for consumer in [
        "(defpackage #:consumer (:use #:lib #:other))",
        "(in-package #:cl-user) '(lib:old-name)",
    ] {
        workspace.set_document("consumer", consumer);
        assert!(
            workspace
                .rename("library", declaration, "new-name")
                .is_none(),
            "{consumer}"
        );
    }
}

#[test]
fn rename_validation_reuses_the_documents_reader_features() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(vec!["TEST-FEATURE".to_owned()]));
    let source = "#+test-feature (defun example (old) old)\n\
                  #-test-feature (defun example (old) old)";
    workspace.set_document("conditional", source);
    workspace.set_initial_package("conditional", Some("CL-USER".into()));
    let offset = source.find("(old)").unwrap() + 1;
    let edits = workspace.rename("conditional", offset, "new").unwrap();
    assert_eq!(edits.len(), 2);
    assert!(
        edits
            .iter()
            .all(|(edit, _)| usize::from(edit.range.end()) < source.find("#-").unwrap())
    );
}

const CONTROL_TARGETS_SOURCE: &str = r#"(defpackage :origin (:use :cl) (:export :again))
(defpackage :client (:use :cl :origin) (:shadow :nil))
(in-package :client)
(block cl:nil
  (tagbody
    again
    00017
    (tagbody
      #x11
      (funcall (lambda () (go +17.)))
      (go origin:again))
    (go #b10001)
    (go |17|)
    1000000000000000000000000000000000000000000000
    (go +1000000000000000000000000000000000000000000000)
    (go 34/2)
    (block nil (return 1))
    (macrolet ((return (form) nil)) (return (return-from cl:nil 2)))
    (dotimes (index (return 3)) loop-marker
      (go loop-marker)
      (return-from cl:nil index))))"#;

#[test]
fn go_resolves_the_nearest_tag_by_symbol_or_integer_identity() {
    let mut workspace = SourceWorkspace::new();
    let source = CONTROL_TARGETS_SOURCE;
    workspace.set_document("control", source);
    let offset = |needle: &str, prefix: &str| source.find(needle).unwrap() + prefix.len();
    let definition = |needle: &str, prefix: &str| {
        workspace
            .definition("control", offset(needle, prefix))
            .map(|location| usize::from(location.selection_range.start()))
    };
    for (source_text, prefix, target) in [
        ("(go +17.)", "(go ", "#x11"),
        ("(go #b10001)", "(go ", "00017"),
        ("(go 34/2)", "(go ", "00017"),
        (
            "(go +100000",
            "(go ",
            "1000000000000000000000000000000000000000000000\n",
        ),
        ("(go origin:again)", "(go ", "again\n"),
        ("(go loop-marker)", "(go ", "loop-marker\n"),
    ] {
        assert_eq!(
            definition(source_text, prefix),
            Some(source.find(target).unwrap()),
            "{source_text}"
        );
    }
    assert_eq!(definition("(go |17|)", "(go "), None);
    let refs = |needle: &str| {
        workspace
            .references("control", source.find(needle).unwrap(), true)
            .into_iter()
            .map(|location| &source[location.range])
            .collect::<Vec<_>>()
    };
    assert_eq!(refs("00017"), ["00017", "#b10001", "34/2"]);
    assert!(
        workspace
            .prepare_rename("control", source.find("#x11").unwrap())
            .is_none()
    );
}

#[test]
fn return_resolves_the_visible_block_named_common_lisp_nil() {
    let mut workspace = SourceWorkspace::new();
    let source = CONTROL_TARGETS_SOURCE;
    workspace.set_document("control", source);
    let offset = |needle: &str, prefix: &str| source.find(needle).unwrap() + prefix.len();
    let definition = |needle: &str, prefix: &str| {
        workspace
            .definition("control", offset(needle, prefix))
            .map(|location| usize::from(location.selection_range.start()))
    };
    for (source_text, prefix, target) in [
        ("(return 1)", "(", "cl:nil\n"),
        ("(return 3)", "(", "dotimes"),
        ("(return-from cl:nil index)", "(return-from ", "dotimes"),
    ] {
        assert_eq!(
            definition(source_text, prefix),
            Some(source.find(target).unwrap()),
            "{source_text}"
        );
    }
    assert_eq!(definition("(return-from cl:nil 2)", "(return-from "), None);
    let refs = |needle: &str| {
        workspace
            .references("control", source.find(needle).unwrap(), true)
            .into_iter()
            .map(|location| &source[location.range])
            .collect::<Vec<_>>()
    };
    assert_eq!(refs("dotimes"), ["dotimes", "return", "cl:nil"]);
    assert!(
        workspace
            .prepare_rename("control", source.find("dotimes").unwrap())
            .is_none()
    );
}

#[test]
fn empty_list_control_names_resolve_as_nil() {
    let mut workspace = SourceWorkspace::new();
    let source = "(in-package :cl-user)\n\
                  (block () (tagbody nil (go ())) (return 1))\n\
                  (prog nil (return-from () 2))\n\
                  (loop (return 3))";
    workspace.set_document("control", source);
    for (needle, prefix, expected) in [
        ("(go ())", "(go ", "nil (go"),
        ("(return 1)", "(", "() (tagbody"),
        ("(return-from () 2)", "(return-from ", "prog nil"),
        ("(return 3)", "(", "loop ("),
    ] {
        let use_offset = source.find(needle).unwrap() + prefix.len();
        let definition = workspace.definition("control", use_offset).unwrap();
        assert_eq!(
            usize::from(definition.selection_range.start()),
            source.find(expected).unwrap()
        );
        assert_eq!(workspace.references("control", use_offset, true).len(), 2);
    }
}

#[test]
fn extended_loop_returns_resolve_by_block_name() {
    let mut workspace = SourceWorkspace::new();
    let source = r#"(in-package :cl-user)
(block nil
  (loop :named walk
        :doing (return-from walk 1)
               (return 2)
        finally (funcall (lambda () (return-from walk 3))))
  (loop do (return 4))
  (loop named nil do (return 5)))
(loop named alone do (return 6))
(let ((value 7))
  (loop named opaque for value = (return-from opaque 8)
        do (return-from opaque value)))"#;
    workspace.set_document("control", source);
    let offset = |needle: &str, prefix: &str| source.find(needle).unwrap() + prefix.len();
    for (needle, prefix, expected, target_prefix) in [
        (
            "(return-from walk 1)",
            "(return-from ",
            ":named walk",
            ":named ",
        ),
        (
            "(return-from walk 3)",
            "(return-from ",
            ":named walk",
            ":named ",
        ),
        ("(return 2)", "(", "block nil", "block "),
        ("(return 4)", "(", "loop do", ""),
        ("(return 5)", "(", "named nil", "named "),
    ] {
        let definition = workspace
            .definition("control", offset(needle, prefix))
            .unwrap();
        assert_eq!(
            usize::from(definition.selection_range.start()),
            offset(expected, target_prefix),
            "{needle}"
        );
    }
    for (needle, prefix) in [
        ("(return 6)", "("),
        ("(return-from opaque 8)", "(return-from "),
        ("(return-from opaque value)", "(return-from "),
        ("opaque value", "opaque "),
    ] {
        assert!(
            workspace
                .definition("control", offset(needle, prefix))
                .is_none(),
            "{needle}"
        );
    }
    assert_eq!(
        workspace
            .references("control", offset(":named walk", ":named "), true)
            .len(),
        3
    );
    assert_eq!(
        workspace
            .references("control", offset("block nil", "block "), true)
            .len(),
        2
    );
}

const LOOP_SCOPE_SOURCE: &str = r#"(defun run (&optional (x (return-from run 1)))
  (block choose
    (flet ((choose (&optional (arg (return-from choose 2)))
             (return-from choose arg)))
      (choose)))
  (tagbody
    step
    (do ((i (return 3) (go step))) ((go step) (go step))
      step
      (go step)))
  (return-from run x))"#;

#[test]
fn callable_implicit_blocks_begin_after_optional_initializers() {
    let mut workspace = SourceWorkspace::new();
    let source = LOOP_SCOPE_SOURCE;
    workspace.set_document("control", source);
    workspace.set_initial_package("control", Some("CL-USER".into()));
    let definition = |needle: &str, prefix: &str| {
        let offset = source.find(needle).unwrap() + prefix.len();
        workspace
            .definition("control", offset)
            .map(|location| usize::from(location.selection_range.start()))
    };
    assert_eq!(definition("(return-from run 1)", "(return-from "), None);
    assert_eq!(
        definition("(return-from run x)", "(return-from "),
        source.find("run (&optional")
    );
    assert_eq!(
        definition("(return-from choose 2)", "(return-from "),
        source.find("choose\n")
    );
    assert_eq!(
        definition("(return-from choose arg)", "(return-from "),
        source.find("choose (&optional")
    );
}

#[test]
fn do_implicit_nil_block_is_visible_during_variable_initialization() {
    let mut workspace = SourceWorkspace::new();
    let source = LOOP_SCOPE_SOURCE;
    workspace.set_document("control", source);
    workspace.set_initial_package("control", Some("CL-USER".into()));
    let offset = source.find("(return 3)").unwrap() + 1;
    let block = workspace.definition("control", offset).unwrap();
    assert_eq!(
        Some(usize::from(block.selection_range.start())),
        source.find("do ((")
    );
}

#[test]
fn do_local_tag_shadows_the_outer_tag_only_in_the_body() {
    let mut workspace = SourceWorkspace::new();
    let source = LOOP_SCOPE_SOURCE;
    workspace.set_document("control", source);
    workspace.set_initial_package("control", Some("CL-USER".into()));
    let targets = source
        .match_indices("(go step)")
        .map(|(offset, _)| {
            let location = workspace
                .definition("control", offset + "(go ".len())
                .unwrap();
            usize::from(location.selection_range.start())
        })
        .collect::<Vec<_>>();
    let outer = source.find("step\n").unwrap();
    let inner = source.rfind("step\n").unwrap();
    assert_eq!(targets, [outer, outer, outer, inner]);
}

#[test]
fn conditional_tags_preserve_uncertainty_about_the_nearest_target() {
    let mut workspace = SourceWorkspace::new();
    let source = "(tagbody 16 (tagbody #+inner #x10 (go 16)))\n\
                  (setq *read-base* 16)\n(tagbody 16 (go 16))";
    workspace.set_document("control", source);
    let jump = source.find("(go 16)").unwrap() + "(go ".len();
    let after_reader_change = source.rfind("(go 16)").unwrap() + "(go ".len();
    assert!(workspace.definition("control", jump).is_none());
    workspace.set_features(Some(vec!["INNER".into()]));
    assert_eq!(
        usize::from(
            workspace
                .definition("control", jump)
                .unwrap()
                .selection_range
                .start()
        ),
        source.find("#x10").unwrap()
    );
    workspace.set_features(Some(vec![]));
    assert_eq!(
        usize::from(
            workspace
                .definition("control", jump)
                .unwrap()
                .selection_range
                .start()
        ),
        source.find("16").unwrap()
    );
    assert!(
        workspace
            .definition("control", after_reader_change)
            .is_none()
    );
}

#[test]
fn special_declarations_redirect_references_only_in_scope() {
    let mut workspace = SourceWorkspace::new();
    let source = r#"(in-package :cl-user)
(defun sample (x)
  (list x
        (locally (declare (special x)) (lambda () x))
        (let* ((x x) (other x))
          (declare (special x))
          (list other x
                (let ((x 3)) x)))
        (lambda (&optional (copy x))
          (declare (special x)) (list copy x))
        (lambda (x &optional (copy x))
          "Docstring."
          (declare (special x)) (list copy x))
        (do ((i 0 x)) (x i) (declare (special x)) x)
        (multiple-value-bind (x) x (declare (special x)) x)))"#;
    workspace.set_document("special", source);
    let definition = |needle: &str, prefix: &str| {
        let offset = source.find(needle).unwrap() + prefix.len();
        workspace
            .definition("special", offset)
            .map(|location| usize::from(location.selection_range.start()))
    };
    let lexical = source.find("x)\n").unwrap();
    for (text, prefix) in [
        ("(list x\n", "(list "),
        ("(x x) (other", "(x "),
        ("(copy x))\n          (declare", "(copy "),
        ("(x) x (declare", "(x) "),
    ] {
        assert_eq!(definition(text, prefix), Some(lexical), "{text}");
    }
    for (text, prefix) in [
        ("(lambda () x)", "(lambda () "),
        ("(other x)", "(other "),
        ("(list other x", "(list other "),
        ("(copy x))\n          \"Docstring.", "(copy "),
        ("(i 0 x)", "(i 0 "),
        ("(x i)", "("),
        ("(declare (special x)) x)))", "(declare (special x)) "),
    ] {
        assert_eq!(definition(text, prefix), None, "{text}");
    }
    assert_eq!(
        definition("(let ((x 3)) x)", "(let ((x 3)) "),
        source.find("x 3")
    );
    assert_eq!(
        definition("(special x))\n          (list other", "(special "),
        source.find("x x)")
    );
    assert!(workspace.semantic_warnings("special").is_empty());
}

#[test]
fn proclaimed_special_variables_keep_their_package_identity() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "variables",
        "(defpackage :vars (:use :cl) (:export :dynamic))\n\
        (in-package :vars) (declaim (special dynamic)) (define-symbol-macro static 2)",
    );
    let source = "(defpackage :client (:use :cl) (:import-from :vars :dynamic))\n\
        (in-package :client)\n\
        (let ((dynamic 1) (vars::static 2)) (list vars:dynamic vars::static))";
    workspace.set_document("client", source);
    let dynamic_reference = source.find("vars:dynamic").unwrap();
    let static_reference = source.rfind("vars::static").unwrap();
    assert!(workspace.definition("client", dynamic_reference).is_none());
    assert_eq!(
        usize::from(
            workspace
                .definition("client", static_reference)
                .unwrap()
                .selection_range
                .start()
        ),
        source.find("vars::static").unwrap()
    );
    assert!(workspace.semantic_warnings("client").is_empty());
}

const NESTED_BINDINGS_SOURCE: &str = r#"(in-package :demo)
(defun compute (x &optional (step 1 step-p))
  "Compute a stepped value."
  (let* ((a (+ x step))
         (b (if step-p a x)))
    (flet ((add-b (val) (+ val b)))
      (block done
        (tagbody
         start
          (when (< b 0) (go start))
          (return-from done (add-b a)))))))
#+custom-feature
(defun feature-enabled () 100)
#-custom-feature
(defun feature-disabled () 200)
"#;

fn workspace_with_nested_bindings() -> SourceWorkspace {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("demo", NESTED_BINDINGS_SOURCE);
    workspace
}

#[test]
fn definition_finds_bindings_in_nested_lexical_scopes() {
    let workspace = workspace_with_nested_bindings();
    let code = NESTED_BINDINGS_SOURCE;
    let b_ref = code.find("b)))").unwrap();
    let b_def = workspace.definition("demo", b_ref).unwrap();
    assert_eq!(
        usize::from(b_def.selection_range.start()),
        code.find("b (if").unwrap()
    );

    let call_offset = code.find("add-b a").unwrap();
    let local_definition = workspace.definition("demo", call_offset).unwrap();
    assert_eq!(
        usize::from(local_definition.selection_range.start()),
        code.find("add-b (val)").unwrap()
    );

    let done_ref = code.find("done (add-b").unwrap();
    let done_def = workspace.definition("demo", done_ref).unwrap();
    assert_eq!(
        usize::from(done_def.selection_range.start()),
        code.find("done\n").unwrap()
    );

    let start_ref = code.find("start))").unwrap();
    let start_def = workspace.definition("demo", start_ref).unwrap();
    assert_eq!(
        usize::from(start_def.selection_range.start()),
        code.find("start\n").unwrap()
    );
}

#[test]
fn rename_edits_all_reported_references_to_a_lexical_variable() {
    let workspace = workspace_with_nested_bindings();
    let code = NESTED_BINDINGS_SOURCE;
    let a_refs = workspace.references("demo", code.find("a x)))").unwrap(), true);
    assert_eq!(a_refs.len(), 3);
    let renamed = workspace
        .rename("demo", code.find("a x)))").unwrap(), "alpha")
        .unwrap();
    assert_eq!(renamed.len(), 3);
}

#[test]
fn hover_at_function_definition_includes_its_docstring() {
    let workspace = workspace_with_nested_bindings();
    let code = NESTED_BINDINGS_SOURCE;
    let hover = workspace
        .hover("demo", code.find("compute").unwrap())
        .unwrap();
    assert!(hover.contents.contains("Compute a stepped value."));
}

#[test]
fn signature_help_marks_the_second_argument_in_a_nested_call() {
    let workspace = workspace_with_nested_bindings();
    let code = NESTED_BINDINGS_SOURCE;
    let sig = workspace
        .signature_help("demo", code.find("val b)").unwrap() + 4)
        .unwrap();
    assert_eq!(sig.label, "(+ &rest numbers)");
    assert_eq!(sig.active_parameter, Some(1));
}

#[test]
fn setting_reader_features_indexes_only_the_active_definition() {
    let mut workspace = workspace_with_nested_bindings();
    let caller = "(in-package :demo)\n(list (feature-enabled) (feature-disabled))";
    workspace.set_document("caller", caller);
    assert!(
        workspace
            .definition("caller", caller.find("feature-enabled").unwrap())
            .is_none()
    );
    workspace.set_features(Some(vec!["CUSTOM-FEATURE".into()]));
    assert!(
        workspace
            .definition("caller", caller.find("feature-enabled").unwrap())
            .is_some()
    );
    assert!(
        workspace
            .definition("caller", caller.find("feature-disabled").unwrap())
            .is_none()
    );
}

#[test]
fn pattern_navigation_distinguishes_constructors_from_bound_variables() {
    let text = "(defpackage :responses (:use :cl)) (in-package :responses)
(zk:defvariant reply (ok value) (pending))
(defun decode (r) (zk:match r ((ok payload) (list payload)) (_ nil)))
(defun use-reply (r) (zk:match r ((ok n) n) (_ nil)))";
    let mut ws = SourceWorkspace::new();
    ws.set_document("variants", text);
    let take = |from: usize, at: usize| {
        let target = ws
            .definition("variants", from)
            .unwrap_or_else(|| panic!("no target at {from}"));
        assert_eq!(usize::from(target.selection_range.start()), at);
    };
    take(text.rfind("ok n").unwrap(), text.find("ok value").unwrap());
    take(text.rfind("n) n").unwrap() + 3, text.rfind("n) n").unwrap());
    take(
        text.find("list payload").unwrap() + 5,
        text.find("ok payload").unwrap() + 3,
    );
    let outline = ws.snapshot("variants").unwrap();
    assert!(
        outline
            .outline()
            .iter()
            .any(|item| item.name == "reply" && item.kind == OutlineKind::Type)
    );
    assert!(
        outline
            .outline()
            .iter()
            .any(|item| item.name == "decode" && item.kind == OutlineKind::Function)
    );
}

#[test]
fn method_pattern_navigation_follows_package_imports() {
    let provider = "(defpackage :wire (:use :cl) (:export :response :received :+limit+))
(in-package :wire)
(defconstant +limit+ 31)
(zk:defvariant response (received data))";
    let client = "(defpackage :app (:use :cl) (:import-from :wire :response :received :+limit+))
(in-package :app)
(defgeneric display (r))
(defmethod display ((r response))
  (zk:match r
    ((received (:const +limit+)) :limit)
    ((received value) (list value))))";
    let mut ws = SourceWorkspace::new();
    ws.set_document("provider", provider);
    ws.set_document("client", client);
    for (use_site, definition) in [
        ("received (:const", "received data"),
        ("+limit+)) :limit", "+limit+ 31"),
    ] {
        let target = ws
            .definition("client", client.find(use_site).unwrap())
            .unwrap();
        assert_eq!(target.document, "provider");
        assert_eq!(
            usize::from(target.selection_range.start()),
            provider.find(definition).unwrap()
        );
    }
    let target = ws
        .definition("client", client.rfind("value").unwrap())
        .unwrap();
    assert_eq!(
        usize::from(target.selection_range.start()),
        client.find("value").unwrap()
    );
}

#[test]
fn method_navigation_uses_definition_scope_for_eql_and_method_scope_for_body() {
    let provider = r#"(defpackage :model (:use :cl) (:export :widget))
(in-package :model)
(defclass widget () ())"#;
    let client = r#"(defpackage :client (:use :cl) (:import-from :model :widget))
(in-package :client)
(let ((shadowed :definition))
  (defmethod standalone ((shadowed widget) (method-key (eql shadowed)))
    (list method-key shadowed))
  (defgeneric (setf probe) (value key &optional extra)
    (keyword::method :around
        ((shadowed model:widget) (inline-key (eql (let ((specializer-local shadowed))
                                                  (list specializer-local))))
         &optional (fallback shadowed))
      (return-from probe (list inline-key fallback shadowed)))))"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("provider", provider);
    workspace.set_document("client", client);
    for reference in ["widget) (method-key", "model:widget"] {
        let definition = target(&workspace, "client", client, reference);
        assert_eq!(definition.document, "provider");
        assert_eq!(
            usize::from(definition.selection_range.start()),
            provider.rfind("widget").unwrap()
        );
    }
    for (reference, shift, binding) in [
        ("eql shadowed", 4, "shadowed :definition"),
        ("method-key shadowed", 11, "shadowed widget"),
        ("specializer-local shadowed", 18, "shadowed :definition"),
        ("list specializer-local", 5, "specializer-local shadowed"),
        ("fallback shadowed))", 9, "shadowed model:widget"),
        ("list inline-key", 5, "inline-key (eql"),
        ("inline-key fallback", 11, "fallback shadowed"),
        ("fallback shadowed)))))", 9, "shadowed model:widget"),
        ("return-from probe", 12, "probe) (value"),
    ] {
        let offset = client.find(reference).unwrap() + shift;
        let definition = workspace
            .definition("client", offset)
            .unwrap_or_else(|| panic!("{reference}"));
        assert_eq!(definition.document, "client");
        assert_eq!(
            usize::from(definition.selection_range.start()),
            client.find(binding).unwrap(),
            "{reference}"
        );
    }
    assert_eq!(
        workspace
            .references("provider", provider.rfind("widget").unwrap(), false)
            .iter()
            .filter(|reference| reference.document == "client")
            .count(),
        2
    );
}

#[test]
fn inline_method_completion_distinguishes_method_and_eql_bindings() {
    let mut workspace = SourceWorkspace::new();
    for (source, visible, hidden) in [
        (
            "(let ((definition-value 1)) (defgeneric probe (a b) (:method (method-value (b (eql (let ((eql-value 1)) (list ^))))) (list method-value))))",
            ["definition-value", "eql-value"],
            "method-value",
        ),
        (
            "(let ((definition-value 1)) (defgeneric probe (a b) (:method (method-value (b t)) (list ^))))",
            ["definition-value", "method-value"],
            "a",
        ),
    ] {
        let (text, offset) = cursor(source);
        workspace.set_document("methods", &text);
        let candidates = workspace.completions("methods", offset);
        for name in visible {
            assert!(
                candidates.iter().any(|candidate| candidate.label == name),
                "{name}: {source}"
            );
        }
        assert!(
            candidates.iter().all(|candidate| candidate.label != hidden),
            "{hidden}: {source}"
        );
    }
}

#[test]
fn match_arity_warnings_select_the_malformed_form() {
    let text = "(defpackage :matrix (:use :cl)) (in-package :matrix)
(zk:defvariant token (letter ch))
(zk:match (:inputs a b) (((letter x)) 1) (((letter x y) _) 2) (_ 3))";
    let mut ws = SourceWorkspace::new();
    ws.set_document("matrix", text);
    let warnings = ws.semantic_warnings("matrix");
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("expected 2 match columns")
                && usize::from(w.range.start()) == text.find("((letter x))").unwrap()),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("expects 1 fields")
                && usize::from(w.range.start()) == text.find("(letter x y)").unwrap()),
        "{warnings:?}"
    );
}

#[test]
fn pattern_navigation_distinguishes_outer_comparisons_and_capture_bindings() {
    let text = r#"(in-package :cl-user)
(defun example (request-id packet text)
  (zk:match packet
    ((list request-id (:= request-id) response) (list request-id response))
    ((zk-bits:bits (u8be size) (:bytes size payload)) payload)
    (_ (zk-lex:lexmatch text :prefix
         ((:with (:capture label "if") :after (tail :as :span)) (list label tail))
         (_ nil)))))"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("pattern-scope", text);
    for (reference, shift, binding, binding_shift) in [
        (":= request-id", 3, "request-id packet", 0),
        ("request-id response", 0, "request-id (:=", 0),
        (":bytes size", 7, "size) (:bytes", 0),
        ("payload)) payload", 10, "payload)) payload", 0),
        ("label tail))", 0, "label \"if\"", 0),
        ("label tail))", 6, "tail :as", 0),
    ] {
        let position = text.find(reference).unwrap() + shift;
        let actual = workspace
            .definition("pattern-scope", position)
            .unwrap_or_else(|| panic!("no definition for {reference}"));
        assert_eq!(
            usize::from(actual.selection_range.start()),
            text.find(binding).unwrap() + binding_shift,
            "{reference}"
        );
    }

    let modern = r#"(in-package :cl-user)
(zk:defvariant (reply (:predicate reply-p))
  (pending)
  (ok (value integer)))
(defun classify (x)
  (declare (type reply x))
  (zk:if-match (zk:view (lambda (r) (ok-value r)) val) x
    :when (plusp val)
    val
    0))"#;
    workspace.set_document("modern-match", modern);
    let guard_target = workspace
        .definition("modern-match", modern.find("plusp val").unwrap() + 6)
        .expect("guard binding in if-match resolves to pattern");
    assert_eq!(
        usize::from(guard_target.selection_range.start()),
        modern.find("val) x").unwrap()
    );
    let body_target = workspace
        .definition("modern-match", modern.rfind("val\n").unwrap())
        .expect("then branch binding in if-match resolves to pattern");
    assert_eq!(
        usize::from(body_target.selection_range.start()),
        modern.find("val) x").unwrap()
    );
    let lambda_target = workspace
        .definition("modern-match", modern.find("ok-value r").unwrap() + 9)
        .expect("view lambda parameter resolves to lambda binding");
    assert_eq!(
        usize::from(lambda_target.selection_range.start()),
        modern.find("(r)").unwrap() + 1
    );
}

#[test]
fn local_package_nicknames_resolve_in_the_referring_package() {
    let mut workspace = SourceWorkspace::new();
    for library in ["left", "right", "short"] {
        let source = format!(
            "(defpackage :{library} (:use :cl) (:export :entry))
                              (in-package :{library}) (defun entry () 1)"
        );
        workspace.set_document(library, source);
    }
    for (owner, target) in [("first", "left"), ("second", "right")] {
        let source = format!(
            "(defpackage :{owner} (:use :cl) (:local-nicknames (:short :{target}))) (in-package :{owner}) (short:entry) (short:ent"
        );
        workspace.set_document(owner, &source);
        assert_eq!(
            workspace
                .definition(owner, source.find("short:entry").unwrap())
                .unwrap()
                .document,
            target
        );
        assert!(
            workspace
                .completions(owner, source.len())
                .iter()
                .any(|entry| entry.label == "short:entry")
        );
    }
    let outside = "(in-package :cl-user) (short:entry)";
    workspace.set_document("outside", outside);
    assert_eq!(
        workspace
            .definition("outside", outside.find("short:entry").unwrap())
            .unwrap()
            .document,
        "short"
    );
    let unknown = "(setq *package* (choose-package)) (short:entry)";
    workspace.set_document("unknown", unknown);
    assert!(
        workspace
            .definition("unknown", unknown.find("short:entry").unwrap())
            .is_none()
    );
}

#[test]
fn simd_package_offers_static_function_and_type_metadata() {
    let mut workspace = SourceWorkspace::new();
    let source = "(zk-simd:load-masked 'zk-simd:f64x8 array 0 mask ";
    workspace.set_document("load", source);

    let signature = workspace.signature_help("load", source.len()).unwrap();
    assert_eq!(
        signature.parameters,
        ["type", "array", "start", "mask", "&optional", "passthrough"]
    );
    assert_eq!(signature.active_parameter, Some(5));

    for (name, text) in [
        ("load-masked", "passthrough or zero"),
        ("f64x8", "An exact pack type"),
    ] {
        let hover = workspace.hover("load", source.find(name).unwrap()).unwrap();
        assert!(hover.contents.contains(text));
    }

    workspace.set_document("simd-completion", "zk-simd:");
    let candidates = workspace.completions("simd-completion", "zk-simd:".len());
    for (label, kind) in [
        ("zk-simd:cmp/=", CompletionKind::Function),
        ("zk-simd:mask-from-bits", CompletionKind::Function),
        ("zk-simd:pack", CompletionKind::Type),
        ("zk-simd:mask", CompletionKind::Type),
        ("zk-simd:integer-overflow", CompletionKind::Type),
    ] {
        assert!(
            candidates
                .iter()
                .any(|c| c.label == label && c.kind == kind)
        );
    }
    for name in zkcl_syntax::simd_builtins::PACK_TYPES {
        let label = format!("zk-simd:{}", name.to_ascii_lowercase());
        assert!(
            candidates
                .iter()
                .any(|c| c.label == label && c.kind == CompletionKind::Type)
        );
    }

    let source = "(zk-simd:reduce-sum value :ord";
    workspace.set_document("reduce", source);
    assert_eq!(
        workspace
            .signature_help("reduce", source.len())
            .unwrap()
            .parameters,
        ["pack", "&key", "(order :left)"]
    );
    assert!(
        workspace
            .completions("reduce", source.len())
            .iter()
            .any(|c| c.label == ":order")
    );
}

#[test]
fn simd_tooling_follows_use_import_and_local_nickname_paths() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "packages",
        "(defpackage :bridge (:use :zk-simd) (:export :load-masked))
         (defpackage :owner (:use :zk-simd) (:local-nicknames (:s :bridge)))
         (defpackage :importer (:use :cl) (:import-from :zk-simd :load-masked))",
    );
    for (package, qualifier) in [("owner", ""), ("owner", "s:"), ("importer", "")] {
        let source = format!("(in-package :{package}) ({qualifier}load-masked type ");
        workspace.set_document("caller", &source);
        let signature = workspace.signature_help("caller", source.len()).unwrap();
        assert_eq!(signature.parameters[1], "array");
        let hover = workspace
            .hover("caller", source.find("load-masked").unwrap())
            .unwrap();
        assert!(hover.contents.contains("zk-simd:load-masked"));

        let prefix = format!("(in-package :{package}) ({qualifier}load-mas");
        workspace.set_document("caller", &prefix);
        assert!(
            workspace
                .completions("caller", prefix.len())
                .iter()
                .any(|candidate| {
                    candidate.label == format!("{qualifier}load-masked")
                        && candidate.kind == CompletionKind::Function
                })
        );
    }
}

#[test]
fn simd_tooling_leaves_shadowed_and_ambiguous_names_unresolved() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "packages",
        "(defpackage :other (:export :add))
         (defpackage :ambiguous (:use :zk-simd :other :cl))
         (defpackage :shadowed (:use :zk-simd) (:shadow :add))
         (defpackage :with-nickname (:local-nicknames (:s :zk-simd)))",
    );
    for (context, qualifier, name) in [
        ("(in-package :shadowed)", "", "add"),
        ("(in-package :ambiguous)", "", "add"),
        ("(in-package :ambiguous)", "", "sqrt"),
        ("(setq *package* (choose-package))", "s:", "add"),
        ("(setq *readtable* (choose-readtable))", "zk-simd:", "add"),
    ] {
        let source = format!("{context} ({qualifier}{name} value ");
        workspace.set_document("caller", &source);
        assert!(
            workspace.signature_help("caller", source.len()).is_none(),
            "{source}"
        );
        assert!(
            workspace
                .hover("caller", source.rfind(name).unwrap())
                .is_none(),
            "{source}"
        );

        let label = format!("{qualifier}{name}");
        let prefix = format!("{context} ({label}");
        workspace.set_document("caller", &prefix);
        assert!(
            workspace
                .completions("caller", prefix.len())
                .iter()
                .all(|c| c.label != label),
            "{prefix}"
        );
    }

    let source = "(flet ((zk-simd:add (local) local)) (zk-simd:add value ";
    workspace.set_document("caller", source);
    let signature = workspace.signature_help("caller", source.len()).unwrap();
    assert_eq!(signature.parameters, ["local"]);
}
