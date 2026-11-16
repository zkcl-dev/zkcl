use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_reprs;

fn runtime_collecting_sequences() -> Runtime {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-sequence-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    runtime
}

#[test]
fn concatenate_produces_the_requested_result_type() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(deftype two-bits () '(vector bit 2))")
        .unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(values (concatenate 'two-bits '(1) #*0)
                     (concatenate '(cons keyword null) #(:tag))
                     (array-element-type (concatenate '(array nil (*)))))"
        ),
        ["#*10", "(:TAG)", "NIL"]
    );
    for source in [
        "(concatenate 'null '(a))",
        "(concatenate 'cons)",
        "(concatenate '(cons keyword null) '(a))",
        "(concatenate '(vector nil) '(nil))",
    ] {
        let Err(error) = runtime.eval(source) else {
            panic!("expected a type error for {source}");
        };
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError)
        );
    }
}

#[test]
fn sequence_traversal_reports_the_improper_cdr() {
    let runtime = Runtime::start().unwrap();
    for call in [
        "(map 'list #'identity (cons 1 #(2)))",
        "(position 1 (cons 1 #(2)))",
    ] {
        let source = format!(
            "(handler-case {call}
               (type-error (condition)
                 (values (type-error-datum condition) (type-error-expected-type condition))))"
        );
        assert_eq!(eval_reprs(&runtime, &source), ["#(2)", "LIST"], "{call}");
    }
}

#[test]
fn sequence_copies_materialize_the_visible_range() {
    let runtime = runtime_collecting_sequences();

    for (source, expected) in [
        (
            "(values (copy-seq nil) (subseq '(a b c) 3 3) (subseq '(a b c) 1 nil))",
            vec!["NIL", "NIL", "(B C)"],
        ),
        (
            "(let* ((item (list 42))
                     (original (list item :items))
                     (copy (copy-seq original))
                     (slice (subseq original 0)))
               (collect-sequence-heap)
               (setf (elt copy 1) :copy)
               (values (eq copy original) (eq (cdr slice) (cdr original))
                       (eq (car copy) item) (eq (car slice) item)
                       original copy slice))",
            vec![
                "NIL",
                "NIL",
                "T",
                "T",
                "((42) :ITEMS)",
                "((42) :COPY)",
                "((42) :ITEMS)",
            ],
        ),
        (
            "(let* ((original (make-array 5 :fill-pointer 3 :adjustable t
                                :initial-contents '(:a :b :c :d :e)))
                     (copy (copy-seq original))
                     (slice (subseq original 1)))
               (setf (aref copy 0) :written)
               (values original copy slice (simple-vector-p copy) (simple-vector-p slice)
                       (array-has-fill-pointer-p copy) (array-dimension original 0)))",
            vec![
                "#(:A :B :C)",
                "#(:WRITTEN :B :C)",
                "#(:B :C)",
                "T",
                "T",
                "NIL",
                "5",
            ],
        ),
        (
            "(let* ((bits (make-array 6 :element-type 'bit :fill-pointer 4
                             :initial-contents '(1 0 1 1 0 1)))
                     (copy (subseq bits 1)))
               (values copy (array-element-type copy) (simple-bit-vector-p copy)
                       (subseq bits 4) (replace copy '(1 0) :start1 1)))",
            vec!["#*010", "BIT", "T", "#*", "#*010"],
        ),
        (
            r#"(let* ((original (copy-seq "aé😀雪z"))
                      (slice (subseq original 1 4))
                      (copy (subseq original 0 nil)))
                 (replace original "xy" :start1 1 :end1 4)
                 (collect-sequence-heap)
                 (values original slice copy (eq original copy) (typep slice 'simple-string)
                         (subseq original 5 5)))"#,
            vec![
                r#""axy雪z""#,
                r#""é😀雪""#,
                r#""aé😀雪z""#,
                "NIL",
                "T",
                r#""""#,
            ],
        ),
        (
            r#"(let* ((backing (copy-seq "aé😀雪z"))
                      (view (make-array 3 :element-type 'character :displaced-to backing
                              :displaced-index-offset 1 :fill-pointer 2))
                      (copy (copy-seq view)))
                 (replace view "BΩp" :start1 1 :start2 1)
                 (collect-sequence-heap)
                 (values backing view copy (typep copy 'simple-string) (length copy)
                         (aref view 2) (fill-pointer view)))"#,
            vec![
                r#""aéΩ雪z""#,
                r#""éΩ""#,
                r#""é😀""#,
                "T",
                "2",
                r#"#\雪"#,
                "2",
            ],
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), expected, "{source}");
    }
}

#[test]
fn replace_reads_aliased_ranges_before_writing() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (
            "(let* ((source (list 0 1 2 3 4))
                     (target (list :a :b :c :d :e))
                     (tail (cddr target))
                     (result (replace target source :start1 1 :end1 4 :start2 2 :end2 4)))
               (values result source (eq result target) (eq tail (cddr target))))",
            vec!["(:A 2 3 :D :E)", "(0 1 2 3 4)", "T", "T"],
        ),
        (
            "(let ((right (list 0 1 2 3 4)) (left (vector 0 1 2 3 4)))
               (replace right right :start1 1 :end1 nil :start2 0 :end2 4)
               (replace left left :start1 0 :end1 4 :start2 1)
               (values right left))",
            vec!["(0 0 1 2 3)", "#(1 2 3 4 4)"],
        ),
        (
            r#"(let ((text (copy-seq "aé😀雪z"))
                     (bits (copy-seq #*01011)))
                 (replace text text :start1 1 :start2 0 :end2 4)
                 (replace bits bits :start1 2 :start2 1 :end2 4)
                 (values text bits))"#,
            vec![r#""aaé😀雪""#, "#*01101"],
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), expected, "{source}");
    }
}

