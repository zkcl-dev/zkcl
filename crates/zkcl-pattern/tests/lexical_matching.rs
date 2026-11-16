use zkcl_pattern::{
    CompileError, FailedSuffixes, Input, InputDomain, MatchMode, Outcome, Program, Rule,
    grapheme_boundaries, parse_regex,
};

fn units(text: &str) -> Vec<u32> {
    text.chars().map(u32::from).collect()
}

fn regex(mode: MatchMode, patterns: &[&str]) -> Program {
    let rules: Vec<_> = patterns
        .iter()
        .map(|pattern| (parse_regex(InputDomain::Text, pattern).unwrap(), 0))
        .collect();
    Program::compile(InputDomain::Text, mode, &rules).unwrap()
}

fn capture(group: usize, body: Rule) -> Rule {
    Rule::Capture {
        group,
        body: Box::new(body),
    }
}

fn literal(text: &str) -> Rule {
    Rule::Literal(units(text))
}

#[test]
fn compilation_errors_preserve_invalid_rule_parameters() {
    for (rule, captures, expected) in [
        (
            Rule::Literal(vec![256]),
            0,
            CompileError::LiteralOutsideDomain {
                unit: 256,
                domain: InputDomain::Bytes,
            },
        ),
        (
            capture(2, literal("a")),
            1,
            CompileError::InvalidCaptureIndex { index: 2, count: 1 },
        ),
        (
            literal("a"),
            (1 << 18) + 1,
            CompileError::CaptureLimit {
                count: (1 << 18) + 1,
                limit: 1 << 18,
            },
        ),
        (
            literal("a").repeat(3, Some(2), true),
            0,
            CompileError::InvertedRepetitionBounds { min: 3, max: 2 },
        ),
        (
            literal("a").repeat(0, Some((1 << 18) + 1), true),
            0,
            CompileError::RepetitionLimit {
                count: (1 << 18) + 1,
                limit: 1 << 18,
            },
        ),
    ] {
        let error = Program::compile(InputDomain::Bytes, MatchMode::Full, &[(rule, captures)])
            .err()
            .expect("invalid rule compiled successfully");
        assert_eq!(error, expected);
    }
}

#[test]
fn match_modes_choose_rules_and_ranges_independently() {
    assert_eq!(
        regex(MatchMode::Prefix, &["=", "=="])
            .run(&units("==x"))
            .unwrap()
            .rule,
        0
    );
    assert_eq!(
        regex(MatchMode::LongestPrefix, &["=", "=="])
            .run(&units("==x"))
            .unwrap()
            .rule,
        1
    );
    assert_eq!(
        regex(MatchMode::LongestPrefix, &["==", "[=]+"])
            .run(&units("==x"))
            .unwrap()
            .rule,
        0
    );
    assert_eq!(
        regex(MatchMode::Search, &["b", "a"])
            .run(&units("a--b"))
            .unwrap()
            .range,
        3..4
    );
    assert_eq!(
        regex(MatchMode::Prefix, &["a|ab"])
            .run(&units("ab"))
            .unwrap()
            .range,
        0..1
    );
    assert_eq!(
        regex(MatchMode::Full, &["a|ab"])
            .run(&units("ab"))
            .unwrap()
            .range,
        0..2
    );
    assert_eq!(
        regex(MatchMode::LongestPrefix, &["a|ab"])
            .run(&units("ab"))
            .unwrap()
            .range,
        0..2
    );
    assert_eq!(
        regex(MatchMode::Prefix, &["a+?"])
            .run(&units("aaa"))
            .unwrap()
            .range,
        0..1
    );
    assert!(matches!(
        Program::compile(
            InputDomain::Text,
            MatchMode::LongestPrefix,
            &[(parse_regex(InputDomain::Text, "a+?").unwrap(), 0)]
        ),
        Err(CompileError::LazyLongestPrefix)
    ));
}

#[test]
fn winning_path_preserves_captures_and_original_assertion_context() {
    let rule = Rule::Sequence(vec![
        capture(0, literal("a").repeat(1, None, true)),
        capture(1, literal("a").repeat(1, None, true)),
    ]);
    let program =
        Program::compile(InputDomain::Text, MatchMode::LongestPrefix, &[(rule, 2)]).unwrap();
    let matched = program.run(&units("aaa!")).unwrap();
    assert_eq!(matched.groups, [Some(0..2), Some(2..3)]);
    let rule = Rule::Choice(vec![
        capture(0, parse_regex(InputDomain::Text, "a$").unwrap()),
        literal("a"),
    ]);
    let program =
        Program::compile(InputDomain::Text, MatchMode::LongestPrefix, &[(rule, 1)]).unwrap();
    assert_eq!(program.run(&units("a!")).unwrap().groups, [None]);
    assert_eq!(program.run(&units("a")).unwrap().groups, [Some(0..1)]);
}

