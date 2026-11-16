use zkcl::Runtime;

mod common;

use common::lisp::eval;

fn assert_fixnums(rt: &Runtime, source: &str, expected: &[isize]) {
    let values = eval(rt, source);
    let actual: Vec<_> = values.iter().map(|value| value.as_fixnum()).collect();
    let expected: Vec<_> = expected.iter().copied().map(Some).collect();
    assert_eq!(actual, expected, "{source}");
}

#[test]
fn local_macro_bindings_are_visible_only_in_their_expansion_environment() {
    let rt = Runtime::start().unwrap();
    rt.register_function("collect-now", |rt, _| {
        rt.collect();
        Ok(vec![])
    })
    .unwrap();
    rt.load_str(
        "(defmacro choose (n) (list '+ n 10))
         (defmacro expansion-in-context (form &environment env)
           (let ((*macroexpand-hook*
                    (lambda (expander raw-form received-env)
                      (collect-now)
                      (if (eq env received-env)
                          (funcall expander raw-form received-env)
                          -1))))
             (multiple-value-bind (expanded-form expanded-p) (macroexpand-1 form env)
               (if expanded-p expanded-form 0))))
         (defmacro function-in-context (form &environment env)
           (let ((expander (macro-function (car form) env)))
             (if expander (funcall expander form env) 0)))
         (defmacro macro-visible-in-context (name &environment env)
           (if (macro-function name env) 1 0))
         (defmacro remember-context (&environment env)
           (setq saved-macro-context env)
           1)
         (defmacro fail-in-context (&environment env)
           (setq failed-macro-context env)
           (error \"expansion failed\"))",
    )
    .unwrap();

    assert_fixnums(
        &rt,
        "(let ((offset 10))
           (macrolet ((add-offset (n) (list '+ 'offset n)))
             (add-offset 4)))",
        &[14],
    );
    assert!(rt.eval("(macro-function 'add-offset)").unwrap()[0].is_nil());
    assert!(rt.eval("(add-offset 4)").is_err());
    assert_fixnums(
        &rt,
        "(values (macrolet ((choose (n) (list '+ n 20))) (choose 2))
                 (choose 2))",
        &[22, 12],
    );
    let global = rt
        .eval("(macrolet ((choose (n) (list '+ n 20))) (macroexpand-1 '(choose 2)))")
        .unwrap();
    let (_, operands) = rt.cons_fields(&global[0]).unwrap();
    let (_, operand_tail) = rt.cons_fields(&operands).unwrap();
    assert_eq!(
        rt.cons_fields(&operand_tail).unwrap().0.as_fixnum(),
        Some(10)
    );
    assert!(!global[1].is_nil());

    // The outer macro is visible to the FLET definition. Its function binding
    // takes precedence in the body, where a deeper MACROLET can shadow it again.
    assert_fixnums(
        &rt,
        "(macrolet ((choose (n) (list '+ n 20)))
           (flet ((choose (n) (choose n)))
             (values (choose 3)
                     (macrolet ((choose (n) (list '+ n 30))) (choose 3))
                     (choose 4))))",
        &[23, 33, 24],
    );
    assert_fixnums(
        &rt,
        "(macrolet ((choose (n) 100))
           (labels ((choose (n)
                      (if (= n 0) 90 (+ 1 (choose (- n 1))))))
             (choose 2)))",
        &[92],
    );
    assert_fixnums(
        &rt,
        "(macrolet ((choose (n) (list '+ n 20)))
           (macrolet ((choose (n) (list '+ n 30))
                      (choose-in-definition-environment (n) (choose n)))
             (values (choose-in-definition-environment 2) (choose 2))))",
        &[22, 32],
    );

    assert_fixnums(
        &rt,
        "(macrolet ((choose (n) (list '+ n 40)))
           (values (expansion-in-context (choose 2))
                   (function-in-context (choose 3))
                   (macro-visible-in-context choose)
                   (flet ((choose (n) (+ n 50)))
                     (expansion-in-context (choose 4)))
                   (labels ((choose (n) (+ n 60)))
                     (macro-visible-in-context choose))
                   (expansion-in-context (choose 5))))",
        &[42, 43, 1, 0, 0, 45],
    );
    assert_fixnums(
        &rt,
        "(macrolet ((relay (n &environment env)
                      (macroexpand (list 'second-local n) env))
                    (second-local (n) (list '+ n 70)))
           (function-in-context (relay 2)))",
        &[72],
    );

    // Keep the environment object rooted in a Lisp variable past expansion.
    // Subsequent lookups should reject the expired handle after collection.
    assert_fixnums(
        &rt,
        "(macrolet ((local-only () 1)) (remember-context))",
        &[1],
    );
    assert!(!rt.eval("saved-macro-context").unwrap()[0].is_nil());
    rt.collect();
    assert!(
        rt.eval("(macro-function 'choose saved-macro-context)")
            .is_err()
    );
    assert!(
        rt.eval("(macroexpand-1 '(choose 3) saved-macro-context)")
            .is_err()
    );
    let failure = rt.eval("(macrolet ((local-only () 1)) (fail-in-context))");
    assert!(
        failure
            .err()
            .unwrap()
            .to_string()
            .contains("expansion failed")
    );
    assert!(!rt.eval("failed-macro-context").unwrap()[0].is_nil());
    assert!(
        rt.eval("(macro-function 'choose failed-macro-context)")
            .is_err()
    );
    assert_fixnums(&rt, "(choose 3)", &[13]);
}
