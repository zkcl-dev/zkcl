#![cfg(feature = "analysis")]

use zkcl_syntax::syntax::lexer::{lex, scan_token};
use zkcl_syntax::syntax::{
    SourceReadtable, SyntaxDiagnosticKind, SyntaxKind, TextRange, TextSize, parse_source,
};
use zkcl_syntax::{
    SourceWorkspace, ast::AstForm, format_source, matching_parens_at, tokens_at_offset,
};

#[test]
fn fixed_readtable_lexer_borrows_tokens_and_keeps_lexical_errors() {
    let text = "#>雪 (\r\n\t#>continued\r\n|unclosed";
    let tokens: Vec<_> = lex(text, SourceReadtable::RawStrings).collect();
    assert_eq!(tokens[0].kind, SyntaxKind::RawString);
    assert_eq!(tokens[0].text, "#>雪 (\r\n\t#>continued");
    assert_eq!(tokens[1].kind, SyntaxKind::Whitespace);
    assert_eq!(tokens[1].text, "\r\n");
    assert_eq!(tokens[2].kind, SyntaxKind::Atom);
    let diagnostic = tokens[2].diagnostic.unwrap();
    assert_eq!(
        diagnostic.kind,
        SyntaxDiagnosticKind::UnterminatedMultipleEscape
    );
    assert_eq!(diagnostic.start, tokens[2].range().start);
    assert_eq!(tokens[2].range().end, text.len());
    for token in tokens {
        assert_eq!(&text[token.range()], token.text);
    }
    let first = lex(text, SourceReadtable::Standard).next().unwrap();
    assert_eq!(
        (first.kind, first.text),
        (SyntaxKind::UnknownDispatch, "#>")
    );
}

#[test]
fn only_literal_standalone_lists_select_a_readtable() {
    let text = concat!(
        "' (zk:in-readtable :raw-strings)\n",
        "#+maybe (zk:in-readtable :raw-strings)\n",
        "(progn (zk:in-readtable :raw-strings))\n",
        "(zk:in-readtable :raw-strings extra)\n",
        "(ZK:IN-READTABLE #| note |# :RAW-STRINGS)\n",
        "#>literal (\n",
        "(zk:in-readtable :standard)\n",
        "#>unknown (balanced)\n",
    );
    let parsed = parse_source(text);
    assert!(parsed.diagnostics().is_empty());
    let [(enable, enabled), (disable, disabled)] = parsed.readtable_changes() else {
        panic!("expected two readtable selections");
    };
    assert_eq!(parsed.initial_readtable(), SourceReadtable::Standard);
    assert_eq!(*enabled, SourceReadtable::RawStrings);
    assert_eq!(*disabled, SourceReadtable::Standard);
    assert_eq!(usize::from(enable.start()), text.find("(ZK:IN").unwrap());
    assert_eq!(
        parsed.readtable_at(enable.start()),
        SourceReadtable::Standard
    );
    assert_eq!(
        parsed.readtable_at(enable.end()),
        SourceReadtable::RawStrings
    );
    assert_eq!(
        parsed.readtable_at(disable.start()),
        SourceReadtable::RawStrings
    );
    assert_eq!(
        parsed.readtable_at(disable.end()),
        SourceReadtable::Standard
    );
}

#[test]
fn typed_forms_expose_group_delimiters_and_reader_operands() {
    let parsed =
        parse_source("; datum\n(a . #2(#\\雪 #\\Space))\n#-(or :a :b) (work)\n(open (inner)");
    let forms: Vec<_> = parsed.forms().collect();
    let list = forms[0].as_list().unwrap();
    assert_eq!(list.dot().unwrap().text(), ".");
    let (open, close) = list.delimiters().unwrap();
    assert_eq!((open.text(), close.text()), ("(", ")"));
    let tail = list.dotted_tail().unwrap();
    let vector = tail.as_vector().unwrap();
    assert_eq!(vector.delimiters().unwrap().0.text(), "#2(");
    let characters: Vec<_> = vector
        .forms()
        .map(|f| f.as_literal().unwrap().character_value())
        .collect();
    assert_eq!(characters, [Some('雪'), Some(' ')]);
    let conditional = forms[1]
        .as_reader_form()
        .unwrap()
        .feature_conditional()
        .unwrap();
    assert!(!conditional.when_present);
    assert_eq!(conditional.is_active(Some(&["A".into()])), Some(false));
    assert_eq!(conditional.is_active(Some(&[])), Some(true));
    assert_eq!(conditional.is_active(None), None);
    assert_eq!(conditional.guarded.into_element().to_string(), "(work)");
    let incomplete = forms[2].as_list().unwrap();
    assert!(incomplete.delimiters().is_none());
    assert!(!incomplete.is_closed());
    assert!(matching_parens_at(&parsed.root(), incomplete.text_range().start()).is_none());
    assert!(parsed.primary_diagnostic().unwrap().needs_more_input());
    assert_eq!(
        parse_source(") (open").primary_diagnostic().unwrap().kind(),
        SyntaxDiagnosticKind::UnexpectedCloseParen
    );
}

