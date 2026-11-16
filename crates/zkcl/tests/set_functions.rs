use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

fn check(runtime: &Runtime, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval_reprs(runtime, source), [*expected], "source: {source}");
    }
}

#[test]
fn union_uses_membership_to_skip_duplicates() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(union nil nil)", "NIL"),
            ("(union '(a b) '(b c))", "(B C A)"),
            ("(union '(a) '(a))", "(A)"),
            ("(union '(a a) nil)", "(A)"),
            ("(union nil '(a b c))", "(A B C)"),
            ("(nunion '(a b) '(b c))", "(B C A)"),
            ("(union '(1 2) '(2 3) :key #'1+)", "(2 3 1)"),
            (r#"(union '("A") '("a") :test #'string-equal)"#, "(\"a\")"),
            ("(union '(a b) '(c d) :test-not #'eq)", "(C D)"),
            (
                "(union '(a b) '(c d) :test #'eq :allow-other-keys t)",
                "(C D A B)",
            ),
        ],
    );
}

#[test]
fn adjoin_inserts_an_absent_member() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(adjoin 'a '(b c))", "(A B C)"),
            ("(adjoin 'a '(a b))", "(A B)"),
            ("(adjoin 1 '(1 2 3) :key #'1+)", "(1 2 3)"),
            ("(adjoin 5 '(1 2) :key #'1+)", "(5 1 2)"),
            ("(adjoin 'b '() :test #'eq)", "(B)"),
        ],
    );
}

#[test]
fn intersection_keeps_matching_members() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(intersection '(a b c) '(b c d))", "(B C)"),
            ("(intersection '(a a b) '(a))", "(A)"),
            ("(intersection nil '(a))", "NIL"),
            ("(nintersection '(a b c) '(b c d))", "(B C)"),
            ("(intersection '(1 2 3) '(2 3 4) :key #'1+)", "(2 3)"),
            (
                r#"(intersection '("a" "b") '("B") :test #'string-equal)"#,
                "(\"b\")",
            ),
        ],
    );
}

#[test]
fn set_difference_removes_matching_members() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(set-difference '(a b c) '(b))", "(A C)"),
            ("(set-difference '(a a b) '(b))", "(A A)"),
            ("(set-difference '(a b c d e) '(d a b e))", "(C)"),
            ("(set-difference nil '(a b))", "NIL"),
            ("(nset-difference '(a b c d e) '(d a b e))", "(C)"),
            ("(set-difference '(1 2 3) '(2) :key #'1+)", "(1 3)"),
            ("(set-difference '(a b) '(z) :test-not #'eq)", "NIL"),
        ],
    );
}

#[test]
fn set_exclusive_or_keeps_unmatched_members() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(set-exclusive-or nil nil)", "NIL"),
            ("(set-exclusive-or '(a b) '(b c))", "(A C)"),
            ("(set-exclusive-or '(a b c d e) '(d a b e))", "(C)"),
            ("(nset-exclusive-or '(a b c d e) '(d a b e))", "(C)"),
            ("(set-exclusive-or '(a a b) '(b))", "(A A)"),
            ("(set-exclusive-or '(1 2) '(2 3) :key #'1+)", "(1 3)"),
        ],
    );
}

#[test]
fn subsetp_requires_a_match_for_each_member() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(subsetp nil '(a b))", "T"),
            ("(subsetp '(a) '(a b))", "T"),
            ("(subsetp '(a z) '(a))", "NIL"),
            ("(subsetp '(1 2) '(2 3) :key #'1+)", "NIL"),
            ("(subsetp '(1 2) '(2 3) :test #'<)", "T"),
        ],
    );
}

#[test]
fn list_set_operations_leave_their_inputs_unchanged() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            (
                "(let ((x (list 1 2)) (y (list 2 3)))
                   (union x y)
                   (list x y))",
                "((1 2) (2 3))",
            ),
            (
                "(let ((x (list 1 2)) (y (list 2 3)))
                   (nunion x y)
                   (list x y))",
                "((1 2) (2 3))",
            ),
            (
                "(let ((x (list 1 2 3)) (y (list 2)))
                   (set-difference x y)
                   (list x y))",
                "((1 2 3) (2))",
            ),
            (
                "(let ((x (list 1 2)) (y (list 2 3)))
                   (intersection x y)
                   (list x y))",
                "((1 2) (2 3))",
            ),
            (
                "(let ((x (list 1 2)) (y (list 2 3)))
                   (set-exclusive-or x y)
                   (list x y))",
                "((1 2) (2 3))",
            ),
        ],
    );
}

#[test]
fn list_set_operations_survive_collection_in_callbacks() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-set-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    check(
        &runtime,
        &[
            (
                "(let ((x (list 1 2 3 4)) (y (list 3 4 5)))
                   (union x y :test (lambda (a b) (collect-set-heap) (= a b))))",
                "(3 4 5 1 2)",
            ),
            (
                "(intersection (list 1 2 3 4) (list 2 4)
                              :key (lambda (n) (collect-set-heap) n))",
                "(2 4)",
            ),
            (
                "(subsetp (list 2 3) (list 1 2 3)
                          :test (lambda (a b) (collect-set-heap) (= a b)))",
                "T",
            ),
        ],
    );
}

#[test]
fn list_set_operations_reject_invalid_arguments() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(union '(a))",
        "(subsetp '(a))",
        "(union '(1 2) '(2 3) :unknown t)",
        "(union 5 '(1))",
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report an error"
        );
    }
}
