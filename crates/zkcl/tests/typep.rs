use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn typep_recognizes_builtin_value_types() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(typep '(a . b) 'list)", true),
        ("(typep '(a . b) 'sequence)", true),
        (
            "(let ((cycle (list 'a))) (setf (cdr cycle) cycle) (typep cycle 'sequence))",
            true,
        ),
        ("(typep nil 'cons)", false),
        ("(typep nil 'symbol)", true),
        ("(typep nil 'symbol nil)", true),
        ("(typep 4 'rational)", true),
        ("(typep \"value\" 'simple-string)", true),
        ("(typep #'list 'function)", true),
        ("(typep (let ((x 1)) (lambda () x)) 'function)", true),
        (
            "(typep (make-condition 'undefined-function) 'cell-error)",
            true,
        ),
        ("(typep (make-condition 'simple-error) 'warning)", false),
    ] {
        assert_eq!(
            !runtime.eval(source).unwrap()[0].is_nil(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn typep_rejects_an_invalid_interval_bound() {
    let runtime = Runtime::start().unwrap();
    let error = runtime.eval("(typep 1 '(integer 0 bad))").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::Error)
    );
}

#[test]
fn typep_respects_integer_interval_endpoints() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(typep 5 '(integer (4) 5))", true),
        ("(typep 4 '(integer (4) *))", false),
        ("(typep -4 '(integer * (-4)))", false),
    ] {
        assert_eq!(
            !runtime.eval(source).unwrap()[0].is_nil(),
            expected,
            "{source}"
        );
    }
}

#[test]
fn typep_combines_membership_predicates() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(typep 4 '(and integer (not (member 0 1 2 3))))", true),
        ("(typep 4 '(or null (eql 4)))", true),
        ("(typep nil '(or))", false),
        ("(typep 42 '(and))", true),
    ] {
        assert_eq!(
            !runtime.eval(source).unwrap()[0].is_nil(),
            expected,
            "{source}"
        );
    }
}
