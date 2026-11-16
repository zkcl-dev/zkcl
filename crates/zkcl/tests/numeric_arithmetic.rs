use zkcl::Runtime;

#[test]
fn variadic_arithmetic_uses_the_precision_of_the_whole_call() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun add-four (a b c d) (+ a b c d))
             (defun subtract-four (a b c d) (- a b c d))
             (defun multiply-four (a b c d) (* a b c d))
             (defun divide-four (a b c d) (/ a b c d))",
        )
        .unwrap();
    for source in [
        "(eql (add-four 16777216f0 1 -16777216f0 0d0) 1d0)",
        "(eql (subtract-four 16777216f0 -1 16777216f0 0d0) 1d0)",
        "(let ((large (scale-float 1f0 80)))
           (eql (multiply-four large large (scale-float 1f0 -80) 1d0)
                (float large 1d0)))",
        "(let ((large (scale-float 1f0 80)))
           (eql (divide-four large (scale-float 1f0 -80) large 1d0)
                (float large 1d0)))",
        "(eql (add-four 16777216f0 1 -16777216f0 0f0) 0f0)",
        "(eql (add-four #c(16777216f0 1f0) 1 -16777216f0 #c(0d0 2d0))
              #c(1d0 3d0))",
        "(eql (add-four #c(2 3) #c(-2 -3) 1/5 2/5) 3/5)",
        "(eql (float-sign (multiply-four -0f0 2 3.0f0 1d0)) -1d0)",
        "(handler-case (divide-four 7 0 'first-invalid 'second-invalid)
           (type-error (c) (and (eq (type-error-expected-type c) 'number)
                                (eq (type-error-datum c) 'first-invalid))))",
        "(handler-case (multiply-four (ash 1 2000) 1f0 'invalid 4)
           (type-error (c) (eq (type-error-datum c) 'invalid)))",
        "(handler-case (divide-four 7 -0f0 1/3 1d0)
           (division-by-zero (c)
             (let ((operands (arithmetic-error-operands c)))
               (and (eq (arithmetic-error-operation c) '/)
                    (equal operands '(7 -0f0 1/3 1d0))
                    (eql (float-sign (second operands)) -1f0)))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(values[0].is_t(), "{source}");
    }
}

#[test]
fn specialized_multiplication_matches_the_cold_execution_path() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun cached-product (a b)
               (multiple-value-call #'list
                 (values :before a)
                 (let ((product (* a b))) (values product :after))))",
        )
        .unwrap();

    let overflow = "(cached-product most-positive-double-float 2d0)";
    let cold_error = runtime.eval(overflow).err().unwrap();
    let value = runtime
        .eval(
            "(and (equal (cached-product 2d0 3d0) '(:before 2d0 6d0 :after))
                  (equal (cached-product 4d0 5d0) '(:before 4d0 20d0 :after))
                  (handler-case (cached-product most-positive-double-float 2d0)
                    (floating-point-overflow (c)
                      (and (eq (arithmetic-error-operation c) '*)
                           (equal (arithmetic-error-operands c)
                                  (list most-positive-double-float 2d0))))))",
        )
        .unwrap();
    assert!(value[0].is_t());
    let warm_error = runtime.eval(overflow).err().unwrap();
    assert_eq!(cold_error.kind(), warm_error.kind());
    assert_eq!(cold_error.byte_range(), warm_error.byte_range());
    let value = runtime
        .eval("(equal (cached-product 3/2 4) '(:before 3/2 6 :after))")
        .unwrap();
    assert!(value[0].is_t());
}

#[test]
fn fixnum_rounding_matches_scaled_bignum_rounding() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(let ((scale (ash 1 100)))
           (dolist (op '(floor ceiling truncate round ffloor fceiling ftruncate fround) t)
             (loop for n from -24 to 24 do
               (loop for d from -12 to 12 unless (zerop d) do
                 (multiple-value-bind (q r) (funcall op n d)
                   (multiple-value-bind (big-q big-r)
                       (funcall op (* n scale) (* d scale))
                     (assert (eql q big-q))
                     (assert (= (* r scale) big-r))
                     (assert (= n (+ (* (truncate q) d) r)))))))))",
        "(and (equal (multiple-value-list (round 5 2)) '(2 1))
              (equal (multiple-value-list (round 7 -2)) '(-4 -1))
              (equal (multiple-value-list (round -5 2)) '(-2 -1))
              (equal (multiple-value-list (round -7 -2)) '(4 1)))",
        "(let* ((n most-negative-fixnum)
                (half (truncate n 2)))
           (and (equal (multiple-value-list (round half n)) (list 0 half))
                (equal (multiple-value-list (round most-positive-fixnum n)) '(-1 -1))))",
        "(let ((n most-negative-fixnum))
           (dolist (op '(floor ceiling truncate round) t)
             (multiple-value-bind (q r) (funcall op n -1)
               (assert (typep q 'bignum))
               (assert (= q (- n)))
               (assert (eql r 0)))
             (assert (equal (multiple-value-list (funcall op n)) (list n 0)))))",
        "(let ((n most-negative-fixnum))
           (dolist (op '(ffloor fceiling ftruncate fround) t)
             (multiple-value-bind (q r) (funcall op n -1)
               (assert (eql q (float (- n) 1f0)))
               (assert (eql r 0)))
             (assert (equal (multiple-value-list (funcall op n))
                            (list (float n 1f0) 0)))))",
        "(handler-case (floor 'wrong 0)
           (type-error (c) (and (eq (type-error-datum c) 'wrong)
                                (eq (type-error-expected-type c) 'real))))",
        "(loop for op in '(floor ceiling truncate round ffloor fceiling ftruncate fround)
           always (handler-case (funcall op -7 0)
                    (division-by-zero (c)
                      (and (eq (arithmetic-error-operation c) op)
                           (equal (arithmetic-error-operands c) '(-7 0))))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(values[0].is_t(), "{source}");
    }
}
