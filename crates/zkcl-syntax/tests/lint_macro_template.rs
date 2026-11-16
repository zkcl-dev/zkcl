#![cfg(feature = "analysis")]

use zkcl_syntax::{LintConfig, LintDiagnostic, LintPreset, SourceWorkspace};

const MULTIPLE_EVALUATION: &str = "suspicious/macro-multiple-evaluation";

fn lint_macro_evaluations(src: &str) -> Vec<LintDiagnostic> {
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("test.lisp", src);
    workspace
        .lint_document("test.lisp", &LintConfig::from_preset(LintPreset::Strict))
        .into_iter()
        .filter(|diag| diag.rule_id == MULTIPLE_EVALUATION)
        .collect()
}

#[test]
fn gray_compound_stream_method_templates_do_not_repeat_evaluation() {
    const PACKAGE_AND_STREAMS: &str =
        include_str!("../../zkcl-runtime/lisp/runtime/streams/gray-streams.lisp");
    const COMPOUND_STREAMS: &str =
        include_str!("../../zkcl-runtime/lisp/runtime/streams/gray-compound-streams.lisp");
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("gray-streams.lisp", PACKAGE_AND_STREAMS);
    workspace.set_document("gray-compound-streams.lisp", COMPOUND_STREAMS);
    let diags: Vec<_> = workspace
        .lint_document(
            "gray-compound-streams.lisp",
            &LintConfig::from_preset(LintPreset::Strict),
        )
        .into_iter()
        .filter(|diag| diag.rule_id == MULTIPLE_EVALUATION)
        .collect();
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn unquoted_names_and_literals_are_excluded_from_evaluation_counts() {
    let src = r#"
(defmacro variable-name (name)
  `(let (,name (,name 0))
     (dolist (,name nil)
       (dotimes (,name 1)
         (multiple-value-bind (,name) (values)
           ,name)))))
(defmacro definition-names (name)
  `(defun ,name (,name)
     (lambda (,name &optional (,name 0 ,name)) ,name)))
(defmacro name-and-data (name)
  `(locally
     (declare (special ,name))
     (list ',name (cl:quote ,name) #',name (function (setf ,name)) (,name) ,name)))
(defmacro class-specializer (class)
  `(defmethod f :before ((x ,class) (y ,class)) ,class))
(defmacro conditional-body (x)
  `(cond ((ready) (+ ,x 1)) (t (- ,x 1)) (nil (list ,x ,x))))
(defmacro type-assertion (type)
  `(list (the ,type x) (the ,type y)))
(defmacro emit-to-string (stream)
  `(with-output-to-string (,stream) (write-string "Report" ,stream)))
(defmacro read-string (stream index)
  `(with-input-from-string (,stream "data" :index ,index) (list (read-char ,stream) ,index)))
"#;
    let diags = lint_macro_evaluations(src);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn repeated_unquoted_expressions_point_to_the_second_occurrence() {
    for src in [
        "(defmacro args (form) `(unknown ,form ,form))",
        "(defmacro funcall-args (form) `(funcall ,form ,form))",
        "(defmacro binding (form) `(let ((v ,form)) ,form))",
        "(defmacro loop-count (form) `(dotimes (i ,form ,form)))",
        "(defmacro mvb (form) `(multiple-value-bind (x) ,form ,form))",
        "(defmacro read-then-inspect (form) `(with-open-file (file ,form) (list (read file) ,form)))",
        "(defmacro cond-test (form) `(cond (,form ,form)))",
        "(defmacro cond-next-test (form) `(cond (,form :early) (,form :later)))",
        "(defmacro branch (form) `(if ,form 1 ,form))",
        "(defmacro lambda-default (form) `(lambda (&optional (x ,form)) ,form))",
        "(defmacro method-body (form) `(defmethod f (x) (+ ,form ,form)))",
        "(defmacro method-eql (form) `(defmethod f ((x (eql ,form)) (y (eql ,form)))))",
        "(defmacro fn-quote (form) `#'(lambda () (+ ,form ,form)))",
    ] {
        let diags = lint_macro_evaluations(src);
        assert_eq!(diags.len(), 1, "{src}: {diags:?}");
        let range = diags[0].range;
        let second = src.rfind("form").unwrap();
        assert_eq!(
            usize::from(range.start())..usize::from(range.end()),
            second..second + 4,
            "{src}"
        );
    }
}

#[test]
fn counts_operand_aliases_and_respects_generated_and_expander_bindings() {
    let source = r#"
(in-package :cl-user)
(defmacro with-resource ((name form) &body body)
  `(let* ((,name ,form) (_copy (list ,name ,name)))
     (unwind-protect (progn (consume ,name) ,@body) (release ,name))))
(defmacro lambda-bindings (name)
  `#'(lambda (,name &optional (_copy (list ,name ,name)))
       (list ,name ,name)))
(defmacro destructure-bindings (name form)
  `(destructuring-bind (,name) ,form (list ,name ,name)))
(defmacro passthrough ((name form) &body body)
  `(with-resource (,name ,form) (consume ,name) (release ,name) ,@body))
(defmacro alias-repeat (form)
  (let* ((alias form) (other alias)) `(list ,alias ,other)))
(defmacro new-local-not-an-operand (form)
  (let ((form (gensym))) `(list ,form ,form)))
(defmacro guarded-branch-only (form)
  (if (consp form)
      (once-only (form) `(list ,form ,form))
      `(list ,form ,form)))
(defmacro parallel-initializers (form)
  `(let ((,form 0) (_copy (list ,form ,form))) ,form))
(defmacro rebound-after-scope (form)
  `(list (let ((,form 0)) (list ,form ,form)) ,form ,form))
(defmacro overwritten-expander-local (form)
  (let ((alias form)) (setq alias :constant) `(list ,alias ,alias)))
"#;
    let diagnostics = lint_macro_evaluations(source);
    let locations: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| {
            let start = usize::from(diagnostic.range.start());
            let end = usize::from(diagnostic.range.end());
            let definition = diagnostic.related.last().unwrap().range;
            (
                &source[start..end],
                &source[usize::from(definition.start())..usize::from(definition.end())],
            )
        })
        .collect();
    assert_eq!(
        locations,
        [
            ("other", "form"),
            ("form", "form"),
            ("form", "form"),
            ("form", "form")
        ]
    );
}

#[test]
fn a_large_template_keeps_following_macros_lintable() {
    let large = ",form ".repeat(10_000);
    let source = format!(
        "(defmacro large (form) `(list {large}))\n(defmacro small (form) `(list ,form ,form))"
    );
    let diagnostics = lint_macro_evaluations(&source);
    let [diagnostic] = diagnostics.as_slice() else {
        panic!("{diagnostics:?}");
    };
    assert_eq!(
        usize::from(diagnostic.range.start()),
        source.rfind("form").unwrap()
    );
}

#[test]
fn once_only_pair_syntax_and_mvb_destructuring_hygiene() {
    let guarded = r#"
(defmacro safe-once ((form) &body body)
  (once-only ((form form))
    `(progn ,form ,form ,@body)))
"#;
    assert!(lint_macro_evaluations(guarded).is_empty());

    let unhygienic = r#"
(defmacro leaky-bind (form &body body)
  `(multiple-value-bind (res) ,form ,@body))
(defmacro leaky-dbind (form &body body)
  `(destructuring-bind (head &optional tail) ,form (list head tail ,@body)))
"#;
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("test.lisp", unhygienic);
    let diags: Vec<_> = workspace
        .lint_document("test.lisp", &LintConfig::from_preset(LintPreset::Strict))
        .into_iter()
        .filter(|diag| diag.rule_id == "suspicious/macro-unhygienic-binding")
        .collect();
    assert_eq!(diags.len(), 3, "{diags:?}");
}
