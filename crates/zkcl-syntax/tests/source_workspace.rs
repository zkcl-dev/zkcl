#![cfg(feature = "analysis")]

use std::sync::Arc;

use zkcl_syntax::{
    SourceEditError, SourcePosition, SourcePositionEncoding, SourceWorkspace,
    syntax::{SyntaxDiagnosticKind, SyntaxKind, parse_source},
};

#[test]
fn source_syntax_round_trips_reader_text_byte_for_byte() {
    let text = concat!(
        "; comment with unmatched )\r\n",
        "  (list \"雪 \\\"quoted\\\" ; )\"\t'(alpha beta))\r\n",
        "#| outer #| inner () |# comment |#\n",
    );
    let parsed = parse_source(text);
    let root = parsed.root();

    assert_eq!(root.text().to_string(), text);
    assert!(parsed.diagnostics().is_empty());
    assert!(root.descendants_with_tokens().any(|entry| {
        entry.into_token().is_some_and(|token| {
            token.kind() == SyntaxKind::BlockComment
                && token.text() == "#| outer #| inner () |# comment |#"
        })
    }));
}

#[test]
fn source_syntax_recovers_with_utf8_byte_ranges() {
    let text = "(list \"雪\"))\n(list 9)";
    let unexpected = text.find("))").unwrap() + 1;
    let parsed = parse_source(text);
    let [diagnostic] = parsed.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", parsed.diagnostics());
    };

    assert_eq!(parsed.root().text().to_string(), text);
    assert_eq!(
        parsed
            .root()
            .children()
            .filter(|child| child.kind() == SyntaxKind::List)
            .count(),
        2
    );
    assert_eq!(
        diagnostic.kind(),
        SyntaxDiagnosticKind::UnexpectedCloseParen
    );
    assert_eq!(
        u32::from(diagnostic.range().start()),
        u32::try_from(unexpected).unwrap()
    );
    assert_eq!(
        u32::from(diagnostic.range().end()),
        u32::try_from(unexpected + 1).unwrap()
    );

    let incomplete = "\"雪\"\n(list 2";
    let opening = incomplete.find('(').unwrap();
    let parsed = parse_source(incomplete);
    let [diagnostic] = parsed.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", parsed.diagnostics());
    };
    assert_eq!(parsed.root().text().to_string(), incomplete);
    assert_eq!(diagnostic.kind(), SyntaxDiagnosticKind::MissingCloseParen);
    assert_eq!(
        u32::from(diagnostic.range().start()),
        u32::try_from(opening).unwrap()
    );
    assert_eq!(
        u32::from(diagnostic.range().end()),
        u32::try_from(opening + 1).unwrap()
    );
}

#[test]
fn document_caches_survive_edits_that_leave_their_text_unchanged() {
    let mut workspace = SourceWorkspace::new();
    assert!(workspace.parse("unknown.lisp").is_none());
    assert!(workspace.diagnostics("unknown.lisp").is_none());

    let original = "(list \"雪\"";
    let still_incomplete = "(list \"犬\"";
    let edited = "(list \"雪\")\n";
    let other = ")";
    workspace.set_document("left.lisp", original);
    workspace.set_document("right.lisp", other);

    let original_parse = workspace.parse("left.lisp").unwrap();
    let original_diagnostics = workspace.diagnostics("left.lisp").unwrap();
    let right_parse = workspace.parse("right.lisp").unwrap();
    let right_diagnostics = workspace.diagnostics("right.lisp").unwrap();
    assert_eq!(original_diagnostics.len(), 1);
    assert_eq!(right_diagnostics.len(), 1);

    workspace.set_document("left.lisp", still_incomplete);
    let intermediate_parse = workspace.parse("left.lisp").unwrap();
    assert_eq!(
        intermediate_parse.root().text().to_string(),
        still_incomplete
    );
    assert!(!Arc::ptr_eq(&original_parse, &intermediate_parse));
    assert_eq!(
        original_diagnostics,
        workspace.diagnostics("left.lisp").unwrap()
    );

    workspace.set_document("left.lisp", edited);
    let edited_parse = workspace.parse("left.lisp").unwrap();
    let edited_diagnostics = workspace.diagnostics("left.lisp").unwrap();
    assert!(!Arc::ptr_eq(&original_parse, &edited_parse));
    assert_eq!(original_parse.root().text().to_string(), original);
    assert_eq!(edited_parse.root().text().to_string(), edited);
    assert!(edited_diagnostics.is_empty());

    assert!(Arc::ptr_eq(
        &right_parse,
        &workspace.parse("right.lisp").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &right_diagnostics,
        &workspace.diagnostics("right.lisp").unwrap()
    ));

    workspace.set_document("left.lisp", edited);
    assert!(Arc::ptr_eq(
        &edited_parse,
        &workspace.parse("left.lisp").unwrap()
    ));
    assert!(Arc::ptr_eq(
        &edited_diagnostics,
        &workspace.diagnostics("left.lisp").unwrap()
    ));
}

