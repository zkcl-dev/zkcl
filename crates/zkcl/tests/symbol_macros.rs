use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn symbol_macros_resolve_in_the_reference_environment() {
    let runtime = Runtime::start().unwrap();
    let fixnums = |source| {
        runtime
            .eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };
    runtime
        .load_str(
            "(defparameter *symbol-macro-box* (list 40))
             (define-symbol-macro symbol-macro-value (car *symbol-macro-box*))
             (defmacro inspect-symbol-macro (place &environment env)
               (multiple-value-bind (expansion expanded) (macroexpand place env)
                 (list 'values (list 'quote expansion) (list 'quote expanded))))",
        )
        .unwrap();
    assert_eq!(fixnums("symbol-macro-value"), [Some(40)]);
    assert_eq!(
        fixnums("(setq symbol-macro-value (+ symbol-macro-value 2))"),
        [Some(42)]
    );
    assert_eq!(
        fixnums(
            "(values (let ((symbol-macro-value 5)) symbol-macro-value)
                     (symbol-macrolet ((symbol-macro-value 7)) symbol-macro-value)
                     symbol-macro-value)"
        ),
        [Some(5), Some(7), Some(42)]
    );
    assert_eq!(
        fixnums(
            "(let ((box (list 40)))
               (symbol-macrolet ((a b) (b (car box)))
                 (let ((get (lambda () a)))
                   (setq a (+ (funcall get) 2))
                   (values a (let ((a 1)) a) (car box)))))"
        ),
        [Some(42), Some(1), Some(42)]
    );

    let expanded = runtime
        .eval("(symbol-macrolet ((x y) (y 42)) (inspect-symbol-macro x))")
        .unwrap();
    assert_eq!(expanded[0].as_fixnum(), Some(42));
    assert!(!expanded[1].is_nil());
    let masked = runtime
        .eval("(let ((symbol-macro-value nil)) (inspect-symbol-macro symbol-macro-value))")
        .unwrap();
    assert_eq!(
        runtime.symbol_name(&masked[0]).as_deref(),
        Some("SYMBOL-MACRO-VALUE")
    );
    assert!(masked[1].is_nil());
    assert!(
        runtime
            .eval("(macro-function 'symbol-macro-value)")
            .unwrap()[0]
            .is_nil()
    );

    runtime
        .load_str(
            "(defparameter *expansion-hook-count* 0)
             (defun count-symbol-expansion (expander form environment)
               (when (eq form 'hooked-symbol)
                 (setq *expansion-hook-count* (+ *expansion-hook-count* 1)))
               (funcall expander form environment))",
        )
        .unwrap();
    assert_eq!(
        fixnums(
            "(let ((*macroexpand-hook* #'count-symbol-expansion))
               (eval '(symbol-macrolet ((hooked-symbol (values 1 2))) hooked-symbol)))"
        ),
        [Some(1), Some(2)]
    );
    assert_eq!(fixnums("*expansion-hook-count*"), [Some(1)]);
    runtime.collect();
    assert_eq!(fixnums("symbol-macro-value"), [Some(42)]);

    for source in [
        "(symbol-macrolet ((*symbol-macro-box* nil)) nil)",
        "(symbol-macrolet ((x 0)) (declare (special x)) x)",
        "(define-symbol-macro *symbol-macro-box* 0)",
        "(declaim (special symbol-macro-value))",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ProgramError)
        );
    }
    assert_eq!(
        fixnums(
            "(handler-case (eval '(symbol-macrolet ((x 0)) (declare (special x)) x))
               (program-error () 42))"
        ),
        [Some(42)]
    );
}
