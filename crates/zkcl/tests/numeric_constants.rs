use zkcl::{BigInt, ConditionClass, EvalErrorKind, Runtime};

#[test]
fn floating_point_constants_expose_format_specific_limits() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(values (typep pi 'double-float)
                     (typep most-negative-short-float 'single-float)
                     (typep least-positive-long-float 'double-float)
                     (> least-positive-normalized-double-float least-positive-double-float 0)
                     (> (+ 1.0 single-float-epsilon) 1.0)
                     (< (- 1.0 single-float-negative-epsilon) 1.0))",
        )
        .unwrap();
    assert_eq!(values.len(), 6);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn numeric_constants_cannot_be_assigned_or_rebound() {
    let rt = Runtime::start().unwrap();
    assert!(rt.eval("(setq pi 0)").is_err());
    assert!(
        rt.eval("(let ((most-positive-double-float 0)) most-positive-double-float)")
            .is_err()
    );
}

#[test]
fn tagbody_accepts_bignum_labels() {
    let rt = Runtime::start().unwrap();
    let result = rt
        .eval(
            "(block nil
               (tagbody
                 (go 36893488147419103232)
                 (return-from nil 0)
                 36893488147419103232
                 (return-from nil 42)))",
        )
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(42));
}

#[test]
fn a_host_bignum_can_be_passed_to_lisp_after_collection() {
    let rt = Runtime::start().unwrap();
    let integer = BigInt::from(1u8) << 100usize;
    let left = rt.make_integer(&integer).unwrap();
    let right = rt.fixnum(42).unwrap();
    let function = rt.eval("#'+").unwrap();
    rt.collect();
    let sum = rt.call(&function[0], &[&left, &right]).unwrap();
    assert_eq!(rt.integer(&sum[0]), Some(integer + 42));
}

#[test]
fn division_by_zero_exposes_the_operation_and_its_operands() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(handler-case (/ 42 0)
               (division-by-zero (c)
                 (values (arithmetic-error-operation c)
                         (arithmetic-error-operands c))))",
        )
        .unwrap();
    let operation = rt.eval("'/").unwrap();
    assert!(values[0].same_identity(&operation[0]));
    let (first, rest) = rt.cons_fields(&values[1]).unwrap();
    let (second, end) = rt.cons_fields(&rest).unwrap();
    assert_eq!(first.as_fixnum(), Some(42));
    assert_eq!(second.as_fixnum(), Some(0));
    assert!(end.is_nil());
    assert_eq!(
        rt.eval("(/ 1 0)").err().unwrap().kind(),
        EvalErrorKind::LispCondition(ConditionClass::DivisionByZero)
    );
}
