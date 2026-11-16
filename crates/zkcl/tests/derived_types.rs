use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

#[test]
fn typep_expands_derived_type_specifiers_in_composite_types() {
    let runtime = Runtime::start().unwrap();
    expands_derived(&runtime);
    reports_errors(&runtime);
}

fn expands_derived(runtime: &Runtime) {
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn (deftype single-integer () 'integer) 'single-integer)"
        ),
        ["SINGLE-INTEGER"],
        "DEFTYPE returns the type name"
    );
    assert_eq!(eval_reprs(runtime, "(typep 5 'single-integer)"), ["T"]);
    assert_eq!(
        eval_reprs(
            runtime,
            "(typep 5 (cons 'and (make-list 300 :initial-element 'single-integer)))"
        ),
        ["T"],
        "the nesting limit does not count sibling expansions"
    );
    assert_eq!(eval_reprs(runtime, "(typep 1.5 'single-integer)"), ["NIL"]);
    assert_eq!(
        eval_reprs(runtime, "(typep 5 '(single-integer))"),
        ["T"],
        "the empty argument list expands too"
    );

    // A derived specifier that builds its expansion from its arguments.
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (deftype bounded (&optional (low 0) (high 10))
                 `(integer ,low ,high))
               (values (typep 5 'bounded)
                       (typep 5 '(bounded 4 6))
                       (typep 7 '(bounded 4 6))))"
        ),
        ["T", "T", "NIL"]
    );

    // Nested composition still reaches the derived specifier.
    assert_eq!(eval_reprs(runtime, "(typep 5 '(or string bounded))"), ["T"]);
    assert_eq!(
        eval_reprs(runtime, "(typep 5 '(and integer (not (eql 5))))"),
        ["NIL"]
    );
}

#[test]
fn array_element_type_aliases_use_the_expanded_storage_type() {
    let runtime = Runtime::start().unwrap();
    eval_reprs(&runtime, "(deftype element-alias (type) type)");
    eval_reprs(
        &runtime,
        "(deftype char-alias () '(element-alias character))",
    );
    eval_reprs(
        &runtime,
        "(deftype char-vector () '(simple-array char-alias (*)))",
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(let ((s (make-string 4 :element-type 'char-alias :initial-element #\雪))
                                     (a (make-array 2 :element-type 'char-alias)))
                                 (values s (typep "zkcl" 'char-vector)
                                         (typep a '(vector char-alias 2))
                                         (equal (upgraded-array-element-type 'char-alias)
                                                (array-element-type s))
                                         (subtypep '(array character) '(array char-alias))
                                         (subtypep '(array char-alias) '(array character))))"#
        ),
        [r#""雪雪雪雪""#, "T", "T", "T", "T", "T"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(values (array-element-type (make-string 0 :element-type '(element-alias nil)))
                               (handler-case
                                   (make-string 1 :element-type '(element-alias standard-char) :initial-element #\雪)
                                 (type-error (c) (type-error-expected-type c))))"#
        ),
        ["NIL", "(ELEMENT-ALIAS STANDARD-CHAR)"]
    );
}

#[test]
fn satisfies_filters_type_members_with_the_named_predicate() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(eval_reprs(&runtime, "(typep 4 '(satisfies evenp))"), ["T"]);
    assert_eq!(
        eval_reprs(&runtime, "(typep 5 '(satisfies evenp))"),
        ["NIL"]
    );
    assert_eq!(
        eval_reprs(&runtime, "(typep 5 '(and integer (satisfies oddp)))"),
        ["T"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(progn
               (defun even-size-p (x) (evenp (array-total-size x)))
               (deftype even-array (&optional type size)
                 `(and (array ,type ,size) (satisfies even-size-p)))
               (values (typep (make-array '(10)) '(even-array t (*)))
                       (typep (make-array '(5)) '(even-array t (*)))
                       (typep 1 '(even-array integer (10)))))"
        ),
        ["T", "NIL", "NIL"]
    );
}

#[test]
fn typep_keeps_operands_alive_while_user_type_code_collects() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-type-heap", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(progn
               (deftype collected (&optional low)
                 (collect-type-heap)
                 `(integer ,low *))
               (values (typep 3 '(collected 1))
                       (typep 0 '(collected 1))))"
        ),
        ["T", "NIL"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(progn
               (defun collected-oddp (n) (collect-type-heap) (oddp n))
               (typep 3 '(satisfies collected-oddp)))"
        ),
        ["T"]
    );
}

fn reports_errors(runtime: &Runtime) {
    for source in [
        "(deftype)",
        "(deftype foo)",
        "(deftype 1 () t)",
        "(typep 1 '(satisfies 5))",
        "(typep 1 (list 'satisfies #'oddp))",
        "(typep 1 '(bounded 0 . 10))",
        "(let ((spec (list 'bounded 0))) (rplacd (cdr spec) spec) (typep 1 spec))",
        "(let ((spec (list 'not nil))) (setf (cadr spec) spec) (typep 1 spec))",
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "{source} should report an error"
        );
    }
}

#[test]
fn compiled_type_checks_expand_the_declared_type() {
    let runtime = Runtime::start().unwrap();
    eval_reprs(
        &runtime,
        "(deftype row-index (limit) `(integer 0 (,limit)))",
    );
    eval_reprs(&runtime, "(deftype small-row () '(row-index 4))");
    eval_reprs(
        &runtime,
        "(defun checked-row (x) (declare (type (or null small-row) x)) x)",
    );
    assert_eq!(
        eval_reprs(&runtime, "(list (checked-row nil) (checked-row 3))"),
        ["(NIL 3)"]
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(handler-case (checked-row 4)
                          (type-error (c) (type-error-expected-type c)))"
        ),
        ["(OR NULL SMALL-ROW)"]
    );
    eval_reprs(
        &runtime,
        "(defvar *ring* (let ((x (list 1))) (setf (cdr x) x) x))",
    );
    eval_reprs(
        &runtime,
        "(deftype ring-type () (list 'or 'null (list 'eql *ring*)))",
    );
    eval_reprs(
        &runtime,
        "(defun check-ring (x) (declare (type ring-type x)) t)",
    );
    assert_eq!(eval_reprs(&runtime, "(check-ring *ring*)"), ["T"]);
}

