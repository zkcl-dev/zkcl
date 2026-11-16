use zkcl::Runtime;

#[test]
fn macroexpand_1_keeps_the_form_alive_during_hook_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-now", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap();
    runtime
        .load_str(
            "(defmacro sample (x) (list 'list x))
             (defmacro relay (x) (list 'sample x))
             (defun collecting-hook (expander form environment)
               (collect-now)
               (funcall expander form environment))",
        )
        .unwrap();

    let first = runtime
        .eval("(let ((*macroexpand-hook* #'collecting-hook)) (macroexpand-1 '(relay 43)))")
        .unwrap();
    let (head, arguments) = runtime.cons_fields(&first[0]).unwrap();
    assert_eq!(runtime.symbol_name(&head).as_deref(), Some("SAMPLE"));
    assert_eq!(
        runtime.cons_fields(&arguments).unwrap().0.as_fixnum(),
        Some(43)
    );
    assert_eq!(runtime.symbol_name(&first[1]).as_deref(), Some("T"));
}

#[test]
fn macroexpand_reports_whether_an_expander_was_invoked() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defmacro sample (x) (list 'list x))
             (defmacro relay (x) (list 'sample x))
             (defmacro empty-expansion () (values))",
        )
        .unwrap();

    let full = runtime.eval("(macroexpand '(relay 43))").unwrap();
    let (head, _) = runtime.cons_fields(&full[0]).unwrap();
    assert_eq!(runtime.symbol_name(&head).as_deref(), Some("LIST"));
    assert_eq!(runtime.symbol_name(&full[1]).as_deref(), Some("T"));

    let empty = runtime.eval("(macroexpand '(empty-expansion))").unwrap();
    assert!(empty[0].is_nil());
    assert_eq!(runtime.symbol_name(&empty[1]).as_deref(), Some("T"));

    let unchanged = runtime
        .eval("(let ((form (list 1 2))) (multiple-value-bind (result expanded) (macroexpand form) (values (eq form result) expanded)))")
        .unwrap();
    assert_eq!(runtime.symbol_name(&unchanged[0]).as_deref(), Some("T"));
    assert!(unchanged[1].is_nil());
}

#[test]
fn a_macroexpand_hook_can_throw_to_the_callers_catch() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defmacro sample (x) (list 'list x))")
        .unwrap();

    let escaped = runtime
        .eval(
            "(catch 'from-hook
               (let ((*macroexpand-hook*
                        (lambda (expander form environment)
                          (throw 'from-hook 78))))
                 (macroexpand-1 '(sample 8)))
               0)",
        )
        .unwrap();
    assert_eq!(escaped[0].as_fixnum(), Some(78));
}

#[test]
fn redefining_a_macro_replaces_its_global_operator_binding() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defmacro sample (x) (list 'list x))")
        .unwrap();

    let expander = runtime.eval("(macro-function 'sample)").unwrap();
    let input = runtime.eval("'(sample 19)").unwrap();
    runtime.eval("(defun sample (x) (+ x 12))").unwrap();
    assert!(runtime.eval("(macro-function 'sample)").unwrap()[0].is_nil());
    assert_eq!(runtime.eval("(sample 5)").unwrap()[0].as_fixnum(), Some(17));
    let saved = runtime
        .call(&expander[0], &[&input[0], &runtime.nil()])
        .unwrap();
    let (head, _) = runtime.cons_fields(&saved[0]).unwrap();
    assert_eq!(runtime.symbol_name(&head).as_deref(), Some("LIST"));

    runtime
        .eval("(defmacro sample (x) (list '+ x 22))")
        .unwrap();
    assert_eq!(runtime.eval("(sample 5)").unwrap()[0].as_fixnum(), Some(27));
    runtime
        .register_function("sample", |rt, _| Ok(vec![rt.fixnum(99).unwrap()]))
        .unwrap();
    assert_eq!(runtime.eval("(sample 5)").unwrap()[0].as_fixnum(), Some(99));
}

#[test]
fn append_reuses_the_final_argument_as_its_tail() {
    let runtime = Runtime::start().unwrap();
    let shared = runtime
        .eval("(let ((tail (list 2 3))) (eq (cdr (append '(1) tail)) tail))")
        .unwrap();
    assert_eq!(runtime.symbol_name(&shared[0]).as_deref(), Some("T"));
    let dotted = runtime.eval("(append '(1 2) 3)").unwrap();
    let (_, tail) = runtime.cons_fields(&dotted[0]).unwrap();
    assert_eq!(runtime.cons_fields(&tail).unwrap().1.as_fixnum(), Some(3));
    assert!(runtime.eval("(append '(1 . 2) '(3))").is_err());
}
