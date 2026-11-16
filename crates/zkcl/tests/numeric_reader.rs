use std::error::Error;

use zkcl::{ConditionClass, EvalError, EvalErrorKind, Runtime};

#[test]
fn standard_readtable_reads_numbers_in_the_dynamic_reader_context() {
    let runtime = Runtime::start().unwrap();
    let check = |source| {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(values.len(), 1, "{source}");
        assert!(!values[0].is_nil(), "{source}");
    };

    assert_eq!(
        runtime.eval("*read-base*").unwrap()[0].as_fixnum(),
        Some(10)
    );
    let format = runtime.eval("*read-default-float-format*").unwrap();
    assert_eq!(
        runtime.symbol_name(&format[0]).as_deref(),
        Some("SINGLE-FLOAT")
    );

    let bignum = runtime
        .eval("-189729874978126783786123")
        .unwrap()
        .pop()
        .unwrap();
    runtime.collect();
    assert_eq!(runtime.repr(&bignum).unwrap(), "-189729874978126783786123");
    check("(typep 189729874978126783786123 'bignum)");
    check("(and (typep 1152921504606846976 'bignum) (= 1152921504606846976 (expt 2 60)))");
    check("(and (= (numerator -18/24) -3) (= (denominator -18/24) 4))");
    check("(= -18446744073709551617/3 (/ (- (1+ (expt 2 64))) 3))");
    check("(and (integerp 10/5) (= 10/5 2) (integerp 0/123))");
    check("(= #xBC/AD (/ 188 173))");
    check("(= #3r120/21 (/ 15 7))");
    check("(= #b11010101 #o325 #16rD5 213)");
    check("(= #x10. #b10. #36r10. 10)");
    check("(= #36rZZ 1295)");
    check("(= #36rZzZzZzZzZzZzZzZz (1- (expt 36 16)))");

    check(
        r#"(let ((*read-base* 16))
              (let ((items (read-from-string "(\\DAD DAD |BEE| BEE 123. 123 1e3 1e-3 A/B)")))
                (and (eq (first items) 'dad)
                     (= (second items) 3501)
                     (eq (third items) 'bee)
                     (= (fourth items) 3054)
                     (= (fifth items) 123)
                     (= (sixth items) 291)
                     (= (seventh items) 483)
                     (typep (eighth items) 'single-float)
                     (= (ninth items) (/ 10 11)))))"#,
    );
    check(
        r#"(let ((*read-base* 2))
              (and (= (read-from-string "9e3") 9000.0)
                   (= (read-from-string "9.") 9)
                   (eq (read-from-string "9") '|9|)))"#,
    );
    check(
        r#"(let ((*read-default-float-format* 'double-float))
              (let ((items (read-from-string "(1.0 1e2 1.2s-2 1f0 .5L0 1.d1)")))
                (and (typep (first items) 'double-float)
                     (typep (second items) 'double-float)
                     (typep (third items) 'single-float)
                     (typep (fourth items) 'single-float)
                     (typep (fifth items) 'double-float)
                     (typep (sixth items) 'double-float)
                     (= (sixth items) 10))))"#,
    );
    check("(and (typep -.0 'single-float) (zerop 1e-1000))");
    check("(and (integerp #C(42 0)) (= #C(42 0) 42) (complexp #C(42 0.0)))");
    check(
        "(and (typep (realpart #C(2/3 1d0)) 'double-float)
              (typep (imagpart #C(1s0 2d0)) 'double-float)
              (= (imagpart #C(1 2/3)) 2/3))",
    );
    check("(and (symbolp '1+) (symbolp '|123|) (symbolp '2\\\\/3) (keywordp :2/3))");
    assert_eq!(
        runtime
            .eval("(values #+(or) #C(BAD 1/0) #-zkcl #xZZ 42)")
            .unwrap()[0]
            .as_fixnum(),
        Some(42)
    );

    for source in [
        "1/0",
        "#x3/0",
        "#x 12",
        "#x|1|",
        "#o9",
        "#36r1a.",
        "#36rZZZZZZZZZZZZZZZZ_1",
        "#1r1",
        "#37r1",
        "#r1",
        "#2b1",
        "#C(1)",
        "#C(1 2 3)",
        "#C(1 x)",
        "#C(1 #C(1 1))",
        "1e99999999",
        "-99999999999999999999_1",
        "1/_4",
        "1/2/3",
        "#:42",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .unwrap_or_else(|| panic!("{source} read successfully"));
        assert_eq!(error.kind(), EvalErrorKind::Read, "{source}: {error}");
    }

    let error = runtime.eval(r#"(read-from-string "  1/0")"#).err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ReaderError)
    );
    let input = error.source().unwrap().downcast_ref::<EvalError>().unwrap();
    assert_eq!(input.byte_range(), 2..5);
}
