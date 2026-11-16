use zkcl::{EvalErrorKind, Runtime};

#[test]
fn reader_interprets_escapes_before_classifying_tokens() {
    let runtime = Runtime::start().unwrap();

    for (source, spelling) in [
        ("'MixEd", "MIXED"),
        ("'|MixEd|", "MixEd"),
        (r"'\mIxEd", "mIXED"),
        (r"'Up|Down|\aRest", "UPDownaREST"),
        ("'|雪 ) ; # ,|", "雪 ) ; # ,"),
        (r"'|\|\\|", "|\\"),
        (r"'\(end", "(END"),
        ("'a#b", "A#B"),
        (r"'a\:b", "A:B"),
        ("'cl-user::|a:b|", "a:b"),
        ("':|a:b|", "a:b"),
        ("'keyword:|a:b|", "a:b"),
        ("'||", ""),
        ("'cl-user::||", ""),
        (":||", ""),
        ("'keyword:||", ""),
        ("':...", "..."),
        ("':15", "15"),
        ("'|.-|", ".-"),
        ("'|...|", "..."),
        ("'|100|", "100"),
        (r"'+\1", "+1"),
        ("'5||", "5"),
        ("'|1.0e6|", "1.0e6"),
        ("'1+", "1+"),
        ("'/5", "/5"),
        ("'_1", "_1"),
        ("'25-dec-83", "25-DEC-83"),
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|err| panic!("{source}: {err}"));
        assert_eq!(
            runtime.symbol_name(&values[0]).as_deref(),
            Some(spelling),
            "{source}"
        );
    }

    for (first, second) in [
        ("'mixed", "'CL-USER::|MIXED|"),
        ("'|CL|:|NIL|", "'nil"),
        ("':|a:b|", "'keyword:|a:b|"),
        ("'||", "'cl-user::||"),
    ] {
        let first = runtime.eval(first).unwrap();
        let second = runtime.eval(second).unwrap();
        assert!(first[0].same_identity(&second[0]));
    }
    let local = runtime.eval("'|a:b|").unwrap();
    let keyword = runtime.eval(":|a:b|").unwrap();
    assert!(!local[0].same_identity(&keyword[0]));

    for (source, expected) in [("10.", 10), ("-123.", -123), ("'+1", 1)] {
        assert_eq!(runtime.eval(source).unwrap()[0].as_fixnum(), Some(expected));
    }
    for source in [
        "(typep 1/2 'ratio)",
        "(typep -.5 'single-float)",
        "(= 1.0e6 1000000)",
    ] {
        assert!(!runtime.eval(source).unwrap()[0].is_nil(), "{source}");
    }
}

#[test]
fn reader_rejects_malformed_tokens() {
    let runtime = Runtime::start().unwrap();

    for source in [
        ":",
        "::name",
        "cl-user::",
        "cl-user:::name",
        "cl-user::name:other",
        "||:name",
        "|cl|:nil",
        "keyword:DOES-NOT-EXIST",
        "...",
        "1/0",
        "1/2/3",
        "27^19",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .unwrap_or_else(|| panic!("accepted {source}"));
        assert_eq!(error.kind(), EvalErrorKind::Read, "{source}: {error}");
    }
}

#[test]
fn host_registration_resolves_escaped_function_names() {
    let runtime = Runtime::start().unwrap();

    runtime
        .register_function("|mixed:case|", |rt, _| Ok(vec![rt.fixnum(41).unwrap()]))
        .unwrap();
    assert_eq!(
        runtime.eval("(|mixed:case|)").unwrap()[0].as_fixnum(),
        Some(41)
    );
}
