use zkcl::Runtime;

#[test]
fn array_access_uses_row_major_storage() {
    let rt = Runtime::start().unwrap();
    rt.register_function("collect-arrays", |rt, _| {
        rt.collect();
        Ok(vec![rt.nil()])
    })
    .unwrap();
    let repr = |source: &str| {
        let values = rt
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        values
            .iter()
            .map(|value| rt.repr(value).unwrap())
            .collect::<Vec<_>>()
    };
    for (source, expected) in [
        (
            "(let ((a (make-array '(2 2) :initial-contents '((1 2) (3 4))))) (list (array-dimensions a) (array-rank a) (array-total-size a) (aref a 1 0) (row-major-aref a 3) (array-row-major-index a 1 1) (array-in-bounds-p a -1 0) (typep a '(simple-array t (2 *)))))",
            "((2 2) 2 4 3 4 3 NIL T)",
        ),
        (
            "(let ((a (make-array nil :initial-contents 42))) (list (aref a) (row-major-aref a 0) (array-dimensions a) (array-total-size a) (typep a '(array t nil))))",
            "(42 42 NIL 1 T)",
        ),
        (
            "(let ((a (make-array '(2 0 3) :initial-contents '(nil nil)))) (list (array-dimensions a) (array-total-size a) (array-in-bounds-p a 0 0 0)))",
            "((2 0 3) 0 NIL)",
        ),
        (
            "(let ((a (make-array 4 :fill-pointer 1 :initial-contents '(10 20 30 40)))) (list (vector-push 11 a) (length a) (aref a 3) (vector-pop a) (fill-pointer a) (array-total-size a) (typep a 'simple-vector)))",
            "(1 2 40 11 1 4 NIL)",
        ),
        (
            "(let ((a (make-array 3 :element-type 'bit :initial-contents '(1 0 1)))) (list (sbit a 2) (setf (bit a 0) 0) (equal a #*001) (bit-vector-p a) (simple-bit-vector-p a) (array-element-type a)))",
            "(1 0 T T T BIT)",
        ),
        (
            "(let ((a (make-array 130 :element-type 'bit :initial-element 1))) (let ((d (make-array 2 :element-type 'bit :displaced-to a :displaced-index-offset 63))) (setf (bit d 1) 0) (collect-arrays) (list (bit d 0) (bit a 64) (bit a 65) (bit a 129))))",
            "(1 0 1 1)",
        ),
        (
            "(let ((octets (make-array 65 :element-type '(unsigned-byte 8))) (bits (make-array 65 :element-type 'bit)))
               (list (setf (aref octets 64) 255) (funcall #'aref octets 64)
                     (setf (aref bits 64) 1) (aref bits 64) (aref bits 63)
                     (handler-case (setf (aref octets 64) 256)
                       (type-error (c) (type-error-datum c)))
                     (aref octets 64)
                     (handler-case (setf (aref bits 65) 9) (program-error () :bounds))))",
            "(255 255 1 1 0 256 255 :BOUNDS)",
        ),
        (
            "(let ((a (vector 10 20 30 40))) (let ((d (make-array 2 :displaced-to a :displaced-index-offset 1))) (setf (aref d 1) 42) (multiple-value-bind (target offset) (array-displacement d) (list (eq target a) offset (aref a 2) (typep d 'simple-vector)))))",
            "(T 1 42 NIL)",
        ),
        (
            "(let ((d (make-array 2 :displaced-to (vector nil (list 42))))) (collect-arrays) (car (aref d 1)))",
            "42",
        ),
        (
            "(let ((a (make-array '(2 2) :initial-element nil))) (setf (row-major-aref a 3) (list 42)) (collect-arrays) (car (aref a 1 1)))",
            "42",
        ),
        (
            "(let ((a (vector 0)) (seen nil)) (let ((r (setf (aref (progn (push 1 seen) a) (progn (push 2 seen) 0)) (progn (push 3 seen) 42)))) (list r (svref a 0) seen)))",
            "(42 42 (3 2 1))",
        ),
        (
            "(let ((a (vector 0))) (list (funcall #'(setf aref) 42 a 0) (elt a 0) (fboundp '(setf row-major-aref))))",
            "(42 42 T)",
        ),
        (
            "(list (aref #5(a b c) 4) (bit #4*10 3) (aref #2A((1 2) (3 4)) 1 0) (aref #0A42))",
            "(C 0 3 42)",
        ),
        (
            "(let ((x 41) (tail (list 42 43))) `#(0 ,x ,@tail))",
            "#(0 41 42 43)",
        ),
        (
            "(list (equalp (vector #\\a #\\b) \"AB\") (equalp #*01 #(0 1)) (equalp #2A((1 2)) #2A((1) (2))))",
            "(T T NIL)",
        ),
        (
            "(let ((table (make-hash-table :test 'equalp))) (setf (gethash (vector #\\a #\\b) table) 42) (gethash \"AB\" table))",
            "42",
        ),
        (
            "(list (aref \"Aé\" 1) (length \"Aé\") (arrayp \"Aé\") (typep \"Aé\" '(array character (2))))",
            "(#\\é 2 T T)",
        ),
        (
            "(let ((a (make-array 2 :element-type 'character :initial-contents '(#\\a #\\b)))) (list (stringp a) (setf (aref a 0) #\\😀) (setf (row-major-aref a 1) #\\é) (collect-arrays) (equal a \"😀é\")))",
            "(T #\\😀 #\\é NIL T)",
        ),
        (
            "(let ((a (make-array 2 :element-type 'base-char :initial-element #\\é))) (list (setf (elt a 0) #\\a) (equal a \"aé\")))",
            "(#\\a T)",
        ),
    ] {
        assert_eq!(repr(source).first().unwrap(), expected, "{source}");
    }
    for source in [
        "(aref (make-array '(1 2)) 0)",
        "(aref (make-array '(1 2)) 1 0)",
        "(make-array '(2 1) :initial-contents '((1 2) (3 4)))",
        "(make-array '(1 2) :fill-pointer t)",
        "(make-array 2 :displaced-to (vector 0))",
        "(setf (bit #*01 0) 9)",
        "(make-array 1 :element-type 'character :initial-element 2)",
        "#2(A B C)",
        "#(.. . t)",
        "#2A((1) nil)",
        "#*12",
    ] {
        assert!(rt.eval(source).is_err(), "invalid input: {source}");
    }
}