#[test]
fn full_text_updates_share_untouched_syntax_across_pending_edits() {
    let mut workspace = SourceWorkspace::new();
    let original = "(defun unchanged () 42)\r\n(list |雪| 7)\r\n";
    workspace.set_document("input", original);
    let old = workspace.snapshot("input").unwrap();
    let old_child = old.parsed().root().first_child().unwrap();
    assert_eq!(old.line_count(), 3);

    workspace.set_document("input", original.replace('雪', "雪😀"));
    let edited = original.replace("|雪| 7", "|雪😀| 8");
    workspace.set_document("input", edited.as_str());
    let snapshot = workspace.snapshot("input").unwrap();
    assert_eq!(snapshot.parsed(), &parse_source(&edited));
    assert_eq!(snapshot.line_count(), old.line_count());
    assert_eq!(snapshot.line_text(1), Some("(list |雪😀| 8)"));
    assert_eq!(old.line_text(1), Some("(list |雪| 7)"));
    let new_child = snapshot.parsed().root().first_child().unwrap();
    assert!(std::ptr::eq(old_child.green(), new_child.green()));
}

#[test]
fn reader_context_changes_reuse_the_parsed_text() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "library",
        "#+tools (defun work () nil)\n#+windows (defun start () nil)\n)",
    );
    workspace.set_document("caller", "(demo::work)");

    let without_features = workspace.snapshot("library").unwrap();
    let parsed = workspace.parse("library").unwrap();
    let diagnostics = workspace.diagnostics("library").unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert!(without_features.outline().is_empty());

    workspace.set_features(Some(vec!["tools".into()]));
    let with_features = workspace.snapshot("library").unwrap();
    assert_eq!(with_features.outline()[0].name, "work");
    assert!(without_features.outline().is_empty());
    assert!(workspace.definition("caller", "(demo::".len()).is_none());

    assert!(workspace.set_initial_package("library", Some("demo".into())));
    assert_eq!(
        workspace
            .definition("caller", "(demo::".len())
            .unwrap()
            .document,
        "library"
    );
    let windows = "x86_64-pc-windows-msvc".parse().unwrap();
    assert!(workspace.set_target(Some(&windows)));
    let with_target = workspace.snapshot("library").unwrap();
    assert_eq!(with_target.outline()[0].name, "start");
    assert_eq!(with_features.outline()[0].name, "work");
    assert!(workspace.definition("caller", "(demo::".len()).is_none());
    assert!(!workspace.set_target(Some(&windows)));
    assert!(Arc::ptr_eq(
        &with_target,
        &workspace.snapshot("library").unwrap()
    ));
    assert!(Arc::ptr_eq(&parsed, &workspace.parse("library").unwrap()));
    assert!(Arc::ptr_eq(
        &diagnostics,
        &workspace.diagnostics("library").unwrap()
    ));
}

#[test]
fn source_snapshot_maps_diagnostics_across_mixed_line_breaks() {
    let mut workspace = SourceWorkspace::new();
    let text = "a雪😀\r\n\ne\u{301}\r)\n";
    workspace.set_document("repl", text);
    let snapshot = workspace.snapshot("repl").unwrap();
    let position = |line, column| SourcePosition { line, column };

    assert_eq!(snapshot.line_count(), 5);
    assert_eq!(snapshot.line_text(0), Some("a雪😀"));
    assert_eq!(snapshot.line_text(1), Some(""));
    assert_eq!(snapshot.line_text(2), Some("e\u{301}"));
    assert_eq!(snapshot.line_text(4), Some(""));
    assert_eq!(snapshot.line_text(5), None);
    assert_eq!(snapshot.position(2), None);
    assert_eq!(snapshot.position(text.len() + 1), None);
    assert_eq!(snapshot.position(4), Some(position(0, 2)));
    assert_eq!(snapshot.position(8), Some(position(0, 3)));
    assert_eq!(snapshot.position(9), Some(position(0, 3)));
    assert_eq!(snapshot.position(10), Some(position(1, 0)));
    assert_eq!(snapshot.position(14), Some(position(2, 2)));
    assert_eq!(snapshot.position(text.len()), Some(position(4, 0)));
    assert_eq!(snapshot.byte_offset(position(0, 3)), Some(8));
    assert_eq!(snapshot.byte_offset(position(2, 1)), Some(12));
    assert_eq!(snapshot.byte_offset(position(4, 1)), None);
    let [diagnostic] = snapshot.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics());
    };
    let offset = u32::from(diagnostic.range().start()) as usize;
    assert_eq!(snapshot.position(offset), Some(position(3, 0)));
    assert_eq!(snapshot.byte_offset(position(3, 0)), Some(offset));
}

