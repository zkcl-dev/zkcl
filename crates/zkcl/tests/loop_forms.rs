use zkcl::{EvalErrorKind, Runtime};

#[test]
fn loop_accumulates_results_from_successive_iteration_bindings() {
    let runtime = Runtime::start().unwrap();
    let repr = |source| {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        values
            .iter()
            .map(|value| runtime.repr(value).unwrap())
            .collect::<Vec<_>>()
    };

    assert_eq!(repr("(loop (return (values 40 2)))"), ["40", "2"]);
    assert_eq!(
        repr(
            "(loop :named outer :for i :from 1 :to 99 :when (= i 3)
                     :return (values i 42))"
        ),
        ["3", "42"]
    );
    assert_eq!(
        repr("(loop for x from 1 to 4 for y = (+ x 10) collect (list x y))"),
        ["((1 11) (2 12) (3 13) (4 14))"]
    );
    assert_eq!(
        repr("(loop for x = 1 then y and y = 2 then x repeat 3 collect (list x y))"),
        ["((1 2) (2 1) (1 2))"]
    );
    assert_eq!(
        repr("(loop for i from 0 below 3 and j from 1 finally (return (list i j)))"),
        ["(2 3)"]
    );
    assert_eq!(
        repr("(loop for i downfrom 7 above 1 by 2 collect i)"),
        ["(7 5 3)"]
    );
    assert_eq!(repr("(loop for v on '(1 . 2) collect v)"), ["((1 . 2))"]);
    assert_eq!(
        repr("(loop for x in '(1 2 3 4 5) by #'cddr collect x)"),
        ["(1 3 5)"]
    );
    assert_eq!(
        repr("(loop for a in '(1 2) and b = a collect (list a b))"),
        ["((1 NIL) (2 1))"]
    );
    assert_eq!(
        repr(
            "(let ((a 9))
               (values (loop with a = 1 and b = a return b)
                       (loop with a = 1 with b = a return b)))"
        ),
        ["9", "1"]
    );
    assert_eq!(
        repr("(loop with i fixnum and j of-type number and k return (values i j k))"),
        ["0", "0", "NIL"]
    );
    assert_eq!(
        repr(
            "(let ((side-effects 0))
               (values
                 (loop for x in nil for y = (incf side-effects) collect y)
                 side-effects))"
        ),
        ["NIL", "0"]
    );
    assert_eq!(
        repr(
            "(let ((orders nil))
               (values
                 (loop for i from (progn (push :start orders) 1)
                             by (progn (push :step orders) 2)
                             to (progn (push :limit orders) 5)
                       collect i)
                 orders))"
        ),
        ["(1 3 5)", "(:LIMIT :STEP :START)"]
    );

    assert_eq!(
        repr(
            "(loop with count = 0
               for x in '(1 2 3 4 5)
               initially (setq count 10)
               when (< x 4) collect x into accumulated and do (incf count)
               finally (return (values accumulated count)))"
        ),
        ["(1 2 3)", "13"]
    );
    assert_eq!(
        repr(
            "(loop for n in '(1 2 4)
               if (if (< n 3) (* n 10) nil)
                 collect it
               else
                 append (list n :end)
               end)"
        ),
        ["(10 20 4 :END)"]
    );
    assert_eq!(
        repr(
            "(let ((effects nil))
               (values (loop for n in '(1 2 3)
                             do (push n effects)
                             thereis (if (= n 2) 42)
                             finally (push :finally effects))
                       effects))"
        ),
        ["42", "(2 1)"]
    );
    assert_eq!(
        repr(
            "(let ((log nil))
               (values
                 (loop for n from 1
                       do (push n log)
                       until (= n 3)
                       collect n
                       finally (push :finished log))
                 log))"
        ),
        ["(1 2)", "(:FINISHED 3 2 1)"]
    );
    assert_eq!(
        repr("(loop for n in '(1 2 3 4) when (= n 3) do (loop-finish) sum n)"),
        ["3"]
    );
    assert_eq!(
        repr(
            "(values (loop repeat -1 always (error \"unexpected iteration\"))
                     (loop for n from 1 to 3 never (= n 2))
                     (loop for n from 1 to 3 sum n count (< n 3))
                     (loop for n in '(-7 -2 -5) maximize n))"
        ),
        ["T", "NIL", "8", "-2"]
    );

    assert_eq!(
        repr("(loop for x across \"abc\" collect x)"),
        ["(#\\a #\\b #\\c)"]
    );
    assert_eq!(repr("(loop for x across #(1 2 3) sum x)"), ["6"]);
    assert_eq!(repr("(loop for x across #() collect x)"), ["NIL"]);
    assert_eq!(
        repr("(loop for i from 1 to 2 for x across #(10 20) collect (list i x))"),
        ["((1 10) (2 20))"]
    );
    assert_eq!(
        repr("(loop for (a . b) in '((1 . 2) (3 . 4)) collect (list a b))"),
        ["((1 2) (3 4))"]
    );
    assert_eq!(
        repr("(loop for (a b) in '((1 2) (3 4)) collect (+ a b))"),
        ["(3 7)"]
    );
    assert_eq!(
        repr("(loop for (a (b c)) in '((1 (2 3))) collect (list a b c))"),
        ["((1 2 3))"]
    );
    assert_eq!(
        repr("(loop for (a . b) on '((1 2) (3 4)) collect (list a b))"),
        ["(((1 2) ((3 4))) ((3 4) NIL))"]
    );
    assert_eq!(
        repr(
            "(loop for (a . b) on '(1 2 . 3)
                   and c = 10 then (1+ c)
                   collect (list a b c) into result
                   finally (return (list result a b c)))"
        ),
        ["(((1 (2 . 3) 10) (2 3 11)) 2 3 11)"]
    );
    assert_eq!(
        repr("(loop for tail on '(1 . 2) finally (return tail))"),
        ["2"]
    );
    assert_eq!(
        repr("(loop for (a . b) in '((1 . 2)) thereis (and (= a 1) b))"),
        ["2"]
    );
    assert_eq!(
        repr("(loop for i from 1 to 2 for (a . b) in '((1 . 2) (3 . 4)) collect (list i a b))"),
        ["((1 1 2) (2 3 4))"]
    );
    assert_eq!(
        repr("(loop for (a b) in '((1 2)) when (> a 0) collect (list a b))"),
        ["((1 2))"]
    );
}

#[test]
fn invalid_loop_clause_combinations_fail_during_macroexpansion() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(loop for x from 0 downto 2 upfrom 1 collect x)",
        "(loop repeat 1 collect 1 always t)",
        "(loop with x = 1 for x from 0 below 2 collect x)",
        "(loop for x in '(1) if)",
        "(loop for (a b) from 1 to 2 collect a)",
        "(loop (list nil) collect 1)",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::MacroExpansion,
            "{source}: {error}"
        );
    }
}

#[test]
fn termination_between_iteration_clauses_precedes_the_following_step() {
    Runtime::start()
        .unwrap()
        .load_str(
            r#"
      (let ((seen nil) (steps 0))
        (let ((sum (loop for i = 1 then (1+ i)
                         until (>= i 4)
                         for value = (progn (incf steps) (push i seen) (* i 2))
                         sum value)))
          (assert (equal (list sum steps seen) '(12 3 (3 2 1))))))
    "#,
        )
        .unwrap();
}
