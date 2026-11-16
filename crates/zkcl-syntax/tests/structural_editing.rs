#![cfg(feature = "analysis")]

use std::sync::Arc;

use zkcl_syntax::{
    SourceWorkspace,
    editing::{Edit, EditError, Selection, StructuralOperation, edit},
    format::TextEdit,
    syntax::{TextRange, TextSize},
};

fn edit_range(
    source: Arc<zkcl_syntax::SourceSnapshot>,
    range: TextRange,
    operation: StructuralOperation,
) -> Result<Edit, EditError> {
    edit(
        source,
        Selection::new(range.start(), range.end()),
        operation,
    )
}

fn snapshot(text: &str) -> Arc<zkcl_syntax::SourceSnapshot> {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("input", text);
    workspace.snapshot("input").unwrap()
}

fn range(_text: &str, start: usize, end: usize) -> TextRange {
    TextRange::new(
        TextSize::try_from(start).unwrap(),
        TextSize::try_from(end).unwrap(),
    )
}

fn cursor(text: &str, byte: usize) -> TextRange {
    range(text, byte, byte)
}

fn selected_text(text: &str, selection: TextRange) -> &str {
    &text[usize::from(selection.start())..usize::from(selection.end())]
}

#[test]
fn wrap_and_splice_keep_reader_forms_and_comments_verbatim() {
    let source = "'alpha ; keep\n #+snow (snow-form)\n";
    let original_snapshot = snapshot(source);
    let selection = range(
        source,
        source.find("'alpha").unwrap(),
        source.find("(snow-form)").unwrap() + "(snow-form)".len(),
    );
    let wrapped = edit_range(original_snapshot, selection, StructuralOperation::Wrap).unwrap();
    assert!(
        wrapped
            .changes()
            .windows(2)
            .all(|pair| { pair[0].range.end() <= pair[1].range.start() })
    );
    let wrapped_text = wrapped.apply();
    assert_eq!(wrapped_text, "('alpha ; keep\n #+snow (snow-form))\n");
    assert_eq!(
        selected_text(&wrapped_text, wrapped.selection().range()),
        "'alpha ; keep\n #+snow (snow-form)"
    );

    let inner = wrapped.selection().range();
    let spliced = edit_range(snapshot(&wrapped_text), inner, StructuralOperation::Splice).unwrap();
    let spliced_text = spliced.apply();
    assert_eq!(spliced_text, source);
    assert_eq!(
        selected_text(&spliced_text, spliced.selection().range()),
        "'alpha ; keep\n #+snow (snow-form)"
    );
}

#[test]
fn splice_rejects_unwrapping_a_reader_operand() {
    for source in ["'(a b)", "#+snow (a b)"] {
        assert_eq!(
            edit_range(
                snapshot(source),
                cursor(source, source.find('a').unwrap()),
                StructuralOperation::Splice,
            )
            .unwrap_err(),
            EditError::UnsupportedSelection
        );
    }
}

#[test]
fn forward_and_backward_slurp_move_the_expected_delimiter() {
    let source = "(a b) c d";
    let forward = edit_range(
        snapshot(source),
        cursor(source, source.find('b').unwrap()),
        StructuralOperation::SlurpForward,
    )
    .unwrap();
    assert_eq!(forward.apply(), "(a b c) d");

    let source = "a (b c) d";
    let backward = edit_range(
        snapshot(source),
        cursor(source, source.find('b').unwrap()),
        StructuralOperation::SlurpBackward,
    )
    .unwrap();
    assert_eq!(backward.apply(), "(a b c) d");
}

#[test]
fn forward_and_backward_barf_move_the_expected_delimiter() {
    let source = "(a b c)";
    let forward = edit_range(
        snapshot(source),
        cursor(source, source.find('b').unwrap()),
        StructuralOperation::BarfForward,
    )
    .unwrap();
    assert_eq!(forward.apply(), "(a b) c");

    let backward = edit_range(
        snapshot(source),
        cursor(source, source.find('b').unwrap()),
        StructuralOperation::BarfBackward,
    )
    .unwrap();
    assert_eq!(backward.apply(), "a (b c)");
}

