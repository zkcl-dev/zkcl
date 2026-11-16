use std::fs;

use zkcl::{ConditionClass, EvalErrorKind, ExecutionStop, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::eval;

fn printed(runtime: &Runtime, source: &str) -> String {
    let values = eval(runtime, source);
    assert_eq!(values.len(), 1, "{source}");
    runtime.string(&values[0]).expect("a printed string")
}

#[test]
fn compiled_printer_macros_render_after_fasl_load() {
    let temporary = TempDir::new("printer-fasl");
    let source = temporary.path().join("printer.lisp");
    fs::write(
        &source,
        r#"(defun render-pretty ()
              (let ((*print-pretty* t))
                (with-output-to-string (out)
                  (pprint-logical-block (out '(hello) :prefix "<" :suffix ">")
                    (princ (pprint-pop) out)))))
           (defun render-unreadable ()
             (with-output-to-string (out)
               (print-unreadable-object (nil out)
                 (write-string "hello" out))))"#,
    )
    .unwrap();
    let compiled = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&compiled).unwrap();
    assert_eq!(printed(&runtime, "(render-pretty)"), "<HELLO>");
    assert_eq!(printed(&runtime, "(render-unreadable)"), "#<hello>");
}

#[test]
fn suspended_logical_block_keeps_its_sharing_labels_with_its_task() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-suspended-printer", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    let values = eval(
        &runtime,
        r#"(let ((started nil) (finished nil))
            (zk-async:with-task-group (group)
              (let ((child (zk-async:spawn-task (group)
                             (let ((item (list :child)) (*print-circle* t))
                               (with-output-to-string (out)
                                 (pprint-logical-block (out (list item item) :prefix "(" :suffix ")")
                                   (write (pprint-pop) :stream out)
                                   (setf started t)
                                   (loop until finished do (zk-async:yield))
                                   (write-char #\Space out)
                                   (write (pprint-pop) :stream out)))))))
                (loop until started do (zk-async:yield))
                (collect-suspended-printer)
                (let* ((item (list :parent))
                       (parent (write-to-string (list item item) :circle t)))
                  (setf finished t)
                  (values parent (zk-async:await child))))))"#,
    );
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("(#1=(:PARENT) #1#)")
    );
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("(#1=(:CHILD) #1#)")
    );
}

#[test]
fn spawned_task_starts_at_its_own_logical_block_depth() {
    let runtime = Runtime::start().unwrap();
    let output = printed(
        &runtime,
        r#"(let ((*print-level* 1))
            (flet ((print-block (out)
                     (pprint-logical-block (out '(:child) :prefix "[" :suffix "]")
                       (write (pprint-pop) :stream out))))
              (with-output-to-string (out)
                (pprint-logical-block (out '(:parent))
                  (print-block out)
                  (write-char #\Space out)
                  (zk-async:with-task-group (group)
                    (write-string
                     (zk-async:await
                      (zk-async:spawn-task (group)
                        (with-output-to-string (child-out)
                          (print-block child-out)
                          (pprint-logical-block (child-out '(:nested))
                            (print-block child-out)))))
                     out))))))"#,
    );
    assert_eq!(output, "# [:CHILD]#");
}

#[test]
fn exhausted_step_budget_discards_the_abandoned_logical_block() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defvar *abandoned-logical-block-stream* nil)
               (defun run-until-budget-exhausted ()
                 (with-output-to-string (out)
                   (let ((*print-circle* t))
                     (pprint-logical-block (out (list :budget))
                       (setf *abandoned-logical-block-stream* (zk:make-weak-pointer out))
                       (loop)))))"#,
        )
        .unwrap();
    runtime.set_step_budget(Some(2048));
    let error = runtime.eval("(run-until-budget-exhausted)").err().unwrap();
    assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));

    runtime.set_step_budget(None);
    assert!(!eval(&runtime, "*abandoned-logical-block-stream*")[0].is_nil());
    runtime.collect();
    assert!(
        eval(
            &runtime,
            "(zk:weak-pointer-value *abandoned-logical-block-stream*)"
        )[0]
        .is_nil()
    );
    assert_eq!(
        printed(
            &runtime,
            "(let ((item (list :after))) (write-to-string (list item item) :circle t))"
        ),
        "(#1=(:AFTER) #1#)"
    );
}