#[test]
fn equivalent_bit_element_types_use_bit_vector_storage() {
    let rt = Runtime::start().unwrap();
    rt.eval(
        "(dolist (element '(bit (unsigned-byte 1) (integer 0 1) (member 0 1)))
           (let* ((type (list 'vector element 2))
                  (array (make-array 2 :element-type element :initial-contents '(1 0))))
             (assert (eq (upgraded-array-element-type element) 'bit))
             (assert (eq (array-element-type array) 'bit))
             (assert (typep array type))
             (assert (subtypep type 'bit-vector))
             (assert (subtypep '(bit-vector 2) type))
             (assert (eq (coerce array type) array))
             (assert (equal (make-sequence type 2 :initial-element 1) #*11))
             (assert (equal (coerce '(1 0) type) #*10))))",
    )
    .unwrap();
}

#[test]
fn float_array_operations_preserve_element_precision() {
    let rt = Runtime::start().unwrap();
    for source in [
        "(let ((a (make-array '(2 2) :element-type 'single-float)))
           (and (eql (aref a 0 0) 0.0f0)
                (eql (setf (aref a 1 0) -0.0f0) -0.0f0)
                (eql (row-major-aref a 2) -0.0f0)
                (eql (setf (row-major-aref a 3) least-positive-single-float)
                     (aref a 1 1))
                (eq (array-element-type a) 'single-float)
                (typep a '(simple-array short-float (2 *)))
                (typep a '(array (or single-float)))
                (typep a '(array (and single-float real)))
                (typep a '(array (member 0.0f0 -0.0f0)))
                (typep a '(array (single-float -10.0f0 10.0f0) 2))
                (typep a (type-of a))
                (not (typep a '(array double-float)))
                (not (typep a '(array t)))))",
        "(progn
           (deftype specialized-double () '(long-float * *))
           (let ((a (make-array nil :element-type 'specialized-double
                                :initial-contents least-positive-double-float)))
             (and (eq (upgraded-array-element-type 'specialized-double) 'double-float)
                  (eq (upgraded-array-element-type 'float) t)
                  (eq (upgraded-array-element-type '(eql 1.0d0)) 'double-float)
                  (eq (upgraded-array-element-type '(or (eql 1.0d0) (member 2.0d0))) 'double-float)
                  (eq (array-element-type a) 'double-float)
                  (typep a '(simple-array specialized-double nil))
                  (subtypep '(simple-array specialized-double nil) '(array double-float *))
                  (subtypep '(simple-array double-float nil) '(array (eql 1.0d0) *))
                  (not (subtypep '(simple-array double-float (*)) '(vector t)))
                  (eql (aref a) least-positive-double-float))))",
        "(let* ((a (make-array 4 :element-type 'double-float :adjustable t
                                :initial-contents (list 1.0d0 -0.0d0 least-positive-double-float 4.0d0)))
                (view (make-array 2 :element-type 'long-float :adjustable t
                                    :displaced-to a :displaced-index-offset 1)))
           (and (eql (aref view 0) -0.0d0)
                (eql (aref view 1) least-positive-double-float)
                (eq (adjust-array a 5) a)
                (eql (aref a 4) 0.0d0)
                (eql (setf (elt view 0) 2.0d0) (aref a 1))
                (eq (adjust-array view 3 :element-type 'double-float :initial-element -0.0d0) view)
                (eql (aref view 0) 2.0d0)
                (eql (aref view 1) least-positive-double-float)
                (eql (aref view 2) -0.0d0)))",
        "(let ((a (make-array 1 :element-type 'single-float :adjustable t :fill-pointer 0)))
           (and (eql (vector-push -0.0f0 a) 0)
                (eql (vector-push-extend least-positive-single-float a) 1)
                (eql (vector-pop a) least-positive-single-float)
                (eql (vector-pop a) -0.0f0)
                (zerop (fill-pointer a))))",
        "(let* ((a (make-sequence '(vector single-float) 2 :initial-element least-positive-single-float))
                (b (coerce a 'list))
                (c (map '(vector single-float) #'identity (cons -0.0f0 b)))
                (d (remove -0.0f0 c))
                (e (make-array 3 :element-type 'single-float :initial-contents c)))
           (and (typep a '(simple-array single-float (2)))
                (eql (car b) least-positive-single-float)
                (eql (aref (reverse c) 2) -0.0f0)
                (typep d '(vector single-float 2))
                (eql (aref (copy-seq e) 0) -0.0f0)))",
        "(let* ((a (make-array 2 :element-type 'single-float :initial-contents '(-0.0f0 1.5f0)))
                (b (make-array 2 :element-type 'double-float :initial-contents '(0.0d0 1.5d0)))
                (table (make-hash-table :test 'equalp)))
           (setf (gethash a table) t)
           (and (equalp a b) (equalp a #(0 3/2)) (gethash b table) (gethash #(0 3/2) table)))",
        "(handler-case (make-array 1 :element-type 'bit :initial-element 2)
           (type-error (c) (and (eql (type-error-datum c) 2)
                                (eq (type-error-expected-type c) 'bit))))",
        "(handler-case (make-array 1 :element-type 'character :initial-contents '(17))
           (type-error (c) (and (eql (type-error-datum c) 17)
                                (eq (type-error-expected-type c) 'character))))",
        "(handler-case (make-array 1 :element-type 'single-float :initial-element 1)
           (type-error (c) (eq (type-error-expected-type c) 'single-float)))",
        "(handler-case (make-array 1 :element-type 'single-float :initial-contents '(1.0d0))
           (type-error (c) (eq (type-error-expected-type c) 'single-float)))",
        "(let ((a (make-array 1 :element-type 'double-float :initial-element -0.0d0)))
           (and (handler-case (setf (aref a 0) 0.0f0)
                  (type-error (c) (eq (type-error-expected-type c) 'double-float)))
                (eql (aref a 0) -0.0d0)))",
        "(let ((a (make-array 1 :element-type 'single-float)))
           (handler-case (make-array 1 :element-type 'double-float :displaced-to a)
             (program-error () t)))",
    ] {
        let result = rt.eval(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(rt.repr(&result[0]).unwrap(), "T", "{source}");
    }
}
