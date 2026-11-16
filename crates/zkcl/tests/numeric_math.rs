use zkcl::{ConditionClass, EvalErrorKind, Runtime};

fn assert_numeric_form(runtime: &Runtime, source: &str) {
    let values = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert_eq!(values.len(), 1, "{source}");
    assert!(!values[0].is_nil(), "{source}");
}

fn assert_numeric_forms(sources: &[&str]) {
    let runtime = Runtime::start().unwrap();
    for source in sources {
        assert_numeric_form(&runtime, source);
    }
}

fn assert_true_forms(runtime: &Runtime, sources: &[&str]) {
    for source in sources {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(values[0].is_t(), "{source}");
    }
}

#[test]
fn integer_arithmetic_extends_beyond_fixnums() {
    assert_numeric_forms(&[
        "(= (+) 0)",
        "(= (*) 1)",
        "(typep (1+ most-positive-fixnum) 'bignum)",
        "(= (1- (1+ most-positive-fixnum)) most-positive-fixnum)",
        "(= (- most-negative-fixnum) (1+ most-positive-fixnum))",
        "(= (ash 1 100) 1267650600228229401496703205376)",
        "(= (funcall (compile nil '(lambda () (ash 16 60)))) 18446744073709551616)",
        "(= (funcall (compile nil '(lambda () (ash -15 60)))) -17293822569102704640)",
        "(= (ash -100000000000000000000000000000000 -100) -79)",
        "(= (ash -5 (- (ash 1 128))) -1)",
        "(= (ash 0 (ash 1 128)) 0)",
        "(= (expt -1 (ash 1 256)) 1)",
        "(= (expt 2 -80) (/ (ash 1 80)))",
        "(= (expt 17 50) 33300140732146818380750772381422989832214186835186851059977249)",
    ]);
}

#[test]
fn rational_arithmetic_normalizes_exact_results() {
    assert_numeric_forms(&[
        "(and (= (/ -54 -36) 3/2) (= (numerator (/ 54 -36)) -3) (= (denominator (/ -54 36)) 2))",
        "(and (= (+ 1/3 2/3) 1) (integerp (+ 1/3 2/3)) (not (integerp (/ 2 3))))",
        "(and (rationalp (/ 7 3 2)) (= (/ 7 3 2) 7/6) (= (/ (/ 2 3)) 3/2))",
        "(and (oddp (1+ (ash -1 80))) (evenp (ash -1 80)))",
    ]);
}

#[test]
fn numeric_comparisons_use_exact_values_across_formats() {
    assert_numeric_forms(&[
        "(and (<= 9007199254740992d0 9007199254740992) (< 9007199254740992d0 9007199254740993))",
        "(and (= 1/2 0.5f0 0.5d0) (/= 1/10 0.1f0 0.1d0))",
        "(not (/= 0.5d0 1/2 0.5f0))",
        "(< -1d100 -1/200 0.0f0 1/200 1d100)",
    ]);
}

#[test]
fn real_conversions_choose_the_requested_float_format() {
    assert_numeric_forms(&[
        "(and (typep (+ 1 2s0) 'single-float) (typep (+ 1 2f0 1d0) 'double-float))",
        "(and (= (rational 0.125d0) 1/8) (typep (float 1/8 0d0) 'double-float))",
    ]);
}

#[test]
fn complex_arithmetic_respects_the_real_number_tower() {
    assert_numeric_forms(&[
        "(and (realp (complex 7/3)) (complexp (complex 1d0)) (= (imagpart (complex 1d0)) 0d0))",
        "(and (typep (realpart (complex 3 1d0)) 'double-float) (zerop (imagpart 1d0)))",
        "(and (= #c(1d0 0d0) 1) (= (complex 2/3 0) 2/3))",
        "(and (= (* #c(1 2) #c(3 4)) #c(-5 10)) (= (/ #c(1 2) #c(3 4)) #c(11/25 2/25)))",
        "(and (= (expt #c(2 2) 4) -64) (= (expt #c(0 1) -3) #c(0 1)))",
        "(and (= (abs #c(3 4)) 5) (= (conjugate #c(3 1/2)) #c(3 -1/2)))",
        "(and (numberp #c(1 1)) (not (realp #c(1 1))) (not (numberp nil)))",
        "(and (zerop (signum 0d0)) (= (signum -1/2) -1) (= (abs -1/2) 1/2))",
    ]);
}

