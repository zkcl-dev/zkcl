#![cfg(feature = "analysis")]

use zkcl_syntax::documentation::DocumentationTests;
use zkcl_syntax::syntax::{SourceReadtable, TextRange, TextSize};
use zkcl_syntax::{LintConfig, SourcePosition, SourcePositionEncoding, SourceWorkspace};

#[test]
fn documentation_follows_source_edits_and_leaves_ansi_docstrings_as_fallback() {
    let source = "(in-package :cl-user)\n;;! Greet `name`.\n(defun greet (name) \"ANSI docs.\" name)\n(greet #\\A)";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", source);
    let call = source.rfind("greet").unwrap();
    let before = workspace.snapshot("library").unwrap();
    assert_eq!(before.documentation()[0].markdown(), "Greet `name`.\n");
    assert!(
        workspace
            .hover("library", call)
            .unwrap()
            .contents
            .contains("Greet `name`.")
    );

    let start = source.find(";;!").unwrap();
    let comment = start..source.find("(defun").unwrap();
    workspace
        .apply_edit("library", comment.clone(), "")
        .unwrap();
    assert!(
        workspace
            .snapshot("library")
            .unwrap()
            .documentation()
            .is_empty()
    );
    assert!(
        workspace
            .hover("library", call - comment.len())
            .unwrap()
            .contents
            .contains("ANSI docs.")
    );
    assert_eq!(before.documentation()[0].markdown(), "Greet `name`.\n");
}

#[test]
fn only_adjacent_own_line_comments_document_active_declarations() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(vec!["enabled".into()]));
    workspace.set_document(
        "library",
        r##"
(in-package :cl-user)
(defparameter *string* ";;! string
;;! still string")
#| ;;! block comment |#
(defun no-docs () nil) ;;! trailing comment
(defun no-inline-docs () nil)
;;! Separated.

(defun no-separated-docs () nil)
(defun no-nested-docs ()
  ;;! Nested.
  (flet ((inner () nil)) (inner)))
;;! Disabled.
#+missing (defun no-disabled-docs () nil)
(progn
  ;;! Active.
  #+enabled (defun active () nil))
"##,
    );
    let snapshot = workspace.snapshot("library").unwrap();
    let [comment] = snapshot.documentation() else {
        panic!("{:?}", snapshot.documentation())
    };
    assert_eq!(comment.markdown(), "Active.\n");
    let definition: std::ops::Range<usize> = comment.definition.into();
    assert_eq!(&snapshot.text()[definition], "(defun active () nil)");
}

#[test]
fn comment_diagnostics_map_to_the_edited_crlf_source() {
    let source = "(in-package :cl-user)\r\n  ;;! ```lisp test\r\n  ;;! (\"😀\"))\r\n  ;;! ```\r\n(defun demo () nil)";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", source);
    let before = workspace.snapshot("library").unwrap();
    let [error] = before.documentation_tests().diagnostics.as_slice() else {
        panic!("{:?}", before.documentation_tests())
    };
    let extra = source.find("))").unwrap() + 1;
    assert_eq!(
        error.range,
        TextRange::at(TextSize::try_from(extra).unwrap(), 1.into())
    );
    assert_eq!(
        before.position_with_encoding(extra, SourcePositionEncoding::Utf16),
        Some(SourcePosition {
            line: 2,
            column: 12
        })
    );
    workspace
        .apply_edit("library", extra..extra + 1, "")
        .unwrap();
    let after = workspace.snapshot("library").unwrap();
    assert!(after.documentation_tests().diagnostics.is_empty());
    assert_eq!(
        after.documentation_tests().examples[0].source(),
        "(\"😀\")\n"
    );
    assert_eq!(before.documentation_tests().diagnostics.len(), 1);
}

#[test]
fn markdown_uses_real_fences_and_preserves_projected_source() {
    let source = "# Example\n\n````lisp\n```lisp test\n(error \"displayed\")\n```\n````\n\n\
                  > ```lisp test\n> (error \"quoted\")\n> ```\n\n\
                  <!--\n```lisp test\n(error \"hidden\")\n```\n-->\n\n\
                  ```lisp te&#115;t\n(error \"entity\")\n```\n\n  ~~~lisp test\r\n\t(+ 40 2)\r\n  ~~~\r\n";
    let tests = DocumentationTests::markdown(source, SourceReadtable::Standard);
    assert!(tests.diagnostics.is_empty(), "{tests:?}");
    let [example] = tests.examples.as_slice() else {
        panic!("{tests:?}")
    };
    assert_eq!(example.source(), "  (+ 40 2)\n");
    let start = source.find("(+ 40 2)").unwrap();
    assert_eq!(
        example.source_range(2..10),
        Some(TextRange::at(TextSize::try_from(start).unwrap(), 8.into()))
    );
    assert_eq!(example.package.as_deref(), Some("COMMON-LISP-USER"));
}

#[test]
fn unclosed_test_fence_is_reported_even_with_indented_markers_in_its_body() {
    let source = "## Usage\n\n```lisp test\n(progn)\n    ```";
    let tests = DocumentationTests::markdown(source, SourceReadtable::Standard);
    assert!(tests.examples.is_empty());
    let [error] = tests.diagnostics.as_slice() else {
        panic!("{tests:?}")
    };
    assert_eq!(error.message, "unclosed lisp test fence");
    let opening = usize::from(error.range.start())..usize::from(error.range.end());
    assert_eq!(&source[opening], "```lisp test");
}

#[test]
fn documentation_examples_use_the_readtable_and_package_at_each_definition() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "library",
        r#"
(zk:in-readtable :raw-strings)
(in-package :first)
;;! ```lisp test
;;! (list
;;!   #>raw ( " \ #| ;
;;!   #>more
;;! )
;;! ```
(defun first () nil)
(in-package :second)
(zk:in-readtable :standard)
;;! ```lisp test
;;! (list :standard)
;;! ```
(defun second () nil)
"#,
    );
    let source = workspace.snapshot("library").unwrap();
    let tests = source.documentation_tests();
    let [raw, standard] = tests.examples.as_slice() else {
        panic!("{tests:?}")
    };
    assert!(tests.diagnostics.is_empty());
    assert_eq!(raw.package.as_deref(), Some("FIRST"));
    assert_eq!(raw.readtable, SourceReadtable::RawStrings);
    assert!(raw.source().contains("#>raw ( \" \\ #| ;\n"));
    assert_eq!(standard.package.as_deref(), Some("SECOND"));
    assert_eq!(standard.readtable, SourceReadtable::Standard);
}