#[test]
fn optional_absence_empty_participation_and_loop_progress_are_distinct() {
    let rule = Rule::Sequence(vec![
        capture(0, literal("x")).repeat(0, Some(1), true),
        capture(1, literal("x").repeat(0, None, true)),
    ]);
    let program = Program::compile(InputDomain::Text, MatchMode::Full, &[(rule, 2)]).unwrap();
    assert_eq!(program.run(&[]).unwrap().groups, [None, Some(0..0)]);

    let child = Rule::Choice(vec![capture(0, literal("")), capture(1, literal("a"))]);
    let rule = child.repeat(0, None, true);
    let program =
        Program::compile(InputDomain::Text, MatchMode::LongestPrefix, &[(rule, 2)]).unwrap();
    assert_eq!(
        program.run(&units("aaa")).unwrap().groups,
        [None, Some(2..3)]
    );
    assert_eq!(program.run(&[]).unwrap().groups, [None, None]);

    let rule = capture(0, literal("")).repeat(2, None, true);
    let program = Program::compile(InputDomain::Text, MatchMode::Full, &[(rule, 1)]).unwrap();
    assert_eq!(program.run(&[]).unwrap().groups, [Some(0..0)]);
}

#[test]
fn incremental_matching_waits_only_for_a_contender_that_can_win() {
    let program = regex(MatchMode::LongestPrefix, &["=", "=="]);
    let mut scan = program.matcher(10);
    let input = units("==x");
    assert!(matches!(
        scan.resume(
            &program,
            Input {
                base: 10,
                units: &input[..1],
                left: None,
                graphemes: &[],
                eof: false
            }
        ),
        Outcome::NeedMore
    ));
    let Outcome::Match(matched) = scan.resume(
        &program,
        Input {
            base: 10,
            units: &input[..2],
            left: None,
            graphemes: &[],
            eof: false,
        },
    ) else {
        panic!("maximum prefix was not ready");
    };
    assert_eq!((matched.rule, matched.range), (1, 10..12));

    let program = regex(MatchMode::Prefix, &["=", "=="]);
    assert!(matches!(
        program.matcher(0).resume(
            &program,
            Input {
                base: 0,
                units: &input[..1],
                left: None,
                graphemes: &[],
                eof: false
            }
        ),
        Outcome::Match(_)
    ));

    let program = regex(MatchMode::LongestPrefix, &["a$", "a"]);
    let mut scan = program.matcher(0);
    let input = units("a");
    assert!(matches!(
        scan.resume(
            &program,
            Input {
                base: 0,
                units: &input,
                left: None,
                graphemes: &[],
                eof: false
            }
        ),
        Outcome::NeedMore
    ));
    let Outcome::Match(matched) = scan.resume(
        &program,
        Input {
            base: 0,
            units: &input,
            left: None,
            graphemes: &[],
            eof: true,
        },
    ) else {
        panic!("EOF must resolve the end assertion");
    };
    assert_eq!(matched.rule, 0);
}

#[test]
fn boundary_context_survives_token_and_chunk_boundaries() {
    let program = regex(MatchMode::LongestPrefix, &[r"\bb", "^b"]);
    let input = units("b");
    assert_eq!(program.run(&input).unwrap().rule, 0);
    let Outcome::Match(matched) = program.matcher(1).resume(
        &program,
        Input {
            base: 1,
            units: &input,
            left: Some(u32::from('a')),
            graphemes: &[],
            eof: true,
        },
    ) else {
        panic!("^b must match at the token origin");
    };
    assert_eq!(matched.rule, 1);

    let program = regex(MatchMode::LongestPrefix, &[r"\X"]);
    let input = units("e\u{301}!");
    let mut state = Default::default();
    let mut breaks = grapheme_boundaries(&input[..1], &mut state);
    let mut scan = program.matcher(0);
    assert!(matches!(
        scan.resume(
            &program,
            Input {
                base: 0,
                units: &input[..1],
                left: None,
                graphemes: &breaks,
                eof: false
            }
        ),
        Outcome::NeedMore
    ));
    breaks.extend(grapheme_boundaries(&input[1..], &mut state));
    let Outcome::Match(matched) = scan.resume(
        &program,
        Input {
            base: 0,
            units: &input,
            left: None,
            graphemes: &breaks,
            eof: true,
        },
    ) else {
        panic!("the grapheme should be complete");
    };
    assert_eq!(matched.range, 0..2);
    assert!(matches!(
        program.matcher(1).resume(
            &program,
            Input {
                base: 1,
                units: &input[1..],
                left: Some(input[0]),
                graphemes: &breaks[1..],
                eof: true
            }
        ),
        Outcome::Miss
    ));
}

