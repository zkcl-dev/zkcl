use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

fn check(runtime: &Runtime, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval_reprs(runtime, source), [*expected], "source: {source}");
    }
}

#[test]
fn numeric_subtypes_follow_the_number_tower() {
    let runtime = Runtime::start().unwrap();
    let subtypep =
        |left: &str, right: &str| format!("(multiple-value-list (subtypep '{left} '{right}))");
    check(
        &runtime,
        &[
            (&subtypep("integer", "integer"), "(T T)"),
            (&subtypep("integer", "rational"), "(T T)"),
            (&subtypep("rational", "integer"), "(NIL T)"),
            (&subtypep("integer", "number"), "(T T)"),
            (&subtypep("fixnum", "integer"), "(T T)"),
            (&subtypep("bignum", "fixnum"), "(NIL T)"),
            (&subtypep("ratio", "rational"), "(T T)"),
            (&subtypep("single-float", "float"), "(T T)"),
            (&subtypep("single-float", "double-float"), "(NIL T)"),
            (&subtypep("complex", "real"), "(NIL T)"),
            (&subtypep("number", "complex"), "(NIL T)"),
        ],
    );
}

#[test]
fn named_subtypes_follow_builtin_type_relationships() {
    let runtime = Runtime::start().unwrap();
    let subtypep =
        |left: &str, right: &str| format!("(multiple-value-list (subtypep '{left} '{right}))");
    check(
        &runtime,
        &[
            (&subtypep("string", "vector"), "(T T)"),
            (&subtypep("string", "sequence"), "(T T)"),
            (&subtypep("simple-vector", "array"), "(T T)"),
            (&subtypep("simple-array", "array"), "(T T)"),
            (&subtypep("keyword", "symbol"), "(T T)"),
            (&subtypep("null", "list"), "(T T)"),
            (&subtypep("cons", "list"), "(T T)"),
            (&subtypep("compiled-function", "function"), "(T T)"),
        ],
    );
}

#[test]
fn subtypep_reports_a_certain_negative_for_builtin_types() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(multiple-value-list (subtypep 'list 'cons))", "(NIL T)"),
            ("(multiple-value-list (subtypep 't 'integer))", "(NIL T)"),
            (
                "(multiple-value-list (subtypep 'character 'integer))",
                "(NIL T)",
            ),
            ("(multiple-value-list (subtypep 'character 'atom))", "(T T)"),
            (
                "(multiple-value-list (subtypep '(integer 1 5) '(integer 1 10)))",
                "(T T)",
            ),
        ],
    );
}

#[test]
fn integer_intervals_are_subtypes_of_their_numeric_bounds() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            (
                "(multiple-value-list (subtypep '(integer 1 10) 'integer))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep '(integer 1 10) 'rational))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep 'integer '(integer)))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep 'integer '(integer * *)))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep '(integer 1 10) '(integer 1 10)))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep '(integer 1 10) '(rational)))",
                "(T T)",
            ),
            (
                "(multiple-value-list (subtypep '(integer 1 10) 'string))",
                "(NIL T)",
            ),
        ],
    );
}

#[test]
fn impossible_array_rank_denotes_the_empty_type() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[(
            "(multiple-value-list (subtypep '(array * 128) nil))",
            "(T T)",
        )],
    );
}

#[test]
fn unrepresentable_byte_width_leaves_subtypep_undecided() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[(
            "(multiple-value-list (subtypep '(unsigned-byte 1000000000000000000) 'integer))",
            "(NIL NIL)",
        )],
    );
}

#[test]
fn subtypep_shortcuts_when_target_is_t() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[(
            "(multiple-value-list (subtypep 'nuclear-fusion 't))",
            "(T T)",
        )],
    );
}