#[test]
fn host_printer_renders_immediate_values_before_reader_initialization() {
    let runtime = Runtime::start().unwrap();
    // The host can render immediate values before initializing the reader.
    assert_eq!(runtime.repr(&runtime.nil()).unwrap(), "NIL");
    assert_eq!(runtime.repr(&runtime.fixnum(-7).unwrap()).unwrap(), "-7");
}

#[test]
fn write_to_string_applies_lisp_print_controls() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(write-to-string 42 :base 10 :radix nil)", "42"),
        ("(write-to-string -42 :base 16 :radix t)", "#x-2A"),
        ("(write-to-string 42 :base 3 :radix t)", "#3r1120"),
        ("(write-to-string 42 :base 10 :radix t)", "42."),
        ("(write-to-string 'face :base 16)", "|FACE|"),
        (
            "(write-to-string '(a (b c) d e) :level 1 :length 3)",
            "(A # D ...)",
        ),
        ("(write-to-string '(a b . c) :length 2)", "(A B . C)"),
        ("(write-to-string '(a b . c) :length 1)", "(A ...)"),
        ("(write-to-string '(a b . c) :length 0)", "(...)"),
        (
            "(write-to-string '(some-name :a) :case :capitalize)",
            "(Some-Name :A)",
        ),
        (
            "(let ((*print-base* 8) (*print-radix* t)) (prin1-to-string 42))",
            "#o52",
        ),
        ("(let ((*print-level* 0)) (prin1-to-string '(1 2)))", "#"),
        (
            "(let ((*print-escape* nil)) (prin1-to-string \"text\"))",
            "\"text\"",
        ),
        (
            "(let ((*print-readably* t)) (princ-to-string \"text\"))",
            "text",
        ),
        (
            "(write-to-string (make-symbol \"MiXeD\") :gensym nil)",
            "|MiXeD|",
        ),
        (
            "(write-to-string '(1 (2 . 3)) :readably t :length 0 :level 0)",
            "(1 (2 . 3))",
        ),
        (r#"(prin1-to-string (make-symbol "a|b"))"#, r"#:|a\|b|"),
        (r#"(prin1-to-string (make-symbol ""))"#, "#:||"),
        (r#"(prin1-to-string (make-symbol "1"))"#, "#:|1|"),
        (r#"(prin1-to-string '|\#dispatch|)"#, "|#dispatch|"),
        (r#"(prin1-to-string "a\"b\\c")"#, r#""a\"b\\c""#),
        (
            r#"(write-to-string #P"/tmp/printer.lisp" :readably t)"#,
            r#"#P"/tmp/printer.lisp""#,
        ),
        (
            r#"(princ-to-string #P"/tmp/printer.lisp")"#,
            "/tmp/printer.lisp",
        ),
    ] {
        assert_eq!(printed(&runtime, source), expected, "{source}");
    }
}

#[test]
fn printed_symbols_track_package_accessibility() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let* ((foreign (make-package "Print-Foreign" :use nil))
                  (visible (intern "EXPORTED" foreign))
                  (hidden (intern "PRINT-HIDDEN-NAME" foreign)))
             (export visible foreign)
             (values visible hidden))"#,
        )
        .unwrap();
    let visible = &values[0];
    let hidden = &values[1];
    assert_eq!(runtime.repr(visible).unwrap(), "|Print-Foreign|:EXPORTED");
    assert_eq!(
        runtime.repr(hidden).unwrap(),
        "|Print-Foreign|::PRINT-HIDDEN-NAME"
    );

    for symbol in [visible, hidden] {
        let printed = runtime.repr(symbol).unwrap();
        let read = runtime.eval(&format!("'{printed}")).unwrap();
        assert!(read[0].same_identity(symbol));
    }

    runtime
        .eval("(import '|Print-Foreign|::PRINT-HIDDEN-NAME)")
        .unwrap();
    assert_eq!(runtime.repr(hidden).unwrap(), "PRINT-HIDDEN-NAME");
    runtime
        .eval("(unintern '|Print-Foreign|::PRINT-HIDDEN-NAME \"Print-Foreign\")")
        .unwrap();
    runtime.collect();
    assert_eq!(runtime.repr(hidden).unwrap(), "#:PRINT-HIDDEN-NAME");

    let empty_package = printed(
        &runtime,
        "(let* ((package (make-package \"\" :use nil)) (x (intern \"ATOM\" package))) (write-to-string x))",
    );
    assert_eq!(empty_package, "||::ATOM");

    assert_eq!(
        printed(
            &runtime,
            r#"(let ((*package* (make-package "Print-Bare" :use nil)))
                               (write-to-string 't))"#
        ),
        "COMMON-LISP:T"
    );
}

#[test]
fn readable_output_roundtrips_escaped_atoms() {
    let runtime = Runtime::start().unwrap();
    let roundtrip = runtime
        .eval(
            r#"(let* ((value '(|a\|b| |a\\b| |é| :|3| |.| |+1e0| "a\"b\\c" -42 . |tail ends|))
                  (printed (write-to-string value :readably t))
                  (read-back (read-from-string printed)))
             (equal value read-back))"#,
        )
        .unwrap();
    assert!(!roundtrip[0].is_nil());
}