#[test]
fn setf_subseq_evaluates_place_subforms_left_to_right() {
    let runtime = runtime_collecting_sequences();
    for (source, expected) in [
        (
            "(let ((target (list 0 1 2 3)) (seen nil) (source (vector 8 9 10)))
               (multiple-value-bind (result excess)
                   (setf (subseq (progn (push 1 seen) target)
                                 (progn (push 2 seen) 1)
                                 (progn (push 3 seen) 2))
                         (progn (push 4 seen) (collect-sequence-heap) (values source :excess)))
                 (values result excess target seen (eq result source))))",
            vec!["#(8 9 10)", "NIL", "(0 8 2 3)", "(4 3 2 1)", "T"],
        ),
        (
            "(let* ((target (vector 0 1 2 3))
                     (short (list 7))
                     (result (funcall (fdefinition '(setf subseq)) short target 1)))
               (setf (subseq target 2 nil) nil)
               (values target (eq short result) (fboundp '(setf subseq))
                       (funcall #'(setf subseq) '(8) target 0 0)))",
            vec!["#(0 7 2 3)", "T", "T", "(8)"],
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), expected, "{source}");
    }
}

#[test]
fn replace_writes_only_the_available_source_elements() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (
            "(let ((target (vector 0 1 2)))
               (replace target '(9) :start1 1 :start1 9 :unknown t :allow-other-keys t))",
            vec!["#(0 9 2)"],
        ),
        (
            r#"(let ((target (copy-seq "ab")))
                 (values (replace target (list #\雪 #\z) :start1 1) target))"#,
            vec![r#""a雪""#, r#""a雪""#],
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), expected, "{source}");
    }
}

#[test]
fn replace_snapshots_displaced_source_views() {
    let runtime = Runtime::start().unwrap();
    // These two views share storage. The implementation takes a snapshot
    // before writing, including when their owners are distinct Lisp objects.
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let* ((backing (vector 0 1 2 3 4))
                     (left (make-array 4 :displaced-to backing))
                     (right (make-array 4 :displaced-to backing :displaced-index-offset 1)))
               (replace right left)
               (values right backing))"
        ),
        ["#(0 1 2 3)", "#(0 0 1 2 3)"]
    );
}

#[test]
fn sequence_ranges_reject_invalid_bounds() {
    let runtime = Runtime::start().unwrap();
    for (source, condition) in [
        ("(copy-seq '(a . b))", ConditionClass::TypeError),
        ("(copy-seq #2A((1)))", ConditionClass::TypeError),
        (
            "(let ((x (list 1 2))) (setf (cddr x) x) (copy-seq x))",
            ConditionClass::TypeError,
        ),
        ("(subseq '(a b) nil)", ConditionClass::TypeError),
        ("(subseq '(a b) 0 -1)", ConditionClass::TypeError),
        ("(subseq '(a b) 2 1)", ConditionClass::Error),
        ("(subseq '(a b) (ash 1 120))", ConditionClass::Error),
        (
            "(let ((a (make-array 4 :fill-pointer 2))) (subseq a 0 3))",
            ConditionClass::Error,
        ),
        (
            "(let ((a (list 0 1))) (setf (subseq a 0) 3))",
            ConditionClass::TypeError,
        ),
        ("(replace (copy-seq #*00) '(2))", ConditionClass::TypeError),
        (
            r#"(replace (copy-seq "xx") '(8))"#,
            ConditionClass::TypeError,
        ),
        (
            "(replace (vector 1) '(1) :unexpected t)",
            ConditionClass::ProgramError,
        ),
        (
            "(replace (vector 1) '(1) :start1)",
            ConditionClass::ProgramError,
        ),
    ] {
        let Err(error) = runtime.eval(source) else {
            panic!("expected a Lisp condition for {source}");
        };
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(condition),
            "{source}: {error}"
        );
    }
}

#[test]
fn replace_validates_source_bounds_before_writing() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((target (list 0 1 2)))
               (handler-case (replace target '(7 8) :end2 9)
                 (error () target)))"
        ),
        ["(0 1 2)"]
    );
}

#[test]
fn displaced_string_writes_preserve_the_backing_boundary() {
    let runtime = runtime_collecting_sequences();
    // Repeated width changes allocate more than the entire runtime heap.
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(let* ((count 1048576)
                      (backing (make-string (+ count 2) :initial-element #\z))
                      (target (make-array count :element-type 'character
                                :displaced-to backing :displaced-index-offset 1))
                      (wide (make-string count :initial-element #\😀))
                      (narrow (make-string count :initial-element #\a)))
                 (dotimes (iteration 24)
                   (replace target wide)
                   (setf (subseq target 0) narrow))
                 (let* ((result (replace target wide))
                        (copy (copy-seq result)))
                   (collect-sequence-heap)
                   (values (eq result target) (char backing 0) (char backing (+ count 1))
                           (char copy (- count 1)) (length copy))))"#
        ),
        ["T", "#\\z", "#\\z", "#\\😀", "1048576"]
    );
}