#[test]
fn indexed_token_lookup_matches_rowan_at_every_boundary() {
    let parsed = parse_source("(雪#|x|# #|😀|# \"snow\" '#(x) (a . #\\) )) )");
    let root = parsed.root();
    for node in root.descendants() {
        for byte in u32::from(node.text_range().start())..=u32::from(node.text_range().end()) {
            let at = TextSize::from(byte);
            let expected: Vec<_> = node.token_at_offset(at).collect();
            let actual: Vec<_> = tokens_at_offset(&node, at).collect();
            assert_eq!(actual, expected, "byte {byte} in {node:?}");
        }
    }
    assert!(
        tokens_at_offset(&root, root.text_range().end() + TextSize::from(1))
            .next()
            .is_none()
    );
}

#[test]
fn snapshot_pairs_only_in_the_current_token_and_readtable() {
    let text = "(zk:in-readtable :raw-strings)\n#>\"雪\n(list |a b| #\\😀)";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("input", text);
    let snapshot = workspace.snapshot("input").unwrap();
    let raw = text.find("#>").unwrap();
    let snow = text.find('雪').unwrap();
    let list = text.find("(list").unwrap();
    let escaped = text.find("a b").unwrap();
    for (at, pairs) in [
        (raw, true),
        (raw + 1, false),
        (snow + 1, false),
        (list - 1, false),
        (list, true),
        (escaped, false),
        (escaped + "a b|".len(), true),
        (text.len(), true),
        (text.len() + 1, false),
    ] {
        assert_eq!(snapshot.should_auto_pair_at(at), pairs, "byte {at}");
    }
    assert_eq!(
        snapshot.text_at(TextRange::new(
            (snow as u32).into(),
            (snow as u32 + 3).into()
        )),
        Some("雪")
    );
    assert_eq!(
        snapshot.text_at(TextRange::empty((snow as u32 + 1).into())),
        None
    );
}

#[test]
fn raw_strings_form_one_token_in_the_selected_readtable() {
    let text = concat!(
        "(zk:in-readtable :raw-strings)\n",
        "(defun  example ()\n",
        "  #>",
        "(greet #\\A \"雪😀\"\r\n",
        "\t#>\r\n",
        "  #>   )  \r\n",
        " )\n",
        "#| retained |#\n",
        "(defun last-form () 42)\n"
    );
    let parsed = parse_source(text);
    assert!(parsed.diagnostics().is_empty());
    let root = parsed.root();
    assert_eq!(root.text().to_string(), text);
    let raw = root
        .descendants_with_tokens()
        .find(|entry| entry.kind() == SyntaxKind::RawString)
        .unwrap();
    assert_eq!(
        raw.to_string(),
        "#>(greet #\\A \"雪😀\"\r\n\t#>\r\n  #>   )  "
    );
    let Some(AstForm::Literal(raw)) = AstForm::cast(raw) else {
        panic!("expected a literal");
    };
    assert_eq!(
        raw.string_value().as_deref(),
        Some("(greet #\\A \"雪😀\"\n\n   )  ")
    );
    let formatted = format_source(text).unwrap();
    assert!(formatted.starts_with("(zk:in-readtable :raw-strings)\n(defun example ()\n"));
    assert!(formatted.contains(raw.text()));
    assert_eq!(
        format_source(&formatted).as_deref(),
        Some(formatted.as_str())
    );
}

