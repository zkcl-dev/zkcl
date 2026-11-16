#![cfg(feature = "analysis")]

use zkcl_syntax::{LintConfig, LintDiagnostic, SourceWorkspace, WorkspaceLintChanges, apply_fixes};

fn lint(source: &str) -> Vec<LintDiagnostic> {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("sample.lisp", source);
    workspace.lint_document("sample.lisp", &LintConfig::default())
}

#[test]
fn proves_code_positions_in_destructuring_and_fresh_symbol_wrappers() {
    let source = r#"
(defmacro with-report ((stream path) &body body)
  (let ((pathname (gensym "PATH")))
    `(let ((,pathname ,path))
       (with-open-file (,stream ,pathname :direction :output)
         ,@body))))
(defun report (destination values)
  (with-report (stream destination)
    (declare (ignorable stream))
    (mapcar #'print values)
    (length (remove-if-not #'keywordp values))))
"#;
    let diagnostics = lint(source);
    assert_eq!(
        apply_fixes(source, &diagnostics, false),
        (
            source
                .replace("(mapcar #'print values)", "(mapc #'print values)")
                .replace(
                    "(length (remove-if-not #'keywordp values))",
                    "(count-if #'keywordp values)"
                ),
            2,
        ),
        "{diagnostics:?}"
    );
    for rule in [
        "perf/mapcar-result-discarded",
        "perf/count-filtered-sequence",
    ] {
        let diagnostic = diagnostics
            .iter()
            .find(|diag| diag.rule_id == rule)
            .unwrap();
        let [related] = diagnostic.related.as_slice() else {
            panic!("{diagnostic:?}")
        };
        assert!(related.document.is_none());
        assert_eq!(
            &source[usize::from(related.range.start())..usize::from(related.range.end())],
            ",@body"
        );
    }
    assert!(
        diagnostics
            .iter()
            .all(|diag| diag.rule_id != "correctness/misplaced-declaration")
    );
}

#[test]
fn propagates_test_tail_and_discarded_positions_through_local_macros() {
    let source = r#"
(defun report (enabled values)
  (macrolet ((report-when (test &body body) `(when ,test ,@body))
             (before-status (&body body) `(progn ,@body :done)))
    (report-when (if enabled t nil)
      (before-status (mapcar #'print values))
      (mapcar #'identity values))))
"#;
    let diagnostics = lint(source);
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 2, "{diagnostics:?}");
    assert_eq!(
        fixed,
        source
            .replace("(if enabled t nil)", "enabled")
            .replace("(mapcar #'print values)", "(mapc #'print values)")
    );
}

#[test]
fn leaves_data_and_unknown_macro_expanders_opaque() {
    let source = r#"
(defmacro mixed (code data) `(progn ',data ,code))
(defmacro inspect-operand (code) `(list ',code ,code))
(defmacro unknown (code) (generate code))
(defmacro inspected-at-expansion (code)
  (assert (consp code))
  `(progn ,code))
(defun use-macros (items)
  (mixed (length (reverse items)) (length (reverse items)))
  (inspect-operand (length (reverse items)))
  (unknown (length (reverse items)))
  (inspected-at-expansion (length (reverse items))))
(macrolet ((mixed (code data) `(quote (,code ,data))))
  (mixed (length (reverse items)) (length (reverse items))))
(defpackage #:another (:use #:cl) (:shadow #:progn))
(in-package #:another)
(defmacro with-my-progn (&body body) `(progn ,@body))
(defun package-shadow (items)
  (with-my-progn (length (reverse items))))
"#;
    let diagnostics = lint(source);
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 1, "{diagnostics:?}");
    assert_eq!(
        fixed,
        source.replacen(
            "(mixed (length (reverse items))",
            "(mixed (length items)",
            1
        )
    );
}

#[test]
fn macro_body_edits_invalidate_callers_with_the_same_parameter_list() {
    const CALL: &str = "(in-package #:consumer)\n(defun export-rows (rows) (helpers:after-job (mapcar #'print rows)))";
    const BEFORE: &str =
        "(in-package #:helpers)\n(defmacro after-job (&body body) `(progn ,@body :done))";
    const AFTER: &str =
        "(in-package #:helpers)\n(defmacro after-job (&body body) `(progn :done ,@body))";
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("packages.lisp", "(defpackage #:helpers (:use #:cl) (:export #:after-job)) (defpackage #:consumer (:use #:cl))");
    workspace.set_document("macro.lisp", BEFORE);
    workspace.set_document("app.lisp", CALL);
    workspace.set_document("unrelated.lisp", "(defun run () 1)");
    let before = workspace.snapshot("macro.lisp").unwrap();
    let config = LintConfig::default();
    let diagnostics = workspace.lint_document("app.lisp", &config);
    let diagnostic = diagnostics
        .iter()
        .find(|diag| diag.rule_id == "perf/mapcar-result-discarded")
        .unwrap();
    assert_eq!(
        diagnostic.related[0].document.as_deref(),
        Some("macro.lisp")
    );
    assert_eq!(
        apply_fixes(CALL, &diagnostics, false).0,
        CALL.replace("mapcar", "mapc")
    );
    workspace.set_document("macro.lisp", AFTER);
    let after = workspace.snapshot("macro.lisp").unwrap();
    let changes = WorkspaceLintChanges::between(Some(&before), Some(&after));
    assert!(changes.affects(&workspace.snapshot("app.lisp").unwrap()));
    assert!(!changes.affects(&workspace.snapshot("unrelated.lisp").unwrap()));
    assert!(
        workspace
            .lint_document("app.lisp", &config)
            .iter()
            .all(|diag| diag.rule_id != "perf/mapcar-result-discarded")
    );
}

#[test]
fn projects_macro_aliases_without_crossing_an_asserted_type() {
    let source = r#"
(in-package :cl-user)
(defmacro aliased-test (test)
  (let* ((copy test) (alias copy)) `(if ,alias :yes :no)))
(defmacro checked (form) `(the boolean ,form))
(defun run (value)
  (aliased-test (not (null value)))
  (if (checked (not (null value))) :yes :no))
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("test.lisp", source);
    let diagnostics = workspace.lint_document("test.lisp", &LintConfig::default());
    let simplifications: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.rule_id == "complexity/double-negation")
        .collect();
    let [simplification] = simplifications.as_slice() else {
        panic!("{diagnostics:?}");
    };
    assert_eq!(
        usize::from(simplification.range.start()),
        source.find("(not (null value))").unwrap()
    );
    assert_eq!(
        usize::from(simplification.related[0].range.start()),
        source.find(",alias").unwrap()
    );
}

#[test]
fn feature_guarded_redefinitions_do_not_claim_a_stable_macro_result_use() {
    const SOURCE: &str = r#"
(defmacro with-report (&body forms) `(progn ,@forms :done))
#+retained (defmacro with-report (&body forms) `(progn ,@forms))
(defun collect-report (rows)
  (with-report (mapcar #'print rows)))
(defun collect-local-report (rows)
  (macrolet (#+retained (with-report (&body forms) `(progn ,@forms)))
    (with-report (mapcar #'print rows))))
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("sample.lisp", SOURCE);
    let config = LintConfig::default();
    let count = |workspace: &SourceWorkspace| {
        workspace
            .lint_document("sample.lisp", &config)
            .into_iter()
            .filter(|diagnostic| diagnostic.rule_id == "perf/mapcar-result-discarded")
            .count()
    };
    assert_eq!(count(&workspace), 0);
    workspace.set_features(Some(Vec::new()));
    assert_eq!(count(&workspace), 2);
    workspace.set_features(Some(vec!["retained".into()]));
    assert_eq!(count(&workspace), 0);
}

#[test]
fn inferred_loop_body_has_the_generated_return_target() {
    let source = r#"
(defmacro visit-all ((item items) &body body) `(dolist (,item ,items) ,@body))
(defmacro visit-tagged ((item items) &body body) `(dolist (,item ,items) restart ,@body))
(defun check (items)
  (visit-all (item items) (if item (return (length (reverse items))) nil))
  (visit-tagged (item items) (unless item (go restart))))
"#;
    let diagnostics = lint(source);
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.rule_id == "perf/length-of-sequence-copy")
    );
    assert!(
        diagnostics.iter().all(|diag| !matches!(
            diag.rule_id,
            "correctness/return-outside-nil-block" | "correctness/unknown-control-target"
        )),
        "{diagnostics:?}"
    );
}

#[test]
fn inferred_templates_support_aux_block_case_mvb_the_and_make_symbol() {
    let source = r#"
(defmacro with-capture-and-branch ((form &aux (tmp (make-symbol "TMP"))) &body body)
  (progn
    `(let ((,tmp ,form))
       (block nil
         (case ,tmp
           (:a ,@body))))))

(defmacro with-mvb-the (pair-form &body body)
  `(the fixnum
        (multiple-value-bind (_a _b) ,pair-form
          (declare (ignore _a _b))
          ,@body)))

(defun run-cases (items val)
  (with-capture-and-branch ((mapcar #'identity items))
    (return (length (reverse items)))
    (mapcar #'print items)
    val)
  (with-mvb-the (values 1 2)
    (mapcar #'print items)
    val))
"#;
    let diagnostics = lint(source);
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.rule_id == "perf/length-of-sequence-copy"),
        "{diagnostics:?}"
    );
    let discarded_count = diagnostics
        .iter()
        .filter(|diag| diag.rule_id == "perf/mapcar-result-discarded")
        .count();
    assert_eq!(discarded_count, 2, "{diagnostics:?}");
    assert!(
        diagnostics
            .iter()
            .all(|diag| diag.rule_id != "correctness/return-outside-nil-block"),
        "{diagnostics:?}"
    );
}

#[test]
fn transparent_macro_bodies_share_lexical_dataflow() {
    let source = r#"
(defmacro with-report (&body forms) `(progn ,@forms))
(defun report (rows)
  (with-report
    (let (status)
      (setq status :draft)
      (setq status :ready)
      (list rows status))))
"#;
    let diagnostics = lint(source);
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    let diagnostic = diagnostics
        .iter()
        .find(|diag| diag.rule_id == "suspicious/overwritten-binding")
        .unwrap();
    assert!(
        diagnostic
            .related
            .iter()
            .any(|note| note.message.contains("macro template"))
    );
    assert_eq!(count, 1, "{diagnostics:?}");
    assert_eq!(fixed, source.replace("      (setq status :draft)\n", ""));
}

#[test]
fn macro_keyword_patterns_select_the_leftmost_executable_operand() {
    let source = r#"
(defmacro guarded ((&key ((:enabled test) nil supplied)) &body forms &environment env)
  (declare (ignore supplied env))
  `(when ,test ,@forms))
(defun run (enabled other xs)
  (guarded (:enabled (if enabled t nil) :enabled (if other t nil))
    (length (reverse xs)))
  (guarded nil (length (reverse xs))))
"#;
    let diagnostics = lint(source);
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 3, "{diagnostics:?}");
    assert_eq!(
        fixed,
        source
            .replace("(if enabled t nil)", "enabled")
            .replace("(length (reverse xs))", "(length xs)")
    );
}

#[test]
fn block_targets_keep_their_source_identity_across_macro_layers() {
    let source = r#"
(defmacro with-exit (name &body body) `(block ,name ,@body))
(defmacro with-fixed-exit (&body body) `(block exit ,@body))
(defun control (rows)
  (with-fixed-exit
    (with-exit stop
      (if rows (return-from exit (length (reverse rows))) (return-from stop 0))))
  (with-exit nil (return :done)))
"#;
    let diagnostics = lint(source);
    assert!(
        diagnostics.iter().all(|diag| {
            diag.rule_id != "correctness/return-outside-nil-block"
                && diag.rule_id != "correctness/unknown-control-target"
        }),
        "{diagnostics:?}"
    );
    let diagnostic = diagnostics
        .iter()
        .find(|diag| diag.rule_id == "perf/length-of-sequence-copy")
        .unwrap();
    assert_eq!(
        diagnostic
            .related
            .iter()
            .filter(|note| note.message.contains("macro template"))
            .count(),
        2
    );
}

#[test]
fn stream_results_and_correctable_case_keep_their_lisp_roles() {
    let source = r#"
(defmacro build-text ((stream) &body forms) `(with-output-to-string (,stream) ,@forms))
(defmacro update-mode (place) `(ccase ,place (:on t) (:off nil)))
(defmacro read-to ((stream index) &body forms)
  `(with-input-from-string (,stream "input" :index ,index) ,@forms))
(defun report (rows flag output)
  (update-mode (first (multiple-value-list (values :on :off))))
  (read-to (stream (nth 0 (multiple-value-list (values :idx)))) (read-char stream))
  (ctypecase (first (multiple-value-list (values 1 2))) (integer 1))
  (list (cond ((if flag t nil)))
        (build-text (stream) (mapcar (lambda (row) (princ row stream)) rows))
        (with-output-to-string (stream) (mapcar (lambda (row) (princ row stream)) rows))
        (with-output-to-string (stream output) (mapcar (lambda (row) (princ row stream)) rows))))
"#;
    let diagnostics = lint(source);
    assert!(
        diagnostics
            .iter()
            .all(|diag| diag.rule_id != "perf/materialized-multiple-values"),
        "{diagnostics:?}"
    );
    let (fixed, count) = apply_fixes(source, &diagnostics, false);
    assert_eq!(count, 2, "{diagnostics:?}");
    assert!(fixed.contains("(cond ((if flag t nil)))"));
    assert!(fixed.contains("(build-text (stream) (mapc "));
    assert!(fixed.contains("(with-output-to-string (stream) (mapc "));
    assert!(fixed.contains("(with-output-to-string (stream output) (mapcar "));
}

#[test]
fn shifted_macro_template_preserves_template_origin_ranges_at_nonzero_offset() {
    let source = r#"
;; leading padding so defmacro starts well past byte offset 0
(defun helper-pad (x) (+ x 1))

(defmacro with-cleanup ((var init) cleanup &body forms)
  `(let ((,var ,init))
     (unwind-protect
          (progn ,@forms)
       ,cleanup)))

(defun run-shifted (res items path)
  (with-cleanup (state (if res t nil))
    (princ state)
    (let ((stream (open path)))
      (when stream
        (close stream)))
    (length (reverse items))))
"#;
    let diagnostics = lint(source);
    let len_diag = diagnostics
        .iter()
        .find(|diag| diag.rule_id == "perf/length-of-sequence-copy")
        .expect("expected length-of-sequence-copy inside shifted macro body");
    let origin = len_diag
        .related
        .iter()
        .find(|note| note.message == "the macro template inserts this operand here")
        .expect("expected macro template origin note");
    let slice = &source[usize::from(origin.range.start())..usize::from(origin.range.end())];
    assert_eq!(slice, ",@forms", "{diagnostics:?}");

    assert!(
        diagnostics
            .iter()
            .all(|diag| diag.rule_id != "suspicious/unguarded-open"),
        "expected protected operand inside unwind-protect to inherit open_guard: {diagnostics:?}"
    );
}