#[test]
fn readable_output_identifies_the_unprintable_object() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(let ((function (lambda () 1)))
           (handler-case (write-to-string (list function) :readably t)
             (print-not-readable (condition)
               (values function (print-not-readable-object condition)))))",
        )
        .unwrap();
    assert!(values[0].same_identity(&values[1]));
    assert_eq!(runtime.repr(&values[0]).unwrap(), "#<FUNCTION>");

    let values = runtime
        .eval(
            r#"(let ((path (make-pathname :name "report" :version 7)))
                (handler-case (write-to-string path :readably t)
                  (print-not-readable (condition)
                    (eq path (print-not-readable-object condition)))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn write_to_string_rejects_invalid_print_options() {
    let runtime = Runtime::start().unwrap();
    let error = runtime.eval("(write-to-string 42 :base 37)").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    let values = runtime
        .eval(
            "(handler-case (write-to-string '(1 2) :length -1)
               (type-error (condition)
                 (typep nil (type-error-expected-type condition))))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn printer_traverses_deep_cons_trees_iteratively() {
    let runtime = Runtime::start().unwrap();
    // Traversal consumes the heap graph without using the native call stack.
    let nil = runtime.nil();
    let mut deep = runtime.fixnum(3).unwrap();
    for _ in 0..20_000 {
        deep = runtime.cons(&deep, &nil).unwrap();
    }
    let actual = runtime.repr(&deep).unwrap();
    assert_eq!(actual, "(".repeat(20_000) + "3" + &")".repeat(20_000));
}

#[test]
fn float_array_elements_reach_pprint_dispatch() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-printer", |rt, _| {
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    let forms = [
        (
            r#"(write-to-string (make-array 2 :element-type 'double-float
                                            :initial-contents '(-0.0d0 2.5d0))
                               :pretty nil)"#,
            "#(-0.0d0 2.5d0)",
        ),
        (
            r#"(write-to-string (make-array '(2 1) :element-type 'single-float
                                            :initial-contents '((1.0f0) (2.0f0)))
                               :pretty nil :length 1)"#,
            "#2A((1.0) ...)",
        ),
        (
            r#"(let ((*print-pprint-dispatch* (copy-pprint-dispatch)))
                 (set-pprint-dispatch 'single-float
                   (lambda (stream value)
                     (collect-printer)
                     (write (truncate value) :stream stream :pretty nil)))
                 (write-to-string (make-array 2 :element-type 'single-float
                                              :initial-contents '(1.5f0 2.5f0))
                                  :pretty t))"#,
            "#(1 2)",
        ),
        (
            r#"(let ((array (make-array 1 :element-type 'single-float)))
                 (handler-case (write-to-string array :readably t)
                   (print-not-readable (condition)
                     (if (eq array (print-not-readable-object condition)) "same array" "wrong object"))))"#,
            "same array",
        ),
    ];
    for (source, expected) in forms {
        assert_eq!(printed(&runtime, source), expected, "{source}");
    }
}

