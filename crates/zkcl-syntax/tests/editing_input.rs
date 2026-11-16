use std::sync::Arc;

use zkcl_syntax::SourceSnapshot;
use zkcl_syntax::editing::{
    Edit, EditError, Motion, Selection, StructuralOperation, edit, insert_delimiter, move_cursor,
};
use zkcl_syntax::syntax::SourceReadtable;

fn snapshot(text: &str) -> Arc<SourceSnapshot> {
    SourceSnapshot::from_text(text, SourceReadtable::Standard)
}

fn marked(text: &str) -> (String, u32) {
    let (before, after) = text.split_once('¦').unwrap();
    (format!("{before}{after}"), before.len() as u32)
}

fn cursor(at: u32) -> Selection {
    Selection::new(at.into(), at.into())
}

fn assert_edit(change: Edit, after: &str) {
    let (text, at) = marked(after);
    assert_eq!(change.apply(), text);
    assert_eq!(change.selection(), cursor(at));
}

fn at_caret(before: &str, operation: StructuralOperation) -> Result<Edit, EditError> {
    let (text, at) = marked(before);
    edit(snapshot(&text), cursor(at), operation)
}

#[test]
fn delete_enters_pairs_and_removes_only_empty_pairs_from_inside() {
    use StructuralOperation::{DeleteBackward, DeleteForward};
    let movement = at_caret("¦(a)", DeleteForward).unwrap();
    assert!(movement.changes().is_empty());
    assert_edit(movement, "(¦a)");
    assert_edit(at_caret("(a)¦", DeleteBackward).unwrap(), "(a¦)");
    assert_edit(at_caret("#(¦)", DeleteBackward).unwrap(), "¦");
    assert_edit(at_caret("(a(¦)b)", DeleteForward).unwrap(), "(a¦ b)");
    assert_edit(at_caret("\"¦\"", DeleteForward).unwrap(), "¦");

    for input in ["(a¦)", "( ¦)", "(#|keep|#¦)", "'(#(x)¦)"] {
        assert_eq!(
            at_caret(input, DeleteForward).unwrap_err(),
            EditError::UnsafeTokenBoundary,
            "{input}"
        );
    }
}

