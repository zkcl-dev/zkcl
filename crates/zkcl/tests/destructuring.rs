use zkcl::Runtime;

#[test]
fn destructuring_defaults_see_bindings_from_earlier_subpatterns() {
    let rt = Runtime::start().unwrap();
    rt.register_function("collect-now", |rt, _| {
        rt.collect();
        Ok(vec![rt.nil()])
    })
    .unwrap();

    let values = rt
        .eval(
            "(let ((input (cons (list 3) 'tail)))
               (destructuring-bind
                   (&whole all (&whole inner x &optional (y (+ x 2) y-p)) . rest)
                   input
                 (values (eq input all) (eq (car input) inner) x y y-p rest)))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert_eq!(values[2].as_fixnum(), Some(3));
    assert_eq!(values[3].as_fixnum(), Some(5));
    assert!(values[4].is_nil());
    assert_eq!(rt.symbol_name(&values[5]).as_deref(), Some("TAIL"));

    let values = rt
        .eval(
            "(destructuring-bind
                 ((a b) &optional ((c d) (list (+ a b) b))
                  &key ((:tuple (x y)) (list (+ c d) c)) (offset c offset-p)
                  &aux (sum (+ x y offset)))
                 '((2 3))
               (values sum offset-p))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(18));
    assert!(values[1].is_nil());

    let values = rt
        .eval(
            "(destructuring-bind
                 (&rest keys &key ((:pair (a b))) (value b) &allow-other-keys)
                 '(:pair (2 7) :pair (50 50) :extra 5)
               (values a b value (lambda () (car keys))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(2));
    assert_eq!(values[1].as_fixnum(), Some(7));
    assert_eq!(values[2].as_fixnum(), Some(7));
    let keyword = rt.call(&values[3], &[]).unwrap();
    assert_eq!(rt.symbol_name(&keyword[0]).as_deref(), Some("PAIR"));

    let retained = rt
        .eval(
            "(destructuring-bind
                 (&key (unused (collect-now)) ((:pair (left right))))
                 (list :pair (list 11 12))
               (values left right))",
        )
        .unwrap();
    assert_eq!(retained[0].as_fixnum(), Some(11));
    assert_eq!(retained[1].as_fixnum(), Some(12));

    let values = rt
        .eval("(destructuring-bind (&body (head . tail)) '(1 . 2) (values head tail))")
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(1));
    assert_eq!(values[1].as_fixnum(), Some(2));

    rt.load_str(
        "(defvar *current* 10)
         (defun see-current () *current*)
         (defmacro tuple ((x y) &optional ((label count) (list 'ok (+ x y))))
           (list 'quote (list x y label count)))
         (defmacro dotted (head &rest tail) (list 'quote (cons head tail)))",
    )
    .unwrap();
    let values = rt
        .eval(
            "(destructuring-bind (*current* &optional (z (see-current))) '(2)
               (declare (special *current*))
               (values (see-current) z))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(2));
    assert_eq!(values[1].as_fixnum(), Some(2));
    assert_eq!(rt.eval("*current*").unwrap()[0].as_fixnum(), Some(10));
    let value = rt.eval("(car (cdr (cdr (cdr (tuple (4 6))))))").unwrap();
    assert_eq!(value[0].as_fixnum(), Some(10));
    let value = rt.eval("(cdr (dotted 1 . marker))").unwrap();
    assert_eq!(rt.symbol_name(&value[0]).as_deref(), Some("MARKER"));

    for source in [
        "(handler-case (destructuring-bind ((x y)) '((1)) x) (program-error () t))",
        "(handler-case (destructuring-bind (x) '(1 2) x) (program-error () t))",
        "(handler-case (destructuring-bind (&key x) '(:x) x) (program-error () t))",
        "(handler-case (macroexpand '(tuple (4))) (program-error () t))",
    ] {
        assert!(!rt.eval(source).unwrap()[0].is_nil(), "{source}");
    }
}

#[test]
fn quote_names_in_macro_generated_destructuring_patterns_are_bindings() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (define-compiler-macro decode-method (&whole whole argument)
        (if (and (consp argument) (eq (car argument) 'quote))
            (destructuring-bind (quote name) argument
              (ecase quote (quote (list 'quote name))))
            whole))
      (let ((expander (compiler-macro-function 'decode-method)))
        (assert (equal (funcall expander '(decode-method 'sample) nil) ''sample)))
      (let ((name (gensym)))
        (assert (eq name (eval (list 'quote name)))))
    "#,
        )
        .unwrap();
}