#[test]
fn raise_replaces_the_enclosing_form_with_the_selected_whole_form() {
    let source = "(outer (inner 1) tail)";
    let inner_start = source.find("(inner").unwrap();
    let inner_end = inner_start + "(inner 1)".len();
    let raise = edit_range(
        snapshot(source),
        range(source, inner_start, inner_end),
        StructuralOperation::Raise,
    )
    .unwrap();
    let raised = raise.apply();
    assert_eq!(raised, "(inner 1)");
    assert_eq!(
        selected_text(&raised, raise.selection().range()),
        "(inner 1)"
    );
}

#[test]
fn unicode_byte_ranges_and_comments_remain_lossless() {
    let source = "(雪 ;保留\n 犬)";
    let dog = source.find('犬').unwrap();
    let barf = edit_range(
        snapshot(source),
        cursor(source, dog),
        StructuralOperation::BarfBackward,
    )
    .unwrap();
    let result = barf.apply();
    assert_eq!(result, "雪 ;保留\n (犬)");
    let selection = barf.selection().range();
    assert_eq!(
        result.get(usize::from(selection.start())..usize::from(selection.end())),
        Some("")
    );
    assert!(result[usize::from(selection.start())..].starts_with('犬'));

    let invalid = edit_range(
        snapshot(source),
        range(source, 1, 2),
        StructuralOperation::Wrap,
    );
    assert_eq!(invalid.unwrap_err(), EditError::InvalidRange);
}

#[test]
fn partial_lists_support_operations_without_a_closing_delimiter() {
    let source = "x (a b";
    let slurp = edit_range(
        snapshot(source),
        cursor(source, source.find('a').unwrap()),
        StructuralOperation::SlurpBackward,
    )
    .unwrap();
    assert_eq!(slurp.apply(), "(x a b");

    let source = "(a b";
    let splice = edit_range(
        snapshot(source),
        cursor(source, source.find('a').unwrap()),
        StructuralOperation::Splice,
    )
    .unwrap();
    assert_eq!(splice.apply(), "a b");
}

#[test]
fn dotted_and_unknown_reader_boundaries_are_rejected() {
    let dotted = "(a . b)";
    assert_eq!(
        edit_range(
            snapshot(dotted),
            cursor(dotted, dotted.find('a').unwrap()),
            StructuralOperation::BarfForward,
        )
        .unwrap_err(),
        EditError::DottedBoundary
    );

    let unknown = "#?opaque (next)";
    assert_eq!(
        edit_range(
            snapshot(unknown),
            cursor(unknown, unknown.find("opaque").unwrap()),
            StructuralOperation::Wrap,
        )
        .unwrap_err(),
        EditError::UnknownReaderBoundary
    );
}

#[test]
fn a_plan_cannot_apply_to_a_newer_snapshot() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("input", "value");
    let old = workspace.snapshot("input").unwrap();
    let plan = edit_range(
        Arc::clone(&old),
        range("value", 0, "value".len()),
        StructuralOperation::Wrap,
    )
    .unwrap();

    workspace.set_document("input", "value and more");
    let newer = workspace.snapshot("input").unwrap();
    assert_eq!(plan.apply_to(&newer).unwrap_err(), EditError::StaleSnapshot);
    assert_eq!(plan.apply_to(&old).unwrap(), "(value)");
}

fn edit_at(source: &str, byte: usize, operation: StructuralOperation) -> String {
    let plan = edit_range(snapshot(source), cursor(source, byte), operation).unwrap();
    plan.apply()
}

#[test]
fn delimiter_removal_never_fuses_atoms_or_escaped_tokens() {
    for (source, at, operation, expected) in [
        ("(foo)bar", 1, StructuralOperation::Splice, "foo bar"),
        ("foo(bar)", 4, StructuralOperation::Splice, "foo bar"),
        (
            "(foo)bar",
            1,
            StructuralOperation::SlurpForward,
            "(foo bar)",
        ),
        (
            "foo(bar)",
            4,
            StructuralOperation::SlurpBackward,
            "(foo bar)",
        ),
        ("(#\\a)b", 1, StructuralOperation::Splice, "#\\a b"),
        ("(|a b|)c", 1, StructuralOperation::Splice, "|a b| c"),
        (
            "(a\\ b)c",
            1,
            StructuralOperation::SlurpForward,
            "(a\\ b c)",
        ),
        ("(a)(b)", 1, StructuralOperation::JoinForward, "(a b)"),
    ] {
        assert_eq!(
            edit_at(source, at, operation),
            expected,
            "{source} {operation:?}"
        );
    }
}