#[test]
fn example_lint_uses_the_definition_context_and_captured_workspace() {
    let library = "(defpackage :greeter (:use :cl) (:export :greet))
                   (in-package :greeter) (defun greet (name) name)";
    let source = r#"
(defpackage :demo (:use :cl :greeter))
(in-package :demo)
(zk:in-readtable :raw-strings)
;;! ```lisp test
;;! (list
;;!   #>ordinary text ( "
;;!   (greet)
;;! )
;;! ```
;;! ```lisp
;;! (greet)
;;! ```
(defun example () nil)
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("library", library);
    workspace.set_document("source", source);

    let config = LintConfig::default();
    let captured = workspace.diagnostic_snapshot("source").unwrap();
    assert!(
        captured
            .source()
            .documentation_tests()
            .diagnostics
            .is_empty()
    );
    let warnings = captured.lint(&config, || false).unwrap();
    let [warning] = warnings.as_slice() else {
        panic!("{warnings:?}");
    };
    assert_eq!(warning.rule_id, "correctness/arity-mismatch");
    assert_eq!(
        warning.message,
        "greet requires at least 1 argument, found 0"
    );
    let call = source.find("(greet)").unwrap();
    assert_eq!(
        warning.range,
        TextRange::at(TextSize::of(&source[..call]), 7.into())
    );
    assert_eq!(warning.related[0].document.as_deref(), Some("library"));
    assert_eq!(workspace.lint_document("source", &config), warnings);

    workspace.set_document("library", library.replace("(name)", "(&optional name)"));
    assert!(workspace.lint_document("source", &config).is_empty());
    assert_eq!(captured.lint(&config, || false).unwrap(), warnings);
}

#[test]
fn declarations_and_reader_conditionals_are_checked_within_each_example() {
    let source = r#"
(in-package :cl-user)
;;! ```lisp test
;;! (defun example-only (arg) arg)
;;! #+enabled (example-only)
;;! #-enabled (cons 1 2 3)
;;! ```
;;! ```lisp test
;;! (example-only)
;;! ```
(defun documented () nil)
(example-only)
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(vec!["enabled".into()]));
    workspace.set_document("source", source);
    let warnings = workspace.lint_document("source", &LintConfig::default());
    let [warning] = warnings.as_slice() else {
        panic!("{warnings:?}");
    };
    assert_eq!(warning.rule_id, "correctness/arity-mismatch");
    assert_eq!(
        usize::from(warning.range.start()),
        source.find("(example-only)").unwrap()
    );
    assert!(
        warning
            .message
            .starts_with("example-only requires at least 1 argument")
    );
    let [definition] = warning.related.as_slice() else {
        panic!("{warning:?}");
    };
    assert_eq!(definition.document, None);
    assert_eq!(
        usize::from(definition.range.start()),
        source.find("example-only (arg)").unwrap()
    );
}

#[test]
fn example_lint_maps_a_multiline_warning_without_a_source_rewrite() {
    let source = r#"
(in-package :cl-user)
;;! ```lisp test
;;! (if (probe-file "example.txt")
;;!     (print :present))
;;! ```
(defun documented () nil)
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("source", source);
    let warnings = workspace.lint_document("source", &LintConfig::default());
    let warning = warnings
        .iter()
        .find(|diagnostic| diagnostic.rule_id == "style/if-to-when")
        .unwrap_or_else(|| panic!("{warnings:?}"));
    assert_eq!(
        &source[std::ops::Range::<usize>::from(warning.range)],
        "(if (probe-file \"example.txt\")\n;;!     (print :present))"
    );
    assert!(warning.fix.is_none());
}
