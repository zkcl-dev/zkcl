//! The Lisp source API works in an ordinary runtime without foreign-call support.

use zkcl::Runtime;

mod common;

use common::lisp::{assert_truthy, eval_reprs};

fn syntax_runtime() -> Runtime {
    let runtime = Runtime::start().unwrap();
    assert_truthy(&runtime, "(null (find-package :zk-syntax))");
    runtime.eval("(require :zk-syntax)").unwrap();
    runtime
}

#[test]
fn syntax_is_a_lazy_builtin_available_through_require() {
    let runtime = syntax_runtime();
    assert_truthy(
        &runtime,
        r#"(let ((workspace (zk-syntax:make-workspace)))
             (and (zk-syntax:workspacep workspace)
                  (zk-syntax:snapshotp
                    (zk-syntax:set-document workspace "buffer" "(hello)"))))"#,
    );
    assert_truthy(
        &runtime,
        r#"(let ((before (symbol-function 'zk-syntax:make-workspace)))
             (require :zk-syntax)
             (eq before (symbol-function 'zk-syntax:make-workspace)))"#,
    );
}

#[test]
fn asdf_can_load_the_bundled_syntax_system() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = Runtime::start().unwrap();
            runtime.eval("(require :asdf)").unwrap();
            runtime.eval(r#"(asdf:load-system "zk-syntax")"#).unwrap();
            assert_truthy(
                &runtime,
                r#"(zk-syntax:workspacep (zk-syntax:make-workspace))"#,
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn lossless_queries_keep_reader_forms_trivia_and_character_units_distinct() {
    let runtime = syntax_runtime();
    assert_truthy(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document
                           workspace "buffer" "(雪 😀) ; keep")))
             (and (= (zk-syntax:byte-offset source 3) 5)
                  (= (zk-syntax:character-index source 9) 4)
                  (handler-case
                      (progn (zk-syntax:character-index source 2) nil)
                    (error () t))
                  (handler-case
                      (progn (zk-syntax:byte-offset source -1) nil)
                    (type-error () t))
                  (equal (mapcar #'zk-syntax:kind
                                 (zk-syntax:children (zk-syntax:root source)))
                         '(:list :whitespace :line-comment))
                  (string= (zk-syntax:node-text (zk-syntax:node-at source 12))
                           "; keep")))"#,
    );
    assert_truthy(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document
                           workspace "reader" "'x #'f #+feature (a . b)")))
             (and (equal (mapcar #'zk-syntax:kind
                                (zk-syntax:forms (zk-syntax:root source)))
                         '(:reader-form :reader-form :reader-form))
                  (string= (zk-syntax:node-text
                             (first (zk-syntax:forms (zk-syntax:root source))))
                           "'x")
                  (zk-syntax:node-equal-p
                    (zk-syntax:root source)
                    (zk-syntax:root (zk-syntax:node-snapshot
                                     (zk-syntax:node-at source 1))))))"#,
    );
}

#[test]
fn source_analysis_never_invokes_the_runtime_reader() {
    let runtime = syntax_runtime();
    runtime
        .eval("(defparameter *source-read-eval-ran* nil)")
        .unwrap();
    assert_truthy(
        &runtime,
        r##"(let* ((workspace (zk-syntax:make-workspace))
                  (source (zk-syntax:set-document
                            workspace "untrusted"
                            "#.(setq *source-read-eval-ran* t) (unfinished")))
              (and (not *source-read-eval-ran*)
                   (zk-syntax:diagnostics source)
                   (getf (first (zk-syntax:diagnostics source))
                         :needs-more-input)
                   (string= (zk-syntax:node-text
                              (first (zk-syntax:forms (zk-syntax:root source))))
                            "#.(setq *source-read-eval-ran* t)")))"##,
    );
}

