use zkcl::{EvalErrorKind, Runtime};

fn eval_fixnums(runtime: &Runtime, source: &str) -> Vec<isize> {
    runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"))
        .iter()
        .map(|value| value.as_fixnum().unwrap())
        .collect()
}

#[test]
fn reader_conditionals_skip_exactly_one_object() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(values #+(and common-lisp zkcl (not (or))) 1 #-zkcl 0 #+(or) 0 2)"
        ),
        [1, 2]
    );
    assert_eq!(
        eval_fixnums(&runtime, "(values #+(or) #+(and) 7 1 #+(or) #+(or) 8 9 2)"),
        [1, 2]
    );
    assert_eq!(eval_fixnums(&runtime, "'#+(or) 9 42 #+(or) 99"), [42]);
    assert_eq!(
        eval_fixnums(&runtime, "(cdr '(1 . #+(or) 99 42 #+(or) :extra))"),
        [42]
    );
    assert_eq!(
        eval_fixnums(&runtime, "(cdr '(1 #+(or) #. (unknown-package:bad) . 42))"),
        [42]
    );
    assert_eq!(
        eval_fixnums(
            &runtime,
            "(values
               #+(or) #(... #\\unknown #:bad:package #2r2 (a . b c)
                            3489728973592874198735987193 #r)
               #+(or) #3= :ignored
               #+(or) #3#
               #+(or) #. (error \"not evaluated\")
               42)"
        ),
        [42]
    );
}

#[test]
fn reader_conditionals_consult_the_current_feature_binding() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("eval-reader-input", |runtime, arguments| {
            let source = runtime.string(&arguments[0]).unwrap();
            runtime.eval(&source).map_err(Into::into)
        })
        .unwrap();
    assert_eq!(
        eval_fixnums(
            &runtime,
            r#"(let ((*features* (list :temporary-feature 'cl-user::user-feature :|MixedCase|)))
                 (eval-reader-input "(values #+temporary-feature 1 #+cl-user::user-feature 2
                                       #+user-feature 99 #+|MixedCase| 3 #+mixedcase 99
                                       #-zkcl 4)"))"#
        ),
        [1, 2, 3, 4]
    );
    assert_eq!(
        eval_fixnums(&runtime, "(values #+temporary-feature 99 #+zkcl 42)"),
        [42]
    );
    let suppressed = runtime
        .eval(r##"(let ((*read-suppress* t)) (eval-reader-input "#. (unknown-package:payload)"))"##)
        .unwrap();
    assert!(suppressed[0].is_nil());

    runtime
        .load_str(
            "(defvar *original-features* *features*)
             (setq *features* (list :loaded-feature))
             #-loaded-feature (error \"should be skipped\")
             #+loaded-feature (setq feature-reader-answer 42)
             (setq *features* *original-features*)
             #+loaded-feature (error \"should also be skipped\")",
        )
        .unwrap();
    assert_eq!(eval_fixnums(&runtime, "feature-reader-answer"), [42]);
}

#[test]
fn reader_conditionals_signal_errors_for_malformed_input() {
    let runtime = Runtime::start().unwrap();
    for malformed in [
        "#+(not) 42",
        "#+(wrong 3) 42",
        "#+3 42",
        "#+unknown-package:feature 42",
        "#+(and) unknown-package:payload",
        "(1 #+(and) . 2)",
        "(1 . 42 #+(and) )",
        "(values 1 #+(or) #< 2)",
        "(values 1 #+(or) ')",
        "(values 1 #+(or) #\\",
        "\"unterminated",
        "#+(or)",
        "#+(and)",
        "#+(or) (1 2",
    ] {
        let error = runtime.eval(malformed).err().expect(malformed);
        assert_eq!(error.kind(), EvalErrorKind::Read, "{malformed}");
    }
}

#[test]
fn reader_conditionals_limit_nested_skips() {
    let runtime = Runtime::start().unwrap();
    let deeply_nested = format!("{}42", "#+(or) ".repeat(300));
    let error = runtime.eval(&deeply_nested).err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Read);
    assert!(error.message().contains("nesting"));
    let many_enabled_prefixes = format!("{}42", "#+(and) ".repeat(512));
    assert_eq!(eval_fixnums(&runtime, &many_enabled_prefixes), [42]);
}
