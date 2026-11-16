#![cfg(feature = "analysis")]

use zkcl_syntax::{OutlineKind, SourceWorkspace, syntax::TextRange};

fn slice(text: &str, range: TextRange) -> &str {
    &text[usize::from(range.start())..usize::from(range.end())]
}

#[test]
fn outline_reports_form_and_name_ranges_in_source_order() {
    let source = r#"
; Prologue
(cl:defun pkg::first () 1)
(other:defun ignored () 2)
(progn
  "(defun inside-a-string () 3)"
  '(defun quoted () 4)
  #+unknown (defun conditional () 5)
  (COMMON-LISP::defmacro |Read me😀| () 6)
  (defparameter *ready* t)
  (defconstant +limit+ 7)
  (defstruct (sample (:constructor make-sample)) field)
  (cl::defun (setf pkg::field) (value object) value)
  (defpackage "example" (:use "COMMON-LISP"))
  (asdf:defsystem "example" :components ((:file "main"))))
(defun unfinished ()
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("test", source);
    let snapshot = workspace.snapshot("test").unwrap();
    let entries = snapshot.outline();
    assert_eq!(
        entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind))
            .collect::<Vec<_>>(),
        [
            ("pkg::first", OutlineKind::Function),
            ("|Read me😀|", OutlineKind::Macro),
            ("*ready*", OutlineKind::Variable),
            ("+limit+", OutlineKind::Constant),
            ("sample", OutlineKind::Type),
            ("(setf pkg::field)", OutlineKind::Function),
            (r#""example""#, OutlineKind::Package),
            (r#""example""#, OutlineKind::System),
        ]
    );
    for entry in entries {
        assert_eq!(slice(source, entry.selection_range), entry.name);
        assert!(entry.range.contains_range(entry.selection_range));
        assert!(slice(source, entry.range).starts_with('('));
        assert!(slice(source, entry.range).ends_with(')'));
    }
    assert_eq!(
        slice(source, entries[0].range),
        "(cl:defun pkg::first () 1)"
    );

    workspace.set_document("test", "(defvar updated 8)");
    let updated = workspace.snapshot("test").unwrap();
    assert_eq!(updated.outline()[0].name, "updated");
    assert_eq!(snapshot.outline()[0].name, "pkg::first");
}

#[test]
fn retains_complete_definitions_before_ambiguous_or_incomplete_input() {
    let mut workspace = SourceWorkspace::new();
    for source in [
        "(progn (defvar known 1) #? (defvar unknown 2)) (defvar after 3)",
        "(defvar known 1) '#+feature (list 2) (defvar possibly_quoted 3)",
        "(progn (defvar known 1) (defvar unfinished",
    ] {
        workspace.set_document("test", source);
        let snapshot = workspace.snapshot("test").unwrap();
        let entries = snapshot.outline();
        assert_eq!(entries.len(), 1, "{source}");
        assert_eq!(entries[0].name, "known");
    }
}
