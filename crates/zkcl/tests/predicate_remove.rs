use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn predicate_removal_preserves_selection_order() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-predicate-key", |runtime, arguments| {
            runtime.collect();
            Ok(vec![arguments[0].clone()])
        })
        .unwrap();
    let repr = |source| {
        runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"))
            .iter()
            .map(|value| runtime.repr(value).unwrap())
            .collect::<Vec<_>>()
    };

    for (source, expected) in [
        ("(remove-if #'null (list nil 1 nil 2))", vec!["(1 2)"]),
        (
            "(remove-if-not (lambda (n) (< 2 n)) '(0 1 2 3 4 5) :start 1 :end 4)",
            vec!["(0 3 4 5)"],
        ),
        (
            "(let ((log nil))
               (values (remove-if (lambda (n) (push n log) (= n 3))
                                  '(0 3 2 3 4 3 5)
                                  :start 1 :end 6 :from-end t :count 2)
                       log))",
            vec!["(0 3 2 4 5)", "(3 4 3)"],
        ),
        (
            "(let ((log nil))
               (values (remove-if (lambda (n) (push n log) (= n 3))
                                  '(1 2 3) :from-end t)
                       log))",
            vec!["(1 2)", "(1 2 3)"],
        ),
        (
            "(let* ((first (list 1 :first))
                     (last (list 3 :last))
                     (source (list first (list 2 :deleted) last))
                     (tail (cddr source))
                     (removed (remove-if (lambda (x) (= (car x) 2)) source
                                         :key #'collect-predicate-key)))
               (values (eq (car removed) first) (eq (cadr removed) last)
                       (eq (cddr source) tail) source removed))",
            vec![
                "T",
                "T",
                "T",
                "((1 :FIRST) (2 :DELETED) (3 :LAST))",
                "((1 :FIRST) (3 :LAST))",
            ],
        ),
        (
            "(let* ((source (list 0 1 2 3 4))
                     (selected (cdr source))
                     (tail (cdddr source))
                     (result (delete-if-not (lambda (x) (member x '(1 3 4))) source
                                            :key #'collect-predicate-key)))
               (values result (eq result selected) (eq (cdr result) tail)))",
            vec!["(1 3 4)", "T", "T"],
        ),
        (
            r#"(values (remove-if (lambda (character) (char= character #\é)) "aé😀é雪")
                       (delete-if (lambda (character) (char= character #\b))
                                  (copy-seq "abbbz") :count 2 :from-end t))"#,
            vec![r#""a😀雪""#, r#""abz""#],
        ),
        (
            "(let* ((bits (make-array 5 :element-type 'bit :initial-contents '(1 0 1 0 1)
                                       :fill-pointer 4))
                     (result (delete-if #'zerop bits :count 1 :from-end t)))
               (values result (array-element-type result) (length result)))",
            vec!["#*101", "BIT", "3"],
        ),
        (
            "(let ((source (list 1 2 3)))
               (values (eq source
                           (remove-if (lambda (x) (error \"unexpected call\")) source
                                      :count (- (ash 1 100))))
                       (remove-if-not #'evenp source :count (ash 1 100))))",
            vec!["T", "(2)"],
        ),
        (
            "(catch :finished
               (remove-if (lambda (x)
                            (if (= x 2) (throw :finished (values 40 2))))
                          (list 1 2 3) :key #'collect-predicate-key))",
            vec!["40", "2"],
        ),
        (
            "(values (remove-if (lambda (x) (values)) '(1 2))
                     (remove-if-not (lambda (x) (values)) '(1 2)))",
            vec!["(1 2)", "NIL"],
        ),
    ] {
        assert_eq!(repr(source), expected, "{source}");
    }
}

#[test]
fn predicate_removal_reports_invalid_sequences_or_options() {
    let runtime = Runtime::start().unwrap();
    for (source, class) in [
        ("(remove-if #'null '(1 . 2))", ConditionClass::TypeError),
        ("(remove-if #'null #2A((1)))", ConditionClass::TypeError),
        (
            "(remove-if #'null '(1) :count t)",
            ConditionClass::TypeError,
        ),
        ("(remove-if #'null '(1) :end 2)", ConditionClass::Error),
        (
            "(delete-if #'null '(1) :test #'eql)",
            ConditionClass::ProgramError,
        ),
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(class),
            "{source}: {error}"
        );
    }
}