#[test]
fn local_targeting_does_not_promote_across_a_quoted_group() {
    assert_eq!(edit_at("'(a b)", 2, StructuralOperation::Wrap), "'((a) b)");
    assert_eq!(
        edit_at("x '(a b)", 4, StructuralOperation::SlurpBackward),
        "'(x a b)"
    );
    assert_eq!(
        edit_at("'(a b) c", 2, StructuralOperation::SlurpForward),
        "'(a b c)"
    );
    assert_eq!(
        edit_at("x #'(a b)", 5, StructuralOperation::SlurpBackward),
        "#'(x a b)"
    );
    assert_eq!(
        edit_at("#+snow (a) b", 8, StructuralOperation::SlurpForward),
        "#+snow (a b)"
    );
    assert_eq!(
        edit_at("x #-snow (a)", 10, StructuralOperation::SlurpBackward),
        "#-snow (x a)"
    );
}

#[test]
fn singleton_and_empty_groups_have_explicit_boundaries() {
    assert_eq!(edit_at("(a)", 1, StructuralOperation::BarfForward), "() a");
    assert_eq!(edit_at("(a)", 1, StructuralOperation::BarfBackward), "a ()");
    assert_eq!(
        edit_at("#(a)", 2, StructuralOperation::BarfBackward),
        "a #()"
    );
    for operation in [
        StructuralOperation::BarfForward,
        StructuralOperation::BarfBackward,
    ] {
        assert_eq!(
            edit_range(snapshot("()"), cursor("()", 1), operation).unwrap_err(),
            EditError::NoSibling
        );
    }
    assert_eq!(edit_at("()", 1, StructuralOperation::Splice), "");
}

#[test]
fn split_and_join_preserve_boundary_comments_and_crlf() {
    let source = "(hello ; stay\r\n world)";
    let split = edit_at(source, 6, StructuralOperation::Split);
    assert_eq!(split, "(hello) ; stay\r\n (world)");
    assert_eq!(edit_at(&split, 1, StructuralOperation::JoinForward), source);
    assert_eq!(
        edit_at("#(a b)", 3, StructuralOperation::Split),
        "#(a) #(b)"
    );
    assert_eq!(
        edit_at("#(a) #(b)", 8, StructuralOperation::JoinBackward),
        "#(a b)"
    );
    assert_eq!(edit_at("()", 1, StructuralOperation::Split), "() ()");
    for source in ["(hello)", "\"hello world\""] {
        assert!(
            edit_range(
                snapshot(source),
                cursor(source, 3),
                StructuralOperation::Split
            )
            .is_err()
        );
    }
    assert!(
        edit_range(
            snapshot("(a) #(b)"),
            cursor("(a) #(b)", 1),
            StructuralOperation::JoinForward
        )
        .is_err()
    );
    assert!(
        edit_range(
            snapshot("'(a) (b)"),
            cursor("'(a) (b)", 2),
            StructuralOperation::JoinForward
        )
        .is_err()
    );
}

#[test]
fn transpose_moves_complete_surface_forms_and_tracks_direction() {
    let source = "'雪 ; stay\r\n #+snow (dog) tail";
    let selected = Selection::new(4.into(), 0.into());
    let plan = edit(
        snapshot(source),
        selected,
        StructuralOperation::TransposeForward,
    )
    .unwrap();
    let result = plan.apply();
    assert_eq!(result, "#+snow (dog) ; stay\r\n '雪 tail");
    assert_eq!(selected_text(&result, plan.selection().range()), "'雪");
    assert!(plan.selection().anchor > plan.selection().head);
    let back = edit(
        snapshot(&result),
        plan.selection(),
        StructuralOperation::TransposeBackward,
    )
    .unwrap();
    assert_eq!(back.apply(), source);
    assert_eq!(
        edit_at("a'b", 0, StructuralOperation::TransposeForward),
        "'b a"
    );
}