#[test]
fn editing_a_readtable_selection_reparses_the_following_forms() {
    let mut workspace = SourceWorkspace::new();
    let source = "(zk:in-readtable :standard)\n#> text (\n";
    workspace.set_document("example", source);
    let old = workspace.snapshot("example").unwrap();
    assert_eq!(old.diagnostics().len(), 1);
    let start = source.find("standard").unwrap();
    workspace
        .apply_edit("example", start..start + "standard".len(), "raw-strings")
        .unwrap();
    let current = workspace.snapshot("example").unwrap();
    assert!(current.diagnostics().is_empty());
    assert_eq!(current.parsed(), &parse_source(current.text()));
    assert_eq!(old.diagnostics().len(), 1);

    let source = "(defparameter *x* 3)\n(zk:in-readtable :standard)\n#>plain-string\n";
    workspace.set_document("example", source);
    let before = workspace.snapshot("example").unwrap();
    assert!(before.diagnostics().is_empty());
    let name = source.find("*x*").unwrap();
    workspace
        .apply_edit("example", name + 1..name + 2, "longer-name")
        .unwrap();
    let after = workspace.snapshot("example").unwrap();
    assert_eq!(after.parsed(), &parse_source(after.text()));
    let start = after.text().find(":standard").unwrap();
    workspace
        .apply_edit("example", start..start + ":standard".len(), ":raw-strings")
        .unwrap();
    let current = workspace.snapshot("example").unwrap();
    assert_eq!(current.parsed(), &parse_source(current.text()));

    workspace.set_readtable(SourceReadtable::RawStrings);
    workspace.set_document("from-asdf", "#> text (\n");
    assert!(
        workspace
            .snapshot("from-asdf")
            .unwrap()
            .diagnostics()
            .is_empty()
    );
    workspace.set_readtable(SourceReadtable::Standard);
    assert_eq!(
        workspace.snapshot("from-asdf").unwrap().diagnostics().len(),
        1
    );
}

#[test]
fn consing_dot_diagnostics_preserve_offending_tokens() {
    let text = "(a . b) (x . (y . z)) (a #|note|# . b) (a |.| \\.) #1=(a . #1#) #(a . (b))";
    let parsed = parse_source(text);
    assert_eq!(parsed.root().text().to_string(), text);
    assert!(parsed.diagnostics().is_empty());
    assert_eq!(
        parsed
            .root()
            .descendants_with_tokens()
            .filter(|item| item.kind() == SyntaxKind::Dot)
            .count(),
        6
    );

    for (text, kind, bad, offset) in [
        (
            "(雪 . b c) (ok)",
            SyntaxDiagnosticKind::ExtraDottedTail,
            "c",
            9,
        ),
        ("(. a)", SyntaxDiagnosticKind::DottedListWithoutHead, ".", 1),
        (
            "(a . #|note|# )",
            SyntaxDiagnosticKind::MissingDottedTail,
            ".",
            3,
        ),
        ("(a . .)", SyntaxDiagnosticKind::UnexpectedDot, ".", 5),
        ("'.", SyntaxDiagnosticKind::UnexpectedDot, ".", 1),
        ("...", SyntaxDiagnosticKind::InvalidDotToken, "...", 0),
    ] {
        let parsed = parse_source(text);
        assert_eq!(parsed.root().text().to_string(), text);
        let [diagnostic] = parsed.diagnostics() else {
            panic!(
                "{text:?}: expected one diagnostic, got {:?}",
                parsed.diagnostics()
            );
        };
        let start = TextSize::from(offset);
        assert_eq!(diagnostic.kind(), kind, "{text:?}");
        assert_eq!(
            diagnostic.range(),
            TextRange::at(start, TextSize::of(bad)),
            "{text:?}"
        );
    }
}

#[test]
fn reader_prefixes_can_receive_operands_after_skipped_feature_forms() {
    for text in [
        "(#+feature head . tail)",
        "(head . #+feature one #-feature two)",
        "(head . '#+feature one two)",
        "#+feature (. allowed-to-be-skipped)",
        "#+feature ...",
    ] {
        let parsed = parse_source(text);
        assert_eq!(parsed.root().text().to_string(), text);
        assert!(
            parsed.diagnostics().is_empty(),
            "{text:?}: {:?}",
            parsed.diagnostics()
        );
    }

    let parsed = parse_source("(head . (#+feature one two) extra)");
    let [diagnostic] = parsed.diagnostics() else {
        panic!(
            "expected the containing list's error, got {:?}",
            parsed.diagnostics()
        );
    };
    assert_eq!(diagnostic.kind(), SyntaxDiagnosticKind::ExtraDottedTail);
}

