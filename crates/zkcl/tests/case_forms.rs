use zkcl::{EvalErrorKind, Runtime};

#[test]
fn case_macros_dispatch_in_the_callers_environment() {
    let runtime = Runtime::start().unwrap();
    let check = |source: &str| {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(values.len(), 1, "{source}");
        assert!(!values[0].is_nil(), "{source}");
    };

    check(
        "(let ((calls 0))
           (multiple-value-bind (first second)
               (case (progn (incf calls) (values 2 9))
                 (1 (error \"wrong case\"))
                 ((2 3) (values :first :second))
                 (t (error \"wrong default\")))
             (and (= calls 1) (eq first :first) (eq second :second))))",
    );
    check(
        "(and (null (case 5 (5)))
              (null (ecase 5 (5)))
              (null (typecase 5 (integer)))
              (null (etypecase 5 (integer)))
              (null (case 5))
              (null (typecase 5))
              (eq (case nil (nil :wrong) ((nil) :right)) :right)
              (eq (case 't ((t) :right) (otherwise :wrong)) :right)
              (eq (ecase t (t :right)) :right)
              (eq (ecase 'otherwise (otherwise :right)) :right)
              (eq (case 11/17 (11/17 :right)) :right)
              (eq (case 1 (1.0 :wrong) (otherwise :right)) :right)
              (eq (case (list 5) (((5)) :wrong) (otherwise :right)) :right)
              (eq (typecase 5 (integer :right) (number :wrong) (t :wrong)) :right)
              (eq (typecase 5 (nil :wrong) (otherwise :right)) :right))",
    );
    check(
        "(and (equal (multiple-value-list (case 1 (1 (values)))) nil)
              (equal (multiple-value-list (etypecase 3 (integer (values 7 8))))
                     '(7 8)))",
    );
    check(
        "(and (handler-case (ecase :wrong ((:x :y) t) (:z t))
                (type-error (c)
                  (let ((type (type-error-expected-type c)))
                    (and (eq (type-error-datum c) :wrong)
                         (typep :x type) (typep :y type) (typep :z type)
                         (not (typep :wrong type))))))
              (handler-case (etypecase :wrong (integer t) ((string 2) t))
                (type-error (c)
                  (let ((type (type-error-expected-type c)))
                    (and (eq (type-error-datum c) :wrong)
                         (typep 5 type) (typep \"ok\" type)
                         (not (typep \"nope\" type))))))
              (block checked
                (handler-bind ((type-error (lambda (c)
                                             (return-from checked
                                               (not (find-restart 'store-value c))))))
                  (ecase :wrong))))",
    );
    check(
        "(and (typep '(setf aref) '(cons (eql setf) (cons symbol null)))
              (typep '(5 . :key) '(cons integer *))
              (typep '(5 . :key) '(cons * keyword))
              (typep '(5 . :key) '(cons))
              (not (typep nil '(cons)))
              (not (typep '(setf . aref) '(cons (eql setf) (cons symbol null)))))",
    );
    check(
        "(progn
           (defun case-to-function (fn)
             (etypecase fn
               (function fn)
               (symbol (symbol-function fn))
               ((cons (eql setf) (cons symbol null)) (fdefinition fn))))
           (let ((items (vector nil)))
             (funcall (case-to-function '(setf aref)) :stored items 0)
             (and (eq (aref items 0) :stored)
                  (eq (funcall (case-to-function 'car) '(:key)) :key)
                  (eq (funcall (case-to-function #'cdr) '(:first . :last)) :last))))",
    );
    check(
        "(progn
           (defparameter *case-state* :outer)
           (let ((items (vector :bad)) (reads 0) (seen nil)
                 (other (make-condition 'type-error)))
             (multiple-value-bind (chosen active dynamically-bound)
                 (let ((*case-state* :inner))
                   (handler-bind
                       ((type-error
                          (lambda (c)
                            (when (and (eq *case-state* :inner)
                                       (not (find-restart 'store-value other))
                                       (find-restart 'store-value c))
                              (push (type-error-datum c) seen)
                              (store-value (if (eq (type-error-datum c) :bad) :again :ready)
                                           c)))))
                     (ccase (aref items (progn (incf reads) 0))
                       ((:ready :done) (values (aref items 0)
                                               (find-restart 'store-value)
                                               *case-state*)))))
               (and (eq chosen :ready) (null active) (eq dynamically-bound :inner)
                    (= reads 1) (equal seen '(:again :bad))
                    (eq *case-state* :outer)))))",
    );
    check(
        "(let ((cell (list :wrong)) (cleaned nil))
           (symbol-macrolet ((place (car cell)))
             (multiple-value-bind (first second)
                 (handler-bind
                     ((type-error (lambda (c)
                                    (let ((expected (type-error-expected-type c)))
                                      (when (and (eq (type-error-datum c) :wrong)
                                                 (typep 12 expected)
                                                 (typep \"ok\" expected))
                                        (unwind-protect
                                             (invoke-restart (find-restart 'store-value c) 12)
                                          (setq cleaned t)))))))
                   (ctypecase place
                     (integer (values place cleaned))
                     (string (error \"wrong type\"))))
               (and (= first 12) second (= (car cell) 12)))))",
    );
    check(
        "(let ((first :bad) (second :old))
           (handler-bind ((type-error (lambda (c) (store-value :ready c))))
             (ccase (values first second)
               (:ready (and (eq first :ready) (null second))))))",
    );
    check("(handler-case (macroexpand '(case)) (program-error () t))");
    runtime
        .register_function("collect-case-frame", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    check(
        "(let ((cell (list (list :before))))
           (handler-bind
               ((type-error (lambda (c)
                              (collect-case-frame)
                              (when (eq (car cell) (type-error-datum c))
                                (store-value :ready c)))))
             (ccase (car cell) (:ready (eq (car cell) :ready)))))",
    );

    for malformed in [
        "(case)",
        "(case 1 ())",
        "(case 1 ((1 . 2) :bad))",
        "(case 1 (t :bad) (1 :valid))",
        "(typecase 1 ())",
    ] {
        let error = runtime.eval(malformed).err().expect("malformed case");
        assert_eq!(error.kind(), EvalErrorKind::MacroExpansion, "{error}");
    }
}