#[test]
fn custom_transactions_validate_original_and_result_coordinates() {
    let source = snapshot("a 雪 b");
    let edit = TextEdit {
        range: range(source.text(), 2, 5),
        replacement: "犬猫".into(),
    };
    let selection = Selection::new(8.into(), 2.into());
    let plan = Edit::from_changes(Arc::clone(&source), vec![edit.clone()], selection).unwrap();
    assert_eq!(plan.apply_to(&source).unwrap(), "a 犬猫 b");
    assert_eq!(plan.selection(), selection);
    assert_eq!(
        Edit::from_changes(
            Arc::clone(&source),
            vec![TextEdit {
                range: range(source.text(), 3, 5),
                replacement: "".into()
            }],
            Selection::new(0.into(), 0.into())
        )
        .unwrap_err(),
        EditError::InvalidRange
    );
    assert_eq!(
        Edit::from_changes(
            Arc::clone(&source),
            vec![edit.clone(), edit],
            Selection::new(0.into(), 0.into())
        )
        .unwrap_err(),
        EditError::OverlappingEdits
    );
    assert_eq!(
        Edit::from_changes(
            Arc::clone(&source),
            vec![],
            Selection::new(3.into(), 3.into())
        )
        .unwrap_err(),
        EditError::InvalidRange
    );
    assert_eq!(
        Edit::from_changes(
            Arc::clone(&source),
            vec![],
            Selection::new(99.into(), 99.into())
        )
        .unwrap_err(),
        EditError::InvalidRange
    );
    let same_text = snapshot(source.text());
    assert_eq!(
        plan.apply_to(&same_text).unwrap_err(),
        EditError::StaleSnapshot
    );
}

#[test]
fn partial_input_is_local_and_comment_cursors_do_not_select_distant_forms() {
    assert_eq!(
        edit_at("(a) (unfinished", 1, StructuralOperation::Wrap),
        "((a)) (unfinished"
    );
    assert_eq!(
        edit_at("(a b", 3, StructuralOperation::BarfBackward),
        "a (b"
    );
    assert!(
        edit_range(
            snapshot("a ; comment\n b"),
            cursor("a ; comment\n b", 6),
            StructuralOperation::Wrap
        )
        .is_err()
    );
    assert_eq!(
        edit_at("(a)   b", 4, StructuralOperation::Wrap),
        "(a)   (b)"
    );
    assert!(edit_range(snapshot(""), cursor("", 0), StructuralOperation::Wrap).is_err());
    assert_eq!(
        edit_range(
            snapshot("a"),
            cursor("a", 0),
            StructuralOperation::TransposeBackward
        )
        .unwrap_err(),
        EditError::NoSibling
    );
}

#[test]
fn dotted_tails_and_reader_context_changes_remain_opaque() {
    for (source, at, operation) in [
        ("(a . (b c))", 6, StructuralOperation::Splice),
        ("(a . (b c))", 6, StructuralOperation::BarfForward),
        ("(a . (b c))", 6, StructuralOperation::Split),
    ] {
        assert_eq!(
            edit_range(snapshot(source), cursor(source, at), operation).unwrap_err(),
            EditError::DottedBoundary
        );
    }
    let source = "(set-macro-character #\\[ #'reader) (a b)";
    let at = source.find("a b").unwrap();
    assert_eq!(
        edit_range(
            snapshot(source),
            cursor(source, at),
            StructuralOperation::Splice
        )
        .unwrap_err(),
        EditError::UnknownReaderBoundary
    );
    let source = "(zk:in-readtable :raw-strings) #>hello";
    assert_eq!(
        edit_range(
            snapshot(source),
            range(source, 0, source.find(')').unwrap() + 1),
            StructuralOperation::Wrap
        )
        .unwrap_err(),
        EditError::UnknownReaderBoundary
    );
}

#[test]
fn raise_preserves_a_cursor_in_the_promoted_form() {
    let source = "(other alpha)tail";
    let plan = edit_range(
        snapshot(source),
        cursor(source, 9),
        StructuralOperation::Raise,
    )
    .unwrap();
    assert_eq!(plan.apply(), "alpha tail");
    assert_eq!(plan.selection().range(), cursor("alpha tail", 2));
}