#[test]
fn reader_token_boundaries_preserve_lisp_form_structure() {
    let text = "; ignore \r this too\n(#\\) #\\( #\\Space #\\雪 #\\)name foo#bar)";
    let parsed = parse_source(text);
    assert_eq!(parsed.root().text().to_string(), text);
    assert!(parsed.diagnostics().is_empty());
    let mut tokens = Vec::new();
    for entry in parsed.root().descendants_with_tokens() {
        if let Some(token) = entry.into_token() {
            tokens.push((token.kind(), token.text().to_owned()));
        }
    }
    assert_eq!(
        tokens[0],
        (SyntaxKind::LineComment, "; ignore \r this too".into())
    );
    assert!(tokens.contains(&(SyntaxKind::Atom, "foo#bar".into())));
    let characters: Vec<_> = tokens
        .iter()
        .filter(|(kind, _)| *kind == SyntaxKind::Character)
        .map(|(_, text)| text.as_str())
        .collect();
    assert_eq!(
        characters,
        ["#\\)", "#\\(", "#\\Space", "#\\雪", "#\\)name"]
    );

    let parsed = parse_source("雪 #\\");
    let [diagnostic] = parsed.diagnostics() else {
        panic!(
            "expected an incomplete character literal, got {:?}",
            parsed.diagnostics()
        );
    };
    assert_eq!(
        diagnostic.kind(),
        SyntaxDiagnosticKind::IncompleteCharacterLiteral
    );
    assert_eq!(diagnostic.range(), TextRange::new(4.into(), 6.into()));
}

#[test]
fn reader_dispatch_tokenization_preserves_utf8_ranges() {
    use SyntaxKind::*;

    let text = r##"#17😀next #\😀name a\雪b |雪\| 😀| "雪\😀" #|雪 #|😀|# 雪|#"##;
    let expected = [
        (UnknownDispatch, "#17😀"),
        (Atom, "next"),
        (Whitespace, " "),
        (Character, r"#\😀name"),
        (Whitespace, " "),
        (Atom, r"a\雪b"),
        (Whitespace, " "),
        (Atom, r"|雪\| 😀|"),
        (Whitespace, " "),
        (String, r#""雪\😀""#),
        (Whitespace, " "),
        (BlockComment, "#|雪 #|😀|# 雪|#"),
    ];

    let mut start = 0;
    for (kind, spelling) in expected {
        let token = scan_token(text, start).unwrap();
        assert_eq!(token.kind, kind);
        assert_eq!(&text[start..token.end], spelling);
        assert!(token.diagnostic.is_none());
        start = token.end;
    }
    assert_eq!(start, text.len());
    assert!(scan_token(text, start).is_none());

    let parsed = parse_source("雪 |😀\\");
    let [diagnostic] = parsed.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", parsed.diagnostics());
    };
    assert_eq!(diagnostic.kind(), SyntaxDiagnosticKind::IncompleteEscape);
    assert_eq!(diagnostic.range(), TextRange::new(9.into(), 10.into()));
    assert!(diagnostic.needs_more_input());
}

#[test]
fn nesting_limit_retains_input_for_recovery_at_the_next_form() {
    let depth = 1024;
    let text = format!("{}x{} (after)", "(".repeat(depth), ")".repeat(depth));
    let parsed = parse_source(&text);
    let root = parsed.root();
    assert_eq!(root.text().to_string(), text);
    assert_eq!(root.children().count(), 2);
    assert_eq!(
        root.children().last().unwrap().text().to_string(),
        "(after)"
    );
    let [diagnostic] = parsed.diagnostics() else {
        panic!("expected one diagnostic, got {:?}", parsed.diagnostics());
    };
    assert_eq!(
        diagnostic.kind(),
        SyntaxDiagnosticKind::NestingLimitExceeded
    );
    assert!(!diagnostic.needs_more_input());
}