#[test]
fn deletion_respects_reader_units_and_comment_boundaries() {
    use StructuralOperation::{DeleteBackward, DeleteForward};
    assert_edit(at_caret("f¦雪g", DeleteForward).unwrap(), "f¦g");
    assert_edit(at_caret(r#""a\¦雪b""#, DeleteBackward).unwrap(), "\"a¦b\"");
    assert_edit(at_caret(r"|a\¦雪b|", DeleteForward).unwrap(), "|a¦b|");
    assert_edit(at_caret(r"(¦#\) a)", DeleteForward).unwrap(), "(¦ a)");
    assert_edit(
        at_caret("(#|k¦eep|# z)", DeleteForward).unwrap(),
        "(#|k¦ep|# z)",
    );
    assert_edit(at_caret("a  ¦ b", DeleteBackward).unwrap(), "a ¦ b");
    assert_edit(at_caret("(a\r\n¦ b)", DeleteBackward).unwrap(), "(a¦ b)");

    for input in ["'¦a b", "a¦ b", "(#|hi #¦|nested|#|#)", "(; keep¦\r\n(a))"] {
        assert_eq!(
            at_caret(input, DeleteForward).unwrap_err(),
            EditError::UnsafeTokenBoundary,
            "{input}"
        );
    }
    assert_eq!(
        at_caret("(a . ¦b)", DeleteForward).unwrap_err(),
        EditError::DottedBoundary
    );
    assert_eq!(
        at_caret("#2(a¦ b)", DeleteBackward).unwrap_err(),
        EditError::UnsupportedSelection
    );
}

#[test]
fn deleting_a_selection_allows_whole_forms_and_rejects_partial_structure() {
    let source = snapshot("before '(a b) after");
    let change = edit(
        Arc::clone(&source),
        Selection::new(14.into(), 7.into()),
        StructuralOperation::DeleteBackward,
    )
    .unwrap();
    assert_edit(change, "before ¦after");
    for (start, end) in [(8, 13), (8, 9), (9, 17)] {
        assert_eq!(
            edit(
                Arc::clone(&source),
                Selection::new(start.into(), end.into()),
                StructuralOperation::DeleteForward
            )
            .unwrap_err(),
            EditError::UnsafeTokenBoundary
        );
    }
}

#[test]
fn delimiter_input_uses_the_source_context() {
    for (before, ch, after) in [
        ("(print ¦)", '(', "(print (¦))"),
        ("(print ¦)", '"', "(print \"¦\")"),
        ("(print ¦foo)", ')', "(print foo)¦"),
        ("(print foo¦", ')', "(print foo)¦"),
        ("\"foo¦\"", '"', "\"foo\"¦"),
        ("\"f¦oo\"", '"', r#""f\"¦oo""#),
        ("; literal ¦", ')', "; literal )¦"),
        (r"#\¦", '(', r"#\(¦"),
        ("|a¦b|", ')', "|a)¦b|"),
    ] {
        let (text, at) = marked(before);
        assert_edit(
            insert_delimiter(snapshot(&text), cursor(at), ch).unwrap(),
            after,
        );
    }
    let (text, at) = marked("(set-macro-character #\\[ #'reader) ¦");
    assert_eq!(
        insert_delimiter(snapshot(&text), cursor(at), '(').unwrap_err(),
        EditError::UnknownReaderBoundary
    );
}

#[test]
fn motion_distinguishes_form_boundaries_from_group_exits() {
    for (before, operation, after) in [
        (
            "¦#'(f 雪) ; comment\n b",
            Motion::ForwardForm,
            "#'(f 雪)¦ ; comment\n b",
        ),
        (
            "(a ¦ ;comment\n b)",
            Motion::ForwardForm,
            "(a  ;comment\n b¦)",
        ),
        ("#'(f 雪)¦", Motion::BackwardForm, "¦#'(f 雪)"),
        ("(a ¦(b c))", Motion::UpList, "(a (b c))¦"),
        ("(a (b¦ c))", Motion::BackwardUpList, "(a ¦(b c))"),
        ("(a ¦#(b c))", Motion::DownList, "(a #(¦b c))"),
        ("¦#'#(b c)", Motion::DownList, "#'#(¦b c)"),
    ] {
        let (text, at) = marked(before);
        let (_, target) = marked(after);
        assert_eq!(
            move_cursor(&snapshot(&text), at.into(), operation).unwrap(),
            Some(target.into()),
            "{before}"
        );
    }
    for (text, op) in [
        ("(a¦)", Motion::ForwardForm),
        ("(¦a)", Motion::BackwardForm),
        ("(a¦", Motion::UpList),
        ("(a ¦b) (c)", Motion::DownList),
    ] {
        let (text, at) = marked(text);
        assert_eq!(
            move_cursor(&snapshot(&text), at.into(), op).unwrap(),
            None,
            "{text}"
        );
    }
}

#[test]
fn indent_changes_only_line_prefixes_and_keeps_selection_direction() {
    let text = "(defun f (x)\r\n\t(+  x  1)\r\n    )\r\n";
    let expected = "(defun f (x)\r\n  (+  x  1)\r\n)\r\n";
    let change = edit(
        snapshot(text),
        Selection::new((text.len() as u32).into(), 0.into()),
        StructuralOperation::Indent,
    )
    .unwrap();
    assert_eq!(change.apply(), expected);
    assert_eq!(
        change.selection(),
        Selection::new((expected.len() as u32).into(), 0.into())
    );
    assert_eq!(change.changes().len(), 2);
    assert_edit(
        at_caret("(defun f ()\n¦", StructuralOperation::Indent).unwrap(),
        "(defun f ()\n  ¦",
    );
    assert_edit(
        at_caret(
            "(let ((a 1)\n ¦(b 2))\n    (list a b))",
            StructuralOperation::Indent,
        )
        .unwrap(),
        "(let ((a 1)\n      ¦(b 2))\n    (list a b))",
    );
}

#[test]
fn indent_does_not_edit_multiline_payloads_or_guess_unknown_readtables() {
    let text = "(progn\n\"a\n   b\"\n#| x\n  y |#\n  (foo))\n(set-macro-character #\\[ #'reader)\n  (leave)";
    let expected = "(progn\n  \"a\n   b\"\n  #| x\n  y |#\n  (foo))\n(set-macro-character #\\[ #'reader)\n  (leave)";
    let selection = Selection::new(0.into(), (text.len() as u32).into());
    let change = edit(snapshot(text), selection, StructuralOperation::Indent).unwrap();
    assert_eq!(change.apply(), expected);
}

#[test]
fn indentation_can_move_line_ending_closers_without_moving_code() {
    assert_edit(
        at_caret(
            "(defun f (x)\r\n  (first x)) ; keep )\r\n  (second x)¦",
            StructuralOperation::InferParens,
        )
        .unwrap(),
        "(defun f (x)\r\n  (first x) ; keep )\r\n  (second x)¦)",
    );
    assert_edit(
        at_caret(
            "(progn\n  (print #\\))\n¦(next)",
            StructuralOperation::InferParens,
        )
        .unwrap(),
        "(progn\n  (print #\\)))\n¦(next)",
    );
    let explicit = at_caret("(f (g)\n  a\n)¦", StructuralOperation::InferParens).unwrap();
    assert!(explicit.changes().is_empty());
}

#[test]
fn indentation_inference_refuses_fixed_delimiter_conflicts_and_opaque_source() {
    for input in ["(f\n  (g x\n  )\n(next)¦", "(a \"unfinished¦"] {
        assert_eq!(
            at_caret(input, StructuralOperation::InferParens).unwrap_err(),
            EditError::CannotInferParens
        );
    }
    assert_eq!(
        at_caret(
            "(set-macro-character #\\[ #'reader)\n(f¦",
            StructuralOperation::InferParens
        )
        .unwrap_err(),
        EditError::UnknownReaderBoundary,
    );
}
