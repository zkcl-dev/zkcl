use zkcl::Runtime;

#[test]
fn array_resizing_preserves_elements_at_retained_coordinates() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-array-storage", |runtime, _| {
            runtime.collect();
            Ok(vec![runtime.nil()])
        })
        .unwrap();
    for (source, expected) in [
        (
            "(let* ((a (make-array '(2 3) :initial-contents '((1 2 3) (4 5 6))))
                    (b (adjust-array a '(3 2) :initial-element 9)))
               (list (eq a b) (array-dimensions a) b))",
            "(NIL (2 3) #2A((1 2) (4 5) (9 9)))",
        ),
        (
            "(let ((a (make-array '(2 2 2) :adjustable t :initial-element nil)))
               (setf (aref a 0 1 1) (list a))
               (let ((b (adjust-array a '(1 3 3) :initial-element 9)))
                 (collect-array-storage)
                 (list (eq a b) (eq a (car (aref b 0 1 1))) (aref b 0 2 0))))",
            "(T T 9)",
        ),
        (
            "(let ((a (make-array '(0 4) :adjustable t)))
               (list (adjust-array a '(2 2) :initial-element 8)
                     (aref (adjust-array (make-array nil :initial-element 2)
                                        nil :initial-contents 42))))",
            "(#2A((8 8) (8 8)) 42)",
        ),
        (
            "(let* ((a (vector 10 11 12 13)) (b (vector 20 21 22 23))
                    (middle (make-array 3 :adjustable t :displaced-to a :displaced-index-offset 1))
                    (view (make-array 2 :displaced-to middle :displaced-index-offset 1)))
               (adjust-array middle 3 :displaced-to b)
               (let ((shared (aref view 0)))
                 (adjust-array middle 4 :displaced-to nil :initial-element 42)
                 (setf (aref view 1) 99)
                 (collect-array-storage)
                 (multiple-value-bind (target offset) (array-displacement middle)
                   (list shared (aref a 2) (aref b 2) (aref middle 2) (aref middle 3) target offset))))",
            "(21 12 22 99 42 NIL 0)",
        ),
        (
            "(let ((a (make-array 4 :adjustable t :fill-pointer 2 :initial-contents '(1 2 3 4))))
               (adjust-array a 6 :fill-pointer nil :initial-element 9)
               (let ((before (fill-pointer a)) (inactive (aref a 3)))
                 (adjust-array a 1 :fill-pointer t :initial-contents '(42))
                 (list before inactive (fill-pointer a) (aref a 0))))",
            "(2 4 1 42)",
        ),
        (
            "(let* ((a (make-array 2 :adjustable t :fill-pointer 2 :initial-contents '(0 1)))
                    (d (make-array 1 :displaced-to a :displaced-index-offset 1)))
               (let ((index (vector-push-extend (list a) a 3)))
                 (collect-array-storage)
                 (list index (>= (array-total-size a) 5) (fill-pointer a)
                       (eq a (car (aref a 2))) (aref d 0))))",
            "(2 T 3 T 1)",
        ),
        (
            "(let ((a (make-array 1 :fill-pointer 0 :initial-element 10)))
               (let ((index (vector-push-extend 42 a)))
                 (list index (vector-push 0 a) (fill-pointer a) (aref a 0))))",
            "(0 NIL 1 42)",
        ),
        (
            "(let ((a (make-array 65 :element-type 'bit :adjustable t :fill-pointer t :initial-element 1)))
               (let ((index (vector-push-extend 0 a 2)))
                 (collect-array-storage)
                 (list index (bit-vector-p a) (aref a 64) (aref a 65) (length a))))",
            "(65 T 1 0 66)",
        ),
        (
            "(let* ((target (vector 10 11 12))
                     (a (make-array 2 :adjustable t :fill-pointer t :displaced-to target :displaced-index-offset 1)))
               (vector-push-extend 42 a)
               (setf (aref a 0) 99)
               (list (array-displacement a) (aref target 1) (aref a 1) (aref a 2)))",
            "(NIL 11 12 42)",
        ),
        (
            "(let ((a (make-array 1 :element-type 'bit :adjustable t :fill-pointer t :initial-element 1)))
               (list (handler-case (vector-push-extend 42 a)
                       (type-error (c) (and (eql (type-error-datum c) 42)
                                            (eq (type-error-expected-type c) 'bit))))
                     (array-total-size a) (fill-pointer a) (aref a 0)))",
            "(T 1 1 1)",
        ),
        (
            "(let ((a (make-array 2 :element-type 'character :adjustable t :fill-pointer 1
                                  :initial-contents '(#\\a #\\b))))
               (let* ((i (vector-push-extend #\\😀 a))
                      (j (vector-push-extend #\\é a 2))
                      (b (adjust-array a 2 :fill-pointer t :initial-contents \"é😀\")))
                 (collect-array-storage)
                 (list i j (eq a b) (stringp a) (string= a \"é😀\") (length a))))",
            "(1 2 T T T 2)",
        ),
        (
            "(let* ((a (make-array '(2 2) :element-type 'character :adjustable t
                                    :initial-contents '(\"ab\" \"é😀\")))
                    (view (make-array 2 :element-type 'character :displaced-to a :displaced-index-offset 1)))
               (adjust-array a '(3 1) :initial-element #\\x)
               (setf (aref view 0) #\\😀)
               (collect-array-storage)
               (list (aref a 0 0) (aref a 1 0) (char view 1)))",
            "(#\\a #\\😀 #\\x)",
        ),
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(runtime.repr(&values[0]).unwrap(), expected, "{source}");
    }
    for source in [
        "(adjust-array (make-array 2) '(2 2))",
        "(adjust-array (make-array 2 :element-type 'bit) 2 :element-type t)",
        "(adjust-array (make-array 2) 2 :fill-pointer t)",
        "(adjust-array (make-array 2 :adjustable t :fill-pointer t) 1)",
        "(let ((a (make-array 2 :adjustable t))) (adjust-array a 2 :displaced-to a))",
        "(let* ((a (make-array 2 :adjustable t)) (d (make-array 1 :displaced-to a)))
           (adjust-array a 1 :displaced-to d))",
        "(vector-push-extend 7 (make-array 1))",
        "(vector-push-extend 7 (make-array 1 :fill-pointer t :adjustable nil))",
        "(vector-push-extend 7 (make-array 1 :fill-pointer t :adjustable t) 0)",
        "(vector-push-extend 7 (make-array 1 :element-type 'bit :fill-pointer t :adjustable t))",
    ] {
        assert!(runtime.eval(source).is_err(), "{source}");
    }
    let values = runtime
        .eval(
            "(let ((a (make-array 1 :fill-pointer t :adjustable t)))
                  (handler-case (vector-push-extend 1 a 0)
                    (type-error (c)
                      (list (type-error-datum c) (type-error-expected-type c)))))",
        )
        .unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(0 (INTEGER 1))");
}
