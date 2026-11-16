use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn string_equality_compares_the_requested_substrings() {
    let runtime = Runtime::start().unwrap();

    for (source, expected) in [
        (r#"(string= "NIL" nil)"#, true),
        (r#"(string= "Abc" "abc")"#, false),
        (
            r#"(string= "together" "frog" :start1 1 :end1 3 :start2 2)"#,
            true,
        ),
        (
            r#"(string= "é😀雪" "雪😀é" :start1 1 :end1 2 :start2 1 :end2 2)"#,
            true,
        ),
        (r#"(string= "😀é" "😀É")"#, false),
        (r#"(string= "😀" "" :start1 1)"#, true),
        (
            r#"(string-equal "xÉ" "yé" :start1 1 :start2 1 :end2 nil)"#,
            true,
        ),
        (r#"(string-equal "ß" "SS")"#, false),
        (r#"(string= "abc" "bc" :start1 1 :start1 0)"#, true),
        (
            r#"(string= "a" "a" :irrelevant 1 :allow-other-keys t)"#,
            true,
        ),
        (r#"(string/= "a" "a")"#, false),
        (r#"(string< "b" "a")"#, false),
        (r#"(string-not-equal "aBc" "ABc")"#, false),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(!result[0].is_nil(), expected, "{source}");
    }
}

#[test]
fn string_designators_preserve_existing_strings() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(stringp "text")"#, true),
        (r#"(stringp 'text)"#, false),
        (r#"(let ((text "abc")) (eq text (string text)))"#, true),
        (r#"(eq (symbol-name 'foo-bar) (string 'foo-bar))"#, true),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(!result[0].is_nil(), expected, "{source}");
    }
}

#[test]
fn strings_are_vector_sequences() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(typep "abc" 'sequence)"#, true),
        ("(typep '(a b c) 'sequence)", true),
        ("(typep '(a b c) 'vector)", false),
        (r#"(typep "abc" 'vector)"#, true),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(!result[0].is_nil(), expected, "{source}");
    }
}

#[test]
fn string_ordering_returns_a_character_index() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(string/= "éa" "éb")"#, 1),
        (r#"(string< "aa" "aaa")"#, 2),
        (r#"(string> "aaa" "aa")"#, 2),
        (r#"(string<= "same" "same")"#, 4),
        (r#"(string>= "𐐀" "𐐀" :end1 1)"#, 1),
        (r#"(string< "𐐀abc" "𐐀def" :start1 1 :start2 1)"#, 1),
        (
            r#"(string-lessp "012AAAA789" "01aaab6" :start1 3 :end1 7 :start2 2 :end2 6)"#,
            6,
        ),
        (r#"(string-greaterp "λZ" "λa" :start1 1 :start2 1)"#, 1),
        (
            r#"(string-not-greaterp "AbcDE" "abcdE" :start1 1 :start2 1)"#,
            5,
        ),
        (r#"(string-not-lessp "ABC" "abc")"#, 3),
        (r#"(string-not-equal "abc" "ABZ")"#, 2),
        (r#"(string< "abc" "a" :start1 2 :end1 2)"#, 2),
        (r#"(string/= "a" "b")"#, 0),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(result[0].as_fixnum(), Some(expected), "{source}");
    }
}

#[test]
fn sequence_length_counts_characters_or_list_elements() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(length "a𐐀é")"#, 3),
        ("(length nil)", 0),
        ("(length '(a (b c) d))", 3),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(result[0].as_fixnum(), Some(expected), "{source}");
    }
}

#[test]
fn string_case_conversion_respects_substring_bounds() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(string-upcase "xéabc" :start 1 :end 3)"#, "xÉAbc"),
        (r#"(string-upcase "ßø")"#, "ßØ"),
        (r#"(string-downcase 'foo-bar)"#, "foo-bar"),
        (
            r#"(string-capitalize "elm 13c arthur;fig don't")"#,
            "Elm 13c Arthur;Fig Don'T",
        ),
        (r#"(string-capitalize "abcDEF" :start 2 :end 5)"#, "abCdeF"),
        (
            r#"(string-upcase "unchanged" :start 3 :end 3)"#,
            "unchanged",
        ),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(
            runtime.string(&result[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }
}

#[test]
fn sequence_length_requires_a_proper_sequence() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(length 8)", ConditionClass::TypeError),
        ("(length '(a . b))", ConditionClass::TypeError),
    ] {
        let Err(error) = runtime.eval(source) else {
            panic!("expected an error from {source}");
        };
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(expected),
            "{source}"
        );
    }
}

#[test]
fn string_operators_reject_invalid_arguments() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(string= 8 "8")"#, ConditionClass::TypeError),
        (r#"(string= "😀" "😀" :end2 2)"#, ConditionClass::Error),
        (
            r#"(string= "a" "a" :start1 nil)"#,
            ConditionClass::TypeError,
        ),
        (
            r#"(string= "a" "a" :irrelevant 1)"#,
            ConditionClass::ProgramError,
        ),
        (
            r#"(string-upcase "abc" :start)"#,
            ConditionClass::ProgramError,
        ),
        (r#"(string-upcase "a" :end 2)"#, ConditionClass::Error),
        (
            r#"(string-upcase "abc" :start 2 :end 1)"#,
            ConditionClass::Error,
        ),
    ] {
        let Err(error) = runtime.eval(source) else {
            panic!("expected an error from {source}");
        };
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(expected),
            "{source}"
        );
    }
}
