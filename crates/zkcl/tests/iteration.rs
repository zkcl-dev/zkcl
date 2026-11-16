use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn iteration_result_forms_observe_the_final_loop_bindings() {
    let runtime = Runtime::start().unwrap();
    let numbers = |source| {
        runtime
            .eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        numbers(
            "(let ((i 4) (calls 0) (sum 0))
               (dotimes (i (progn (setq calls (+ calls 1)) i) (values i calls sum))
                 (if (= i 1) (go skip))
                 (setq sum (+ sum i))
                skip
                 (setq i 99)))"
        ),
        [Some(4), Some(1), Some(5)]
    );
    assert_eq!(
        numbers("(dotimes (i -3 (values i 8)) (error \"ran a negative loop\"))"),
        [Some(0), Some(8)]
    );
    assert_eq!(
        numbers("(dotimes (i 4 99) (if (= i 2) (return (values i 13))))"),
        [Some(2), Some(13)]
    );
    assert_eq!(
        numbers("(dotimes (i (return (values 17 23))) (error \"ran the body\"))"),
        [Some(17), Some(23)]
    );
    assert_eq!(
        numbers(
            "(let ((functions nil))
               (dotimes (i 3) (setq functions (cons (lambda () i) functions)))
               (values (funcall (caddr functions)) (funcall (cadr functions))
                       (funcall (car functions))))"
        ),
        [Some(0), Some(1), Some(2)]
    );
    assert!(runtime.eval("(dotimes (i 0 (values)))").unwrap().is_empty());

    assert_eq!(
        numbers("(loop initially (incf total) finally (return total) with total = 41 repeat 0)"),
        [Some(42)]
    );

    let values = runtime
        .eval(
            "(let ((item '(7 11 23)) (calls 0) (sum 0))
               (dolist (item (progn (setq calls (+ calls 1)) item)
                             (values item sum calls))
                 (if (= item 11) (go skip))
                 (setq sum (+ sum item))
                skip
                 (setq item 91)))",
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert_eq!(values[1].as_fixnum(), Some(30));
    assert_eq!(values[2].as_fixnum(), Some(1));
    assert_eq!(
        numbers(
            "(let ((item 77))
               (dolist (item '(1 42 3) 99)
                 (declare (special item))
                 (if (= item 42)
                     (return-from nil (values item (symbol-value 'item))))))"
        ),
        [Some(42), Some(42)]
    );
    assert!(runtime.eval("(dolist (item nil item))").unwrap()[0].is_nil());
    assert_eq!(
        numbers(
            "(let ((total 0))
               (dolist (item '(\"a\" \"bc\"))
                 (declare (string item))
                 (incf total (length item)))
               total)"
        ),
        [Some(3)]
    );
    let error = runtime
        .eval("(dolist (item '(1 . 2)) (values item))")
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );

    assert_eq!(
        numbers("(let ((x 9)) (values (do ((x 1) (y x)) (t y)) (do* ((x 1) (y x)) (t y))))"),
        [Some(9), Some(1)]
    );
    assert_eq!(
        numbers("(do ((i 0 (+ i 1)) (previous 99 i)) ((= i 3) (values i previous)))"),
        [Some(3), Some(2)]
    );
    assert_eq!(
        numbers("(do* ((i 0 (+ i 1)) (current 99 i)) ((= i 3) (values i current)))"),
        [Some(3), Some(3)]
    );
    assert_eq!(
        numbers(
            "(let ((n 97))
               (do ((n 40) (observed n))
                   (t (values (symbol-value 'n) observed))
                 (declare (special n))))"
        ),
        [Some(40), Some(97)]
    );
    assert_eq!(
        numbers(
            "(let ((n 97))
               (do* ((n 40) (observed (symbol-value 'n)))
                    (t (values n observed))
                 (declare (special n))))"
        ),
        [Some(40), Some(40)]
    );
    assert_eq!(
        numbers(
            "(let ((log 0))
               (do ((i 0 (progn (setq log (+ (* log 10) 1)) (+ i 1)))
                    (x 42 (progn (setq log (+ (* log 10) 2)) x))
                    unset (also-unset))
                   ((= i 2) (values log x unset also-unset))
                 (go skip)
                 (error \"GO did not jump\")
                skip))"
        ),
        [Some(1212), Some(42), None, None]
    );
    assert_eq!(
        numbers(
            "(let ((n 80))
               (do ((n 1 (+ n 1)))
                   ((= n 3) (values n (symbol-value 'n)))
                 (declare (special n))))"
        ),
        [Some(3), Some(3)]
    );
    assert_eq!(
        numbers("(do () (nil) (return (values 40 2)))"),
        [Some(40), Some(2)]
    );
    assert!(runtime.eval("(do ((i 0)) (t))").unwrap()[0].is_nil());
    assert!(runtime.eval("(do () (t (values)))").unwrap().is_empty());
}

#[test]
fn less_than_checks_later_operands_even_after_a_false_comparison() {
    let runtime = Runtime::start().unwrap();
    let ordered = runtime
        .eval("(values (< -9 0 7) (< -9) (< -9 0 -1))")
        .unwrap();
    assert!(!ordered[0].is_nil());
    assert!(!ordered[1].is_nil());
    assert!(ordered[2].is_nil());
    let error = runtime.eval("(< 2 1 nil)").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
}
