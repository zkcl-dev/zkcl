use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::lisp::eval_reprs;

#[test]
fn coerce_preserves_identity_when_the_value_already_has_the_requested_type() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-coerce-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    for (source, expected) in [
        (
            "(values (make-sequence 'list 2 :initial-element :x)
                     (make-sequence 'null 0)
                     (make-sequence 'cons 1)
                     (make-sequence '(cons integer (cons integer null)) 2 :initial-element 9))",
            vec!["(:X :X)", "NIL", "(NIL)", "(9 9)"],
        ),
        (
            "(values (make-sequence '(vector * 2) 2 :initial-element :v)
                     (make-sequence '(array t 1) 2 :initial-element :a)
                     (make-sequence '(simple-array (unsigned-byte 1) (3)) 3 :initial-element 1)
                     (make-sequence '(simple-bit-vector 2) 2))",
            vec!["#(:V :V)", "#(:A :A)", "#*111", "#*00"],
        ),
        (
            r#"(values (make-sequence '(string 3) 3 :initial-element #\雪)
                        (make-sequence '(vector character 2) 2 :initial-element #\😀)
                        (make-sequence 'simple-string 2)
                        (make-sequence 'base-string 0))"#,
            vec![r#""雪雪雪""#, r#""😀😀""#, r#""  ""#, r#""""#],
        ),
        (
            "(let* ((item (list 42))
                     (array (make-sequence 'vector 2 :initial-element item
                              :initial-element :ignored :unknown nil :allow-other-keys t))
                     (list (coerce array 'list)))
               (collect-coerce-heap)
               (values (eq (aref array 0) item) (eq (cadr list) item)
                       (eq array (coerce array 'vector)) list
                       (eq list (coerce list 'list))))",
            vec!["T", "T", "T", "((42) (42))", "T"],
        ),
        (
            r#"(let* ((text (make-array 4 :element-type 'character :fill-pointer 2
                                :initial-contents "a雪bc"))
                       (list (coerce text 'list))
                       (copy (coerce text '(string 2)))
                       (vector (coerce text '(vector t))))
                 (setf (char text 0) #\z)
                 (collect-coerce-heap)
                 (values list text copy vector (typep copy 'simple-string)
                         (eq text (coerce text '(string 4)))))"#,
            vec![
                "(#\\a #\\雪)",
                r#""z雪""#,
                r#""a雪""#,
                "#(#\\a #\\雪)",
                "T",
                "T",
            ],
        ),
        (
            r#"(values (coerce '(1 0 1) '(bit-vector 3))
                       (coerce '(#\雪 #\😀) '(simple-array character (*)))
                       (coerce #*101 '(simple-vector 3))
                       (coerce #(42) '(cons fixnum null))
                       (coerce #() 'null))"#,
            vec!["#*101", r#""雪😀""#, "#(1 0 1)", "(42)", "NIL"],
        ),
        (
            r#"(values (coerce 'a 'character) (coerce "😀" 'character)
                        (coerce #\a 'standard-char) (coerce 7/2 'float)
                        (typep (coerce 9 'short-float) 'single-float)
                        (typep (coerce 9 'long-float) 'double-float))"#,
            vec!["#\\A", "#\\😀", "#\\a", "3.5", "T", "T"],
        ),
        (
            "(let ((fraction 7/2) (number 3.5d0) (pair (cons 1 2)))
               (values (coerce fraction 'complex)
                       (eq number (coerce number 'float))
                       (eq pair (coerce pair t))
                       (typep (coerce 1.5d0 'single-float) 'single-float)
                       (typep (coerce 1.5 'double-float) 'double-float)
                       (typep (coerce number 'complex) '(complex double-float))
                       (= (imagpart (coerce -5.5 'complex)) 0.0)
                       (= (coerce (ash 1 70) 'double-float) (ash 1 70))))",
            vec!["7/2", "T", "T", "T", "T", "T", "T", "T"],
        ),
        (
            "(let ((number #c(1.0 -0.0)))
               (values (coerce 2/3 '(complex rational))
                       (eql (coerce 2d0 '(complex *)) #c(2d0 0d0))
                       (eql (coerce 2 '(complex single-float)) #c(2.0 0.0))
                       (eql (coerce number '(complex double-float)) #c(1d0 -0d0))
                       (eq number (coerce number '(complex short-float)))))",
            vec!["2/3", "T", "T", "T", "T"],
        ),
        (
            "(progn
               (defparameter *coerce-global* 40)
               (defun coerce-global-car (items) (car items))
               (let ((*coerce-global* 3))
                 (flet ((coerce-global-car (items) (declare (ignore items)) 0))
                   (let ((f (coerce '(lambda (x) (+ x *coerce-global*)) 'function)))
                     (collect-coerce-heap)
                     (values (funcall f 2) (eq f (coerce f 'function))
                             (funcall (coerce 'coerce-global-car 'function) '(17 24)))))))",
            vec!["5", "T", "17"],
        ),
        (
            "(let ((a (vector nil)))
               (values (funcall (coerce '(setf aref) 'function) :written a 0) a))",
            vec![":WRITTEN", "#(:WRITTEN)"],
        ),
        (
            "(let ((coerce-lexical-only 7))
               (handler-case (funcall (coerce '(lambda () coerce-lexical-only) 'function))
                 (unbound-variable () :null-lexical-environment)))",
            vec![":NULL-LEXICAL-ENVIRONMENT"],
        ),
        (
            "(let ((value (list :gc)) (last-length 0) (identity nil))
               (dotimes (iteration 7)
                 (let ((items (make-sequence 'list 450000 :initial-element value)))
                   (setq last-length (length items)
                         identity (eq (car items) value))))
               (values last-length identity))",
            vec!["450000", "T"],
        ),
    ] {
        assert_eq!(eval_reprs(&runtime, source), expected, "{source}");
    }

    for source in [
        "(make-sequence 'list -1)",
        "(make-sequence 'sequence 0)",
        "(make-sequence 'null 1)",
        "(make-sequence 'cons 0)",
        "(make-sequence '(cons fixnum null) 2 :initial-element 1)",
        "(make-sequence '(vector * 2) 3)",
        "(make-sequence '(vector * 2) (ash 1 120))",
        "(make-sequence '(array * (2 2)) 3)",
        "(make-sequence 'bit-vector 2 :initial-element 3)",
        "(coerce #() 'nil)",
        "(coerce #(a b c) '(vector * 4))",
        r#"(coerce "abc" '(string 2))"#,
        "(coerce '(:x) '(vector bit))",
        "(coerce #(:x) '(cons fixnum null))",
        "(coerce '(:x) 'string)",
        "(coerce '(a . b) 'vector)",
        "(coerce '(quote (1 2)) 'function)",
        "(coerce 65 'character)",
        r#"(coerce "ab" 'character)"#,
        "(coerce 1.5 'rational)",
        "(coerce 2/3 'integer)",
        "(coerce #c(1 2) 'float)",
        "(coerce 1.0d0 '(complex rational))",
        "(coerce #c(1.0 2.0) '(complex rational))",
        "(coerce 1 '(complex cons))",
    ] {
        let Err(error) = runtime.eval(source) else {
            panic!("expected a Lisp condition for {source}");
        };
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}: {error}",
        );
    }
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (coerce 'if 'function)
               (error () :error))"
        ),
        [":ERROR"]
    );
}