#[test]
fn coverage_uses_the_selected_rule_and_returns_a_real_missing_input() {
    let program = regex(MatchMode::Prefix, &["a", "ab"]);
    let proof = program.coverage();
    assert_eq!(proof.exhaustive, Some(false));
    assert_eq!(proof.witness.as_deref(), Some(&[][..]));
    assert_eq!(proof.selectable, Some(vec![true, false]));
    assert_eq!(
        regex(MatchMode::LongestPrefix, &["a", "ab"])
            .coverage()
            .selectable,
        Some(vec![true, true])
    );
    assert_eq!(
        regex(MatchMode::Full, &["a*", ".*"]).coverage().exhaustive,
        Some(true)
    );
    let rules = ["^$", r"[\x00-\xfe]+"]
        .map(|pattern| (parse_regex(InputDomain::Bytes, pattern).unwrap(), 0));
    let program = Program::compile(InputDomain::Bytes, MatchMode::Full, &rules).unwrap();
    assert_eq!(program.coverage().witness, Some(vec![0xff]));
    assert!(program.run(&[0xff]).is_none());
}

#[test]
fn repeated_prefix_lookahead_does_not_rescan_a_failed_suffix() {
    let input = vec![u32::from('a'); 4096];
    for mode in [MatchMode::Prefix, MatchMode::LongestPrefix] {
        let program = regex(mode, &["a+b", "a"]);
        let mut failed = FailedSuffixes::default();
        let mut checkpoints = 0;
        for offset in 0..input.len() {
            let outcome = program
                .matcher(offset)
                .resume_with(
                    &program,
                    Input {
                        base: 0,
                        units: &input,
                        left: None,
                        graphemes: &[],
                        eof: true,
                    },
                    Some(&mut failed),
                    || {
                        checkpoints += 1;
                        Ok::<_, std::convert::Infallible>(())
                    },
                )
                .unwrap();
            let Outcome::Match(matched) = outcome else {
                panic!("expected a token at {offset} in {mode:?}");
            };
            assert_eq!((matched.rule, matched.range), (1, offset..offset + 1));
        }
        assert!(
            checkpoints < input.len() + 48,
            "{mode:?} revisited the failed suffix: {checkpoints}"
        );
    }
}

#[test]
fn search_full_and_zero_width_loops_preserve_priority_and_progress() {
    // Search prefers earlier rule, then leftmost match within that rule.
    let program = regex(MatchMode::Search, &["bc|c", "a"]);
    let matched = program.run(&units("abc")).unwrap();
    assert_eq!((matched.rule, matched.range), (0, 1..3));

    // Captures in repeated empty-capable loops must still make progress after min.
    let rule = parse_regex(InputDomain::Text, "(?:^|\\b)*a").unwrap();
    let program = Program::compile(InputDomain::Text, MatchMode::Full, &[(rule, 0)]).unwrap();
    assert!(program.run(&units("a")).is_some());
    assert!(program.run(&units("ba")).is_none());

    // Coverage distinguishes a$ vs a in Prefix vs LongestPrefix.
    let prefix_cov = regex(MatchMode::Prefix, &["a", "a$"]).coverage();
    assert_eq!(prefix_cov.selectable, Some(vec![true, false]));
    let exact_cov = regex(MatchMode::Full, &["^$", ".+"]).coverage();
    assert_eq!(exact_cov.exhaustive, Some(true));
    assert_eq!(exact_cov.selectable, Some(vec![true, true]));
}

#[test]
fn unicode_and_uts18_lexer_rules_select_and_capture_accurately() {
    let rules = vec![
        (parse_regex(InputDomain::Text, "if").unwrap(), 0),
        (parse_regex(InputDomain::Text, "else").unwrap(), 0),
        (
            capture(
                0,
                parse_regex(InputDomain::Text, r"\p{XID_Start}\p{XID_Continue}*").unwrap(),
            ),
            1,
        ),
        (parse_regex(InputDomain::Text, "==").unwrap(), 0),
        (parse_regex(InputDomain::Text, "=").unwrap(), 0),
        (
            capture(0, parse_regex(InputDomain::Text, r"\p{ASCII}+").unwrap()),
            1,
        ),
        (
            capture(0, parse_regex(InputDomain::Text, r"\p{Assigned}").unwrap()),
            1,
        ),
    ];
    let program = Program::compile(InputDomain::Text, MatchMode::LongestPrefix, &rules).unwrap();
    let kw = program.run(&units("if€")).unwrap();
    assert_eq!((kw.rule, kw.range), (0, 0..2));
    assert!(kw.groups.is_empty());

    let ascii = program.run(&units("if!")).unwrap();
    assert_eq!((ascii.rule, ascii.range), (5, 0..3));
    assert_eq!(ascii.groups, [Some(0..3)]);

    let ident = program.run(&units("if中==")).unwrap();
    assert_eq!((ident.rule, ident.range), (2, 0..3));
    assert_eq!(ident.groups, [Some(0..3)]);

    let eq = program.run(&units("==€")).unwrap();
    assert_eq!((eq.rule, eq.range), (3, 0..2));

    let non_ascii_sym = program.run(&units("€")).unwrap();
    assert_eq!((non_ascii_sym.rule, non_ascii_sym.range), (6, 0..1));
    assert_eq!(non_ascii_sym.groups, [Some(0..1)]);
}