#[test]
fn zero_base_expt_checks_the_real_part_of_the_exponent_before_coercion() {
    assert_numeric_forms(&[
        "(eql (expt 0 #c(2 1/2)) 0)",
        "(eql (expt 0 (complex (/ (ash 1 1100)) 1)) 0)",
        "(eql (expt 0 (complex (ash 1 1024) 1)) 0)",
        "(zerop (expt 0 (/ (ash 1 1100))))",
        "(eql (expt 0d0 #c(2 1/2)) #c(0d0 0d0))",
        "(eql (expt #c(0.0 0.0) #c(2 1/2)) #c(0.0 0.0))",
        "(eql (expt #c(0d0 0d0) 0) #c(1d0 0d0))",
        "(handler-case (expt 0 (complex 0 1)) (division-by-zero () t))",
        "(handler-case (expt 0 (complex (- (/ (ash 1 1100))) 1)) (division-by-zero () t))",
    ]);
}

#[test]
fn division_rounding_returns_the_required_remainder() {
    assert_numeric_forms(&[
        "(multiple-value-bind (q r) (floor -7/3) (and (= q -3) (= r 2/3)))",
        "(multiple-value-bind (q r) (ceiling -7/3) (and (= q -2) (= r -1/3)))",
        "(multiple-value-bind (q r) (truncate 7 -3) (and (= q -2) (= r 1)))",
        "(multiple-value-bind (q r) (round 5/2) (and (= q 2) (= r 1/2)))",
        "(multiple-value-bind (q r) (round -7/2) (and (= q -4) (= r 1/2)))",
        "(multiple-value-bind (q r) (round (1+ (ash 1 100)) 2) (and (= q (ash 1 99)) (= r 1)))",
        "(multiple-value-bind (q r) (ftruncate 3.5d0) (and (typep q 'double-float) (= q 3d0) (typep r 'double-float) (= r 0.5d0)))",
        "(multiple-value-bind (q r) (floor 3.5d0) (and (integerp q) (= q 3) (typep r 'double-float) (= r 0.5d0)))",
        "(and (= (mod 7 -3) -2) (= (rem 7 -3) 1) (= (mod -7 3) 2) (= (rem -7 3) -1))",
        "(and (zerop (rem most-negative-fixnum -1)) (zerop (mod most-negative-fixnum -1))
              (= (mod 1 most-negative-fixnum) (1+ most-negative-fixnum))
              (handler-case (rem 1 0)
                (division-by-zero (c) (and (eq 'rem (arithmetic-error-operation c))
                                           (equal '(1 0) (arithmetic-error-operands c))))))",
    ]);
}

#[test]
fn invalid_arithmetic_reports_the_condition_at_the_call() {
    let runtime = Runtime::start().unwrap();
    let zero = runtime.eval("(/ 5 3 0)").err().unwrap();
    assert_eq!(
        zero.kind(),
        EvalErrorKind::LispCondition(ConditionClass::DivisionByZero),
    );
    assert_numeric_form(
        &runtime,
        "(handler-case (/ 5 3 0)
           (division-by-zero (condition)
             (and (eq (arithmetic-error-operation condition) '/)
                  (equal (arithmetic-error-operands condition) '(5 3 0)))))",
    );
    assert_numeric_form(
        &runtime,
        "(handler-case (< 0 #c(1 1)) (type-error (c) (eq (type-error-expected-type c) 'real)))",
    );
    assert_numeric_form(
        &runtime,
        "(handler-case (+ 0 'foo) (type-error (c) (eq (type-error-datum c) 'foo)))",
    );
    assert_numeric_form(
        &runtime,
        "(handler-case (* 1d300 1d300) (floating-point-overflow () t))",
    );
    assert_numeric_form(
        &runtime,
        "(handler-case (round 5 0)
           (division-by-zero (condition)
             (and (eq (arithmetic-error-operation condition) 'round)
                  (equal (arithmetic-error-operands condition) '(5 0)))))",
    );
}

#[test]
fn ash_rejects_a_left_shift_that_cannot_fit_in_memory() {
    let runtime = Runtime::start().unwrap();
    let huge_shift = runtime.eval("(ash 1 (ash 1 200))").err().unwrap();
    assert_eq!(huge_shift.kind(), EvalErrorKind::OutOfMemory);
}

#[test]
fn floating_multiply_add_rounds_the_product_separately() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defun fp-step (a b c) (+ (* a b) c))")
        .unwrap();
    assert_true_forms(
        &rt,
        &[
            // The separately rounded products are 1. An FMA would return a negative value.
            "(eql (fp-step 1.0001220703125f0 0.9998779296875f0 -1.0f0) 0.0f0)",
            "(eql (fp-step (+ 1d0 (scale-float 1d0 -27))
                       (- 1d0 (scale-float 1d0 -27)) -1d0) 0d0)",
        ],
    );
}

#[test]
fn real_float_operations_preserve_the_result_format() {
    let rt = Runtime::start().unwrap();
    assert_true_forms(
        &rt,
        &[
            "(eql (float-sign (* -0.0f0 1.0d0)) -1.0d0)",
            "(eql (+ least-positive-single-float least-positive-single-float)
              (scale-float 1.0f0 -148))",
            "(eql (/ 1f0 2.0d0) 0.5d0)",
        ],
    );
}

#[test]
fn float_arithmetic_conditions_retain_the_original_operands() {
    let rt = Runtime::start().unwrap();
    assert_true_forms(
        &rt,
        &[
            "(handler-case (/ 1.0d0 -0.0f0)
           (division-by-zero (c) (and (eq '/ (arithmetic-error-operation c))
                                      (equal (list 1.0d0 -0.0f0) (arithmetic-error-operands c)))))",
            "(handler-case (* most-positive-single-float 2.0f0)
           (floating-point-overflow (c) (and (eq '* (arithmetic-error-operation c))
                                             (equal (list most-positive-single-float 2.0f0)
                                                    (arithmetic-error-operands c)))))",
        ],
    );
}

#[test]
fn compiled_real_comparison_keeps_exact_numeric_boundaries() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defun fp-below (x y) (if (< x y) 1 0))")
        .unwrap();
    assert_true_forms(
        &rt,
        &["(and (= 1 (fp-below 1.0f0 1.0000000000000002d0))
              (= 0 (fp-below 1.0000000000000002d0 1.0f0))
              (= 0 (fp-below -0.0f0 0d0))
              (= 1 (fp-below 9007199254740992d0 9007199254740993))
              (= 1 (fp-below 1/10 0.1d0)))"],
    );
}

#[test]
fn logarithms_preserve_input_distances_near_one() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(let* ((d (1+ (ash 1 4000))) (delta (/ (ash 1 3940) d)))
           (values (log (+ 1 delta)) (log (- 1 delta))
                   (realpart (log (- (+ 1 delta))))
                   (log 1.0000000000000002d0 2d0)))",
        )
        .unwrap();
    // ln(1 ± delta) rounds to ±2^-60 in binary32.
    let unit = 2.0_f32.powi(-60);
    for (value, expected) in values.iter().zip([unit, -unit, unit]) {
        assert_eq!(
            value.as_single_float().unwrap().to_bits(),
            expected.to_bits()
        );
    }
    // High-precision ln(1 + 2^-52) / ln(2), rounded to binary64.
    assert!(
        values[3]
            .as_double_float()
            .unwrap()
            .to_bits()
            .abs_diff(0x3cb7_1547_652b_82fd)
            <= 1
    );
}

#[test]
fn logarithms_with_zero_base_return_float_zero() {
    let rt = Runtime::start().unwrap();
    assert_true_forms(
        &rt,
        &[
            "(eql (log 42 0) 0f0)",
            "(eql (log #c(1d0 2d0) 0) 0d0)",
            "(eql (log (ash 1 5000) -0d0) 0d0)",
        ],
    );
}

#[test]
fn base_two_logarithm_recovers_the_subnormal_exponent() {
    let rt = Runtime::start().unwrap();
    assert_true_forms(
        &rt,
        &["(eql (log least-positive-double-float 2d0) -1074d0)"],
    );
}

#[test]
fn a_unit_logarithm_base_signals_division_by_zero() {
    let rt = Runtime::start().unwrap();
    assert_true_forms(
        &rt,
        &["(handler-case (log 2.0d0 1.0d0) (division-by-zero () t))"],
    );
}

#[test]
fn logarithms_change_base_before_rounding_exact_operands() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(let* ((delta (/ (ash 1 3940) (1+ (ash 1 4000))))
                    (above (+ 1 delta)) (below (- 1 delta)))
               (values (log (+ 1 (/ 1 (ash 1 60))) 2)
                       (log below 10d0) (log above 3/2)
                       (log (ash 1 5000) (+ (ash 1 1200) 17))
                       (log (/ 1 (ash 1 4000)) 3/2)))",
        )
        .unwrap();
    // Reference values computed with high-precision natural logarithms.
    for (index, bits) in [
        (0, 0x21b8_aa3b),
        (2, 0x221d_d7ea),
        (3, 0x4085_5555),
        (4, 0xc5d5_b05c),
    ] {
        assert_eq!(values[index].as_single_float().unwrap().to_bits(), bits);
    }
    assert!(
        values[1]
            .as_double_float()
            .unwrap()
            .to_bits()
            .abs_diff(0xbc1b_cb7b_1526_e50e)
            <= 1
    );

    for source in [
        "(eql (log (ash 1 5000) 2d0) 5000d0)",
        "(eql (log 8f0 2d0) 3d0)",
        "(eql (log 4d0 1/2) -2d0)",
        "(let ((d (/ 1 (ash 1 4000)))) (eql (log (+ 1 d) (- 1 (* 16 d))) -0.0625f0))",
        "(let ((d (/ 1 (ash 1 1200)))) (eql (log (+ 1 (* d 3)) (+ 1 d)) 3f0))",
        "(handler-case (log 2d0 (+ 1 (/ 1 (ash 1 4000))))
           (floating-point-overflow () t))",
        "(handler-case (log (ash 1 5000) 1)
           (division-by-zero (c) (eq 'log (arithmetic-error-operation c))))",
        "(handler-case (log 0 2) (division-by-zero () t))",
        "(eql (log 42 #c(0d0 -0d0)) 0d0)",
    ] {
        let values = rt
            .eval(source)
            .unwrap_or_else(|err| panic!("{source}: {err}"));
        assert!(values[0].is_t(), "{source}");
    }
}

#[test]
fn logarithms_follow_the_principal_branch() {
    let rt = Runtime::start().unwrap();
    for source in [
        "(eql (log -1d0) (complex 0d0 pi))",
        "(eql (log #c(-1d0 -0d0)) (complex 0d0 (- pi)))",
        "(eql (log #c(-1d0 0d0)) (complex 0d0 pi))",
        "(eql (log #c(0 1) #c(0 -1)) #c(-1f0 0f0))",
        "(eql (realpart (log #c(3/5 4/5))) 0f0)",
        "(let* ((d (/ 1 (ash 1 4000))) (x (complex 1 (* 2 d))) (b (complex 1 (- d))))
           (= (log x b) -2f0))",
        "(let ((x (ash 1 4000)))
           (let ((result (log (complex x x) 2d0)))
             (and (= 4000.5d0 (realpart result))
                  (< (abs (- (imagpart result) 1.1330900354567985d0)) 1d-15))))",
        "(let ((result (log (complex 1d0 (scale-float 1d0 -30)))))
           (and (= (scale-float 1d0 -61) (realpart result))
                (= (scale-float 1d0 -30) (imagpart result))))",
        "(let* ((x most-positive-double-float) (result (log (complex x x))))
           (and (< (abs (- 710.12928648366397d0 (realpart result))) 2d-13)
                (= (/ pi 4) (imagpart result))))",
        "(let* ((delta (/ 1 (ash 1 60))) (result (log (- (+ 1 delta)) 2d0)))
           (and (> (realpart result) 0d0) (> (imagpart result) 0d0)))",
    ] {
        let values = rt
            .eval(source)
            .unwrap_or_else(|err| panic!("{source}: {err}"));
        assert!(values[0].is_t(), "{source}");
    }
}