#[test]
fn custom_transactions_reject_unsorted_and_duplicate_insertions() {
    let source = snapshot("abc");
    let selected = Selection::new(0.into(), 0.into());
    let insertion = TextEdit {
        range: cursor("abc", 1),
        replacement: "x".into(),
    };
    assert_eq!(
        Edit::from_changes(
            Arc::clone(&source),
            vec![insertion.clone(), insertion],
            selected
        )
        .unwrap_err(),
        EditError::OverlappingEdits
    );
    assert_eq!(
        Edit::from_changes(
            source,
            vec![
                TextEdit {
                    range: cursor("abc", 2),
                    replacement: "x".into()
                },
                TextEdit {
                    range: cursor("abc", 1),
                    replacement: "y".into()
                }
            ],
            selected
        )
        .unwrap_err(),
        EditError::OverlappingEdits
    );
}

#[test]
fn reader_conditional_feature_expression_cannot_absorb_its_operand() {
    let source = "#+(and snow rain) (guarded) next";
    for operation in [
        StructuralOperation::SlurpForward,
        StructuralOperation::SlurpBackward,
        StructuralOperation::BarfForward,
        StructuralOperation::BarfBackward,
        StructuralOperation::Split,
    ] {
        assert_eq!(
            edit_range(snapshot(source), cursor(source, 7), operation).unwrap_err(),
            EditError::UnsupportedSelection
        );
    }
    assert_eq!(
        edit_at(
            source,
            source.find("guarded").unwrap(),
            StructuralOperation::SlurpForward
        ),
        "#+(and snow rain) (guarded next)"
    );
}

