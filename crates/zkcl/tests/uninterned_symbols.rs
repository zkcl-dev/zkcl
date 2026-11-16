use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn uninterned_symbols_have_distinct_identity() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-symbols-now", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(values '#:same '#:same 'same
                       (make-symbol "SAME") (make-symbol "SAME"))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(runtime.symbol_name(value).as_deref(), Some("SAME"));
        assert!(values[..i].iter().all(|other| !value.same_identity(other)));
    }

    let values = runtime
        .eval(
            r#"(let ((fresh (make-symbol "NIL"))
                     (mixed (make-symbol "a:雪"))
                     (escaped '#:|a:雪|))
                 (values (symbol-name fresh) (symbol-name mixed)
                         (symbol-name escaped) (symbol-name '#:||)
                         (symbol-name '#:|42|) (symbol-name nil) (symbol-name t)
                         (symbol-package fresh) (symbol-package escaped)
                         (boundp fresh) (eq fresh nil)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 11);
    for (value, text) in values
        .iter()
        .zip(["NIL", "a:雪", "a:雪", "", "42", "NIL", "T"])
    {
        assert_eq!(runtime.string(value).as_deref(), Some(text));
    }
    assert!(values[7..].iter().all(|value| value.is_nil()));

    let values = runtime
        .eval(
            "(values (symbol-package 'car) (symbol-package nil) (symbol-package t)
                     (symbol-package :same) (symbol-package 'same))",
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values.iter().all(|value| !value.is_nil()));
    assert!(values[0].same_identity(&values[1]));
    assert!(values[0].same_identity(&values[2]));
    assert!(!values[0].same_identity(&values[3]));
    assert!(!values[0].same_identity(&values[4]));
    assert!(!values[3].same_identity(&values[4]));

    let values = runtime
        .eval(
            r#"(let* ((s (make-symbol "stable name"))
                      (before (symbol-name s)))
                 (collect-symbols-now)
                 (values before (symbol-name s)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("stable name"));
    assert!(values[0].same_identity(&values[1]));

    for source in ["'#:CL-USER::NAME", "'#::NAME", "'#:42"] {
        let error = runtime
            .eval(source)
            .err()
            .unwrap_or_else(|| panic!("accepted {source}"));
        assert_eq!(error.kind(), EvalErrorKind::Read, "{source}: {error}");
    }
}

#[test]
fn gensym_uses_the_dynamic_counter_only_for_generated_suffixes() {
    let runtime = Runtime::start().unwrap();
    let global_counter = runtime.eval("*gensym-counter*").unwrap()[0].as_fixnum();
    let values = runtime
        .eval(
            r#"(let ((*gensym-counter* 4))
                 (let* ((fixed (gensym 42))
                        (another (gensym 42))
                        (prefixed (gensym "mixed:雪-"))
                        (default (gensym)))
                   (values fixed another prefixed default
                           *gensym-counter* (symbol-package prefixed))))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 6);
    for (value, spelling) in values.iter().zip(["G42", "G42", "mixed:雪-4", "G5"]) {
        assert_eq!(runtime.symbol_name(value).as_deref(), Some(spelling));
    }
    assert!(!values[0].same_identity(&values[1]));
    assert_eq!(values[4].as_fixnum(), Some(6));
    assert!(values[5].is_nil());
    assert_eq!(
        runtime.eval("*gensym-counter*").unwrap()[0].as_fixnum(),
        global_counter
    );

    for source in [
        "(make-symbol 1)",
        "(symbol-name 1)",
        "(symbol-package 1)",
        "(gensym -1)",
        "(gensym 'not-a-string)",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .unwrap_or_else(|| panic!("accepted {source}"));
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError),
            "{source}: {error}"
        );
    }
}

#[test]
fn macro_expansion_keeps_same_named_uninterned_bindings_distinct() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-symbols-now", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    runtime
        .load_str(
            "(defmacro generated-parameters ()
               (let ((left (make-symbol \"SAME\")) (right (make-symbol \"SAME\")))
                 `(funcall (lambda (,left ,right)
                             (collect-symbols-now)
                             (+ ,left ,right))
                           40 2)))
             (defmacro install-hidden-function ()
               (let ((name (make-symbol \"HIDDEN\")))
                 `(progn (defun ,name (n) (+ n 2)) ',name)))
             (defmacro install-hidden-macro ()
               (let ((name (make-symbol \"HIDDEN\")))
                 `(progn (defmacro ,name () 42)
                         (setq hidden-macro-name ',name)
                         (collect-symbols-now)
                         (,name))))
             (defmacro replace-hidden-macro ()
               `(defun ,hidden-macro-name () 17))
             (defmacro generated-macrolet ()
               (let ((answer (make-symbol \"LOCAL\")) (relay (make-symbol \"LOCAL\")))
                 `(macrolet ((,answer () 40)
                             (,relay (&environment env)
                               (collect-symbols-now)
                               (macroexpand-1 '(,answer) env)))
                    (+ (,relay) 2))))",
        )
        .unwrap();

    assert_eq!(
        runtime.eval("(generated-parameters)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    assert_eq!(
        runtime.eval("(generated-macrolet)").unwrap()[0].as_fixnum(),
        Some(42)
    );

    let mut values = runtime.eval("(install-hidden-function)").unwrap();
    let function_symbol = values.pop().unwrap();
    assert_eq!(
        runtime.eval("(install-hidden-macro)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    let mut values = runtime.eval("hidden-macro-name").unwrap();
    let macro_symbol = values.pop().unwrap();
    assert!(!function_symbol.same_identity(&macro_symbol));
    runtime.collect();
    let n = runtime.fixnum(40).unwrap();
    assert_eq!(
        runtime.call(&function_symbol, &[&n]).unwrap()[0].as_fixnum(),
        Some(42)
    );
    let expansion = runtime
        .eval("(macroexpand-1 (list hidden-macro-name))")
        .unwrap();
    assert_eq!(expansion.len(), 2);
    assert_eq!(expansion[0].as_fixnum(), Some(42));
    assert!(!expansion[1].is_nil());

    runtime.eval("(replace-hidden-macro)").unwrap();
    runtime.collect();
    assert!(runtime.eval("(macro-function hidden-macro-name)").unwrap()[0].is_nil());
    assert_eq!(
        runtime.call(&macro_symbol, &[]).unwrap()[0].as_fixnum(),
        Some(17)
    );
}
