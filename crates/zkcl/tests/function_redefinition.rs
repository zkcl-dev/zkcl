use zkcl::Runtime;

#[test]
fn redefinition_changes_named_calls_without_replacing_saved_functions() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(setq offset 10)
             (defun convert (x) (values (+ offset x) x))
             (defun twice (x) (multiple-value-bind (a b) (convert x) (values a (+ b b))))
             (defun count-down (n acc) (if (= n 0) acc (count-down (- n 1) (+ 1 acc))))",
        )
        .unwrap();
    let name = runtime.eval("'convert").unwrap().pop().unwrap();
    let old = runtime.eval("#'convert").unwrap().pop().unwrap();
    assert_eq!(runtime.symbol_name(&name).as_deref(), Some("CONVERT"));
    assert!(name.same_identity(&runtime.eval("'cl-user::convert").unwrap()[0]));
    assert!(!name.same_identity(&runtime.eval(":convert").unwrap()[0]));
    assert!(!name.same_identity(&runtime.eval("'cl::convert").unwrap()[0]));
    assert!(
        runtime.eval("(eq :convert 'keyword:convert)").unwrap()[0]
            .same_identity(&runtime.eval("t").unwrap()[0])
    );
    assert!(runtime.eval("'cl-user:convert").is_err());

    let numbers = |source| {
        runtime
            .eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };
    assert_eq!(numbers("(twice 5)"), [Some(15), Some(10)]);
    assert_eq!(numbers("(count-down 1024 0)"), [Some(1024)]);
    assert_eq!(numbers("(funcall #'values 3 4)"), [Some(3), Some(4)]);
    assert_eq!(numbers("((lambda (x) (+ x 1)) 8)"), [Some(9)]);
    assert_eq!(
        numbers("(let ((convert 9)) (funcall #'convert convert))"),
        [Some(19), Some(9)]
    );
    assert_eq!(numbers("(let ((x 3)) (setq x 4) x)"), [Some(4)]);
    assert_eq!(
        numbers(
            "(if nil
                 (progn (defun car (x) x) (defmacro or (&rest args) args))
                 (car (list 42)))"
        ),
        [Some(42)]
    );
    assert_eq!(
        numbers(
            "(handler-case (defun car (x) x)
               (program-error () (car (list 42))))"
        ),
        [Some(42)]
    );
    assert_eq!(
        numbers("(progn (defun :keyword-function () 42) (:keyword-function))"),
        [Some(42)]
    );

    runtime
        .load_str("(defun convert (x) (values 1 x))")
        .unwrap();
    runtime.collect();
    let argument = runtime.fixnum(9).unwrap();
    let old_results = runtime.call(&old, &[&argument]).unwrap();
    assert_eq!(old_results[0].as_fixnum(), Some(19));
    assert_eq!(old_results[1].as_fixnum(), Some(9));
    assert_eq!(
        runtime.call(&name, &[&argument]).unwrap()[0].as_fixnum(),
        Some(1)
    );
    assert_eq!(numbers("(twice 5)"), [Some(1), Some(10)]);

    runtime
        .load_str("(in-package keyword) tag (cl:in-package cl-user)")
        .unwrap();
    assert_eq!(
        runtime.current_package().as_deref(),
        Some("COMMON-LISP-USER")
    );
    assert_eq!(
        runtime
            .symbol_name(&runtime.eval("keyword:tag").unwrap()[0])
            .as_deref(),
        Some("TAG")
    );
    assert!(runtime.eval("(convert)").is_err());
    assert!(runtime.eval("(unknown-function 1)").is_err());
}