#[test]
fn source_snapshot_converts_code_unit_positions_without_splitting_a_character() {
    use SourcePositionEncoding::{Utf8, Utf16, Utf32};

    let mut workspace = SourceWorkspace::new();
    let text = "a雪😀)\r\n\n";
    workspace.set_document("input", text);
    let snapshot = workspace.snapshot("input").unwrap();
    let position = |line, column| SourcePosition { line, column };

    for (offset, columns) in [
        (0, [0, 0, 0]),
        (1, [1, 1, 1]),
        (4, [4, 2, 2]),
        (8, [8, 4, 3]),
    ] {
        for (encoding, column) in [Utf8, Utf16, Utf32].into_iter().zip(columns) {
            let expected = position(0, column);
            assert_eq!(
                snapshot.position_with_encoding(offset, encoding),
                Some(expected)
            );
            assert_eq!(
                snapshot.byte_offset_with_encoding(expected, encoding),
                Some(offset)
            );
        }
    }
    for (encoding, column) in [(Utf8, 9), (Utf16, 5), (Utf32, 4)] {
        let end = position(0, column);
        assert_eq!(snapshot.position_with_encoding(9, encoding), Some(end));
        assert_eq!(snapshot.position_with_encoding(10, encoding), Some(end));
        assert_eq!(snapshot.byte_offset_with_encoding(end, encoding), Some(9));
        assert_eq!(
            snapshot.byte_offset_with_encoding(position(0, column + 1), encoding),
            None
        );
        assert_eq!(
            snapshot.position_with_encoding(11, encoding),
            Some(position(1, 0))
        );
        assert_eq!(
            snapshot.position_with_encoding(text.len(), encoding),
            Some(position(2, 0))
        );
        assert_eq!(
            snapshot.byte_offset_with_encoding(position(2, 0), encoding),
            Some(text.len())
        );
        assert_eq!(
            snapshot.byte_offset_with_encoding(position(3, 0), encoding),
            None
        );
        assert_eq!(snapshot.position_with_encoding(5, encoding), None);
    }
    assert_eq!(
        snapshot.byte_offset_with_encoding(position(0, 2), Utf8),
        None
    );
    assert_eq!(
        snapshot.byte_offset_with_encoding(position(0, 3), Utf16),
        None
    );

    let [diagnostic] = snapshot.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", snapshot.diagnostics());
    };
    let start = u32::from(diagnostic.range().start()) as usize;
    let end = u32::from(diagnostic.range().end()) as usize;
    assert_eq!(
        snapshot.position_with_encoding(start, Utf16),
        Some(position(0, 4))
    );
    assert_eq!(
        snapshot.position_with_encoding(end, Utf16),
        Some(position(0, 5))
    );
    let symbol_start = snapshot
        .byte_offset_with_encoding(position(0, 2), Utf16)
        .unwrap();
    workspace
        .apply_edit("input", symbol_start..start, "苗")
        .unwrap();
    assert_eq!(workspace.snapshot("input").unwrap().text(), "a雪苗)\r\n\n");
    assert_eq!(snapshot.text(), text);
}