#[test]
fn every_cursor_in_partial_reader_syntax_returns_an_edit_or_an_error() {
    let operations = [
        StructuralOperation::Wrap,
        StructuralOperation::Splice,
        StructuralOperation::Raise,
        StructuralOperation::SlurpForward,
        StructuralOperation::SlurpBackward,
        StructuralOperation::BarfForward,
        StructuralOperation::BarfBackward,
        StructuralOperation::Split,
        StructuralOperation::JoinForward,
        StructuralOperation::JoinBackward,
        StructuralOperation::TransposeForward,
        StructuralOperation::TransposeBackward,
    ];
    for text in [
        "",
        "()()",
        "a'b",
        "'(a (b c))d",
        "#+(or a b) (x y) z",
        "#-(and) ()",
        "(a . (b))",
        "(a ;x\r\n b",
        "#| ( #| nested |# ) |# (雪 犬)",
        "(a \"unfinished\\",
        "(a |unclosed",
        "x #?opaque (y)",
        "#(a #\\) b)",
        "#'",
        "(a .",
        "a)",
    ] {
        let source = snapshot(text);
        for byte in (0..=text.len()).filter(|&byte| text.is_char_boundary(byte)) {
            for operation in operations {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    edit(
                        Arc::clone(&source),
                        Selection::new(
                            TextSize::try_from(byte).unwrap(),
                            TextSize::try_from(byte).unwrap(),
                        ),
                        operation,
                    )
                }));
                assert!(
                    result.is_ok(),
                    "panic for {text:?} at {byte}, {operation:?}"
                );
                if let Ok(change) = result.unwrap() {
                    let output = change.apply();
                    let selected = change.selection().range();
                    assert!(
                        output
                            .get(usize::from(selected.start())..usize::from(selected.end()))
                            .is_some(),
                        "invalid result selection for {text:?} at {byte}, {operation:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn join_at_a_gap_uses_the_two_neighboring_groups() {
    for (source, at, expected) in [
        ("(a) (b)", 3, "(a b)"),
        ("(a)  (b)", 4, "(a  b)"),
        ("((a) (b))", 4, "((a b))"),
        ("(a)(b)", 3, "(a b)"),
    ] {
        for operation in [
            StructuralOperation::JoinForward,
            StructuralOperation::JoinBackward,
        ] {
            assert_eq!(
                edit_at(source, at, operation),
                expected,
                "{source:?}, {at}, {operation:?}"
            );
        }
    }
}

#[test]
fn nested_comments_are_never_promoted_to_an_enclosing_form() {
    for (text, byte) in [
        ("(a ; comment\n b)", 6),
        ("(a #| comment |# b)", 8),
        ("'(a ;comment\n b)", 8),
    ] {
        assert_eq!(
            edit_range(
                snapshot(text),
                cursor(text, byte),
                StructuralOperation::Wrap
            )
            .unwrap_err(),
            EditError::NoForm
        );
    }
}

#[test]
fn all_group_operations_reject_partial_nonempty_forms() {
    let text = "(abc def) tail";
    for operation in [
        StructuralOperation::Splice,
        StructuralOperation::SlurpForward,
        StructuralOperation::SlurpBackward,
        StructuralOperation::BarfForward,
        StructuralOperation::BarfBackward,
        StructuralOperation::JoinForward,
        StructuralOperation::JoinBackward,
    ] {
        assert_eq!(
            edit_range(snapshot(text), range(text, 2, 3), operation).unwrap_err(),
            EditError::UnsupportedSelection,
            "{operation:?}"
        );
    }
}

#[test]
fn selections_cannot_include_unselected_parent_delimiters() {
    for (text, start, end) in [
        ("(a b) tail", 3, 5),
        ("(a b) tail", 0, 2),
        ("'(a b) tail", 4, 6),
    ] {
        assert_eq!(
            edit_range(
                snapshot(text),
                range(text, start, end),
                StructuralOperation::Wrap
            )
            .unwrap_err(),
            EditError::UnsupportedSelection
        );
    }
    assert_eq!(
        edit_range(
            snapshot("(a  b  )"),
            range("(a  b  )", 3, 7),
            StructuralOperation::Wrap
        )
        .unwrap()
        .apply(),
        "(a  (b)  )"
    );
}

#[test]
fn line_comment_end_is_still_inside_the_comment() {
    for text in ["(a) ; tail", "(a) ; tail\n"] {
        assert_eq!(
            edit_range(snapshot(text), cursor(text, 10), StructuralOperation::Wrap).unwrap_err(),
            EditError::NoForm
        );
    }
}

#[test]
fn counted_vector_lengths_are_never_invalidated_by_delimiter_changes() {
    for (text, byte, operation) in [
        ("#2(a b) #2(c d)", 3, StructuralOperation::JoinForward),
        ("#2(a b)", 3, StructuralOperation::Split),
        ("#0() a", 2, StructuralOperation::SlurpForward),
        ("#2((a b) c)", 5, StructuralOperation::Splice),
        ("#2((a b) c)", 5, StructuralOperation::SlurpForward),
        ("#2((a b) c)", 5, StructuralOperation::BarfForward),
    ] {
        assert_eq!(
            edit_range(snapshot(text), cursor(text, byte), operation).unwrap_err(),
            EditError::UnsupportedSelection,
            "{text}, {operation:?}"
        );
    }
    let text = "#2((a b) c)";
    assert_eq!(
        edit_range(
            snapshot(text),
            range(text, 4, 7),
            StructuralOperation::Raise
        )
        .unwrap_err(),
        EditError::UnsupportedSelection
    );
    assert_eq!(edit_at(text, 4, StructuralOperation::Raise), "#2(a c)");
    assert_eq!(
        edit_at("#2(a b)", 3, StructuralOperation::Wrap),
        "#2((a) b)"
    );
    assert_eq!(
        edit_at("#2(a b)", 3, StructuralOperation::TransposeForward),
        "#2(b a)"
    );
    assert_eq!(
        edit_range(
            snapshot("#2(a b)"),
            range("#2(a b)", 0, 7),
            StructuralOperation::Wrap
        )
        .unwrap()
        .apply(),
        "(#2(a b))"
    );
}

#[test]
fn repeated_reader_prefixes_do_not_hide_inner_group_forms() {
    assert_eq!(edit_at("''(a b)", 3, StructuralOperation::Raise), "a");
    assert_eq!(
        edit_at("''(a b)", 3, StructuralOperation::Wrap),
        "''((a) b)"
    );
}

#[test]
fn constrained_standard_reader_literals_are_opaque_inside_the_operand() {
    for (text, byte) in [
        ("#C(1 2) 3", 3),
        ("#S(point x 1) tail", 9),
        ("#2A((1 2) (3 4)) tail", 4),
    ] {
        for operation in [
            StructuralOperation::Wrap,
            StructuralOperation::Splice,
            StructuralOperation::SlurpForward,
            StructuralOperation::SlurpBackward,
            StructuralOperation::BarfForward,
            StructuralOperation::BarfBackward,
            StructuralOperation::Split,
            StructuralOperation::JoinForward,
            StructuralOperation::JoinBackward,
            StructuralOperation::TransposeForward,
            StructuralOperation::TransposeBackward,
        ] {
            assert!(
                edit_range(snapshot(text), cursor(text, byte), operation).is_err(),
                "{text}, {operation:?}"
            );
        }
    }
    assert_eq!(
        edit_at("#C(1 2) tail", 0, StructuralOperation::Wrap),
        "(#C(1 2)) tail"
    );
    assert_eq!(
        edit_at(
            "#S(point x 1) tail",
            0,
            StructuralOperation::TransposeForward
        ),
        "tail #S(point x 1)"
    );
    assert_eq!(
        edit_at("#.(a b)", 3, StructuralOperation::Wrap),
        "#.((a) b)"
    );
}

#[test]
fn unrestricted_structure_values_and_array_elements_remain_editable() {
    assert_eq!(
        edit_at("#0A(a b)", 4, StructuralOperation::Wrap),
        "#0A((a) b)"
    );
    assert_eq!(
        edit_at("#S(point :x (a b))", 13, StructuralOperation::Wrap),
        "#S(point :x ((a) b))"
    );
    assert_eq!(
        edit_at("#2A(((a b) c) (d e))", 6, StructuralOperation::Wrap),
        "#2A((((a) b) c) (d e))"
    );
    assert_eq!(
        edit_at("#2A((1 2) (3 4))", 5, StructuralOperation::TransposeForward),
        "#2A((2 1) (3 4))"
    );
    let source = "#S(point :x (a b) :y (c d))";
    assert_eq!(
        edit_range(
            snapshot(source),
            cursor(source, 13),
            StructuralOperation::SlurpForward
        )
        .unwrap_err(),
        EditError::UnsupportedSelection
    );
}

#[test]
fn raw_string_payload_spaces_belong_to_the_result_selection() {
    let raw_snapshot = |text| {
        let mut workspace = SourceWorkspace::new();
        workspace.set_readtable(zkcl_syntax::syntax::SourceReadtable::RawStrings);
        workspace.set_document("raw", text);
        workspace.snapshot("raw").unwrap()
    };
    let change = edit(
        raw_snapshot("#>foo  \nbar"),
        Selection::new(0.into(), 7.into()),
        StructuralOperation::TransposeForward,
    )
    .unwrap();
    assert_eq!(change.apply(), "bar\n#>foo  ");
    assert_eq!(change.selection().range(), range("bar\n#>foo  ", 4, 11));
    let change = edit(
        raw_snapshot("(#>foo  \n)"),
        Selection::new(1.into(), 8.into()),
        StructuralOperation::Raise,
    )
    .unwrap();
    assert_eq!(change.apply(), "#>foo  ");
    assert_eq!(change.selection().range(), range("#>foo  ", 0, 7));
}

#[test]
fn comma_context_and_readlabel_scope_cannot_be_broken() {
    for (text, at, operation) in [
        ("`(a ,b)", 2, StructuralOperation::BarfForward),
        ("`(,a b)", 3, StructuralOperation::BarfBackward),
        ("(#1=(a) #1#)", 7, StructuralOperation::Split),
    ] {
        assert_eq!(
            edit_range(snapshot(text), cursor(text, at), operation).unwrap_err(),
            EditError::UnknownReaderBoundary,
            "{text}, {operation:?}"
        );
    }
    let text = "(#1=(a) #1#)";
    assert_eq!(
        edit_range(
            snapshot(text),
            range(text, 1, 7),
            StructuralOperation::TransposeForward
        )
        .unwrap_err(),
        EditError::UnknownReaderBoundary
    );
    assert_eq!(
        edit_at(text, 0, StructuralOperation::Wrap),
        "((#1=(a) #1#))"
    );
    assert_eq!(
        edit_at(
            "(#1=(a) #1#) tail",
            0,
            StructuralOperation::TransposeForward
        ),
        "tail (#1=(a) #1#)"
    );
}

#[test]
fn incomplete_block_comment_end_does_not_target_preceding_code() {
    for text in ["a #|unfinished", "a #|outer #|inner|#"] {
        assert_eq!(
            edit_range(
                snapshot(text),
                cursor(text, text.len()),
                StructuralOperation::Wrap
            )
            .unwrap_err(),
            EditError::NoForm
        );
    }
}
