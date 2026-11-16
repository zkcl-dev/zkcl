use std::error::Error;

use zkcl::{ConditionClass, EvalError, EvalErrorKind, Runtime};

#[test]
fn read_from_string_returns_the_index_after_one_object() {
    let runtime = Runtime::start().unwrap();
    for (source, value, position) in [
        (r#"(read-from-string " 1 3 5" t nil :start 2)"#, 3, 5),
        (
            r#"(read-from-string "零é 17 9" t nil :start 2 :end 6)"#,
            17,
            6,
        ),
        (
            r#"(read-from-string "零é 17 9" t nil :start 2 :end 6 :preserve-whitespace t)"#,
            17,
            5,
        ),
        (
            r#"(read-from-string "7 42" t nil :start 2 :start 0 :other 99 :allow-other-keys t)"#,
            42,
            4,
        ),
        (r#"(read-from-string "42" :start 8)"#, 42, 2),
        (r#"(funcall #'read-from-string "42  7")"#, 42, 3),
        (r#"(read-from-string "42)")"#, 42, 2),
    ] {
        let values = runtime.eval(source).unwrap();
        assert_eq!(values.len(), 2, "{source}");
        assert_eq!(values[0].as_fixnum(), Some(value), "{source}");
        assert_eq!(values[1].as_fixnum(), Some(position), "{source}");
    }

    let values = runtime.eval(r#"(read-from-string "\"λ\"  tail")"#).unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("λ"));
    assert_eq!(values[1].as_fixnum(), Some(4));

    let values = runtime
        .eval(r#"(read-from-string "(a b c)" t nil :end nil)"#)
        .unwrap();
    let (car, _) = runtime.cons_fields(&values[0]).unwrap();
    assert_eq!(runtime.symbol_name(&car).as_deref(), Some("A"));
    assert_eq!(values[1].as_fixnum(), Some(7));

    let values = runtime.eval(r#"(read-from-string "(a)  ")"#).unwrap();
    assert_eq!(values[1].as_fixnum(), Some(4));
    let values = runtime
        .eval(r#"(read-from-string "(a)  " t nil :preserve-whitespace t)"#)
        .unwrap();
    assert_eq!(values[1].as_fixnum(), Some(3));
}

#[test]
fn read_from_string_returns_the_eof_value_at_the_range_end() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(r#"(read-from-string "  " nil 'at-end)"#)
        .unwrap();
    assert_eq!(runtime.symbol_name(&values[0]).as_deref(), Some("AT-END"));
    assert_eq!(values[1].as_fixnum(), Some(2));
    let values = runtime
        .eval(r#"(read-from-string "abc" nil 99 :start 1 :end 1)"#)
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(99));
    assert_eq!(values[1].as_fixnum(), Some(1));
}

#[test]
fn read_from_string_reports_specific_conditions() {
    let runtime = Runtime::start().unwrap();
    for (source, class) in [
        (r#"(read-from-string "")"#, ConditionClass::EndOfFile),
        (
            r#"(read-from-string "(" nil 99)"#,
            ConditionClass::EndOfFile,
        ),
        (
            r#"(read-from-string "'" nil 99)"#,
            ConditionClass::EndOfFile,
        ),
        (
            r#"(read-from-string "\"broken" nil 99)"#,
            ConditionClass::EndOfFile,
        ),
        (
            r#"(read-from-string ")" nil 99)"#,
            ConditionClass::ReaderError,
        ),
        (r#"(read-from-string 42)"#, ConditionClass::TypeError),
        (
            r#"(read-from-string "a" t nil :start -1)"#,
            ConditionClass::TypeError,
        ),
        (
            r#"(read-from-string "a" t nil :unknown 7)"#,
            ConditionClass::ProgramError,
        ),
        (
            r#"(read-from-string "a" t nil :start)"#,
            ConditionClass::ProgramError,
        ),
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(class),
            "{source}: {error}"
        );
    }

    let values = runtime
        .eval(
            r#"(handler-case (read-from-string ")")
                 (reader-error (failure)
                   (values (typep failure 'parse-error)
                           (typep failure 'stream-error)
                           (typep failure 'end-of-file))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert!(values[2].is_nil());

    let values = runtime
        .eval(
            r#"(handler-case (read-from-string "(" nil)
                 (stream-error (failure) (typep failure 'end-of-file)))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn read_from_string_reports_reader_errors_in_bytes() {
    let runtime = Runtime::start().unwrap();
    let error = runtime
        .eval(r#"(read-from-string "ä)" nil nil :start 1)"#)
        .err()
        .unwrap();
    let nested = error.source().unwrap().downcast_ref::<EvalError>().unwrap();
    assert_eq!(nested.kind(), EvalErrorKind::Read);
    assert_eq!(nested.byte_range(), 2..3);
}
