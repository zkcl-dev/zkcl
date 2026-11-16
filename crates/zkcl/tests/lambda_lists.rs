use zkcl::Runtime;

fn assert_values(runtime: &Runtime, source: &str, expected: &[Option<isize>]) {
    let results = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("failed to evaluate {source}: {error}"));
    assert_eq!(results.len(), expected.len(), "{source}");
    for (result, expected) in results.iter().zip(expected) {
        if let Some(expected) = expected {
            assert_eq!(result.as_fixnum(), Some(*expected), "{source}");
        } else {
            assert!(result.is_nil(), "expected NIL from {source}");
        }
    }
}

#[test]
fn auxiliary_variables_can_shadow_earlier_bindings() {
    let runtime = Runtime::start().unwrap();
    assert_values(
        &runtime,
        "(funcall (lambda (x &aux (original (lambda () x))
                                  (x (1+ x)) (x (* x 2)))
                    (values (funcall original) x))
                  10)",
        &[Some(10), Some(22)],
    );
    assert_values(
        &runtime,
        "(progn
           (defmethod auxiliary-method ((x integer) &aux (x (1+ x))) x)
           (auxiliary-method 10))",
        &[Some(11)],
    );
    assert_values(
        &runtime,
        "(destructuring-bind ((x &aux (x (1+ x))) y) '((10) 8)
           (values x y))",
        &[Some(11), Some(8)],
    );
}

#[test]
fn lambda_list_defaults_see_earlier_parameter_bindings() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun options (a &optional (b (+ a 1) b-p) (c (if b-p b (+ b 10)) c-p))
               (values a b c (if b-p 1 0) (if c-p 1 0)))
             (defun keys (x &rest all &key ((switch mode) 20 mode-p) (next (+ mode x)))
               (values mode next (if mode-p 1 0) (car (cdr all))))
             (defun strict (&key (x 5)) x)
             (defun loose (&key (x 5) &allow-other-keys) x)
             (defun override (&key (allow-other-keys 4)) allow-other-keys)
             (defparameter *lambda-special* 2)
             (defun captured (x &optional (*lambda-special* (+ x 1))
                                   &aux (sum (+ *lambda-special* x)))
               (values (lambda () sum) (lambda () *lambda-special*)))
             (defun declared (seed &optional (opt (+ seed 1) present)
                                  &aux (seed-copy (+ opt seed)))
               (declare (special opt present seed-copy))
               (values (symbol-value 'opt)
                       (if (symbol-value 'present) 1 0)
                       (symbol-value 'seed-copy)))
             (setq free-special 12)",
        )
        .unwrap();

    assert_values(
        &runtime,
        "(options 5)",
        &[Some(5), Some(6), Some(16), Some(0), Some(0)],
    );
    assert_values(
        &runtime,
        "(options 5 nil)",
        &[Some(5), None, None, Some(1), Some(0)],
    );
    assert_values(
        &runtime,
        "(options 5 8 9)",
        &[Some(5), Some(8), Some(9), Some(1), Some(1)],
    );
    assert_values(
        &runtime,
        "(handler-case (options) (program-error () 40))",
        &[Some(40)],
    );
    assert_values(
        &runtime,
        "(handler-case (options 1 2 3 4) (program-error () 41))",
        &[Some(41)],
    );

    assert_values(
        &runtime,
        "(let ((n 8) (trace 0))
           (funcall (lambda (&optional (n (progn (setq trace 1) (+ n 1)))
                                       (next (progn (setq trace (+ (* trace 10) 2)) (+ n 1)))
                             &aux (sum (progn (setq trace (+ (* trace 10) 3)) (+ n next))))
                      (values n next sum trace))))",
        &[Some(9), Some(10), Some(19), Some(123)],
    );
    assert_values(
        &runtime,
        "(multiple-value-bind (get-sum get-special) (captured 5)
           (values (funcall get-sum) (funcall get-special)))",
        &[Some(11), Some(2)],
    );
    assert_values(&runtime, "(declared 3)", &[Some(4), Some(0), Some(7)]);
    assert_values(&runtime, "(declared 3 9)", &[Some(9), Some(1), Some(12)]);
    assert_values(&runtime, "(boundp 'opt)", &[None]);
    assert_values(
        &runtime,
        "(let ((free-special 90))
           (funcall (lambda (&optional (x free-special))
                      (declare (special free-special))
                      (values x free-special))))",
        &[Some(90), Some(12)],
    );

    assert_values(
        &runtime,
        "(keys 10 'switch 7 :next 99 'switch 8)",
        &[Some(7), Some(99), Some(1), Some(7)],
    );
    assert_values(
        &runtime,
        "(keys 10 'switch 7)",
        &[Some(7), Some(17), Some(1), Some(7)],
    );
    assert_values(&runtime, "(keys 10)", &[Some(20), Some(30), Some(0), None]);
    assert_values(&runtime, "(strict :x 8 :x 9)", &[Some(8)]);
    assert_values(
        &runtime,
        "(strict :ignored 7 :allow-other-keys t :allow-other-keys nil)",
        &[Some(5)],
    );
    assert_values(
        &runtime,
        "(loose :ignored 7 :allow-other-keys nil)",
        &[Some(5)],
    );
    assert_values(
        &runtime,
        "(override :allow-other-keys 17 :ignored 7)",
        &[Some(17)],
    );
    assert_values(&runtime, "(strict :allow-other-keys nil)", &[Some(5)]);
    for call in [
        "(strict :ignored 7 :allow-other-keys nil :allow-other-keys t)",
        "(strict :x)",
        "(strict 4 9)",
        "(strict :ignored 7)",
        "(keys 10 :switch 7)",
    ] {
        assert_values(
            &runtime,
            &format!("(handler-case {call} (program-error () 42))"),
            &[Some(42)],
        );
    }

    assert_values(
        &runtime,
        "(apply #'values 1 '(2 3))",
        &[Some(1), Some(2), Some(3)],
    );
    assert_values(
        &runtime,
        "(apply 'options '(5 6))",
        &[Some(5), Some(6), Some(6), Some(1), Some(0)],
    );
    assert_values(
        &runtime,
        "(apply #'apply #'values '((8 9)))",
        &[Some(8), Some(9)],
    );
    assert_values(&runtime, "(apply #'values nil)", &[]);
    assert_values(
        &runtime,
        "(handler-case (apply #'values) (program-error () 43))",
        &[Some(43)],
    );
    assert!(runtime.eval("(apply #'values '(1 . 2))").is_err());
    assert!(runtime.eval("(apply 3 '(1 2))").is_err());

    runtime
        .register_function("collect-for-default", |rt, _| {
            rt.collect();
            Ok(vec![rt.fixnum(11).unwrap()])
        })
        .unwrap();
    assert_values(
        &runtime,
        "(apply #'(lambda (&key (a (collect-for-default)) b) (values a (car b)))
                (list :b (list 42)))",
        &[Some(11), Some(42)],
    );
}