#[test]
fn compiled_typep_calls_the_expander_in_the_current_dynamic_environment() {
    let runtime = Runtime::start().unwrap();
    eval_reprs(&runtime, "(defvar *checked-type* 'integer)");
    eval_reprs(&runtime, "(deftype current-choice () *checked-type*)");
    eval_reprs(
        &runtime,
        "(defun check-choice (x spec) (if (typep x spec) 'matches nil))",
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(list (check-choice 1 'current-choice)
                             (let ((*checked-type* 'string)) (check-choice 1 'current-choice)))"
        ),
        ["(MATCHES NIL)"]
    );
    eval_reprs(&runtime, "(deftype current-choice () 'string)");
    assert_eq!(
        eval_reprs(&runtime, "(check-choice 1 'current-choice)"),
        ["NIL"]
    );

    eval_reprs(
        &runtime,
        "(deftype declaring () (signal 'simple-condition) 'fixnum)",
    );
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((seen 0))
                         (values (handler-bind ((simple-condition (lambda (c)
                                  (declare (ignore c)) (incf seen))))
                                  (eval '(the declaring 17))) seen))"
        ),
        ["17", "1"]
    );

    // A hot TYPEP site must call the predicate in the surrounding dynamic context.
    eval_reprs(&runtime, "(defun leave-choice (x) (throw 'choice x))");
    assert_eq!(
        eval_reprs(
            &runtime,
            "(loop for i below 3 collect
                         (catch 'choice (check-choice i '(satisfies leave-choice))))"
        ),
        ["(0 1 2)"]
    );
}
