#![cfg(feature = "analysis")]

use std::num::NonZeroUsize;

use zkcl_syntax::{
    FixApplicability, LintConfig, LintDiagnostic, LintFix, LintPreset, SourceWorkspace, TextEdit,
    apply_fixes, syntax::TextRange,
};

fn lint(source: &str, preset: LintPreset) -> Vec<LintDiagnostic> {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("test.lisp", source);
    workspace.lint_document("test.lisp", &LintConfig::from_preset(preset))
}

fn span(source: &str, range: TextRange) -> &str {
    &source[usize::from(range.start())..usize::from(range.end())]
}

fn spans<'a>(source: &'a str, diagnostics: &[LintDiagnostic], rule: &str) -> Vec<&'a str> {
    diagnostics
        .iter()
        .filter(|diag| diag.rule_id == rule)
        .map(|diag| span(source, diag.range))
        .collect()
}

#[test]
fn list_emptiness_uses_the_type_of_the_actual_lexical_binding() {
    let source = r#"
(defun lists (xs arg)
  (declare (type list xs))
  (plusp (length xs))
  (eql (length xs) 0.0)
  (let ((xs arg)) (plusp (length xs)))
  (locally (declare (list arg)) (= 0 (length arg))))
(defun vectors (vec text seq)
  (declare (vector vec) (type string text))
  (plusp (length vec))
  (plusp (length text))
  (plusp (length seq))
  (list (zerop (length #(1))) (zerop (length "a"))))
(defun asserted (items)
  (when (zerop (length (the list items))) :empty)
  (when (< 0 (length '(a b))) :nonempty))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "perf/length-zero-comparison"),
        [
            "(plusp (length xs))",
            "(= 0 (length arg))",
            "(zerop (length (the list items)))",
            "(< 0 (length '(a b)))"
        ]
    );
}

#[test]
fn double_negation_keeps_boolean_normalization_in_value_positions() {
    let source = r#"
(defun normalized (value) (not (null value)))
(defun returned (value) (cond ((not (null value)))))
(defun used-as-test (value) (if (not (null value)) :true :false))
(defun already-boolean (value) (not (null (null value))))
(defun asserted-boolean (value) (if (the boolean (not (null value))) :true :false))
(defun thereis (items) (loop for x in items thereis (not (null x))))
(defun collect-it (items) (loop for x in items when (not (null x)) collect it))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "complexity/double-negation"),
        ["(not (null value))", "(not (null (null value)))"]
    );
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 2, "{diagnostics:?}");
    assert!(fixed.contains("(if value :true :false)"), "{fixed}");
    assert!(fixed.contains("(defun already-boolean (value) (null value))"));
    assert!(fixed.contains("(defun normalized (value) (not (null value)))"));
}

#[test]
fn string_comparison_fixes_account_for_mutation_and_mismatch_indices() {
    let source = r#"
(defun changing (s)
  (string= (string-upcase s)
           (string-upcase (progn (setf (char s 0) #\b) "a"))))
(defun destructive (s) (string= (nstring-upcase s) "A"))
(defun mismatch () (string/= (subseq "xa" 1) "b"))
(defun equality () (string= (subseq "xa" 1) "a"))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let fixes: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.rule_id,
                "perf/string-equal-case-fold" | "perf/string-comparison-subseq"
            )
        })
        .map(|diagnostic| diagnostic.fix.as_ref().map(|fix| fix.applicability))
        .collect();
    assert_eq!(
        fixes,
        [
            Some(FixApplicability::MaybeIncorrect),
            None,
            Some(FixApplicability::MaybeIncorrect)
        ]
    );
    assert_eq!(
        apply_fixes(source, &diagnostics, false),
        (source.to_owned(), 0)
    );
}

#[test]
fn keeps_local_references_possible_inside_opaque_macro_operands() {
    let source = r#"
(in-package :cl-user)
(defmacro opaque (form) (identity form))
(defun caller (outer)
  (flet ((possible () 41) (unused () 42))
    (declare (ignorable outer))
    (opaque (possible))))
"#;
    let diagnostics = lint(source, LintPreset::Strict);
    assert_eq!(
        spans(source, &diagnostics, "style/unused-local-function"),
        ["unused"]
    );
    assert!(spans(source, &diagnostics, "correctness/invalid-declaration").is_empty());
}

#[test]
fn spelling_hints_leave_known_and_feature_guarded_redefinitions_alone() {
    let source = r#"
(in-package :cl-user)
(defun function-p (object) (functionp object))
(defun function-p (object) (typep object 'function))
#+implementation-feature (defstruct (token (:predicate type-p)) payload)
(function-p nil)
(type-p nil)
(zeropx 0)
"#;
    let diagnostics = lint(source, LintPreset::Strict);
    assert_eq!(
        spans(
            source,
            &diagnostics,
            "suspicious/likely-misspelled-operator"
        ),
        ["zeropx"]
    );
}

#[test]
fn free_ignore_applies_to_reads_in_its_body_only() {
    let source = r#"
(defun outside (value)
  (let ((copy value))
    (declare (ignore value))
    copy)
  value)
(defun inside (value)
  (locally (declare (ignore value)) (print value)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let uses: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.rule_id == "suspicious/used-ignored-binding")
        .collect();
    assert_eq!(uses.len(), 1, "{diagnostics:?}");
    assert_eq!(
        usize::from(uses[0].range.start()),
        source.rfind("value").unwrap()
    );
    assert_eq!(span(source, uses[0].related[0].range), "value");
}

#[test]
fn apply_on_fresh_arguments_preserves_written_order_and_comments() {
    let source = r#"
(defun submit (send stream read-key read-value groups)
  (apply send stream (list (funcall read-key) ; preserve this argument comment
                           (funcall read-value)))
  (apply send (list))
  (apply #'+ '(1 2))
  (apply #'append groups)
  (flet ((list (x) (cons x nil)))
    (apply send (list stream))))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "perf/apply-call-arguments-limit"),
        ["(apply #'append groups)"]
    );
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 2, "{diagnostics:?}");
    assert!(
        fixed.contains("(funcall send stream (funcall read-key) ; preserve this argument comment")
    );
    assert!(fixed.contains("(funcall send)"));
    assert!(fixed.contains("(apply send (list stream))"));
    assert_eq!(
        apply_fixes(&fixed, &lint(&fixed, LintPreset::Default), false).1,
        0
    );
}

#[test]
fn value_list_fixes_preserve_cardinality_stage_and_expression_boundaries() {
    let source = r#"
(defun present-p (key table)
  (second (multiple-value-list (gethash key table))))
(defun all (x y)
  (values-list (multiple-value-list (values x y))))
(defun adjacent (x\( y z xs)
  (list x\((values-list(multiple-value-list y))z
        (nth 0(multiple-value-list y))
        (length(reverse xs))))
(defun keep-docstring (x)
  (values-list (multiple-value-list "ordinary expression"))
  x)
(defun keep-tagbody-evaluation (x)
  (tagbody (values-list (multiple-value-list x))))
(values-list (multiple-value-list (eval-when (:compile-toplevel) :compiled)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 7, "{diagnostics:?}");
    assert!(fixed.contains("(nth-value 1 (gethash key table))"));
    assert!(fixed.contains("(defun all (x y)\n  (values x y))"));
    assert!(fixed.contains("(list x\\( y z\n        (nth-value 0 y)\n        (length xs))"));
    assert!(fixed.contains("(values \"ordinary expression\")"));
    assert!(fixed.contains("(tagbody (multiple-value-prog1 x))"));
    assert!(fixed.contains("(values-list (multiple-value-list (eval-when"));
    assert_eq!(
        apply_fixes(&fixed, &lint(&fixed, LintPreset::Default), false).1,
        0
    );
}

#[test]
fn value_list_fusion_respects_index_limits_and_uncertain_package_changes() {
    let source = r#"
(defun dynamic (index)
  (list (nth index (multiple-value-list (values :a :b)))
        (nth 42 (multiple-value-list (values :a :b)))))
#+unknown (in-package #:another-package)
(defun ambiguous (f)
  (apply f (list 1))
  (first (multiple-value-list (values :a :b))))
(in-package #:cl-user)
(defun known (f)
  (apply f (list 1)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let fixes: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.rule_id == "perf/materialized-multiple-values")
        .filter_map(|diag| diag.fix.as_ref())
        .collect();
    assert_eq!(fixes.len(), 2, "{diagnostics:?}");
    assert!(
        fixes
            .iter()
            .all(|fix| fix.applicability == FixApplicability::MaybeIncorrect)
    );
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 1, "{diagnostics:?}");
    assert_eq!(
        fixed,
        source.replace("  (apply f (list 1)))", "  (funcall f 1))")
    );
}

#[test]
fn place_positions_only_lint_value_operands() {
    let source = r#"
(defun places (x y xs)
  (setf (values (first (multiple-value-list x))) y)
  (push x (nth 0 (multiple-value-list y)))
  (check-type (first (multiple-value-list y)) list)
  (rotatef y (apply #'aref xs (list 1)))
  (incf (nth (nth 0 (multiple-value-list x)) xs)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "perf/materialized-multiple-values"),
        ["(multiple-value-list x)"]
    );
    assert!(spans(source, &diagnostics, "perf/apply-fresh-list").is_empty());
    assert_eq!(
        apply_fixes(source, &diagnostics, false).0,
        source.replace("(nth 0 (multiple-value-list x))", "(nth-value 0 x)")
    );
}

#[test]
fn indexed_list_loop_uses_binding_identity_and_offers_a_reviewable_traversal() {
    let source = r#"
(defun send-rows (rows send)
  (dotimes (i (length rows))
    (let ((row (nth i rows)))
      (funcall send row))))
(defun send-with-index (rows send)
  (dotimes (i (length rows))
    (let ((row (nth i rows)))
      (funcall send row i))))
(defun shadows (rows other send)
  (dotimes (i (length rows))
    (let ((i 0)) (funcall send (nth i rows)))
    (let ((rows other)) (funcall send (nth i rows)))))
(defun delayed (rows send)
  (dotimes (i (length rows))
    (funcall send (lambda () (nth i rows)))))
(defun once (rows)
  (dotimes (i (length rows) (nth i rows))))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let indexed: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.rule_id == "perf/indexed-list-loop")
        .collect();
    assert_eq!(indexed.len(), 2, "{diagnostics:?}");
    assert_eq!(span(source, indexed[0].related[0].range), "(length rows)");
    assert_eq!(
        indexed[0].fix.as_ref().map(|fix| fix.applicability),
        Some(FixApplicability::MaybeIncorrect)
    );
    assert!(indexed[1].fix.is_none());
    assert_eq!(apply_fixes(source, &diagnostics, false).1, 0);
    let (fixed, count) = apply_fixes(source, &diagnostics, true);
    assert_eq!(count, 1, "{fixed}");
    assert!(fixed.contains("(dolist (row rows)\n    (funcall send row))"));
}

#[test]
fn loop_mutation_diagnostics_target_the_iteration_binding() {
    let source = r#"
(defun counters (xs run)
  (dolist (x xs (setq x :done))
    (let ((x 0)) (incf x) (funcall run x))
    (funcall run (lambda () (setq x :later)))
    (setq x :mutated)
    (funcall run x)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let calls: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "suspicious/mutated-loop-variable")
        .collect();
    assert_eq!(calls.len(), 1, "{diagnostics:?}");
    let after_mutation = &source[usize::from(calls[0].range.end())..];
    assert!(after_mutation.starts_with(" :mutated"));
}

#[test]
fn sequence_fixes_keep_reads_and_callbacks_without_building_unused_results() {
    let source = r#"
(defun stats (items)
  (list (length (remove-if-not #'symbolp ; only named items
                               items))
        (first (remove-if-not #'symbolp (the list items)))
        (length (reverse (copy-seq items)))))
(defun notify (items)
  (map 'vector #'print items)
  (maplist #'print items)
  (map 'string #'identity "ok")
  (map 'list #'print items))
(defun length-property (items)
  (= (length items) (length (reverse items))))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let (fixed, num_fixes) = apply_fixes(source, &diagnostics, false);
    assert_eq!(num_fixes, 5, "{fixed}");
    assert!(fixed.contains("(count-if #'symbolp ; only named items"));
    assert!(fixed.contains("(find-if #'symbolp (the list items))"));
    assert!(fixed.contains("(length items)"));
    assert!(fixed.contains("(map nil #'print items)"));
    assert!(fixed.contains("(mapl #'print items)"));
    assert!(fixed.contains("(map 'string #'identity \"ok\")"));
    assert!(fixed.contains("(map 'list #'print items)"));
    assert!(fixed.contains("(= (length items) (length (reverse items)))"));
    assert_eq!(
        apply_fixes(&fixed, &lint(&fixed, LintPreset::Default), false).1,
        0
    );
}

#[test]
fn sequence_fixes_resolve_both_operator_bindings_and_replacement_names() {
    let source = r#"
(defpackage #:example (:use #:cl) (:shadow #:count-if #:mapc))
(in-package #:example)
(defun report (items predicate)
  (mapcar #'print items)
  (list (length (remove-if-not #'symbolp items))
        (length (remove-if-not predicate items))
        (first (remove-if-not #'symbolp items))
        (first (remove-if-not #'symbolp (the list items) :count 1))
        (flet ((reverse (items) (cons :generated items)))
          (length (reverse items)))
        (flet ((symbolp (item) (print item)))
          (length (remove-if-not #'symbolp items)))))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let review_fixes: Vec<_> = diagnostics
        .iter()
        .filter_map(|diag| diag.fix.as_ref())
        .filter(|fix| fix.applicability == FixApplicability::MaybeIncorrect)
        .collect();
    assert_eq!(review_fixes.len(), 3, "{diagnostics:?}");
    let (fixed, num_fixes) = apply_fixes(source, &diagnostics, false);
    assert_eq!(num_fixes, 2, "{fixed}");
    assert!(fixed.contains("(common-lisp:mapc #'print items)"));
    assert!(fixed.contains("(common-lisp:count-if #'symbolp items)"));
    assert!(fixed.contains("(length (reverse items))"));
}

#[test]
fn if_chain_fix_keeps_branch_order_and_implicit_nil() {
    let source = r#"
(defun response (a b c)
  (if a (values :a 1)
      (if b (values :b 2)
          (if c (values :c 3)))))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let (fixed, num_fixes) = apply_fixes(source, &diagnostics, false);
    assert_eq!(num_fixes, 1, "{diagnostics:?}");
    assert_eq!(
        fixed,
        r#"
(defun response (a b c)
  (cond (a (values :a 1))
        (b (values :b 2))
        (c (values :c 3))))
"#
    );
    assert!(lint(&fixed, LintPreset::Default).is_empty());
}

#[test]
fn append_in_loop_offers_a_reviewable_collect_rewrite_for_returned_accumulators() {
    let source = r#"
(defun results (records)
  (let ((out nil))
    (dolist (record records out)
      (setf out (append out (list (normalize record)))))))
(defun uses-prefix (records)
  (let (out)
    (dolist (record records)
      (setf out (append out (list (normalize record out)))))
    out))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let lints: Vec<_> = diagnostics
        .iter()
        .filter(|diag| diag.rule_id == "perf/append-in-loop")
        .collect();
    assert_eq!(lints.len(), 2, "{diagnostics:?}");
    assert_eq!(
        lints[0].fix.as_ref().map(|fix| fix.applicability),
        Some(FixApplicability::MaybeIncorrect)
    );
    assert!(lints[1].fix.is_none());
    assert_eq!(apply_fixes(source, &diagnostics, false).1, 0);
    let (fixed, num_fixes) = apply_fixes(source, &diagnostics, true);
    assert_eq!(num_fixes, 1);
    assert!(fixed.contains("(loop for record in records\n        collect (normalize record))"));
    assert!(fixed.contains("(normalize record out)"));
}

#[test]
fn parameter_grammar_preserves_only_the_evaluated_fields() {
    let source = r#"
(defun bad (&key ((:mode start)) ((:mode end)) &optional x) (list start end x))
(defun missing (&rest) nil)
(defun literal (t) t)
(defun duplicate (x &optional (value (cons 1) x)) (list x value))
(defun improper (x . rest) (list x rest))
(defun ordinary (&optional (value (cons 2))) value)
(defgeneric special (a &optional (b)))
(defmethod special ((a (eql (cons 3))) &optional b) (list a b))
(defmacro macro (&whole form &environment env (x &optional (y (cons 4))) . rest)
  (list form env x y rest))
(defmacro nested ((&environment bad)) bad)
(destructuring-bind ((a) . rest) data (list a rest))
(let ((2^n 8)) (list 2^n))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-lambda-list"),
        ["&optional", "&rest", ".", "&environment"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-binding-name"),
        ["t"]
    );
    assert_eq!(
        spans(source, &diagnostics, "suspicious/duplicate-key-parameter"),
        [":mode"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        ["(cons 1)", "(cons 2)", "(cons 3)", "(cons 4)"]
    );
    let duplicate = diagnostics
        .iter()
        .find(|diag| diag.rule_id == "correctness/duplicate-binding")
        .unwrap();
    assert_eq!(span(source, duplicate.range), "x");
    assert_eq!(span(source, duplicate.related[0].range), "x");
    assert!(duplicate.related[0].range.start() < duplicate.range.start());
}

#[test]
fn macro_arguments_and_nested_backquotes_do_not_become_calls() {
    let source = r#"
(in-package :cl-user)
(defmacro raw (&whole source &environment env value)
  (declare (ignore source env)) `',value)
(raw (cons :quoted-data))
(external:foreign-dsl ((cons :external-data)))
(macrolet ((local (x) `',x))
  (local (cons :opaque))
  #'local)
(let ((inner (cons :initializer))) (list inner))
#'(lambda (&optional (x (cons :default))) (cons x))
`(outer `(inner ,(cons :template) ,,(cons :active)))
`(defun f (x) (cons x))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        [
            "(cons :initializer)",
            "(cons :default)",
            "(cons x)",
            "(cons :active)"
        ]
    );
    assert_eq!(
        spans(
            source,
            &diagnostics,
            "correctness/function-of-macro-or-special"
        ),
        ["local"]
    );
    assert!(spans(source, &diagnostics, "correctness/invalid-call-operator").is_empty());
}

#[test]
fn template_diagnostics_point_back_to_the_parameter_and_other_use() {
    let source = r#"
(defmacro alternate (value) `(case (choose) (:a ,value) (:b ,value)))
(defmacro repeated (value) `(list ,value ,value))
(defmacro repeating (value) `(dotimes (i 4) (print ,value)))
"#;
    let diagnostics = lint(source, LintPreset::Strict);
    assert_eq!(
        spans(source, &diagnostics, "suspicious/macro-multiple-evaluation"),
        ["value"]
    );
    assert_eq!(
        spans(source, &diagnostics, "suspicious/macro-evaluation-in-loop"),
        ["value"]
    );
    assert_eq!(
        diagnostics
            .iter()
            .find(|d| d.rule_id == "suspicious/macro-multiple-evaluation")
            .unwrap()
            .related
            .len(),
        2
    );
}

#[test]
fn context_facts_do_not_cross_mutation_or_symbol_macro_reads() {
    let source = r#"
(defun test (value)
  (when (consp value) (when (consp value) :redundant))
  (when value (setf value nil) (when value :changed))
  (when value (let ((value nil)) (when value :rebound)))
  (when (eql "a" value) (when (eql "a" value) :distinct-literal-site))
  (symbol-macrolet ((next (read))) (when next (when next :still-unknown)))
  (cond (value :first) ((setf value t) :side-effect) (value :unknown))
  (or value (setf value nil) value))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(
            source,
            &diagnostics,
            "suspicious/redundant-enclosing-condition"
        ),
        ["(consp value)"]
    );
    assert!(spans(source, &diagnostics, "suspicious/duplicate-cond-test").is_empty());
    assert!(spans(source, &diagnostics, "suspicious/duplicate-operand").is_empty());
}

#[test]
fn keyword_check_respects_positional_and_leftmost_rules() {
    let source = r#"
(in-package :cl-user)
(defun keyed (required &optional optional &rest tail &key ((:|Exact Key| k)))
  (list required optional tail k))
(keyed 1 :optional :|Exact Key| 2)
(keyed 1 2 :oops 3 :allow-other-keys t)
(keyed 1 2 :oops 4 :allow-other-keys nil :allow-other-keys t)
(keyed 1 2 :|Exact Key| 3 :|Exact Key| (print 4))
(keyed 1 2 :dangling)
(keyed 1 2 17 0)
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/unknown-keyword-arg"),
        [":oops"]
    );
    assert_eq!(
        spans(source, &diagnostics, "suspicious/duplicate-keyword-arg"),
        [":allow-other-keys", ":|Exact Key|"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/non-symbol-keyword-arg"),
        ["17"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/odd-key-value-args"),
        [":dangling"]
    );
}

#[test]
fn literal_contracts_distinguish_array_rank_list_tail_and_indirect_calls() {
    let source = r#"
(funcall #'cons :a)
(funcall (lambda (a b) (list a b)) 1)
((lambda (a b) (list a b)) 2)
(apply #'cons '(1 2))
(apply #'cons '(1))
(apply #'cons tail)
(apply #'list #(1 2))
(funcall "cons" 1 2)
(car 42)
(char '(a b) 0)
(elt data -1)
(aref #(1 2) 0 0)
(aref #4(:repeat) 3)
(aref #4(:repeat) 4)
(nth 7 '(1 2))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        [
            "(funcall #'cons :a)",
            "(funcall (lambda (a b) (list a b)) 1)",
            "((lambda (a b) (list a b)) 2)",
            "(apply #'cons '(1))"
        ]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-apply-tail"),
        ["#(1 2)"]
    );
    assert_eq!(
        spans(
            source,
            &diagnostics,
            "correctness/invalid-function-designator"
        ),
        ["\"cons\""]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/literal-argument-type"),
        ["42", "'(a b)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/literal-array-rank"),
        ["#(1 2)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/literal-sequence-bounds"),
        ["4"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/negative-index"),
        ["-1"]
    );
}

#[test]
fn format_locations_map_through_unicode_and_reader_escapes() {
    let source = r#"
(format nil "雪\~{~A~]" values)
(format nil "~vA" 6)
(format nil "~'雪,2A" :ok)
(format nil "~~ ~:[no~;yes~]" flag)
(format nil "~V,A ~:A" 1 :ok)
(format nil "incomplete ~")
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let invalid: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "correctness/invalid-format-control")
        .collect();
    assert_eq!(
        invalid
            .iter()
            .map(|d| span(source, d.range))
            .collect::<Vec<_>>(),
        ["~]", "~"]
    );
    assert_eq!(span(source, invalid[0].related[0].range), "\\~{");
    assert_eq!(
        spans(source, &diagnostics, "correctness/format-arity-mismatch"),
        ["(format nil \"~vA\" 6)", "(format nil \"~V,A ~:A\" 1 :ok)"]
    );
}

#[test]
fn batch_lint_and_single_document_share_cross_file_definition_locations() {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document(
        "library.lisp",
        "(in-package :cl-user) (defun combine (a b) (list a b))",
    );
    let source = "(in-package :cl-user) (combine :only-one)";
    workspace.set_document("user.lisp", source);
    let config = LintConfig::default();
    let batch = workspace.lint_documents(["missing", "user.lisp", "library.lisp"], &config);
    assert_eq!(batch[1], workspace.lint_document("user.lisp", &config));
    assert!(batch[0].is_empty());
    assert!(batch[2].is_empty());
    let [diagnostic] = batch[1].as_slice() else {
        panic!("{:?}", batch[1]);
    };
    assert_eq!(
        diagnostic.related[0].document.as_deref(),
        Some("library.lisp")
    );
    assert_eq!(
        span(
            workspace.snapshot("library.lisp").unwrap().text(),
            diagnostic.related[0].range
        ),
        "combine"
    );
}

#[test]
fn diagnostic_snapshots_retain_aliased_sources_and_positions_across_edits() {
    use zkcl_diagnostics::{LabelRole, SourceDocument, SourcePosition, SourcePositionEncoding};

    let library_uri = "untitled:library";
    let library = |version, prefix| {
        SourceDocument::input(&format!(
            "(in-package :cl-user)\n{prefix}(defun combine (a b) (list a b))"
        ))
        .with_uri(library_uri)
        .with_version(version)
    };
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("caller", "(in-package :cl-user)\n(combine :one)");
    workspace.set_document("alternate", "(in-package :cl-user)\n(defun combine (a) a)");
    workspace.set_visible_sources("caller".into(), vec!["caller".into(), "library".into()]);
    workspace.set_document_source("library", library(1, "")).unwrap();
    let before = workspace.diagnostic_snapshot("caller").unwrap();
    workspace
        .set_document_source("library", library(2, "#|雪😀|# "))
        .unwrap();
    let moved = workspace.diagnostic_snapshot("caller").unwrap();
    workspace.set_visible_sources("caller".into(), vec!["caller".into(), "alternate".into()]);
    let switched = workspace.diagnostic_snapshot("caller").unwrap();

    let config = LintConfig::default();
    assert!(switched.document("library").is_none());
    assert!(switched.lint(&config, || false).unwrap().is_empty());
    let encodings = [
        SourcePositionEncoding::Utf8,
        SourcePositionEncoding::Utf16,
        SourcePositionEncoding::Utf32,
    ];
    for (view, version, columns) in [(&before, 1, [7, 7, 7]), (&moved, 2, [19, 15, 14])] {
        assert!(view.document("alternate").is_none());
        let diagnostics = view.lint(&config, || false).unwrap();
        let [arity] = diagnostics.as_slice() else {
            panic!("expected one arity diagnostic, got {diagnostics:?}");
        };
        assert_eq!(arity.rule_id, "correctness/arity-mismatch");
        assert_eq!(arity.related[0].document.as_deref(), Some("library"));
        let report = arity
            .to_diagnostic(view.source().source(), |name| {
                view.document(name).map(|snapshot| snapshot.source())
            })
            .unwrap();
        let related = report
            .labels
            .iter()
            .find(|label| label.role == LabelRole::Secondary)
            .unwrap();
        let retained = report.source(related.span.snapshot()).unwrap();
        assert_eq!(retained.uri(), Some(library_uri));
        assert_eq!(retained.version(), Some(version));
        let range = related.span.range();
        assert_eq!(&retained.text().unwrap()[range.clone()], "combine");
        for (encoding, column) in encodings.into_iter().zip(columns) {
            assert_eq!(
                retained.position_with_encoding(range.start, encoding),
                Some(SourcePosition { line: 1, column }),
            );
        }
    }
}

#[test]
fn parallel_lint_preserves_visibility_and_request_order() {
    let input = |body: &str| {
        format!(
            "{}\n(in-package :cl-user) {body}",
            "; padding\n".repeat(1000)
        )
    };
    let workspace = || {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("library.lisp", input("(defun combine (a b) (list a b))"));
        workspace.set_document("caller.lisp", input("(combine :one)"));
        workspace.set_document("other.lisp", input("(combine :one)"));
        workspace.set_visible_sources("caller.lisp".into(), vec!["library.lisp".to_owned()]);
        workspace.set_visible_sources("other.lisp".into(), Vec::<String>::new());
        workspace
    };
    let requested = [
        "other.lisp",
        "caller.lisp",
        "missing",
        "caller.lisp",
        "library.lisp",
    ];
    let config = LintConfig::default();
    let serial = workspace().lint_documents(requested, &config);
    let parallel =
        workspace().lint_documents_with_jobs(requested, &config, NonZeroUsize::new(4).unwrap());
    assert_eq!(parallel, serial);
    assert!(parallel[0].is_empty());
    let [diagnostic] = parallel[1].as_slice() else {
        panic!("{:?}", parallel[1]);
    };
    assert_eq!(diagnostic.rule_id, "correctness/arity-mismatch");
    assert_eq!(
        diagnostic.related[0].document.as_deref(),
        Some("library.lisp")
    );
    assert!(parallel[2].is_empty());
    assert_eq!(parallel[3], parallel[1]);
}

#[test]
fn malformed_input_leaves_following_and_finished_inner_forms_lintable() {
    let source = r#"
) (cons :after-stray-paren)
(zk-lex:lexmatch)
(cons :following-lexmatch)
((list :dynamic-function) :argument)
(cons :dotted . tail)
(let ((x 1 2)) (cond () (t :good)))
(tagbody a (go a) #x10 16)
(dolist (element elements) loop-tag (go loop-tag))
(defun recovering (value)
  (print value)
  (cons :finished)
  (cons :editing
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        [
            "(cons :after-stray-paren)",
            "(cons :following-lexmatch)",
            "(cons :finished)"
        ]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/dotted-call"),
        ["."]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-call-operator"),
        ["(list :dynamic-function)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-binding-spec"),
        ["(x 1 2)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/malformed-clause"),
        ["()"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/duplicate-tagbody-tag"),
        ["16"]
    );
    assert!(spans(source, &diagnostics, "correctness/unknown-control-target").is_empty());
}

#[test]
fn repair_groups_are_atomic_at_utf8_boundaries_and_conflicts() {
    let source = "(cons :only)雪";
    let mut diagnostic = lint(source, LintPreset::Strict).into_iter().next().unwrap();
    let range = |a: u32, b: u32| TextRange::new(a.into(), b.into());
    let edit = |start, end| TextEdit {
        range: range(start, end),
        replacement: "X".into(),
    };
    diagnostic.fix = Some(LintFix {
        title: "test edit group".into(),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![
            edit(0, 1),
            edit(source.len() as u32 - 2, source.len() as u32),
        ],
    });
    assert_eq!(
        apply_fixes(source, &[diagnostic.clone()], false),
        (source.into(), 0)
    );
    diagnostic.fix.as_mut().unwrap().edits = vec![edit(0, 3), edit(2, 4)];
    assert_eq!(
        apply_fixes(source, &[diagnostic.clone()], false),
        (source.into(), 0)
    );
    diagnostic.fix.as_mut().unwrap().applicability = FixApplicability::HasPlaceholders;
    diagnostic.fix.as_mut().unwrap().edits = vec![edit(0, 1)];
    assert_eq!(apply_fixes(source, &[diagnostic], true), (source.into(), 0));
}

#[test]
fn reader_checks_keep_quote_and_unknown_feature_boundaries_distinct() {
    let source = r#"
'(,stray)
(list #+unknown (cons :conditional) (cons :certain))
`(template ,(cons :evaluated))
'(#.(never-run) #.(never-run-either))
"#;
    let diagnostics = lint(source, LintPreset::Strict);
    assert_eq!(
        spans(
            source,
            &diagnostics,
            "correctness/unquote-outside-backquote"
        ),
        [","]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        ["(cons :certain)", "(cons :evaluated)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "security/read-time-eval"),
        ["#.", "#."]
    );
}

#[test]
fn overwrite_checks_respect_dynamic_bindings_and_evaluated_rhs() {
    let source = r#"
(in-package :cl-user)
(defvar state)
(defun assignments ()
  (setq state 1)
  (setq state 2)
  (let ((val 3))
    (setq val 4)
    (setq val 5)
    val)
  (let* ((val 6) (snapshot val))
    (setq val 7)
    (list snapshot val))
  (let ((val 8))
    (setq val (invoke-user))
    val))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "suspicious/overwritten-binding"),
        ["3", "val"]
    );
}

#[test]
fn deep_recovery_and_mixed_reader_input_keep_every_span_on_source_boundaries() {
    let mut source = "(progn ".repeat(270);
    source.push_str("(format nil \"雪~{~]\" item)");
    source.push_str(&")".repeat(270));
    source.push_str("\n) ' , . #+(or missing) #.(never-run) |unterminated");
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("broken.lisp", &source);
    let snapshot = workspace.diagnostic_snapshot("broken.lisp").unwrap();
    for diagnostic in snapshot.source().diagnostics() {
        span(&source, diagnostic.range());
    }
    let config = LintConfig::from_preset(LintPreset::All);
    for diagnostic in snapshot.lint(&config, || false).unwrap() {
        span(&source, diagnostic.range);
        for note in diagnostic.related {
            assert!(note.document.is_none());
            span(&source, note.range);
        }
    }
    assert!(snapshot.lint(&config, || true).is_none());
}

#[test]
fn value_count_analysis_joins_paths_and_checks_consumers_and_callbacks() {
    let source = r#"
(defun check (key table flag)
  (multiple-value-bind (value found extra)
      (if flag (gethash key table) (values 1 2))
    (list value found extra))
  (nth-value 2 (progn (print key) (gethash key table)))
  (nth-value 1 (if flag (values :one) (values :one :two)))
  (nth-value 8 (unresolved-source))
  (multiple-value-call #'cons (values :one) (values))
  (mapcar #'cons '(1 2))
  (mapcar #'cons '(1 2) '(3 4))
  (maphash (lambda (value) (print value)) table)
  (multiple-value-list (prog1 (values) (print :zero)))
  (prog1 (values :one) (print :one)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "suspicious/missing-multiple-values"),
        ["extra"]
    );
    assert_eq!(
        spans(source, &diagnostics, "suspicious/nth-value-out-of-range"),
        ["2"]
    );
    assert_eq!(
        spans(source, &diagnostics, "suspicious/prog1-drops-values"),
        ["(prog1 (values) (print :zero))"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        [
            "(multiple-value-call #'cons (values :one) (values))",
            "#'cons",
            "(lambda (value) (print value))",
        ]
    );
}

#[test]
fn used_results_and_binding_declarations_have_distinct_roles() {
    let source = r#"
(defun harmless (value) (incf value))
(defun useful (value) (list (setf value 3)))
(defun unused-with-type (|MixedCase|)
  "Keep this docstring."
  (declare (type integer |MixedCase|))
  42)
(defun ignore-contradiction (value) (declare (ignore value)) (list value))
(defun ignore-valid (value) (declare (ignorable value)) (list value))
(defun discarded () (let ((value 0)) (incf value) (print :other)))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "suspicious/write-only-variable"),
        ["value"]
    );
    let unused = diagnostics
        .iter()
        .find(|d| d.rule_id == "style/unused-variable")
        .unwrap();
    assert_eq!(span(source, unused.range), "|MixedCase|");
    let fix = unused.fix.as_ref().unwrap();
    assert_eq!(fix.applicability, FixApplicability::MaybeIncorrect);
    let (updated, _) = apply_fixes(source, std::slice::from_ref(unused), true);
    assert!(updated.contains("\"Keep this docstring.\"\n  (declare (ignore |MixedCase|)) (declare (type integer |MixedCase|))"));
    assert_eq!(
        spans(source, &diagnostics, "suspicious/used-ignored-binding"),
        ["value"]
    );
}

#[test]
fn class_options_and_named_control_targets_preserve_their_syntactic_roles() {
    let source = r#"
(defclass box () ((content :initform 0 :initform (cons 1) :reader a :reader b
                          :allocation :world))
  (:default-initargs :content (cons 2) :content 3))
(defclass other () ((content :initform)))
(loop named scan do (return-from scan :ok))
(loop named scan do (return :no-nil-block))
(defun both-exit (flag)
  (if flag (return-from both-exit :ok) (error "bad"))
  (print :dead))
(defun can-continue ()
  (block local (return-from local :ok))
  (print :live))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/duplicate-class-option"),
        [":initform", ":content"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-class-option"),
        [":world", ":initform"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/arity-mismatch"),
        ["(cons 1)", "(cons 2)"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/return-outside-nil-block"),
        ["(return :no-nil-block)"]
    );
    assert!(spans(source, &diagnostics, "correctness/unknown-control-target").is_empty());
    assert_eq!(
        spans(source, &diagnostics, "suspicious/unreachable-code"),
        ["(print :dead)"]
    );
}

#[test]
fn typo_fixes_are_distinct_from_automatic_comment_preserving_rewrites() {
    let source = r#"
(in-package :cl-user)
(lenght '(1 2))
'(cl:lenght)
#+not-selected '(cl:nonexistent-test-name)
(make-array 3 :initial-eleemnt 0)
(defun cleanup (value)
  (list (progn #| Keep this explanation. |# value) (and (values value t))))
"#;
    let diagnostics = lint(source, LintPreset::Strict);
    for rule in [
        "correctness/unknown-common-lisp-symbol",
        "suspicious/likely-misspelled-operator",
        "correctness/unknown-keyword-arg",
    ] {
        let diagnostic = diagnostics
            .iter()
            .find(|d| d.rule_id == rule)
            .unwrap_or_else(|| panic!("missing {rule}: {diagnostics:?}"));
        assert_eq!(
            diagnostic.fix.as_ref().unwrap().applicability,
            FixApplicability::MaybeIncorrect
        );
    }
    let comment_region = diagnostics
        .iter()
        .find(|d| d.rule_id == "complexity/redundant-progn")
        .unwrap();
    assert!(comment_region.fix.is_none());
    let (fixed, _) = apply_fixes(source, &diagnostics, false);
    assert!(fixed.contains("#| Keep this explanation. |#"));
    assert!(fixed.contains(":initial-eleemnt"));
    assert!(!fixed.contains("(and (values value t))"));
}

#[test]
fn assignment_checks_point_to_a_constant_and_reject_compound_setq_places() {
    let source =
        "(in-package :cl-user) (defconstant +limit+ 8) (setq +limit+ 9) (setq (car items) 1)";
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "correctness/assign-to-constant"),
        ["+limit+"]
    );
    assert_eq!(
        spans(source, &diagnostics, "correctness/invalid-setq-target"),
        ["(car items)"]
    );
    let constant = diagnostics
        .iter()
        .find(|d| d.rule_id == "correctness/assign-to-constant")
        .unwrap();
    assert_eq!(span(source, constant.related[0].range), "+limit+");
    assert!(constant.related[0].range.start() < constant.range.start());
}

#[test]
fn safe_rewrites_respect_package_shadowing_result_identity_and_reader_features() {
    let source = r#"
(defpackage :sample (:use :cl) (:shadow :when :not))
(in-package :sample)
(defun f (value)
  (cl:if value (print :yes))
  (list (cl:if value nil t)
        (format nil "fresh")
        (or (if (some #'identity value) t nil) :fallback)))
(defun g (value) (list (and #+enabled value)))
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(vec!["enabled".into()]));
    workspace.set_document("sample.lisp", source);
    let diagnostics =
        workspace.lint_document("sample.lisp", &LintConfig::from_preset(LintPreset::Strict));
    let (fixed, _) = apply_fixes(source, &diagnostics, false);
    assert!(fixed.contains("(cl:if value (print :yes))"));
    assert!(fixed.contains("(cl:if value nil t)"));
    assert!(fixed.contains("(format nil \"fresh\")"));
    assert!(fixed.contains("(if (some #'identity value) t nil)"));
    assert!(fixed.contains("(and #+enabled value)"));
}

#[test]
fn straight_line_overwrites_track_binding_identity_and_stop_at_unknown_effects() {
    let source = r#"
(defun flow (x y)
  (setq x :dead)
  (setq y :other)
  (setq x :replaces-dead)
  (setq y x)
  (setq x :after-read)
  (signal :unknown-effect)
  (setq x :after-signal)
  (list x y))
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.rule_id == "suspicious/overwritten-binding")
        .collect();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(span(source, hits[0].range), "x");
    assert_eq!(span(source, hits[1].range), "y");
    assert!(source[usize::from(hits[0].related[0].range.end())..].starts_with(" :replaces-dead"));
}

#[test]
fn dead_store_fix_preserves_effectful_initializers_and_docstring_positions() {
    let source = r#"
(defun clean (x y)
  (setq x :unused)
  (setq y :other)
  (setq x :used)
  (list x y))
(defun preserve-output (x)
  (setq x (print :logged))
  (setq x :done)
  x)
(defun keep-docstring-position (x)
  (setq x 1)
  "This is an expression, not the docstring."
  (setq x 2)
  x)
"#;
    let diagnostics = lint(source, LintPreset::Default);
    let (fixed, num_fixes) = apply_fixes(source, &diagnostics, false);
    assert_eq!(num_fixes, 1, "{diagnostics:?}");
    assert_eq!(fixed, source.replace("  (setq x :unused)\n", ""));
}

#[test]
fn unreachable_code_tracks_exhaustive_cond_constant_when_unless_and_flet() {
    let source = r#"
(defun cond-exhaustive (p)
  (cond (p (return-from cond-exhaustive 1))
        (t (error "boom")))
  :dead-after-cond)

(defun cond-partial (p)
  (cond (p (return-from cond-partial 1)))
  :live-after-partial-cond)

(defun cond-normal-default (p)
  (cond (p (return-from cond-normal-default 1))
        (t 2))
  :live-after-normal-default)

(defun constant-guards ()
  (when t (return-from constant-guards :a))
  :dead-after-when)

(defun constant-unless ()
  (unless nil (return-from constant-unless :b))
  :dead-after-unless)

(defun flet-exits ()
  (flet ((helper () 1))
    (return-from flet-exits (helper)))
  :dead-after-flet)
"#;
    let diagnostics = lint(source, LintPreset::Default);
    assert_eq!(
        spans(source, &diagnostics, "suspicious/unreachable-code"),
        vec![
            ":dead-after-cond",
            ":dead-after-when",
            ":dead-after-unless",
            ":dead-after-flet",
        ]
    );
}
