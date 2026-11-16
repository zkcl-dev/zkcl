use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

fn check(runtime: &Runtime, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval_reprs(runtime, source), [*expected], "source: {source}");
    }
}

#[test]
fn element_search_uses_the_requested_match_criterion() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(find 3 '(1 2 3 4))", "3"),
            ("(find 9 '(1 2 3))", "NIL"),
            ("(find nil '())", "NIL"),
            ("(position 'c '(a b c d))", "2"),
            ("(position 'z '(a b c))", "NIL"),
            ("(count 1 '(1 2 1 3))", "2"),
            ("(count 'x '(a b))", "0"),
            ("(find 2 '(1 2 3 4 2) :from-end t)", "2"),
            ("(position 2 '(1 2 3 2) :from-end t)", "3"),
            ("(position 2 '(1 2 3 2))", "1"),
            ("(count 2 '(1 2 3 2) :start 2)", "1"),
            ("(count 2 '(2 2 2 2) :start 1 :end 3)", "2"),
            ("(position 'c '(a b c d) :start 2)", "2"),
            ("(position 'b '(a b c d) :start 2)", "NIL"),
            ("(find 'x '((a 1) (b x)) :key #'cadr)", "(B X)"),
            ("(position 'x '((a 1) (b x)) :key #'cadr)", "1"),
            ("(find 'y '((a 1) (b x)) :key #'cadr)", "NIL"),
            ("(find 1 '((a 1) (b 2)) :key #'cadr)", "(A 1)"),
            ("(position 2 '((a 1) (b 2)) :key #'cadr)", "1"),
            ("(count 2 '((a 2) (b 2)) :key #'cadr)", "2"),
            ("(position 2 '(1 2 3) :test #'>)", "0"),
            ("(find 5 '(1 2 3) :test-not #'=)", "1"),
            ("(find \"b\" '(\"a\" \"b\") :test #'string=)", "\"b\""),
        ],
    );
}

#[test]
fn predicate_search_can_match_the_complement() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            ("(find-if #'evenp '(1 2 3))", "2"),
            ("(find-if #'evenp '(1 3 5))", "NIL"),
            ("(find-if-not #'evenp '(2 4 5))", "5"),
            ("(position-if #'evenp '(1 3 4 6))", "2"),
            ("(position-if-not #'evenp '(2 3 4))", "1"),
            ("(count-if #'evenp '(1 2 4 5))", "2"),
            ("(count-if-not #'evenp '(1 2 4 5))", "2"),
            ("(find-if #'evenp '(1 2 3 4) :from-end t)", "4"),
            ("(position-if #'evenp '(2 1 4 3) :from-end t)", "2"),
            ("(find-if #'plusp '((a 1) (b x)) :key #'cadr)", "(A 1)"),
            ("(count-if #'plusp '((a 1) (b -2)) :key #'cadr)", "1"),
        ],
    );
}

#[test]
fn element_search_traverses_specialized_vectors() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            (r#"(find #\c "abcd")"#, "#\\c"),
            (r#"(position #\b "abc")"#, "1"),
            (r#"(position #\b "abcab" :from-end t)"#, "4"),
            (r#"(count #\a "banana")"#, "3"),
            ("(find 2 #(1 2 3))", "2"),
            ("(position 3 #(1 2 3))", "2"),
            ("(count 1 #*1011)", "3"),
            ("(find 1 #*0010 :from-end t)", "1"),
        ],
    );
}

#[test]
fn string_search_uses_character_indices_and_logical_array_bounds() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        &[
            (r#"(position #\猫 "é😀a猫é😀" :start 2 :end 5)"#, "3"),
            (
                r#"(position #\é "é😀a猫é😀" :from-end t :start 1 :end 5)"#,
                "4",
            ),
            (r#"(position #\😀 "é😀" :start 1 :end 1)"#, "NIL"),
            (r#"(count #\猫 "猫a猫得猫" :start 1 :end 4 :key nil)"#, "1"),
            (r#"(find 1 "猫")"#, "NIL"),
            (r#"(count 1 "")"#, "0"),
            (
                r#"(find #\猫 "猫" :unknown t :allow-other-keys t)"#,
                "#\\猫",
            ),
            (
                r#"(let* ((base (copy-seq "纹αa猫a😀Z"))
                          (slice (make-array 5 :element-type 'character
                                               :displaced-to base :displaced-index-offset 1
                                               :fill-pointer 3)))
                     (list (position #\猫 slice) (position #\a slice :from-end t)
                           (count #\a slice) (position #\😀 slice)))"#,
                "(2 1 1 NIL)",
            ),
            (
                r#"(let ((text (copy-seq "abcd")))
                     (setf (char text 2) #\雪)
                     (list (position #\雪 text) (find #\c text) (count #\d text)))"#,
                "(2 NIL 1)",
            ),
            (
                r#"(let ((calls 0))
                     (list (position #\A "éaA" :test (lambda (a b) (incf calls) (char-equal a b)))
                           calls))"#,
                "(1 2)",
            ),
        ],
    );
    for source in [
        r#"(position #\x "é" :end 2)"#,
        r#"(count #\x "é" :bogus t)"#,
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report an error"
        );
    }
}

#[test]
fn searches_survive_collection_during_callbacks() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-search-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    check(
        &runtime,
        &[
            (
                "(find 3 '(1 2 3 4 5)
                   :test (lambda (a b) (collect-search-heap) (= a b)))",
                "3",
            ),
            (
                "(position 4 '((a 1) (b 2) (c 3) (d 4))
                   :from-end t
                   :key (lambda (entry) (collect-search-heap) (cadr entry)))",
                "3",
            ),
            (
                "(count-if (lambda (n) (collect-search-heap) (evenp n))
                           '(1 2 3 4 6))",
                "3",
            ),
            (
                "(let ((values (list 1 2 3 4)))
                   (find-if (lambda (n) (collect-search-heap) (= n 3)) values))",
                "3",
            ),
        ],
    );
}

#[test]
fn element_search_rejects_invalid_arguments() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(find 1 5)",
        "(position 1 '(1 2) :start 5)",
        "(find-if 3 '(1 2))",
        "(count 1 '(1 2) :start 2 :end 1)",
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report an error"
        );
    }
}