#[test]
fn held_snapshots_outlive_workspace_edits_until_the_last_reference_drops() {
    let mut workspace = SourceWorkspace::new();
    let original = "(list \"雪\")\n";
    let start = original.find('雪').unwrap();
    let end = start + '雪'.len_utf8();
    workspace.set_document("input", original);
    workspace.set_document("unrelated", "42");
    let before = workspace.snapshot("input").unwrap();
    let other = workspace.snapshot("unrelated").unwrap();

    assert_eq!(
        workspace.apply_edit("missing", 0..0, "x"),
        Err(SourceEditError::UnknownDocument),
    );
    assert_eq!(
        workspace.apply_edit("input", start + 1..end, "x"),
        Err(SourceEditError::InvalidRange),
    );
    assert_eq!(
        workspace.apply_edit("input", end..start, "x"),
        Err(SourceEditError::InvalidRange),
    );
    workspace.apply_edit("input", start..end, "雪").unwrap();
    assert!(Arc::ptr_eq(&before, &workspace.snapshot("input").unwrap()));

    workspace.apply_edit("input", start..end, "犬😀").unwrap();
    let after = workspace.snapshot("input").unwrap();
    assert_eq!(after.text(), "(list \"犬😀\")\n");
    assert_eq!(after.parsed().root().text().to_string(), after.text());
    assert_eq!(after.line_text(0), Some("(list \"犬😀\")"));
    assert_eq!(before.text(), original);
    assert_eq!(before.line_text(0), Some("(list \"雪\")"));

    assert!(workspace.remove_document("input"));
    assert!(!workspace.remove_document("input"));
    assert!(workspace.snapshot("input").is_none());
    assert!(workspace.parse("input").is_none());
    assert!(workspace.diagnostics("input").is_none());
    assert_eq!(after.text(), "(list \"犬😀\")\n");
    assert!(Arc::ptr_eq(
        &other,
        &workspace.snapshot("unrelated").unwrap()
    ));

    let closed = Arc::downgrade(&after);
    drop(after);
    workspace.set_document("replacement", "()");
    assert!(closed.upgrade().is_none());
    assert_eq!(workspace.snapshot("replacement").unwrap().text(), "()");
}

#[test]
fn encoded_edits_validate_and_commit_one_snapshot() {
    let mut workspace = SourceWorkspace::new();
    let prefix = "; unchanged comment\r\n".repeat(900);
    let original = format!("{prefix}(list \"😀\" 1)\r\n(last)\n");
    workspace.set_document("input", &original);
    let before = workspace.snapshot("input").unwrap();
    let old_root = before.parsed().root();
    let range = |start, end| {
        let at = |column| SourcePosition { line: 900, column };
        Some(at(start)..at(end))
    };
    let encoding = SourcePositionEncoding::Utf16;

    assert_eq!(
        workspace.apply_position_edits(
            "input",
            encoding,
            [(range(11, 12), "42"), (range(8, 9), "雪")]
        ),
        Err(SourceEditError::InvalidRange),
    );
    assert!(Arc::ptr_eq(&before, &workspace.snapshot("input").unwrap()));
    workspace
        .apply_position_edits(
            "input",
            encoding,
            [(range(7, 9), "雪"), (range(7, 8), "😀")],
        )
        .unwrap();
    assert!(Arc::ptr_eq(&before, &workspace.snapshot("input").unwrap()));

    workspace
        .apply_position_edits(
            "input",
            encoding,
            [(range(7, 9), "雪"), (range(10, 11), "42")],
        )
        .unwrap();
    let edited = workspace.snapshot("input").unwrap();
    let expected = format!("{prefix}(list \"雪\" 42)\r\n(last)\n");
    assert_eq!(edited.parsed(), &parse_source(&expected));
    assert_eq!(edited.text(), expected);
    assert!(std::ptr::eq(
        edited.parsed().root().children().last().unwrap().green(),
        old_root.children().last().unwrap().green(),
    ));
    assert_eq!(before.text(), original);
}

#[test]
fn local_reparse_shares_unedited_subtrees_with_the_previous_snapshot() {
    let mut workspace = SourceWorkspace::new();
    let text = "(defun before () :雪)\r\n(defun middle () (list :x))\n(defun after () :y)\n";
    workspace.set_document("input", text);
    let before = workspace.parse("input").unwrap();
    let start = text.find(":x").unwrap();
    workspace
        .apply_edit("input", start..start + 2, ":犬")
        .unwrap();

    let after = workspace.snapshot("input").unwrap();
    assert_eq!(after.parsed(), &parse_source(after.text()));
    assert_eq!(before.root().text().to_string(), text);
    let old_lists: Vec<_> = before.root().children().collect();
    let new_lists: Vec<_> = after.parsed().root().children().collect();
    for index in [0, 2] {
        assert!(std::ptr::eq(
            old_lists[index].green(),
            new_lists[index].green(),
        ));
    }
}

