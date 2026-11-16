#![cfg(feature = "analysis")]

use zkcl_syntax::{
    FixApplicability, LintCategory, LintConfig, LintLevel, LintPreset, LintTag, SourceWorkspace,
    all_rules, apply_fixes, find_rule,
    syntax::{SyntaxDiagnostic, SyntaxDiagnosticKind},
};

fn lint_ids(src: &str, preset: LintPreset) -> Vec<(&'static str, String)> {
    let mut ws = SourceWorkspace::new();
    ws.set_document("test.lisp", src);
    let cfg = LintConfig::from_preset(preset);
    ws.lint_document("test.lisp", &cfg)
        .into_iter()
        .map(|d| (d.rule_id, d.message))
        .collect()
}

fn rule_spans<'a>(src: &'a str, rule: &str) -> Vec<&'a str> {
    let mut ws = SourceWorkspace::new();
    ws.set_document("test.lisp", src);
    ws.lint_document("test.lisp", &LintConfig::from_preset(LintPreset::Default))
        .into_iter()
        .filter(|diagnostic| diagnostic.rule_id == rule)
        .map(|diagnostic| {
            &src[usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end())]
        })
        .collect()
}

#[test]
fn lint_checks_foreign_signatures_at_the_source_without_configuration() {
    let source = r#"
(defpackage #:driver (:use #:cl #:zk-ffi) (:local-nicknames (#:native #:zk-ffi)))
(in-package #:driver)
(bind-function :host "set" :void :unsigned-long :pointer)
(native:bind-function :host "set" :void :uint_32)
(native:bind-pointer address :int :void)
(native:bind-variadic-function :host "sum" #x0 :int :int)
(native:bind-variadic-pointer address 3 :int :int)
(native:make-callback #'identity '(:struct :int (:struct :void)) :int)
(native:type-size '(:struct))
(native:type-alignment '(:struct :uint8 (:struct :double :pointer)))
(native:field-offset :uint32 0)
(native:field-offset '(:struct :int :double) #x2)
(native:mem-ref address '(:struct :int))
(native:bind-function :host "dynamic" (choose-type) result-type)
(native:type-size '(:struct #+uncertain-feature :void :int))
(native:type-size '(:struct :int . (:double)))
(defun local (address)
  (flet ((bind-pointer (pointer type extra) (list pointer type extra)))
    (bind-pointer address :void :unknown)))
(funcall (native:bind-function :host "sum" :int :int :int) 1)
(native:mem-ref address)
(native:with-foreign-memory (car (native:type-size :void))
  (native:mem-set car :void 0 1))
"#;
    assert_eq!(
        rule_spans(source, "correctness/ffi-declaration"),
        [
            ":uint_32",
            ":void",
            "#x0",
            "3",
            ":void",
            "(:struct)",
            ":uint32",
            "#x2",
            "(:struct :int)",
            ":void",
            ":void"
        ],
    );
    assert_eq!(
        rule_spans(source, "correctness/arity-mismatch"),
        [
            "(native:bind-function :host \"sum\" :int :int :int)",
            "(native:mem-ref address)",
        ],
    );
}

#[test]
fn lint_detects_repeated_fixed_host_binding_inside_a_loop() {
    let source = r#"
(dotimes (i n)
  (funcall (zk-ffi:bind-function :host "tick" :void :int) i)
  (funcall (zk-ffi:bind-function library "tick" :void :int) i)
  (funcall (zk-ffi:bind-function :host "tick" :void type) i))
(funcall (zk-ffi:bind-function :host "tick" :void :int) 0)
"#;
    assert_eq!(
        rule_spans(source, "perf/foreign-binding-in-loop"),
        [r#"(zk-ffi:bind-function :host "tick" :void :int)"#],
    );
}

#[test]
fn lexical_pattern_diagnostics_map_regex_offsets_through_lisp_escaping() {
    let source = r#"(in-package :cl-user)
      (zk-lex:lexmatch text ((:with (:seq "=" (:re "雪\\q")) :after tail) tail) (_ nil))
      (zk-lex:lexmatch (:bytes data) ((:re "\\xff") :ok) (_ nil))
      (zk:match text ((zk-lex:lex (:re "雪\\q")) :ok) (_ nil))
      (defun classify (x)
        "Docstring."
        (declare (type boolean x))
        (zk:match x (t 1) (nil 0)))
      (zk:if-match (cons a b) pair :when (numberp a) (+ a b) 0)
      (zk:let-match ((pair (cons 1 2))) pair)"#;
    assert_eq!(
        rule_spans(source, "correctness/zk-match-mismatch"),
        [r"\\", r"\\"]
    );
    assert!(rule_spans(source, "correctness/arity-mismatch").is_empty());
    assert!(rule_spans(source, "correctness/unresolved-operator").is_empty());
}

#[test]
fn lint_restart_case_checks_expressions_not_names_or_lambda_lists() {
    let src = r#"
(defun choose (x)
  (restart-case (continue :a :b :c)
    (continue (&optional (function (cons :restart-default)))
      :report (lambda (stream) (cons stream))
      :interactive list
      :test (lambda (condition) (not condition))
      (declare (ignore function))
      (cons :clause))
    (other () :test when))
  (cl:restart-case x ()))
"#;
    assert_eq!(
        rule_spans(src, "correctness/arity-mismatch"),
        [
            "(continue :a :b :c)",
            "(cons :restart-default)",
            "(cons stream)",
            "(cons :clause)"
        ]
    );
    assert_eq!(
        rule_spans(src, "correctness/function-of-macro-or-special"),
        ["when"]
    );
}

#[test]
fn lint_compiler_macro_checks_initforms_and_body_not_binding_syntax() {
    let src = r#"
(define-compiler-macro expand-me ((function &optional (nth-value (cons :nested)))
                                  &optional (documentation (cons :default))
                                  &environment env)
  (declare (ignore function nth-value documentation env))
  (return-from expand-me (cons :body)))
(define-compiler-macro (setf update-me) (function &rest args)
  (return-from update-me (list function args)))
"#;
    assert_eq!(
        rule_spans(src, "correctness/arity-mismatch"),
        ["(cons :nested)", "(cons :default)", "(cons :body)"]
    );
    assert!(rule_spans(src, "correctness/unknown-control-target").is_empty());
}

#[test]
fn lint_setf_method_uses_the_accessor_as_its_implicit_block_name() {
    let src = r#"
(defmethod (setf widget-value) (new-value (widget t))
  (declare (ignore widget))
  (return-from widget-value new-value))
(defmethod (setf widget-other) (new-value (widget t))
  (declare (ignore widget))
  (return-from missing-block new-value))
"#;
    assert_eq!(
        rule_spans(src, "correctness/unknown-control-target"),
        ["missing-block"]
    );
}

#[test]
fn lint_literal_format_only_reports_proven_underflow() {
    let src = r#"
(defun write-samples (x extra)
  (format nil "no directives" (incf extra))
  (format nil "~A" x extra)
  (format nil "~A" x)
  (format nil "~D ~A" x)
  (format nil "~[~A~;~A ~A~]" 0 x)
  (format nil "~V,A" x)
  (format nil "~~~A"))
"#;
    assert_eq!(
        rule_spans(src, "correctness/format-arity-mismatch"),
        [
            r#"(format nil "~D ~A" x)"#,
            r#"(format nil "~V,A" x)"#,
            r#"(format nil "~~~A")"#
        ]
    );
}

#[test]
fn lint_cleanup_retains_lexical_checks_without_assuming_target_extent() {
    let src = r#"
(defun clean ()
  (block nil (unwind-protect (return 1) (return 2)))
  (block outer (unwind-protect :ok (return-from outer :cleanup)))
  (tagbody (unwind-protect :ok (go resume)) resume)
  (catch 'key (unwind-protect (throw 'key 1) (throw 'key 2)))
  (unwind-protect :ok (catch 'local (throw 'local :ok))))
(defun invalid-return () (unwind-protect :ok (return :missing)))
(defun invalid-block () (unwind-protect :ok (return-from absent :missing)))
(defun invalid-go () (unwind-protect :ok (go nowhere)))
"#;
    let correctness: Vec<_> = lint_ids(src, LintPreset::Default)
        .into_iter()
        .filter(|(id, _)| id.starts_with("correctness/"))
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        correctness,
        [
            "correctness/return-outside-nil-block",
            "correctness/unknown-control-target",
            "correctness/unknown-control-target",
        ]
    );
    assert_eq!(
        rule_spans(src, "correctness/unknown-control-target"),
        ["absent", "nowhere"]
    );
}

#[test]
fn rules_have_documentation_and_resolve_by_id_or_short_name() {
    let rules = all_rules();
    assert!(
        rules.len() >= 60,
        "expected at least 60 lint rules, found {}",
        rules.len()
    );
    for rule in rules {
        assert!(!rule.id.is_empty());
        assert!(!rule.short_name.is_empty());
        assert!(!rule.summary.is_empty());
        assert!(!rule.rationale.is_empty());
        assert!(!rule.bad_example.is_empty());
        assert!(!rule.good_example.is_empty());
        assert_eq!(find_rule(rule.id).map(|r| r.id), Some(rule.id));
        assert_eq!(find_rule(rule.short_name).map(|r| r.id), Some(rule.id));
    }
    assert!(find_rule("security/read-time-eval").is_some());
    assert!(find_rule("defpackage-interned-symbol").is_some());
}

#[test]
fn lint_reports_invalid_literal_operations_and_malformed_forms() {
    let src = "\
(defun buggy (a a)
  (/ a 0)
  (format t \"~A ~A\" a)
  (subseq '(1 2) 5 10)
  (eval-when (bogus) 1)
  (nreverse '(1 2 3))
  (setf '(1 2) 3)
  (case a
    (otherwise :other)
    (1 :one)))
";
    let ids: Vec<_> = lint_ids(src, LintPreset::Default)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(ids.contains(&"correctness/duplicate-binding"), "{ids:?}");
    assert!(ids.contains(&"correctness/division-by-zero"), "{ids:?}");
    assert!(
        ids.contains(&"correctness/format-arity-mismatch"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"correctness/literal-sequence-bounds"),
        "{ids:?}"
    );
    assert!(ids.contains(&"correctness/eval-when-invalid"), "{ids:?}");
    assert!(ids.contains(&"correctness/literal-mutation"), "{ids:?}");
    assert!(ids.contains(&"suspicious/setf-non-place"), "{ids:?}");
    assert!(
        ids.contains(&"correctness/case-non-final-default"),
        "{ids:?}"
    );
}

#[test]
fn lint_skips_shadowed_division_and_quoted_setf() {
    let src = "\
(defun clean (x)
  (flet ((/ (a b) (+ a b)))
    (list (/ x 0) '(setf (car '(1 2)) 3))))
";
    let ids: Vec<_> = lint_ids(src, LintPreset::Strict)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(!ids.contains(&"correctness/division-by-zero"), "{ids:?}");
    assert!(!ids.contains(&"correctness/literal-mutation"), "{ids:?}");
}

#[test]
fn arity_lint_distinguishes_type_specifiers_and_multiple_value_bindings_from_calls() {
    let src = "\
(defun check (selector vector)
  (let (documentation body)
    (multiple-value-setq (documentation body) (values 1 2))
    (list documentation body))
  (multiple-value-bind (documentation body) (values 1 2)
    (list documentation body))
  (check-type selector (or null (eql t) character))
  (check-type (aref vector (eql t)) integer)
  (multiple-value-bind (value) (documentation selector)
    (eql value)))
";
    let arity_errors: Vec<_> = lint_ids(src, LintPreset::Default)
        .into_iter()
        .filter_map(|(id, message)| (id == "correctness/arity-mismatch").then_some(message))
        .collect();
    assert_eq!(arity_errors.len(), 3, "{arity_errors:?}");
    assert_eq!(
        arity_errors
            .iter()
            .filter(|m| m.starts_with("eql "))
            .count(),
        2
    );
    assert_eq!(
        arity_errors
            .iter()
            .filter(|m| m.starts_with("documentation "))
            .count(),
        1
    );
}

#[test]
fn strict_lint_reports_shadowed_clauses_and_suspicious_calls() {
    let src = "\
(defun traps (x xs k tbl)
  (handler-case (read-it x)
    (error (e) nil)
    (file-error (e) :retry))
  (typecase x
    (number :num)
    (integer :int))
  (multiple-value-bind (a b) (+ 1 2) (list a b))
  (prog1 (gethash k tbl) (incf *reads*))
  (and x x)
  (member \"ok\" xs)
  (open x))
";
    let ids: Vec<_> = lint_ids(src, LintPreset::Strict)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(
        ids.contains(&"suspicious/shadowed-handler-clause"),
        "{ids:?}"
    );
    assert!(ids.contains(&"suspicious/swallowed-condition"), "{ids:?}");
    assert!(
        ids.contains(&"suspicious/shadowed-typecase-clause"),
        "{ids:?}"
    );
    assert!(ids.contains(&"suspicious/single-value-in-mvb"), "{ids:?}");
    assert!(ids.contains(&"suspicious/prog1-drops-values"), "{ids:?}");
    assert!(ids.contains(&"suspicious/duplicate-operand"), "{ids:?}");
    assert!(ids.contains(&"suspicious/default-eql-on-string"), "{ids:?}");
    assert!(ids.contains(&"suspicious/resource-leak-open"), "{ids:?}");
}

#[test]
fn lint_accepts_subtypep_value_bindings_protected_open_and_string_tests() {
    let src = "\
(defun safe-traps (t1 t2 path tbl)
  (multiple-value-bind (sub certain) (subtypep t1 t2)
    (when (and sub certain)
      (with-open-file (s path)
        (read-line s))
      (unwind-protect
          (let ((raw (open path)))
            (read-line raw))
        (close path))))
  (member \"ok\" '(\"ok\") :test #'string=)
  (prog1 (gethash :a tbl) (terpri)))
";
    let ids: Vec<_> = lint_ids(src, LintPreset::Strict)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert!(!ids.contains(&"suspicious/single-value-in-mvb"), "{ids:?}");
    assert!(!ids.contains(&"suspicious/resource-leak-open"), "{ids:?}");
    assert!(
        !ids.contains(&"suspicious/default-eql-on-string"),
        "{ids:?}"
    );
}

#[test]
fn prog1_with_a_single_valued_first_form_needs_no_warning() {
    let diagnostics = lint_ids("(defun f () (prog1 (+ 1 2) (terpri)))", LintPreset::Strict);
    assert!(
        !diagnostics
            .iter()
            .any(|(id, _)| *id == "suspicious/prog1-drops-values")
    );
}

#[test]
fn loop_open_checks_immediate_cleanup_of_the_same_stream() {
    let guarded = "when stream do
      (return (let ((renamed nil))
                (unwind-protect (work stream renamed)
                  (when (open-stream-p stream) (close stream))
                  (finish renamed))))";
    let same_stream = "if stream doing (unwind-protect (work stream) (close stream))";
    let cases = [
        ("(open path)", guarded, 0),
        ("(open path)", same_stream, 0),
        ("(open (prepare-path (open path)))", same_stream, 1),
        ("(open path)", "when stream do (work stream)", 1),
        (
            "(open path)",
            "when other do (unwind-protect (work) (close stream))",
            1,
        ),
        (
            "(open path)",
            "when stream do (unwind-protect (work) (close other))",
            1,
        ),
        (
            "(open path)",
            "when stream do (work stream) (unwind-protect (work) (close stream))",
            1,
        ),
        (
            "(open path)",
            "when stream do (unwind-protect (work) (finish) (close stream))",
            1,
        ),
        (
            "(open path)",
            "when stream do (let ((stream nil)) (unwind-protect (work) (close stream)))",
            1,
        ),
        (
            "(open path)",
            "when stream do (let ((prepared (work))) (unwind-protect (finish prepared) (close stream)))",
            1,
        ),
        (
            "(open path)",
            "when stream do (unwind-protect (setf stream nil) (close stream))",
            1,
        ),
        (
            "(open path)",
            "when stream do (unwind-protect (multiple-value-setq (stream) (values nil)) (close stream))",
            1,
        ),
        (
            "(open path)",
            "when stream do (return :ok) finally (close stream)",
            1,
        ),
    ];
    for (open, clauses, expected) in cases {
        let source = format!(
            "(in-package :cl-user) (defun prepare-path (path) path)
             (defun example (path other) (loop repeat 2 for stream = {open} {clauses}))"
        );
        let hits = lint_ids(&source, LintPreset::Default)
            .iter()
            .filter(|(id, _)| *id == "suspicious/resource-leak-open")
            .count();
        assert_eq!(hits, expected, "{source}");
    }

    let shadowed_close = "\
(defpackage :sample (:use :cl) (:shadow :close))
(in-package :sample)
(defun example (path)
  (loop for stream = (open path) when stream do
    (unwind-protect (work stream) (close stream))))
";
    assert!(
        lint_ids(shadowed_close, LintPreset::Default)
            .iter()
            .any(|(id, _)| *id == "suspicious/resource-leak-open")
    );
}

#[test]
fn macro_lint_flags_repeated_evaluation_and_symbol_capture() {
    let src = "\
(defmacro bad-double (x)
  `(+ ,x ,x))

(defmacro bad-capture (expr &body body)
  `(let ((tmp ,expr))
     ,@body))

(defmacro branch-only-once (flag x)
  `(if ,flag (+ ,x 1) (- ,x 1)))

(defmacro safe-macro (name sym)
  (let ((s (intern (symbol-name sym))))
    (ignore-errors (list name s))
    `(list ',sym #',sym)))

(defmacro closed-helper (op a)
  `(flet ((helper (x) (funcall #',op x x)))
     (let ((res (helper ,a)))
       (+ res 1))))
";
    let diags = lint_ids(src, LintPreset::Strict);
    let ids: Vec<_> = diags.iter().map(|(id, _)| *id).collect();
    // `bad-double` triggers multiple evaluation.
    assert_eq!(
        ids.iter()
            .filter(|&&id| id == "suspicious/macro-multiple-evaluation")
            .count(),
        1,
        "{diags:?}"
    );
    // `bad-capture` triggers unhygienic binding (`tmp`).
    assert_eq!(
        ids.iter()
            .filter(|&&id| id == "suspicious/macro-unhygienic-binding")
            .count(),
        1,
        "{diags:?}"
    );
    // `safe-macro` inside defmacro must NOT trigger `intern-without-package` or `swallowed-condition`.
    assert!(
        !ids.contains(&"security/intern-without-package"),
        "{diags:?}"
    );
    assert!(
        !ids.contains(&"suspicious/swallowed-condition"),
        "{diags:?}"
    );
}

#[test]
fn safe_redundant_let_fix_preserves_multiple_value_truncation() {
    let mut ws = SourceWorkspace::new();
    let src = "\
(defun mv (x)
  (let ((v (values 1 2)))
    v))
(defun sv (x)
  (let ((v (+ x 1)))
    v))
";
    ws.set_document("mv.lisp", src);
    let diags = ws.lint_document("mv.lisp", &LintConfig::from_preset(LintPreset::Strict));
    let mv_fix = diags
        .iter()
        .find(|d| {
            d.rule_id == "complexity/redundant-let"
                && src[usize::from(d.range.start())..].starts_with("(let ((v (values")
        })
        .and_then(|d| d.fix.as_ref())
        .expect("mv redundant-let fix");
    assert_eq!(mv_fix.applicability, FixApplicability::MachineApplicable);

    let sv_fix = diags
        .iter()
        .find(|d| {
            d.rule_id == "complexity/redundant-let"
                && src[usize::from(d.range.start())..].starts_with("(let ((v (+")
        })
        .and_then(|d| d.fix.as_ref())
        .expect("sv redundant-let fix");
    assert_eq!(sv_fix.applicability, FixApplicability::MachineApplicable);

    let (fixed_safe, count_safe) = apply_fixes(src, &diags, false);
    assert_eq!(count_safe, 2);
    assert!(fixed_safe.contains("(values (values 1 2))"));
    assert!(fixed_safe.contains("(+ x 1)") && !fixed_safe.contains("(let ((v (+ x 1)))"));
}

#[test]
fn strict_lint_reports_package_conditional_and_sequence_idioms() {
    let src = "\
(defpackage my-app
  (:use cl)
  (:export run))
(in-package :my-app)

(defun bad-style (flag items &key width &allow-other-keys)
  (declare (type list items))
  (when (= (length items) 0)
    (if flag
        (progn
          (print width)
          (terpri))))
  (first (sort items #'<)))
";
    let diags = lint_ids(src, LintPreset::Strict);
    let ids: Vec<_> = diags.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&"style/defpackage-interned-symbol"), "{ids:?}");
    assert!(ids.contains(&"style/no-allow-other-keys"), "{ids:?}");
    assert!(ids.contains(&"perf/length-zero-comparison"), "{ids:?}");
    assert!(ids.contains(&"style/progn-in-conditional"), "{ids:?}");
    assert!(ids.contains(&"perf/first-of-sort"), "{ids:?}");
    assert!(
        !ids.contains(&"style/if-to-when"),
        "should not double-emit if-to-when alongside progn-in-conditional: {ids:?}"
    );
}

#[test]
fn strict_lint_warns_about_sharp_dot_and_unguarded_read_from_string() {
    let diagnostics = lint_ids(
        "(defun reads-source () (read-from-string \"42\"))\n\
         (defun uses-read-time-eval () #.(+ 1 2))",
        LintPreset::Strict,
    );
    let ids: Vec<_> = diagnostics.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&"security/read-time-eval"), "{ids:?}");
    assert!(ids.contains(&"security/unsafe-read-eval"), "{ids:?}");
}

#[test]
fn lint_suppresses_matching_diagnostics_and_flags_unused_suppressions() {
    let mut ws = SourceWorkspace::new();
    let src = "\
(defun suppressed (unused) ; zkcl:allow style/unused-variable -- intentional API slot
  1)

; mallet:suppress style/if-to-when
(defun next-form (c)
  (if c (print 1)))

(defun stale (x) ; zkcl:allow style/unused-variable -- x actually used
  x)
";
    ws.set_document("sup.lisp", src);
    let cfg = LintConfig::from_preset(LintPreset::All);
    let diags = ws.lint_document("sup.lisp", &cfg);
    assert!(!diags.iter().any(|d| d.rule_id == "style/unused-variable"));
    assert!(!diags.iter().any(|d| d.rule_id == "style/if-to-when"));
    assert!(
        diags
            .iter()
            .any(|d| d.rule_id == "pedantic/stale-suppression"),
        "expected stale suppression diagnostic, got: {diags:?}"
    );
}

#[test]
fn sexp_config_applies_rule_overrides_and_macro_traversal() {
    let sexp = "\
(:preset :default
 :max-complexity 20
 :rules ((perf/first-of-sort :deny)
         (style/unused-variable :allow))
 :macros ((with-db :lint-as :let)
          (my-dsl :lint-as :ignore)))
";
    let cfg = LintConfig::parse_sexp(sexp).expect("parse sexp config");
    assert_eq!(cfg.preset, LintPreset::Default);
    assert_eq!(cfg.max_complexity, 20);
    assert_eq!(
        cfg.level_for(find_rule("perf/first-of-sort").unwrap()),
        LintLevel::Deny
    );
    assert_eq!(
        cfg.level_for(find_rule("style/unused-variable").unwrap()),
        LintLevel::Allow
    );

    let mut ws = SourceWorkspace::new();
    let src = "\
(defun run-query (unused)
  (my-dsl this-is-not-evaluated (/ 1 0) (match 1 2))
  (with-db ((conn (open \"/tmp/db\")))
    conn)
  (first (sort (list 2 1) #'<)))
";
    ws.set_document("cfg.lisp", src);
    let diags = ws.lint_document("cfg.lisp", &cfg);
    assert!(
        !diags.iter().any(|d| d.rule_id == "style/unused-variable"),
        "{diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.rule_id == "correctness/division-by-zero"),
        "my-dsl :lint-as :ignore should suppress body checks: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.rule_id == "perf/first-of-sort" && d.level == LintLevel::Deny),
        "{diags:?}"
    );
    assert!(diags.iter().all(|d| d.tags.is_empty()
        || d.tags.contains(&LintTag::Unnecessary)
        || d.tags.contains(&LintTag::Deprecated)));
}

#[test]
fn sexp_config_error_preserves_the_reader_diagnostic() {
    let source = "; 😀\r\n)";
    let error = LintConfig::parse_sexp(source).unwrap_err();
    let diagnostic = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<SyntaxDiagnostic>())
        .expect("configuration error has a reader diagnostic");

    assert_eq!(
        diagnostic.kind(),
        SyntaxDiagnosticKind::UnexpectedCloseParen
    );
    assert_eq!(usize::from(diagnostic.range().start()), source.len() - 1);
    assert_eq!(usize::from(diagnostic.range().end()), source.len());
}

#[test]
fn toml_config_reads_preset_nesting_limit_and_category_override() {
    let toml_cfg = LintConfig::parse_toml(
        "preset = \"strict\"\nmax_nesting = 5\n[rules]\nsecurity = \"deny\"\n",
    )
    .expect("parse toml config");
    assert_eq!(toml_cfg.preset, LintPreset::Strict);
    assert_eq!(toml_cfg.max_nesting, 5);
    assert_eq!(
        toml_cfg.category_overrides.get(&LintCategory::Security),
        Some(&LintLevel::Deny)
    );
    assert!(find_rule("pedantic/deprecated-if-not").is_some());
}

#[test]
fn shadowed_not_and_length_avoid_false_positives_and_fixes_respect_shadowing() {
    let src = r#"
(defun local-shadow (x)
  (flet ((not (v) v)
         (length (v) 42))
    (when (not x) 1)
    (= (length x) 0)))

(defun local-unless-shadow (x)
  (flet ((unless (v) v))
    (when (not x) 1)))
"#;
    let mut ws = SourceWorkspace::new();
    ws.set_document("test.lisp", src);
    let diags = ws.lint_document("test.lisp", &LintConfig::from_preset(LintPreset::Default));
    assert!(
        diags
            .iter()
            .all(|d| d.rule_id != "perf/length-zero-comparison"),
        "{diags:?}"
    );
    let when_not: Vec<_> = diags
        .iter()
        .filter(|d| d.rule_id == "style/when-or-unless-not")
        .collect();
    // Only the unshadowed `(not x)` inside `local-unless-shadow` triggers, and its fix uses `common-lisp:unless`.
    assert_eq!(when_not.len(), 1, "{when_not:?}");
    let (fixed, count) = apply_fixes(src, &diags, false);
    assert_eq!(count, 1);
    assert!(fixed.contains("(common-lisp:unless x 1)"), "{fixed}");
}

#[test]
fn defclass_duplicate_slots_compare_symbol_identity_not_raw_spelling() {
    let src = r#"
(defpackage #:pkg-a (:use #:cl) (:export #:item))
(defpackage #:pkg-b (:use #:cl) (:export #:item))
(defpackage #:consumer (:use #:cl))
(in-package #:consumer)
(defclass multi-pkg ()
  ((pkg-a:item :initarg :a)
   (pkg-b:item :initarg :b)
   (#:anon :initarg :c)
   (#:anon :initarg :d)
   (local-slot :initarg :e)
   (consumer::local-slot :initarg :f)))
"#;
    let dupes = rule_spans(src, "correctness/duplicate-binding");
    assert_eq!(dupes, ["consumer::local-slot"]);
}