#[test]
fn complex_float_division_checks_the_range_of_the_quotient() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(let* ((limit most-positive-double-float) (z (complex limit limit)))
           (eql (/ z #c(1d0 1d0)) (complex limit 0d0)))",
        "(let* ((limit most-positive-single-float) (z (complex limit limit)))
           (eql (/ z #c(1.0 1.0)) (complex limit 0.0)))",
        "(let* ((tiny 0.75d-308) (z (/ #c(1d0 1d0) (complex tiny tiny))))
           (eql z (complex (/ tiny) 0d0)))",
        "(let* ((x 1.5d308) (expect (/ x 1.5d0)))
           (eql (/ (complex x 0d0) #c(0.75d0 0.75d0)) (complex expect (- expect))))",
        "(let* ((tiny least-positive-double-float) (z (complex tiny tiny)))
           (eql (/ (complex (* 2 tiny) tiny) z) #c(1.5d0 -0.5d0)))",
        "(eql (/ #c(-0d0 -0d0) #c(1d308 0d0)) #c(-0d0 0d0))",
        "(let ((z (/ #c(1d308 -1d308))))
           (and (= (realpart z) (imagpart z))
                (< 0d0 (realpart z) least-positive-normalized-double-float)))",
        "(handler-case (/ #c(1d0 1d0) #c(0d0 -0d0)) (division-by-zero () t))",
        "(let ((x #c(1d0 1d0)) (y (complex least-positive-double-float 0d0)))
           (handler-case (/ x y)
             (floating-point-overflow (c)
               (and (eq (arithmetic-error-operation c) '/)
                    (equal (arithmetic-error-operands c) (list x y))))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(values.len(), 1, "{source}");
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn scaling_reaches_subnormal_values_without_losing_sign() {
    assert_numeric_forms(&[
        "(eql (scale-float least-positive-single-float 149) 1.0)",
        "(eql (scale-float 1.0 -149) least-positive-single-float)",
        "(= (rational (scale-float most-positive-single-float -200))
            (/ (rational most-positive-single-float) (ash 1 200)))",
        "(eql (scale-float -1.0 (- (ash 1 100))) -0.0)",
    ]);
}

#[test]
fn float_precision_counts_only_significant_subnormal_bits() {
    assert_numeric_forms(&["(and (= (float-digits 1.0) 24) (= (float-digits 1d0) 53)
              (= (float-precision (* 3 least-positive-single-float)) 2)
              (= (float-precision least-positive-double-float) 1))"]);
}

#[test]
fn float_decomposition_reconstructs_the_original_bits() {
    assert_numeric_forms(&[
        "(every (lambda (value)
                 (multiple-value-bind (fraction exponent sign) (decode-float value)
                   (multiple-value-bind (mantissa scale integer-sign) (integer-decode-float value)
                     (and (eql value (* sign (scale-float fraction exponent)))
                          (eql value (* integer-sign (scale-float (float mantissa value) scale)))))))
               (list -0.0 0d0 -1.0 1d0 least-negative-single-float
                     least-positive-double-float most-positive-single-float
                     most-negative-double-float))",
        "(and (multiple-value-bind (fraction exponent sign) (decode-float -0.0)
               (and (eql fraction 0.0) (eql exponent 0) (eql sign -1.0)))
              (multiple-value-bind (significand exponent sign) (integer-decode-float -0.0)
                (and (eql significand 0) (eql exponent 0) (eql sign -1))))",
    ]);
}

#[test]
fn scale_float_signals_overflow_for_an_unrepresentable_exponent() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        runtime
            .eval("(scale-float 1.0 (ash 1 100))")
            .err()
            .unwrap()
            .kind(),
        EvalErrorKind::LispCondition(ConditionClass::FloatingPointOverflow),
    );
}

#[test]
fn ratio_conversion_rounds_to_the_nearest_single_float() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(= (float (+ 1 (/ (ash 1 24)))) 1.0)",
        "(= (float (+ 1 (/ (ash 1 24)) (/ (ash 1 100))))
            (+ 1 (/ (ash 1 23))))",
        "(eql (float (- (/ (ash 1 150)))) -0.0)",
        "(eql (float (+ (/ (ash 1 150)) (/ (ash 1 250))))
              least-positive-single-float)",
        "(eql (float (+ (rational most-positive-single-float) 1/2))
              most-positive-single-float)",
        "(eql (float (- (ash 1 128) (ash 1 103) 1/2))
              most-positive-single-float)",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(values.len(), 1, "{source}");
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn ratio_conversion_signals_overflow_beyond_single_float_range() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(float (+ (ash 1 128) 1/2))",
        "(float (+ (- (ash 1 128) (ash 1 103)) 1/2))",
    ] {
        assert_eq!(
            runtime.eval(source).err().unwrap().kind(),
            EvalErrorKind::LispCondition(ConditionClass::FloatingPointOverflow),
            "{source}",
        );
    }
}

#[test]
fn distant_byte_fields_use_the_integers_sign_bits() {
    let runtime = Runtime::start().unwrap();
    let source = "(let ((far (ash 1 100)))
                    (and (zerop (ldb (byte 0 far) -1))
                         (not (ldb-test (byte 0 far) -1))
                         (= (ldb (byte 3 far) -2) 7)
                         (ldb-test (byte far far) -1)
                         (not (ldb-test (byte 1 0) -2))
                         (ldb-test (byte 1 1) -2)
                         (zerop (mask-field (byte far far) 17))
                         (= (dpb -1 (byte far far) -7) -7)
                         (= (deposit-field 1 (byte far far) 8) 8)
                         (= (dpb 11 (byte far 3) 1) 89)
                         (= (deposit-field 29 (byte 3 1) 17) 29)))";
    let values = runtime.eval(source).unwrap();
    assert_eq!(values.len(), 1);
    assert!(!values[0].is_nil());
    assert_eq!(
        runtime
            .eval("(mask-field (byte 2 (ash 1 100)) -1)")
            .err()
            .unwrap()
            .kind(),
        EvalErrorKind::OutOfMemory,
    );
}
