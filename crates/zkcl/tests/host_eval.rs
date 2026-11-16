use zkcl::Runtime;

#[test]
fn host_eval_returns_the_values_of_the_evaluated_form() {
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
        numbers("(let ((x 4)) (let ((x 20) (y (+ x 3))) (values x y)))"),
        [Some(20), Some(7)],
    );
    assert_eq!(
        numbers("(let* ((x 20) (y (+ x 3))) (values x y))"),
        [Some(20), Some(23)],
    );
    assert_eq!(
        numbers(
            "(multiple-value-bind (a b c) (if nil (values 9) (values 10 20)) (values (+ a b) b c))"
        ),
        [Some(30), Some(20), None],
    );
    assert!(
        runtime
            .eval("(progn (values 1 2) (values))")
            .unwrap()
            .is_empty()
    );
    let nil = runtime.eval("(values nil)").unwrap();
    assert_eq!(nil.len(), 1);
    assert!(nil[0].is_nil());
    assert_eq!(numbers("(+ (values 2 99) (cdr '(0 . 40)))"), [Some(42)]);
    assert_eq!(
        numbers("(if (eq nil '()) (car '(42 3)) (car 7))"),
        [Some(42)]
    );
    assert_eq!(numbers("; comment\n(+ 1 2)"), [Some(3)]);
    assert_eq!(
        numbers("(values most-negative-fixnum most-positive-fixnum)"),
        [Some(isize::MIN >> 3), Some(isize::MAX >> 3)]
    );
    assert!(runtime.eval("(setq most-positive-fixnum 42)").is_err());
    let comparisons = runtime
        .eval("(values (<= -2 0 0 42) (> 42 0 -2) (>= 42 0 0 -2) (<= 42 41) (> 1 1))")
        .unwrap();
    assert_eq!(
        comparisons
            .iter()
            .map(|value| value.is_nil())
            .collect::<Vec<_>>(),
        [false, false, false, true, true]
    );
    assert_eq!(
        numbers("(locally (declare (optimize (safety 3))) (the (values fixnum) (values 41 42)))"),
        [Some(41), Some(42)]
    );
    assert_eq!(
        numbers("(let ((cell (list 41))) (setf (the fixnum (car cell)) 42) (car cell))"),
        [Some(42)]
    );
    let predicates = runtime
        .eval("(values (symbolp nil) (symbolp 1) (keywordp :test) (keywordp '#:test))")
        .unwrap();
    assert_eq!(
        predicates
            .iter()
            .map(|value| value.is_nil())
            .collect::<Vec<_>>(),
        [false, true, false, true]
    );
    assert_eq!(runtime.eval("(car 42)").err().unwrap().offset(), 1);
    assert!(runtime.eval("(+ 1 2) (+ 3 4)").is_err());
}
