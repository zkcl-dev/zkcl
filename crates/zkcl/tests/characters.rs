use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn character_literals_round_trip_in_printed_syntax() {
    let runtime = Runtime::start().unwrap();

    for (source, expected) in [
        (r"#\A", 'A'),
        (r"#\a", 'a'),
        (r"#\😀", '😀'),
        (r"#\)", ')'),
        (r"#\;", ';'),
        (r"#\\", '\\'),
        (r"#\ ", ' '),
        (r"#\Space", ' '),
        (r"#\NewLINE", '\n'),
        (r"#\N\ewline", '\n'),
        (r"#\N|ewline|", '\n'),
        (r"#\Tab", '\t'),
        (r"#\Rubout", '\x7f'),
        (r"#\U+0000", '\0'),
    ] {
        let value = runtime.eval(source).unwrap().pop().unwrap();
        assert_eq!(value.as_character(), Some(expected), "{source}");
        let host = runtime.character(expected);
        assert!(host.same_identity(&value), "{source}");
        let printed = runtime.repr(&value).unwrap();
        let restored = runtime.eval(&printed).unwrap().pop().unwrap();
        assert!(
            restored.same_identity(&value),
            "{source} printed as {printed:?}"
        );
    }

    // Standard-reader lexical boundaries include delimiter characters.
    let list = runtime.eval(r"'(#\) #\( #\; #\' #\ )").unwrap();
    assert_eq!(runtime.repr(&list[0]).unwrap(), r"(#\) #\( #\; #\' #\ )");
}