#[test]
fn nodes_and_edits_retain_their_snapshot_across_edit_remove_and_moving_gc() {
    let runtime = syntax_runtime();
    runtime
        .load_str(
            r#"(defparameter *syntax-retained-node* nil)
               (defparameter *syntax-retained-edit* nil)
               (let* ((workspace (zk-syntax:make-workspace))
                      (source (zk-syntax:set-document workspace "buffer" "(alpha beta)")))
                 (setq *syntax-retained-node* (zk-syntax:node-at source 1)
                       *syntax-retained-edit*
                         (zk-syntax:structural-edit source 1 1 :barf-forward))
                 (zk-syntax:set-document workspace "buffer" "(new text)")
                 (zk-syntax:remove-document workspace "buffer"))
               (dotimes (i 500) (make-string 128 :initial-element #\x))
               (zk:gc)"#,
        )
        .unwrap();
    assert_truthy(
        &runtime,
        r#"(and (string= (zk-syntax:node-text *syntax-retained-node*) "alpha")
                (zk-syntax:snapshot-equal-p
                  (zk-syntax:node-snapshot *syntax-retained-node*)
                  (zk-syntax:edit-source *syntax-retained-edit*))
                (string= (zk-syntax:apply-edit *syntax-retained-edit*)
                         "(alpha) beta"))"#,
    );
    runtime
        .load_str(
            r#"(defparameter *syntax-node-weak*
                 (zk:make-weak-pointer *syntax-retained-node*))
               (defparameter *syntax-edit-weak*
                 (zk:make-weak-pointer *syntax-retained-edit*))
               (setq *syntax-retained-node* nil
                     *syntax-retained-edit* nil)
               (zk:gc)"#,
        )
        .unwrap();
    assert_truthy(
        &runtime,
        r#"(and (null (zk:weak-pointer-value *syntax-node-weak*))
                (null (zk:weak-pointer-value *syntax-edit-weak*)))"#,
    );
}

#[test]
fn directed_custom_edits_are_atomic_and_reject_stale_documents() {
    let runtime = syntax_runtime();
    assert_truthy(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document workspace "buffer" "abc"))
                 (edit (zk-syntax:make-edit
                         source '((0 0 "(") (3 3 ")")) 4 1)))
             (and (equal (multiple-value-list (zk-syntax:edit-selection edit)) '(4 1))
                  (string= (zk-syntax:apply-edit edit) "(abc)")
                  (handler-case
                      (progn
                        (let ((other (zk-syntax:make-workspace)))
                          (zk-syntax:set-document other "buffer" "abc")
                          (zk-syntax:apply-edit-to-document other "buffer" edit))
                        nil)
                    (zk-syntax:edit-error (condition)
                      (eq (zk-syntax:edit-error-reason condition) :stale-snapshot)))
                  (progn
                    (zk-syntax:replace-text workspace "buffer" 0 1 "z")
                    (handler-case
                        (progn
                          (zk-syntax:apply-edit-to-document workspace "buffer" edit)
                          nil)
                      (zk-syntax:edit-error (condition)
                        (eq (zk-syntax:edit-error-reason condition) :stale-snapshot))))
                  (string= (zk-syntax:text (zk-syntax:snapshot workspace "buffer"))
                           "zbc")))"#,
    );
    assert_truthy(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document workspace "buffer" "abc")))
             (and (handler-case
                      (progn
                        (zk-syntax:make-edit
                          source '((0 2 "x") (1 3 "y")) 0 0)
                        nil)
                    (zk-syntax:edit-error (condition)
                      (eq (zk-syntax:edit-error-reason condition) :overlapping-edits)))
                  (string= (zk-syntax:text (zk-syntax:snapshot workspace "buffer"))
                           "abc")))"#,
    );
}

#[test]
fn native_structural_commands_keep_form_boundaries_and_report_refusals() {
    let runtime = syntax_runtime();
    assert_truthy(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document workspace "buffer" "(foo)bar"))
                 (edit (zk-syntax:structural-edit source 1 1 :splice))
                 (after (zk-syntax:apply-edit-to-document workspace "buffer" edit)))
             (and (string= (zk-syntax:text after) "foo bar")
                  (string= (zk-syntax:text source) "(foo)bar")
                  (equal (mapcar #'zk-syntax:node-text
                                 (zk-syntax:forms (zk-syntax:root after)))
                         '("foo" "bar"))))"#,
    );
    let reason = eval_reprs(
        &runtime,
        r#"(let* ((workspace (zk-syntax:make-workspace))
                 (source (zk-syntax:set-document workspace "buffer" "(a . b) c")))
             (handler-case
                 (zk-syntax:structural-edit source 1 1 :slurp-forward)
               (zk-syntax:edit-error (condition)
                 (zk-syntax:edit-error-reason condition))))"#,
    );
    assert_eq!(reason, [":DOTTED-BOUNDARY"]);
}
