use zkcl::{EvalErrorKind, Runtime};

fn numbers(runtime: &Runtime, source: &str) -> Vec<Option<isize>> {
    runtime
        .eval(source)
        .unwrap()
        .iter()
        .map(|value| value.as_fixnum())
        .collect()
}

#[test]
fn compiler_lowered_macro_expansions_can_be_evaluated_after_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *definition*
                (macroexpand '(defun expanded-definition (x) (list x 42))))",
        )
        .unwrap();
    runtime.collect();
    runtime.eval("(eval *definition*)").unwrap();
    let result = runtime.eval("(expanded-definition 40)").unwrap();
    assert_eq!(runtime.repr(&result[0]).unwrap(), "(40 42)");
}

#[test]
fn conditional_macros_preserve_the_selected_forms_values() {
    let runtime = Runtime::start().unwrap();
    let truth = runtime.eval("(and)").unwrap();
    assert_eq!(runtime.symbol_name(&truth[0]).as_deref(), Some("T"));
    assert!(runtime.eval("(and (values))").unwrap().is_empty());
    assert_eq!(
        numbers(&runtime, "(and 1 (values 2 3))"),
        [Some(2), Some(3)]
    );
    let long_and = format!("(and {} (values 4 5))", "t ".repeat(512));
    assert_eq!(numbers(&runtime, &long_and), [Some(4), Some(5)]);
    let short_circuit = runtime
        .eval("(and (values nil 9) (error \"unreachable\"))")
        .unwrap();
    assert_eq!(short_circuit.len(), 1);
    assert!(short_circuit[0].is_nil());
    assert_eq!(
        numbers(&runtime, "(unless (values) (values 10 11))"),
        [Some(10), Some(11)]
    );
    assert_eq!(
        numbers(
            &runtime,
            "(let ((n 0)) (when (setq n (+ n 1)) (setq n (+ n 1)) (values n (+ n 1))))"
        ),
        [Some(2), Some(3)],
    );
    let skipped = runtime.eval("(when nil (error \"unreachable\"))").unwrap();
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].is_nil());
}

#[test]
fn prog_initializers_follow_let_scoping() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            "(let ((x 1)) (values (prog ((x 2) (y x)) (return y)) (prog* ((x 2) (y x)) (return y))))"
        ),
        [Some(1), Some(2)],
    );
    assert_eq!(
        numbers(
            &runtime,
            "(prog ((x 40))
               (declare (special x))
               (go done)
               (error \"unreachable\")
               done
               (return (values (symbol-value 'x) 42)))",
        ),
        [Some(40), Some(42)],
    );
    let normal_return = runtime.eval("(prog () (values 1 2))").unwrap();
    assert_eq!(normal_return.len(), 1);
    assert!(normal_return[0].is_nil());
}

#[test]
fn prog1_returns_only_the_first_primary_value() {
    let runtime = Runtime::start().unwrap();
    let zero_values = runtime.eval("(prog1 (values) (values 99))").unwrap();
    assert_eq!(zero_values.len(), 1);
    assert!(zero_values[0].is_nil());
    assert_eq!(
        numbers(&runtime, "(prog1 (values 12 13) (values 99))"),
        [Some(12)]
    );
}

#[test]
fn prog2_returns_the_second_forms_primary_value() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            "(let ((n 0))
               (values (prog2 (setq n 5) (values n 99) (setq n 6)) n))",
        ),
        [Some(5), Some(6)],
    );
}

#[test]
fn nth_value_evaluates_its_index_before_the_values_form() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        numbers(
            &runtime,
            "(let ((n 0))
               (values (nth-value n (progn (setq n 1) (values 41 99))) n))",
        ),
        [Some(41), Some(1)],
    );
    assert_eq!(numbers(&runtime, "(nth-value 2 (values 1 2 3))"), [Some(3)]);
    let missing = runtime.eval("(nth-value 3 (values 1 2 3))").unwrap();
    assert_eq!(missing.len(), 1);
    assert!(missing[0].is_nil());
}

#[test]
fn multiple_value_list_preserves_order() {
    let runtime = Runtime::start().unwrap();
    let list = runtime.eval("(multiple-value-list (values 7 8))").unwrap();
    let (first, rest) = runtime.cons_fields(&list[0]).unwrap();
    assert_eq!(first.as_fixnum(), Some(7));
    let (second, end) = runtime.cons_fields(&rest).unwrap();
    assert_eq!(second.as_fixnum(), Some(8));
    assert!(end.is_nil());
}

