use std::fs;

use zkcl::Runtime;

mod common;

use common::files::TempDir;
use common::lisp::assert_truthy;

#[test]
fn eql_specializers_run_once_in_the_environment_of_method_definition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defgeneric sum-integers (x y z)
              (declare (optimize speed))
              (:documentation "An integer sum.")
              (:generic-function-class standard-generic-function)
              (:method-class standard-method)
              (:method-combination standard)
              (:method ((x integer) (y integer) (z integer)) (+ x y z)))

            (defparameter *specializer-evaluations* nil)
            (let ((a 10) (b 20))
              (defmethod key-pair ((x (eql (progn (push 'first *specializer-evaluations*) a)))
                                   (y (eql (progn (push 'second *specializer-evaluations*) b))))
                (list x y a b)))

            (defgeneric prefer-right (x y) (:argument-precedence-order y x))
            (defmethod prefer-right ((x integer) y)
              (declare (ignore x y))
              'left)
            (defmethod prefer-right (x (y integer))
              (declare (ignore x y))
              (return-from prefer-right (list 'right (next-method-p) (call-next-method))))

            (defgeneric next-arguments (x &optional y))
            (defmethod next-arguments ((x number) &optional (y 80 supplied-p))
              (values x y supplied-p (next-method-p)))
            (defmethod next-arguments ((x integer) &optional (y 70 supplied-p))
              (if supplied-p
                  (call-next-method (+ x y) y)
                  (progn (setq x -1 y -2) (call-next-method))))

            (defgeneric method-keywords (x &key common))
            (defmethod method-keywords ((x number) &key common wide)
              (list :number common wide))
            (defmethod method-keywords ((x integer) &key ((:common shared)) narrow &aux (saved narrow))
              (list :integer shared saved (call-next-method)))

            (defgeneric (setf generic-place) (value object))
            (defmethod (setf generic-place) (value (object cons))
              (return-from generic-place (rplaca object value)))
            "#,
        )
        .unwrap();
    runtime.collect();

    for check in [
        "(typep (fdefinition 'sum-integers) 'standard-generic-function)",
        "(= (sum-integers 10 20 12) 42)",
        "(equal (key-pair 10 20) '(10 20 10 20))",
        "(equal *specializer-evaluations* '(second first))",
        "(equal (prefer-right 1 2) '(right t left))",
        "(equal (multiple-value-list (next-arguments 4)) '(4 80 nil nil))",
        "(equal (multiple-value-list (next-arguments 4 2)) '(6 2 t nil))",
        "(equal (method-keywords 1 :common 1 :wide 2 :narrow 3) '(:integer 1 3 (:number 1 2)))",
        "(eq (fdefinition '(setf generic-place)) #'(setf generic-place))",
        "(let ((cell (list 1))) (setf (generic-place cell) 4) (equal cell '(4)))",
    ] {
        assert_truthy(&runtime, check);
    }
    for form in [
        "(macroexpand-1 '(defgeneric (not-setf example) (x)))",
        "(macroexpand-1 '(defgeneric malformed (x x)))",
        "(macroexpand-1 '(defgeneric malformed (&optional (x nil))))",
        "(macroexpand-1 '(defgeneric malformed (&key (x nil))))",
        "(macroexpand-1 '(defgeneric malformed (&aux)))",
        "(macroexpand-1 '(defgeneric malformed (x y) (:argument-precedence-order x x)))",
        "(macroexpand-1 '(defgeneric malformed (x) (declare)))",
        "(macroexpand-1 '(defgeneric malformed (x) (:documentation 1)))",
        "(macroexpand-1 '(defmethod malformed ((x (eql nil t))) x))",
    ] {
        assert_truthy(
            &runtime,
            &format!("(handler-case (progn {form} nil) (program-error () t))"),
        );
    }

    let temp_dir = TempDir::new("generic-macros");
    let source = temp_dir.path().join("method.lisp");
    let compiled = temp_dir.path().join("method.fasl");
    fs::write(
        &source,
        "(defgeneric load-time-generic (x))
         (defmethod load-time-generic ((x (eql (progn
                                     (push 'load-time *specializer-evaluations*) 64))))
           (return-from load-time-generic (values x :compiled)))",
    )
    .unwrap();
    let result = runtime
        .eval(&format!(
            "(compile-file {:?} :output-file {:?} :verbose nil :print nil)",
            source.to_str().unwrap(),
            compiled.to_str().unwrap()
        ))
        .unwrap();
    assert!(!result[0].is_nil());
    assert!(result[2].is_nil());
    assert_truthy(&runtime, "(not (fboundp 'load-time-generic))");
    assert_truthy(
        &runtime,
        "(equal *specializer-evaluations* '(second first))",
    );
    runtime
        .eval(&format!("(load {:?})", compiled.to_str().unwrap()))
        .unwrap();
    runtime.collect();
    assert_truthy(
        &runtime,
        "(equal (multiple-value-list (load-time-generic 64)) '(64 :compiled))",
    );
    assert_truthy(
        &runtime,
        "(equal *specializer-evaluations* '(load-time second first))",
    );
}