#[test]
fn edits_that_change_reader_boundaries_match_a_full_parse() {
    for (text, old, new) in [
        ("(list x) (list y)", "x", "(x"),
        ("(list x) (list y)", "x", "x;"),
        ("(list x) (list y)", "x", "x) ("),
        ("(list x) (list y)", "x", "#| x"),
        ("(list x) (list y)", "x", "\"x"),
        ("(list x) (list y)", ") (", ") '("),
        ("' (list x) (list y)", "x", "(x"),
        ("(list x) (list y", "x", "雪"),
    ] {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("input", text);
        workspace.parse("input").unwrap();
        let start = text.find(old).unwrap();
        workspace
            .apply_edit("input", start..start + old.len(), new)
            .unwrap();
        let after = workspace.snapshot("input").unwrap();
        assert_eq!(after.parsed(), &parse_source(after.text()), "{text:?}");
    }
}

#[test]
fn consecutive_edits_reuse_an_unaffected_subtree_from_the_last_parse() {
    let mut ws = SourceWorkspace::new();
    let source = "(defun f () (list :alpha :beta) (unchanged x))";
    ws.set_document("x", source);
    let original = ws.parse("x").unwrap();
    let pos = source.find("alpha").unwrap();
    ws.apply_edit("x", pos + 1..pos + 4, "犬z").unwrap();
    ws.apply_edit("x", pos + 1..pos + 4, "lambda").unwrap();
    let after = ws.snapshot("x").unwrap();
    assert_eq!(after.parsed(), &parse_source(after.text()));
    let sibling = |parsed: &zkcl_syntax::syntax::ParsedSource| {
        parsed
            .root()
            .first_child()
            .unwrap()
            .children()
            .last()
            .unwrap()
    };
    assert!(std::ptr::eq(
        sibling(&original).green(),
        sibling(after.parsed()).green()
    ));
}

#[test]
fn batched_reader_boundary_edits_match_a_full_parse() {
    let mut ws = SourceWorkspace::new();
    for (text, changes) in [
        (
            "(a :bc :def)",
            vec![(3..5, ":xyz"), (1..2, "雪"), (7..7, "'")],
        ),
        ("(abc def)", vec![(1..4, ".."), (5..6, "|")]),
        ("(a b)", vec![(2..2, "\\"), (3..3, ";")]),
    ] {
        ws.set_document("x", text);
        ws.parse("x").unwrap();
        for (range, replacement) in changes {
            ws.apply_edit("x", range, replacement).unwrap();
        }
        let after = ws.snapshot("x").unwrap();
        assert_eq!(after.parsed(), &parse_source(after.text()));
    }
}

#[test]
fn editing_a_dot_updates_the_list_kind() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("input", "(a b) (c)");
    let before = workspace.parse("input").unwrap();
    let unaffected = before.root().children().nth(1).unwrap();
    for (range, replacement) in [(3..4, ". b"), (3..6, "b")] {
        workspace.apply_edit("input", range, replacement).unwrap();
        let after = workspace.snapshot("input").unwrap();
        assert_eq!(after.parsed(), &parse_source(after.text()));
        let sibling = after.parsed().root().children().nth(1).unwrap();
        assert!(std::ptr::eq(unaffected.green(), sibling.green()));
    }
}

#[test]
fn incremental_call_edit_clears_arity_warning_in_feature_selected_form() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(vec![":feat-a".into()]));
    let src = "\
(in-package #:demo)
#+feat-a (defun enabled-fn (unused) 1)
#+feat-b (defun disabled-fn () (car 1 2))
#-feat-b (defun fallback-fn (x) (cons x 1 #+feat-a 2))
";
    workspace.set_document("sample.lisp", src);

    let snap = workspace.snapshot("sample.lisp").unwrap();
    let names: Vec<_> = snap.outline().iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["enabled-fn", "fallback-fn"]);

    let warnings = workspace.semantic_warnings("sample.lisp");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.message == "unused lexical variable `unused`")
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.message == "cons takes at most 2 arguments, found 3")
    );

    // Fix the arity error via incremental `apply_edit` and verify warnings update.
    let bad_call = "(cons x 1 #+feat-a 2)";
    let call_start = src.find(bad_call).unwrap();
    workspace
        .apply_edit(
            "sample.lisp",
            call_start..call_start + bad_call.len(),
            "(cons x 1)",
        )
        .unwrap();
    let updated_warnings = workspace.semantic_warnings("sample.lisp");
    assert_eq!(updated_warnings.len(), 1, "{updated_warnings:?}");
    assert_eq!(
        updated_warnings[0].message,
        "unused lexical variable `unused`"
    );
}