#[test]
fn handler_clauses_bind_returned_values_using_ordinary_lambda_lists() {
    let runtime = Runtime::start().unwrap();
    assert_values(
        &runtime,
        "(handler-case (values 3 4 5)
           (:no-error (a &optional (b 0 b-p) &rest extra &aux (sum (+ a b)))
             (values sum (if b-p 1 0) (car extra))))",
        &[Some(7), Some(1), Some(5)],
    );
    assert_values(
        &runtime,
        "(handler-case (handler-case (values 10 11) (program-error () 0) (:no-error (a) a))
           (program-error () 7))",
        &[Some(7)],
    );
    assert_values(
        &runtime,
        "(restart-case (invoke-restart 'choose 6 :k 10)
           (choose (a &rest extra &key (k (+ a 1)) &aux (sum (+ a k)))
             (values sum (car (cdr extra)))))",
        &[Some(16), Some(10)],
    );
    assert_values(
        &runtime,
        "(block done
           (restart-case (invoke-restart 'choose)
             (choose (&optional (a (return-from done 47))) a)))",
        &[Some(47)],
    );
}

#[test]
fn parameter_declarations_follow_the_binding_scope_of_default_forms() {
    let runtime = Runtime::start().unwrap();
    assert_values(
        &runtime,
        "(funcall (lambda (x &optional (y (setq x (+ x 1)))
                                       (z (setq y (+ y 2))))
                    (declare (fixnum x y))
                    (values x y z))
                  7)",
        &[Some(8), Some(10), Some(10)],
    );
    assert_values(
        &runtime,
        "(let ((later nil))
           (funcall (lambda (&optional (early (setq later 3)) (later 9))
                      (declare (type (integer 8 99) later))
                      (values early later))))",
        &[Some(3), Some(9)],
    );
    assert_values(
        &runtime,
        "(let ((outside 1))
           (funcall (lambda (&optional (a (setq outside nil)) (b (setq outside 2)))
                      (declare (fixnum outside))
                      (values a b outside))))",
        &[None, Some(2), Some(2)],
    );
    assert_values(
        &runtime,
        "(handler-case
            (funcall (lambda (x &optional (a (setq x nil)) (b (setq x 2)))
                       (declare (optimize (safety 0)) (fixnum x))
                       (values a b x))
                     1)
          (type-error () 42))",
        &[Some(42)],
    );
}
