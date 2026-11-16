use zkcl::Runtime;

#[test]
fn macroexpand_follows_successive_expansions_to_the_ordinary_call() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(progn
               (defmacro when-do (test &body body)
                 `(if ,test (progn ,@body) nil))
               (defun conditional (x)
                 (when-do (= x 3) (values (+ x 2) x)))
               (conditional 3))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(5));
    assert_eq!(values[1].as_fixnum(), Some(3));
    assert!(rt.eval("(conditional 4)").unwrap()[0].is_nil());

    rt.load_str(
        "(defmacro bump (x &optional (amount 2 provided))
           (if provided (list '+ x amount) (list '+ amount x)))
         (defmacro relay (x) (list 'bump x))
         (defmacro exit-macro (x)
           (return-from exit-macro (list '+ x 4))
           0)",
    )
    .unwrap();
    assert_eq!(rt.eval("(relay 6)").unwrap()[0].as_fixnum(), Some(8));
    assert_eq!(rt.eval("(exit-macro 8)").unwrap()[0].as_fixnum(), Some(12));
    assert_eq!(
        rt.eval("(let ((n 10)) (bump n 5))").unwrap()[0].as_fixnum(),
        Some(15)
    );

    let once = rt.eval("(macroexpand-1 '(relay 6))").unwrap();
    let full = rt.eval("(macroexpand '(relay 6))").unwrap();
    assert!(!once[1].is_nil());
    assert!(!full[1].is_nil());
    let (head, _) = rt.cons_fields(&once[0]).unwrap();
    assert_eq!(rt.symbol_name(&head).as_deref(), Some("BUMP"));
    let (head, _) = rt.cons_fields(&full[0]).unwrap();
    assert_eq!(rt.symbol_name(&head).as_deref(), Some("+"));

    assert_eq!(
        rt.eval("(flet ((bump (n) (+ n 100))) (bump 2))").unwrap()[0].as_fixnum(),
        Some(102)
    );
    rt.eval("(defmacro recursive (x) (list 'recursive x))")
        .unwrap();
    let error = rt.eval("(recursive 5)").err().unwrap();
    assert!(error.to_string().contains("expansion depth"));
    assert_eq!(rt.eval("(bump 1)").unwrap()[0].as_fixnum(), Some(3));
}

#[test]
fn macro_lambda_lists_bind_the_original_call() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defmacro inspect-macro-call (&whole call &environment environment argument . nil)
           (list 'quote (list (car call) argument environment)))
         (defmacro tail-of (first . rest) (list 'quote rest))",
    )
    .unwrap();
    let result = rt.eval("(inspect-macro-call 19)").unwrap();
    let (name, rest) = rt.cons_fields(&result[0]).unwrap();
    assert_eq!(rt.symbol_name(&name).as_deref(), Some("INSPECT-MACRO-CALL"));
    let (argument, rest) = rt.cons_fields(&rest).unwrap();
    assert_eq!(argument.as_fixnum(), Some(19));
    let (environment, _) = rt.cons_fields(&rest).unwrap();
    assert!(environment.is_nil());
    let tail = rt.eval("(tail-of 5 7 8)").unwrap();
    assert_eq!(rt.cons_fields(&tail[0]).unwrap().0.as_fixnum(), Some(7));
}