#[test]
fn recursive_print_object_calls_share_the_enclosing_print_context() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-printer", |rt, _| {
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    for form in [
        r#"(defclass printer-box () ((value :initarg :value :accessor printer-value)))"#,
        r#"(defmethod print-object ((box printer-box) stream)
           (write-char #\< stream) (collect-printer) (write (printer-value box) :stream stream) (write-char #\> stream))"#,
        r#"(define-condition printer-problem (error) () (:report "usual report"))"#,
        r#"(defmethod print-object :around ((c printer-problem) stream)
           (write-char #\[ stream) (call-next-method) (write-char #\] stream))"#,
        r#"(defstruct printer-record value)"#,
        r#"(defvar *printer-box* (make-instance 'printer-box :value "text"))"#,
    ] {
        runtime
            .eval(form)
            .unwrap_or_else(|err| panic!("{form}: {err}"));
    }
    runtime
        .eval("(define-condition base-report (error) ())")
        .unwrap();
    runtime
        .eval(r#"(defmethod print-object ((x base-report) s) (write-string "base" s))"#)
        .unwrap();
    runtime
        .eval(r#"(define-condition child-report (base-report) () (:report "child"))"#)
        .unwrap();
    runtime.eval("(defclass repeated-output () ())").unwrap();
    runtime
        .eval("(defvar *printer-shared* (list 'shared))")
        .unwrap();
    runtime
        .eval(
            r#"(defmethod print-object ((o repeated-output) s)
        (format s "~S ~S" *printer-shared* *printer-shared*))"#,
        )
        .unwrap();
    for (source, expected) in [
        ("(prin1-to-string *printer-box*)", "<\"text\">"),
        ("(princ-to-string *printer-box*)", "<text>"),
        (
            r#"(format nil "~A/~S/~W" *printer-box* *printer-box* *printer-box*)"#,
            "<text>/<\"text\">/<\"text\">",
        ),
        (
            "(with-output-to-string (stream) (prin1 (vector *printer-box* 'after) stream))",
            "#(<\"text\"> AFTER)",
        ),
        (
            "(write-to-string (make-printer-record :value *printer-box*) :escape nil)",
            "#S(PRINTER-RECORD :VALUE <text>)",
        ),
        (
            "(write-to-string (list *printer-box*) :pretty t :escape nil)",
            "(<text>)",
        ),
        (
            "(princ-to-string (list (make-condition 'printer-problem)))",
            "([usual report])",
        ),
        (
            "(let ((x (make-instance 'printer-box :value '(1 2)))) (write-to-string (list x) :level 2))",
            "(<#>)",
        ),
        (
            "(let ((x (make-instance 'printer-box :value nil))) (setf (printer-value x) x) (write-to-string x :circle t))",
            "#1=<#1#>",
        ),
        (
            "(write-to-string (list *printer-box* *printer-box*) :circle t :escape nil)",
            "(#1=<text> #1#)",
        ),
        ("(write-to-string *printer-box* :readably t)", "<\"text\">"),
        (
            "(write-to-string (make-instance 'repeated-output) :circle t)",
            "#1=(SHARED) #1#",
        ),
        ("(princ-to-string (make-condition 'child-report))", "child"),
        ("(prin1-to-string (make-condition 'child-report))", "base"),
        (
            r#"(let ((x (make-instance 'standard-object)))
                (handler-case (write-to-string x :readably t)
                  (print-not-readable (c)
                    (if (eq (print-not-readable-object c) x) "caught original" "wrong object"))))"#,
            "caught original",
        ),
    ] {
        assert_eq!(printed(&runtime, source), expected, "{source}");
    }
}

#[test]
fn stream_specializers_invalidate_cached_print_methods() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"(defclass destination-aware () ())
        (defvar *for-printing* (make-instance 'destination-aware))
        (defmethod print-object ((x destination-aware) s) (write-string "general" s))"#,
    )
    .unwrap();
    for _ in 0..2 {
        assert_eq!(printed(&rt, "(prin1-to-string *for-printing*)"), "general");
    }
    rt.eval(
        r#"(defmethod print-object ((x destination-aware) (s string-stream))
                   (write-string "specific" s))"#,
    )
    .unwrap();
    assert_eq!(printed(&rt, "(prin1-to-string *for-printing*)"), "specific");
    rt.eval(
        r#"(defmethod print-object ((x destination-aware) (s string-stream))
                   (write-string "redefined" s))"#,
    )
    .unwrap();
    assert_eq!(
        printed(&rt, "(prin1-to-string *for-printing*)"),
        "redefined"
    );
    assert_eq!(
        printed(
            &rt,
            "(with-output-to-string (s) (prin1 *for-printing* (make-broadcast-stream s)))"
        ),
        "general"
    );
}

