use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

#[test]
fn label_references_reuse_the_original_symbol() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(eval_reprs(&runtime, "((lambda (#1=#:foo) #1#) 17)"), ["17"]);
    assert_eq!(
        eval_reprs(&runtime, "((lambda (&rest #1=#:foo) #1#) 'a 'b)"),
        ["(A B)"]
    );
    assert_eq!(
        eval_reprs(&runtime, "(eq '#1=#:foo '#1#)"),
        ["T"],
        "a label reference reuses the labelled token"
    );
    assert_eq!(
        eval_reprs(&runtime, "(let ((x '(1 2))) (eq '#0=#:name '#0#))"),
        ["T"]
    );

    for source in ["'#1#", "'(#1=(a) #1=(b))", "'#x="] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report a reader error"
        );
    }
}

#[test]
fn recursive_read_macros_share_the_enclosing_label_scope() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-reader-heap", |runtime, args| {
            runtime.collect();
            Ok(args.to_vec())
        })
        .unwrap();
    let source = r##"
        (let ((*readtable* (copy-readtable nil)))
          (set-macro-character #\! (lambda (s c) (declare (ignore c))
                                    (collect-reader-heap (read s t nil t))))
          (set-macro-character #\[ (lambda (s c) (declare (ignore c))
                                    (read-delimited-list #\] s t)))
          (set-syntax-from-char #\] #\))
          (set-macro-character #\? (lambda (s c) (declare (ignore c))
                                    (let ((v (read s t nil t)))
                                      (collect-reader-heap (vector v v)))))
          (set-macro-character #\% (lambda (s c) (declare (ignore c)) (read s)))
          (let* ((result (read-from-string "(#1=(a) !#1# !#2=(b) #2# [#1# #2#])"))
                 (c (read-from-string "#3=(head . !#3#)"))
                 (v (read-from-string "#4=#(!#4#)"))
                 (m (read-from-string "#7=?#7#"))
                 (aliases (read-from-string "(#5=(#6=!#5#) #6#)"))
                 (local (read-from-string "(#1=(a) %#1=(b) !#1#)")))
            (and (eq (first result) (second result))
                 (eq (third result) (fourth result))
                 (eq (first result) (first (fifth result)))
                 (eq (third result) (second (fifth result)))
                 (eq c (cdr c)) (eq v (aref v 0))
                 (eq m (aref m 0)) (eq m (aref m 1))
                 (eq (first aliases) (second aliases))
                 (eq (first aliases) (caar aliases))
                 (eq (first local) (third local))
                 (not (eq (first local) (second local)))
                 (handler-case (progn (read-from-string "(!#2=(a) #2=(b))") nil)
                   (reader-error () t))
                 (handler-case (progn (read-from-string "#1=!#1#") nil)
                   (reader-error () t)))))"##;
    assert_eq!(eval_reprs(&runtime, source), ["T"]);
}

#[test]
fn recursive_read_inherits_the_outer_whitespace_mode() {
    let runtime = Runtime::start().unwrap();
    let source = r##"
        (let ((*readtable* (copy-readtable nil)))
          (set-macro-character #\! (lambda (s c) (declare (ignore c))
                                    (read s t nil t)))
          (set-macro-character #\? (lambda (s c) (declare (ignore c))
                                    (read-preserving-whitespace s t nil t)))
          (set-macro-character #\% (lambda (s c) (declare (ignore c))
                                    (read s)))
          (set-macro-character #\& (lambda (s c) (declare (ignore c))
                                    (read-preserving-whitespace s)))
          (and (equal (multiple-value-list (read-from-string "!?ab  x")) '(ab 5))
               (equal (multiple-value-list
                        (read-from-string "?!ab  x" t nil :preserve-whitespace t))
                      '(ab 4))
               (equal (multiple-value-list
                        (read-from-string "!%ab  x" t nil :preserve-whitespace t))
                      '(ab 5))
               (equal (multiple-value-list (read-from-string "!&ab  x")) '(ab 4))
               (with-input-from-string (s "!?ab  x")
                 (and (eq (read s) 'ab) (char= (read-char s) #\Space)
                      (char= (read-char s) #\x)))
               (with-input-from-string (s "?!ab  x")
                 (and (eq (read-preserving-whitespace s) 'ab)
                      (char= (read-char s) #\Space) (char= (read-char s) #\Space)))
               (with-input-from-string (s (format nil "x~%z"))
                 (and (null (multiple-value-list
                              (funcall (get-macro-character #\;) s #\;)))
                      (char= (read-char s) #\z)))))"##;
    assert_eq!(eval_reprs(&runtime, source), ["T"]);
}

#[test]
fn saved_standard_macro_functions_read_in_the_enclosing_context() {
    let runtime = Runtime::start().unwrap();
    let source = r##"
        (let ((*readtable* (copy-readtable nil))
              (quote-reader (get-macro-character #\'))
              (dispatch-reader (get-macro-character #\#)))
          (set-macro-character #\' (lambda (s c) (funcall quote-reader s c)))
          (set-macro-character #\@ (lambda (s c) (funcall dispatch-reader s c)))
          (let ((result (read-from-string "('#1=(a) '@1#)")))
            (and (eq (cadar result) (cadadr result))
                 (equal (read-from-string "(1 @|ignored|# 2)") '(1 2))
                 (equal (read-from-string "(1 @-(and) nil 2)") '(1 2))
                 (equal (read-from-string "(1 @+(and) nil 2)") '(1 nil 2)))))"##;
    assert_eq!(eval_reprs(&runtime, source), ["T"]);
}

#[test]
fn reader_resolves_circular_labels_in_nested_objects() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(defstruct reader-node parent)").unwrap();
    let source = r##"
        (let ((*readtable* (copy-readtable nil)))
          (set-macro-character #\! (lambda (s c)
            (declare (ignore c))
            (let* ((v (read s t nil t)) (h (make-hash-table :test 'eq)))
              (setf (gethash v h) (make-reader-node :parent v))
              h)))
          (let* ((h (read-from-string "#1=!#1#"))
                 (node (gethash h h)))
            (and (reader-node-p node) (eq (reader-node-parent node) h))))"##;
    assert_eq!(eval_reprs(&runtime, source), ["T"]);
}

#[test]
fn reader_preserves_both_references_to_each_shared_cons() {
    let runtime = Runtime::start().unwrap();
    let mut input = String::from("(#0=(leaf)");
    for label in 1..=32 {
        let previous = label - 1;
        input.push_str(&format!(" #{label}=(#{previous}# #{previous}#)"));
    }
    input.push(')');
    let form = format!(
        "(let ((nodes (read-from-string \"{input}\")))
           (loop for (parent child) on nodes while child
                 always (and (eq parent (first child)) (eq parent (second child)))))"
    );
    assert_eq!(eval_reprs(&runtime, &form), ["T"]);
}
