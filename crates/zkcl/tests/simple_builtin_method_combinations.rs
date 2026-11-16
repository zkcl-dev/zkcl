use zkcl::Runtime;

mod common;

use common::lisp::assert_truthy;

#[test]
fn builtin_operators_combine_applicable_primary_methods_in_precedence_order() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defvar *combination-log* nil)

            (defgeneric combination-sum (x)
              (:method-combination +)
              (:method + ((x integer)) (car (push 8 *combination-log*)))
              (:method + ((x rational)) (car (push 4 *combination-log*)))
              (:method + ((x number)) (car (push 2 *combination-log*)))
              (:method + ((x t)) (car (push 1 *combination-log*))))

            (defgeneric combination-sum-last (x)
              (:method-combination + :most-specific-last)
              (:method + ((x integer)) (car (push 8 *combination-log*)))
              (:method + ((x t)) (car (push 1 *combination-log*))))

            (defgeneric combination-conjunction (x)
              (:method-combination and)
              (:method and ((x integer)) (push 4 *combination-log*) t)
              (:method and ((x rational)) (push 3 *combination-log*) nil)
              (:method and ((x number)) (push 2 *combination-log*) t)
              (:method and ((x t)) (push 1 *combination-log*) 'a))

            (defgeneric combination-disjunction (x)
              (:method-combination or)
              (:method or ((x integer)) (push 4 *combination-log*) nil)
              (:method or ((x t)) (push 1 *combination-log*) 'b))

            (defgeneric combination-append (x)
              (:method-combination append)
              (:method append ((x integer)) '(d))
              (:method append ((x rational)) '(c))
              (:method append ((x t)) '(a)))

            (defgeneric combination-max (x)
              (:method-combination max)
              (:method max ((x integer)) 8)
              (:method max ((x rational)) 4)
              (:method max ((x t)) 1))

            (defgeneric combination-min (x)
              (:method-combination min)
              (:method min ((x integer)) 1)
              (:method min ((x t)) 4))

            (defgeneric combination-list (x)
              (:method-combination list)
              (:method list ((x integer)) 'd)
              (:method list ((x t)) 'a))

            (defgeneric combination-nconc (x)
              (:method-combination nconc)
              (:method nconc ((x integer)) (list 'd))
              (:method nconc ((x t)) (list 'a)))

            (defgeneric combination-progn (x)
              (:method-combination progn)
              (:method progn ((x integer)) (push 4 *combination-log*) 'a)
              (:method progn ((x rational)) (push 3 *combination-log*) nil)
              (:method progn ((x t)) (push 1 *combination-log*) 'd))

            (defgeneric combination-around (x)
              (:method-combination +)
              (:method + ((x integer)) 1)
              (:method :around ((x real)) (list 'around (call-next-method))))

            (defgeneric combination-invalid (x)
              (:method-combination +)
              (:method nonsense ((x t)) 0))

            (defgeneric combination-unqualified (x)
              (:method-combination and)
              (:method ((x t)) t))

            (defgeneric combination-before (x)
              (:method-combination +)
              (:method + ((x t)) 1))

            (defgeneric combination-no-primary (x)
              (:method-combination progn))
            "#,
        )
        .unwrap();
    runtime.collect();

    for check in [
        // + sums the primary values; the default order is most specific first.
        "(equal (let ((*combination-log* nil)) (list (combination-sum 1) *combination-log*)) '(15 (1 2 4 8)))",
        "(equal (let ((*combination-log* nil)) (list (combination-sum 2/3) *combination-log*)) '(7 (1 2 4)))",
        "(equal (let ((*combination-log* nil)) (list (combination-sum 'a) *combination-log*)) '(1 (1)))",
        // :most-specific-last reverses only the primary methods.
        "(equal (let ((*combination-log* nil)) (list (combination-sum-last 1) *combination-log*)) '(9 (8 1)))",
        // and short-circuits on the first nil primary value.
        "(equal (let ((*combination-log* nil)) (list (combination-conjunction 1) *combination-log*)) '(nil (3 4)))",
        "(equal (let ((*combination-log* nil)) (list (combination-conjunction 'a) *combination-log*)) '(a (1)))",
        // or stops at the first non-nil primary value.
        "(equal (let ((*combination-log* nil)) (list (combination-disjunction 1) *combination-log*)) '(b (1 4)))",
        "(equal (let ((*combination-log* nil)) (list (combination-disjunction 'a) *combination-log*)) '(b (1)))",
        // append, list, and nconc collect the primary values in call order.
        "(equal (combination-append 1) '(d c a))",
        "(equal (combination-append 3/2) '(c a))",
        "(equal (combination-append 'a) '(a))",
        "(equal (combination-list 1) '(d a))",
        "(equal (combination-list 'a) '(a))",
        "(equal (combination-nconc 1) '(d a))",
        // max and min merge the primary values numerically.
        "(eql (combination-max 1) 8)",
        "(eql (combination-max 3/2) 4)",
        "(eql (combination-max 'a) 1)",
        "(eql (combination-min 1) 1)",
        "(eql (combination-min 3/2) 4)",
        // progn runs every primary method and returns the last value.
        "(equal (let ((*combination-log* nil)) (list (combination-progn 1) *combination-log*)) '(d (1 3 4)))",
        "(equal (let ((*combination-log* nil)) (list (combination-progn 3/2) *combination-log*)) '(d (1 3)))",
        // Around methods keep the standard combination semantics.
        "(equal (combination-around 1) '(around 1))",
        // No applicable primary method signals an error even with an around method.
        "(eq (handler-case (combination-around 'a) (error () :error)) :error)",
        // Unsupported or missing qualifiers are rejected when the generic function runs.
        "(eq (handler-case (combination-invalid 1) (error () :error)) :error)",
        "(eq (handler-case (combination-unqualified 1) (error () :error)) :error)",
        "(eq (handler-case (progn (defmethod combination-before :before ((x t)) 2)
                                  (combination-before 1))
                          (error () :error))
             :error)",
        "(eq (handler-case (progn (defmethod combination-no-primary :around ((x t)) 1)
                                  (combination-no-primary 1))
                          (error () :error))
             :error)",
        // Invalid method combination names and options are rejected by DEFGENERIC.
        "(eq (handler-case (eval '(defgeneric combination-unknown (x)
                                    (:method-combination not-a-combination)))
                          (error () :error))
             :error)",
        "(eq (handler-case (eval '(defgeneric combination-option (x)
                                    (:method-combination + :bogus)))
                          (error () :error))
             :error)",
    ] {
        assert_truthy(&runtime, check);
    }
}

#[test]
fn builtin_operators_control_short_circuiting_and_multiple_values() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defgeneric values-and (x) (:method-combination and)
                (:method and ((x integer)) (values (plusp x) :ignored))
                (:method and (x) (values nil :last)))
             (defgeneric values-or (x) (:method-combination or)
                (:method or ((x integer)) (values x :ignored))
                (:method or ((x number)) (values nil :ignored))
                (:method or (x) (values :last :extra)))
             (defgeneric values-progn (x) (:method-combination progn)
                (:method progn ((x integer)) (values :first :ignored))
                (:method progn (x) (values))
                (:method :around (x) (call-next-method)))
             (defvar *operand-effects* nil)
             (defgeneric eager-sum (x) (:method-combination +)
                (:method + ((x integer)) (push :first *operand-effects*) :not-a-number)
                (:method + (x) (push :last *operand-effects*) 2))",
        )
        .unwrap();

    let values = runtime
        .eval(
            "(list (multiple-value-list (values-and 1))
                   (multiple-value-list (values-and -1))
                   (multiple-value-list (values-or 1))
                   (multiple-value-list (values-or 1/2))
                   (multiple-value-list (values-or nil))
                   (multiple-value-list (values-progn 1))
                   (handler-case (eager-sum 1) (type-error () *operand-effects*)))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&values[0]).unwrap(),
        "((NIL :LAST) (NIL) (1) (:LAST :EXTRA) (:LAST :EXTRA) NIL (:LAST :FIRST))"
    );
}