#[test]
fn standard_expanders_return_executable_if_forms() {
    let runtime = Runtime::start().unwrap();
    let expansion = runtime.eval("(macroexpand-1 '(cl:when t 7))").unwrap();
    let (operator, _) = runtime.cons_fields(&expansion[0]).unwrap();
    assert_eq!(runtime.symbol_name(&operator).as_deref(), Some("IF"));
    assert_eq!(runtime.symbol_name(&expansion[1]).as_deref(), Some("T"));
    let expander = runtime.eval("(macro-function 'cl:and)").unwrap();
    let input = runtime.eval("'(and 1 2)").unwrap();
    runtime.collect();
    let expanded = runtime
        .call(&expander[0], &[&input[0], &runtime.nil()])
        .unwrap();
    let (operator, _) = runtime.cons_fields(&expanded[0]).unwrap();
    assert_eq!(runtime.symbol_name(&operator).as_deref(), Some("IF"));
}

#[test]
fn macroexpand_hook_can_transfer_control() {
    let runtime = Runtime::start().unwrap();
    let hooked = runtime
        .eval(
            "(catch 'hook
               (let ((*macroexpand-hook*
                        (lambda (expander form environment)
                          (throw 'hook (car form)))))
                 (macroexpand-1 '(when t 7))))",
        )
        .unwrap();
    assert_eq!(runtime.symbol_name(&hooked[0]).as_deref(), Some("WHEN"));
}

#[test]
fn standard_macros_report_malformed_forms() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(when)",
        "(and . 7)",
        "(prog)",
        "(prog2 1)",
        "(nth-value 1)",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(error.kind(), EvalErrorKind::MacroExpansion, "{source}");
    }
    assert_eq!(
        numbers(
            &runtime,
            "(handler-case (macroexpand-1 '(when)) (program-error (c) 55))"
        ),
        [Some(55)],
    );
}

#[test]
fn short_circuiting_preserves_value_counts() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(or (values))", vec![]),
        ("(or nil (values))", vec![]),
        (
            "(or (values nil 9) (values 10 11))",
            vec![Some(10), Some(11)],
        ),
        ("(or (values 0 9) (error \"unreachable\"))", vec![Some(0)]),
        (
            "(let ((n 0))
               (values (or (setq n (+ n 1)) (error \"unreachable\")) n))",
            vec![Some(1), Some(1)],
        ),
        (
            "(block |OR EXIT|
               (or nil (return-from |OR EXIT| (values 20 21)))
               99)",
            vec![Some(20), Some(21)],
        ),
        ("(cond (t (values)))", vec![]),
        (
            "(cond ((values nil 9)) (t (values 30 31)))",
            vec![Some(30), Some(31)],
        ),
        (
            "(cond ((values 0 12)) (t (error \"unreachable\")))",
            vec![Some(0)],
        ),
        (
            "(let ((n 0))
               (values (cond ((progn (setq n (+ n 1)) nil))
                             ((setq n (+ n 1)))
                             ((error \"unreachable\")))
                       n))",
            vec![Some(2), Some(2)],
        ),
        (
            "(block |COND EXIT|
               (cond (t (return-from |COND EXIT| (values 40 41))))
               99)",
            vec![Some(40), Some(41)],
        ),
    ] {
        let actual = runtime
            .eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{source}");
    }
    for source in ["(or)", "(cond)", "(cond (nil) ((values)))"] {
        let values = runtime.eval(source).unwrap();
        assert!(values.len() == 1 && values[0].is_nil(), "{source}");
    }
    for source in ["(cond ())", "(cond 1)", "(cond (nil . 1))"] {
        assert_eq!(
            runtime.eval(source).err().unwrap().kind(),
            EvalErrorKind::MacroExpansion,
            "{source}",
        );
    }

    let long_or = format!("(or {} 50)", "nil ".repeat(512));
    let long_cond = format!("(cond {} (t 51))", "(nil) ".repeat(512));
    for (source, expected) in [(long_or, 50), (long_cond, 51)] {
        assert_eq!(
            runtime.eval(&source).unwrap()[0].as_fixnum(),
            Some(expected)
        );
    }
}

#[test]
fn conditional_macro_temporaries_survive_collection_during_expansion() {
    let runtime = Runtime::start().unwrap();
    // Move the freshly generated symbols before the compiler decodes the expansion.
    runtime
        .register_function("collect-expansion", |runtime, arguments| {
            let values = runtime.call(&arguments[0], &[&arguments[1], &arguments[2]])?;
            runtime.collect();
            Ok(values)
        })
        .unwrap();
    runtime
        .load_str("(setq *macroexpand-hook* #'collect-expansion)")
        .unwrap();
    let values = runtime.eval("(or nil (cond ((values 60 61))))").unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].as_fixnum(), Some(60));
}

#[test]
fn expanded_ignore_errors_returns_the_original_condition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (assert (equal (multiple-value-list (eval (macroexpand-1 '(ignore-errors (values 1 2)))))
                     '(1 2)))
      (defparameter *caught-error* (make-condition 'simple-error))
      (assert (equal (multiple-value-list
                       (eval (macroexpand-1 '(ignore-errors (error *caught-error*)))))
                     (list nil *caught-error*)))
    "#,
        )
        .unwrap();
}