#[test]
fn print_object_obeys_the_callers_dynamic_control_stack() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (define-condition printer-ping (condition) ())
        (defclass control-printer () ())
        (defvar *printer-action*)
        (defvar *printer-cleanups*)
        (defmethod print-object ((object control-printer) stream)
          (unwind-protect
              (ecase *printer-action*
                (:signal (signal 'printer-ping))
                (:restart (cerror "Resume printing" "test condition"))
                (:throw (throw 'outside :transferred)))
            (incf *printer-cleanups*))
          (write-string "ok" stream))
        (defun print-event (op object stream)
          (ecase op
            (:plain (princ object stream))
            (:nested (prin1 (list object) stream))
            (:format (format stream "~A" object))))"#,
    )
    .unwrap();
    for (op, output) in [(":plain", "ok"), (":nested", "(ok)"), (":format", "ok")] {
        for action in [":signal", ":restart"] {
            let source = format!(
                r#"(let ((*printer-action* {action}) (*printer-cleanups* 0) (seen 0))
                (let ((printed (handler-bind
                                 ((printer-ping (lambda (c) (declare (ignore c)) (incf seen)))
                                  (simple-error (lambda (c) (incf seen) (invoke-restart (find-restart 'continue c)))))
                               (with-output-to-string (s) (print-event {op} (make-instance 'control-printer) s)))))
                  (and (string= printed "{output}") (= seen 1) (= *printer-cleanups* 1))))"#
            );
            assert!(
                !rt.eval(&source)
                    .unwrap_or_else(|e| panic!("{op} {action}: {e}"))[0]
                    .is_nil(),
                "{op} {action}"
            );
        }
        let source = format!(
            r#"(let ((*printer-action* :throw) (*printer-cleanups* 0) (*print-escape* :outside))
            (and (eq (catch 'outside
                       (let ((*print-escape* :inside))
                         (with-output-to-string (s) (print-event {op} (make-instance 'control-printer) s))))
                     :transferred)
                 (= *printer-cleanups* 1) (eq *print-escape* :outside)))"#
        );
        assert!(
            !rt.eval(&source)
                .unwrap_or_else(|e| panic!("{op} throw: {e}"))[0]
                .is_nil(),
            "{op} throw"
        );
    }
}