#[test]
fn character_operators_use_unicode_scalar_values() {
    let runtime = Runtime::start().unwrap();
    let truthy = |source| !runtime.eval(source).unwrap()[0].is_nil();
    assert!(truthy(r#"(eql #\😀 (char "a😀b" 1))"#));
    assert!(truthy(r#"(char= #\A (schar "😀A" 1) (character 'a))"#));
    assert!(truthy(r#"(char= #\a (character '|a|) (character "a"))"#));
    assert!(truthy(
        r"(char= #\A (char-upcase #\a) (code-char (char-int #\A)))"
    ));
    assert!(truthy(r"(char= #\a (char-downcase #\A))"));
    assert!(truthy(r"(char-equal #\A #\a #\A)"));
    assert!(!truthy(r"(eql #\A #\a)"));
    assert!(!truthy(r"(equal #\A #\a)"));
    assert!(truthy(r"(equalp '(#\É . #\A) '(#\é . #\a))"));
    assert!(truthy(r"(char/= #\a #\A #\b)"));
    assert!(!truthy(r"(char-not-equal #\a #\b #\A)"));
    assert!(truthy(r"(char< #\0 #\A #\B #\a)"));
    assert!(truthy(r"(char-greaterp #\z #\B #\a)"));
    assert!(truthy(r"(char-not-greaterp #\A #\a #\b)"));
    assert!(truthy(r"(char-not-lessp #\B #\b #\a)"));
    assert!(truthy(r"(and (characterp #\é) (not (typep #\é 'symbol)))"));
    assert!(truthy(
        r"(and (typep #\é 'character) (typep #\é 'base-char)
               (not (typep #\é 'extended-char))
               (not (typep #\é 'standard-char))
               (typep #\Newline 'standard-char))",
    ));
    assert!(truthy(
        r"(and (standard-char-p #\Space) (graphic-char-p #\Space)
                         (standard-char-p #\Newline) (not (graphic-char-p #\Newline)))"
    ));
    assert!(truthy(
        r"(and (alpha-char-p #\é) (alphanumericp #\8) (both-case-p #\é)
                         (lower-case-p #\é) (upper-case-p #\É))"
    ));
    assert!(!truthy(r"(alphanumericp #\²)"));
    assert!(truthy(
        r"(and (char= (digit-char 10 16) #\A)
                         (= (digit-char-p #\a 16) 10)
                         (= (digit-char-p #\0) 0)
                         (not (digit-char 10))
                         (not (digit-char-p #\a)))"
    ));
    assert!(truthy(
        r#"(char= (name-char "newline") (name-char 'linefeed) #\Newline)"#
    ));
    assert!(truthy(
        r#"(char= (name-char "nul") (name-char "null") (code-char 0) #\Nul #\Null)"#
    ));
    assert!(truthy(r#"(string= (char-name #\Null) "Nul")"#));
    assert!(truthy(
        r#"(char= (name-char "REPLACEMENT_CHARACTER") (code-char #xFFFD) #\replacement_character)"#
    ));
    assert!(truthy(
        r#"(and (char= #\No-break_space (name-char "nO-bREAk_sPAcE") (code-char #xA0))
                 (string= (char-name #\No-break_space) "U+00A0")
                 (char= (read-from-string (prin1-to-string #\No-break_space)) #\No-break_space)
                 (char= #\Line_Separator (name-char 'line_separator) (code-char #x2028))
                 (char= #\Paragraph_Separator (name-char "paragraph_separator") (code-char #x2029))
                 (char= #\Narrow_No-break_space (code-char #x202F))
                 (null (name-char "NO_BREAK_SPACE"))
                 (null (name-char "NO-BREAK SPACE")))"#
    ));
    assert!(truthy(
        r#"(loop for name in '("nul" "soh" "stx" "etx" "eot" "enq" "ack" "bel"
                              "bs" "ht" "lf" "vt" "ff" "cr" "so" "si"
                              "dle" "dc1" "dc2" "dc3" "dc4" "nak" "syn" "etb"
                              "can" "em" "sub" "esc" "fs" "gs" "rs" "us" "del")
                 for code in (append (loop for n below 32 collect n) '(127))
                 for character = (code-char code)
                 always (and (eql (name-char name) character)
                             (eql (name-char (char-name character)) character)
                             (eql (read-from-string (prin1-to-string character)) character)))"#
    ));
    assert!(truthy(
        r#"(and (string= (char-name #\Bel) "U+0007")
                 (string= (char-name #\Vt) "U+000B")
                 (string= (char-name #\Ht) "Tab")
                 (string= (char-name #\Del) "Rubout")
                 (eql #\Line_Tabulation #\Vt)
                 (eql #\Start_Of_Heading #\Soh))"#
    ));
    assert!(truthy(
        r##"(with-input-from-string (input "#\\bEl(#\\V|t| #\\S\\i)")
              (and (eql (read input) (code-char 7))
                   (equal (mapcar #'char-code (read input)) '(11 15))
                   (eq (read input nil :eof) :eof)))"##
    ));
    assert!(!truthy(r#"(name-char "unregistered-name")"#));
    assert!(truthy(r"(char= (name-char (char-name #\😀)) #\😀)"));
    assert!(truthy(r#"(string= (char-name #\Space) "Space")"#));
    assert!(truthy(
        r#"(and (string= (string #\😀) "😀") (string-equal #\É #\é))"#
    ));
    assert!(truthy(
        r#"(and (typep "😀" '(string 1))
                         (typep "" '(simple-base-string *))
                         (not (typep "😀" '(string 2))))"#
    ));
    assert!(truthy(r#"(string= (princ-to-string #\a) "a")"#));
    assert!(truthy(r##"(string= (prin1-to-string #\Space) "#\\ ")"##));
    assert_eq!(
        runtime.eval("char-code-limit").unwrap()[0].as_fixnum(),
        Some(0x11_0000)
    );
    assert_eq!(
        runtime.eval(r"(char-code #\😀)").unwrap()[0].as_fixnum(),
        Some(0x1_f600)
    );
    assert!(!truthy("(code-char 55296)")); // Surrogates have no scalar character.
    assert!(runtime.eval("(setq char-code-limit 0)").is_err());

    let generated = runtime
        .eval(r"(progn (defmacro character-sample () #\😀) (character-sample))")
        .unwrap()
        .pop()
        .unwrap();
    runtime.collect();
    assert_eq!(generated.as_character(), Some('😀'));
}

#[test]
fn character_designators_distinguish_read_errors_from_type_errors() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        runtime.eval(r"#+absent #\unknown-character 42").unwrap()[0].as_fixnum(),
        Some(42)
    );
    for source in [
        r"#2\A",
        r"#\unknown-character",
        r"#\No:Package",
        r"#\U+D800",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(error.kind(), EvalErrorKind::Read, "{source}");
        assert_eq!(error.byte_range(), 0..source.len(), "{source}");
    }
    for source in [
        "(character 6)",
        r#"(character "too long")"#,
        r#" (char "a" -1)"#,
        "(char= 5)",
        "(code-char char-code-limit)",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}"
        );
    }
    runtime
        .load_str(r"(defpackage #\Q (:use :cl)) (in-package #\Q)")
        .unwrap();
    let package = runtime
        .eval(r#"(eq (find-package #\Q) (find-package "Q"))"#)
        .unwrap();
    assert!(!package[0].is_nil());
}
