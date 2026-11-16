use std::ops::Range;

use super::*;

fn reparse(before: &ParsedSource, text: &str, range: Range<usize>, new: &str) -> ParsedSource {
    let mut edited = text.to_owned();
    edited.replace_range(range.clone(), new);
    let actual = before
        .reparse(
            &zkcl_diagnostics::SourceDocument::input(text)
                .edited(range.clone(), new)
                .unwrap(),
            byte_range(range.start, range.end),
            new.len(),
        )
        .expect("valid edit to a supported source buffer");
    let expected = parse_source_with_readtable(&edited, before.initial_readtable());
    assert_eq!(
        actual,
        expected,
        "{text:?} {range:?} -> {new:?}\nactual:\n{:#?}\nexpected:\n{:#?}",
        actual.root(),
        expected.root(),
    );
    actual
}

fn replace(before: &ParsedSource, text: &str, old: &str, new: &str) -> ParsedSource {
    let start = text.find(old).expect("unique edit anchor");
    reparse(before, text, start..start + old.len(), new)
}

#[test]
fn boundary_edits_keep_the_corresponding_green_nodes() {
    let text = "(same :雪)\r\n(middle)\r\n(same :雪)";
    let before = parse_source(text);
    let old: Vec<_> = before.root().children().collect();
    let middle = text.find("(middle)").unwrap();
    for (range, new) in [
        (middle..middle, "(same :雪)\n"),
        (middle..middle + "(middle)".len(), ""),
        (middle - 2..middle, "  \t\n;; 😀\n"),
        (middle..middle, ")"),
        (text.len()..text.len(), " (same :雪)"),
    ] {
        let after = reparse(&before, text, range.clone(), new);
        let after_root = after.root();
        for sibling in [&old[0], &old[2]] {
            let old_start = usize::from(sibling.text_range().start());
            let position = if old_start >= range.end {
                old_start - range.len() + new.len()
            } else {
                old_start
            };
            let current = after_root
                .children()
                .find(|child| usize::from(child.text_range().start()) == position)
                .expect("the corresponding node still exists");
            assert!(std::ptr::eq(sibling.green(), current.green()));
        }
    }
}

#[test]
fn recovers_at_an_unchanged_suffix_with_its_diagnostics() {
    let text = "(before :😀)\n'  ) (middle :雪) (after . ) (end";
    let before = parse_source(text);
    let old_root = before.root();
    let old_after = old_root.children().nth(4).unwrap();
    let old_end = old_root.children().last().unwrap();

    let after = replace(&before, text, ") (middle :雪)", "(middle :犬)");
    let root = after.root();
    assert!(std::ptr::eq(
        old_after.green(),
        root.children().nth(2).unwrap().green()
    ));
    assert!(std::ptr::eq(
        old_end.green(),
        root.children().last().unwrap().green()
    ));
    let [dotted, end] = after.diagnostics() else {
        panic!("expected the two diagnostics in the unchanged suffix");
    };
    assert_eq!(dotted.kind(), SyntaxDiagnosticKind::MissingDottedTail);
    assert!(!dotted.needs_more_input());
    assert_eq!(end.kind(), SyntaxDiagnosticKind::MissingCloseParen);
    assert!(end.needs_more_input());

    let text = "(before) #x ";
    let after = reparse(&parse_source(text), text, text.len() - 1..text.len(), "");
    assert!(after.primary_diagnostic().unwrap().needs_more_input());
}

#[test]
fn readtable_state_and_raw_string_lookahead_follow_the_edit() {
    let text = concat!(
        "(before) (zk:in-readtable ; quick note\n :raw-strings)\r\n",
        "#>one (ok)\r\n   #xA0\r\n",
        "(zk:in-readtable :standard)\n(after :雪)",
    );
    let before = parse_source(text);
    let old_root = before.root();
    let last = old_root.children().last().unwrap();

    let after = replace(&before, text, "quick", "😀");
    assert!(std::ptr::eq(
        last.green(),
        after.root().children().last().unwrap().green()
    ));
    assert_eq!(
        usize::from(after.readtable_changes()[0].0.end()) + 1,
        usize::from(before.readtable_changes()[0].0.end()),
    );

    for (old, new) in [
        ("xA0", ">雪"),
        (":raw-strings", ":standard"),
        ("\r\n   ", "\r\n\t"),
    ] {
        let after = replace(&before, text, old, new);
        assert!(std::ptr::eq(
            last.green(),
            after.root().children().last().unwrap().green()
        ));
    }
}

#[test]
fn local_and_full_parses_agree_across_lexical_and_recovery_boundaries() {
    use SourceReadtable::{RawStrings, Standard};
    let cases = [
        (Standard, ""),
        (Standard, "é #\\😀 `(a ,@b . c) #x"),
        (Standard, "(a) (b .. c) (d . e f)"),
        (Standard, "(a) ; note\r\n|雪 😀| #xAf"),
        (Standard, "' ) #+feature (x) '(y"),
        (Standard, "(x #|a #|b|# c|# \"🍎\") (y)"),
        (Standard, "(zk:in-readtable :raw-strings)\n#>a\n #>b\n(c)"),
        (RawStrings, "#>first\r\r\n #xA\n(y) #>last"),
        (RawStrings, "#-skip #1>a\n (ok) #|open"),
        (RawStrings, "#+'a #>β\r\n  #>c\n) é"),
    ];
    for (readtable, text) in cases {
        let before = parse_source_with_readtable(text, readtable);
        let boundaries: Vec<_> = text
            .char_indices()
            .map(|(at, _)| at)
            .chain([text.len()])
            .collect();
        for (index, &start) in boundaries.iter().enumerate() {
            for &end in &boundaries[index..] {
                for new in [
                    "", "x", " ", "\n", "雪", "#>", "😀|", "(r)", ")", "'", "\\", "\"", "#|",
                ] {
                    reparse(&before, text, start..end, new);
                }
            }
        }
    }
}
