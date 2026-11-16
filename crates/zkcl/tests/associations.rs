use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn list_searches_return_the_original_first_match() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-and-keep", |runtime, args| {
            runtime.collect();
            Ok(vec![args[0].clone()])
        })
        .unwrap();

    let truth = |source: &str| {
        let result = runtime
            .eval(source)
            .unwrap_or_else(|err| panic!("{source}: {err}"));
        assert_eq!(result.len(), 1, "{source}");
        assert!(!result[0].is_nil(), "{source}");
    };
    for source in [
        "(let ((xs (list 1 2 3 4))) (eq (member 3 xs) (cddr xs)))",
        "(let ((xs '(1 2 3 . 4))) (eq (member 3 xs) (cddr xs)))",
        "(let ((rows (list nil (cons nil 5) (cons nil 6))))
           (eq (assoc-if #'null rows) (cadr rows)))",
        "(let ((rows (list (cons 'a 7) (cons 'b 8))))
           (eq (rassoc 9 rows :key (lambda (value) (+ value 1))) (cadr rows)))",
        "(let ((rows (list (cons 'a 7) (cons 'b 8))))
           (eq (rassoc-if (lambda (value) (= value 8)) rows) (cadr rows)))",
        "(let ((row (cons 'a 1))) (eq (rassoc-if-not (lambda (x) (values)) (list row)) row))",
        "(let ((rows (list (cons 1 'a) (cons 2 'b))))
           (eq (assoc-if-not (lambda (value) (= value 1)) rows) (cadr rows)))",
        "(let ((rows (list (cons 1 'a) (cons 2 'b))))
           (eq (assoc 1 rows :test-not '=) (cadr rows)))",
        "(let ((xs (list 99 4 7)))
           (eq (member 6 xs
                       :key (lambda (x) (values (+ x 1) nil))
                       :test (lambda (item candidate) (values (= (- item candidate) 1) nil)))
               (cdr xs)))",
        "(let ((xs (list 1 2)))
           (eq (member 2 xs :key nil :key 'ignored-key :extra 3 :allow-other-keys t)
               (cdr xs)))",
        "(let* ((first (cons (list 1) 'first))
                (second (cons (list 2) 'second))
                (rows (list first second)))
           (eq (assoc (car second) rows :key #'collect-and-keep
                      :test (lambda (item key) (eq (collect-and-keep item) key)))
               second))",
        "(let* ((xs (list 1 2 3 4)) (seen nil))
           (member-if-not (lambda (n) (setq seen (cons n seen)) (< n 3)) xs)
           (equal seen '(3 2 1)))",
    ] {
        truth(source);
    }
    for source in [
        "(member-if (lambda (x) (values)) '(1 2 3))",
        "(assoc nil '(nil nil))",
        "(rassoc 5 '((name . 1) (other . 2)))",
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(result.len(), 1, "{source}");
        assert!(result[0].is_nil(), "{source}");
    }

    runtime.load_str("(defvar *search-cleanup* 0)").unwrap();
    let thrown = runtime
        .eval(
            "(catch :stop
               (member 2 '(1 2 3) :key (lambda (n)
                 (unwind-protect (throw :stop (values n 17))
                   (setq *search-cleanup* 5)))))",
        )
        .unwrap();
    assert_eq!(
        thrown
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(1), Some(17)]
    );
    assert_eq!(
        runtime.eval("*search-cleanup*").unwrap()[0].as_fixnum(),
        Some(5)
    );
    let restarted = runtime
        .eval(
            "(restart-case
               (member 0 '(1) :test (lambda (a b) (invoke-restart 'stop-search a b)))
               (stop-search (a b) (values a b)))",
        )
        .unwrap();
    assert_eq!(
        restarted
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(0), Some(1)]
    );

    for (source, class) in [
        ("(assoc 'a '(not-a-pair))", ConditionClass::TypeError),
        ("(member 9 '(1 . 2))", ConditionClass::TypeError),
        ("(member 1)", ConditionClass::ProgramError),
        (
            "(member-if #'null '(nil) :test #'eq)",
            ConditionClass::ProgramError,
        ),
        (
            "(let ((xs (list 1))) (rplacd xs xs) (member 2 xs :key #'collect-and-keep))",
            ConditionClass::TypeError,
        ),
    ] {
        let err = runtime.eval(source).err().expect(source);
        assert_eq!(
            err.kind(),
            EvalErrorKind::LispCondition(class),
            "{source}: {err}"
        );
    }

    let args = runtime
        .eval("(values #'assoc 'b '((a . 1) (b . 2)))")
        .unwrap();
    let result = runtime.call(&args[0], &[&args[1], &args[2]]).unwrap();
    let entry = runtime.cons_fields(&result[0]).unwrap();
    assert!(entry.0.same_identity(&args[1]));
    assert_eq!(entry.1.as_fixnum(), Some(2));
}
