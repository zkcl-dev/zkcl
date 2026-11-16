use zkcl::Runtime;

#[test]
fn float_classes_report_distinct_formats() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(let ((single (find-class 'single-float))
               (double (find-class 'double-float))
               (float (find-class 'float)))
           (and (not (eq single double))
                (eq (class-of 1.0s0) single)
                (eq (class-of 1.0f0) single)
                (eq (class-of 1.0d0) double)
                (eq (class-of 1.0l0) double)
                (null (find-class 'short-float nil))
                (null (find-class 'long-float nil))
                (member single (zk-mop:class-direct-subclasses float))
                (member double (zk-mop:class-direct-subclasses float))
                (typep single 'built-in-class)
                (typep double 'built-in-class)
                (typep 1.0f0 (list 'and single float))
                (not (typep 1.0f0 double))
                (not (typep 1.0d0 single))
                (eq (class-of (coerce 1 double)) double)
                (equal (multiple-value-list (subtypep 'short-float single)) '(t t))
                (equal (multiple-value-list (subtypep double 'long-float)) '(t t))
                (equal (multiple-value-list (subtypep single double)) '(nil t))))",
        "(loop for value in (list 1.0f0 1.0d0)
               for expected in '(single-float double-float)
               for class = (class-of value)
               always (and (eq (class-name class) expected)
                           (eq (type-of value) expected)
                           (equal (zk-mop:class-direct-superclasses class)
                                  (list (find-class 'float)))
                           (equal (mapcar #'class-name (zk-mop:class-precedence-list class))
                                  (list expected 'float 'real 'number t))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn single_and_double_float_specializers_select_distinct_methods() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defgeneric float-format-chain (object))
             (defmethod float-format-chain ((object number)) (list :number))
             (defmethod float-format-chain ((object float))
               (cons :float (call-next-method)))
             (defmethod float-format-chain ((object single-float))
               (cons :single (call-next-method)))
             (defmethod float-format-chain ((object double-float))
               (cons :double (call-next-method)))",
        )
        .unwrap();

    let source = "(equal (mapcar #'float-format-chain '(1.0f0 1.0d0 2.0f0 2.0d0 1))
                 '((:single :float :number) (:double :float :number)
                   (:single :float :number) (:double :float :number) (:number)))";
    let values = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert!(!values[0].is_nil(), "{source}");
}

#[test]
fn numeric_class_prototypes_keep_their_identity_through_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *numeric-classes*
               (mapcar #'find-class
                       '(number real rational integer ratio complex float single-float double-float)))
             (defparameter *numeric-prototypes*
               (mapcar #'zk-mop:class-prototype *numeric-classes*))",
        )
        .unwrap();
    runtime.collect();

    let values = runtime
        .eval(
            "(loop for class in *numeric-classes*
                   for prototype in *numeric-prototypes*
                   for prototype-kind in '(integer integer integer integer ratio complex
                                           single-float single-float double-float)
                   always (and (eq (zk-mop:class-prototype class) prototype)
                               (typep prototype class)
                               (eq (class-name (class-of prototype)) prototype-kind)
                               (handler-case (progn (allocate-instance class) nil)
                                 (error () t))
                               (handler-case (progn (make-instance class) nil)
                                 (error () t))))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

fn assert_numeric_predicate(runtime: &Runtime, source: &str, expected: bool) {
    let values = runtime.eval(source).unwrap();
    assert_eq!(values.len(), 1, "{source}");
    assert_eq!(!values[0].is_nil(), expected, "{source}");
}

#[test]
fn numeric_equality_observes_each_predicates_comparison_rules() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(eql 9223372036854775808 9223372036854775808)", true),
        ("(eql 2/4 1/2)", true),
        ("(equal (list #C(1/2 2)) (list #C(2/4 2)))", true),
        ("(eql 0.0 -0.0)", false),
        ("(eql 1.0f0 1.0d0)", false),
        ("(eql #C(1.0 -0.0) #C(1.0 0.0))", false),
        ("(equalp 0.0 -0.0d0)", true),
        ("(equalp #C(2.0d0 -0.0d0) 2)", true),
        ("(equalp 1/10 0.1f0)", false),
        ("(equalp 1/2 0.5f0)", true),
        ("(equalp 9223372036854775808 9223372036854775808.0d0)", true),
        (
            "(equalp 9223372036854775809 9223372036854775808.0d0)",
            false,
        ),
        (
            "(= (sxhash (list 2/4 #C(1 2))) (sxhash (list 1/2 #C(1 2))))",
            true,
        ),
    ] {
        assert_numeric_predicate(&runtime, source, expected);
    }

    assert_numeric_predicate(
        &runtime,
        "(= (sxhash #C(2.0d0 0.0d0)) (sxhash #C(2.0d0 -0.0d0)))",
        true,
    );
}

#[test]
fn numeric_type_specifiers_enforce_representation_limits() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(typep 9223372036854775808 'bignum)", true),
        ("(typep 9223372036854775808 'fixnum)", false),
        (
            "(typep 9223372036854775808 '(integer 9223372036854775808 *))",
            true,
        ),
        (
            "(typep 9223372036854775808 '(integer (9223372036854775808) *))",
            false,
        ),
        ("(typep 1/2 '(rational 0 (1)))", true),
        ("(typep 1/10 '(real 0.1f0 *))", false),
        ("(typep 1.0d0 '(float 0.0f0 2.0d0))", true),
        ("(typep 1.0d0 '(long-float (1.0d0) *))", false),
        ("(typep 1.0f0 'short-float)", true),
        ("(typep 1.0f0 'long-float)", false),
        ("(typep #C(1 2) '(complex (eql 1)))", true),
        ("(typep #C(1/2 2) '(complex integer))", true),
        ("(typep #C(1.0d0 0.0d0) '(complex short-float))", false),
        ("(typep #C(1.0d0 0.0d0) '(complex long-float))", true),
        ("(typep 1 '(complex *))", false),
        ("(eq (type-of 1/2) 'ratio)", true),
        ("(eq (type-of 0) 'bit)", true),
        ("(typep 2 (type-of 5))", true),
        ("(typep -1 (type-of 5))", false),
        ("(typep 1 (type-of 9223372036854775808))", false),
        (
            "(typep 9223372036854775808 (type-of 9223372036854775808))",
            true,
        ),
        ("(typep #C(1.0f0 2.0f0) (type-of #C(1.0d0 2.0d0)))", false),
        ("(typep -128 '(signed-byte 8))", true),
        ("(typep -129 '(signed-byte 8))", false),
        ("(typep 128 '(signed-byte 8))", false),
        ("(typep 255 '(unsigned-byte 8))", true),
        ("(typep 256 '(unsigned-byte 8))", false),
        ("(typep -1 'unsigned-byte)", false),
        (
            "(typep 9223372036854775808 '(unsigned-byte 9223372036854775808))",
            true,
        ),
        (
            "(typep 9223372036854775808 '(mod 9223372036854775809))",
            true,
        ),
    ] {
        assert_numeric_predicate(&runtime, source, expected);
    }

    assert_numeric_predicate(
        &runtime,
        "(and (eql (car (assoc 9223372036854775808 '((9223372036854775808 . 41)))) 9223372036854775808)
              (typep 9223372036854775808 '(member nil 9223372036854775808))
              (typep 1/2 '(eql 2/4)))",
        true,
    );
}

#[test]
fn numeric_hash_keys_use_the_tables_test_after_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-numeric-types", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    assert_numeric_predicate(
        &runtime,
        "(let ((table (make-hash-table :test 'eq))
               (left (+ 9223372036854775808 1))
               (right (+ 9223372036854775808 1)))
           (setf (gethash left table) 42)
           (collect-numeric-types)
           (multiple-value-bind (value present-p) (gethash right table :absent)
             (and (eql left right)
                  (eq (not (null present-p)) (eq left right))
                  (if present-p (eql value 42) (eq value :absent)))))",
        true,
    );
    assert_numeric_predicate(
        &runtime,
        "(let ((table (make-hash-table :test 'eql)))
           (setf (gethash 9223372036854775808 table) 41
                 (gethash #C(1.0f0 0.0f0) table) 42)
           (collect-numeric-types)
           (and (= (gethash 9223372036854775808 table) 41)
                (= (gethash #C(1.0f0 0.0f0) table) 42)
                (null (gethash #C(1.0f0 -0.0f0) table))
                (null (gethash 1.0f0 table))))",
        true,
    );
    assert_numeric_predicate(
        &runtime,
        "(let ((table (make-hash-table :test 'equal)))
           (setf (gethash (list 9223372036854775808 1/2) table) 41)
           (collect-numeric-types)
           (and (= (gethash (list 9223372036854775808 2/4) table) 41)
                (null (gethash (list 9223372036854775808 0.5d0) table))))",
        true,
    );
    assert_numeric_predicate(
        &runtime,
        "(let ((table (make-hash-table :test 'equalp)))
           (setf (gethash 1/2 table) 41
                 (gethash #C(2.0d0 -0.0d0) table) 42
                 (gethash 9223372036854775808 table) 43
                 (gethash (list #C(1.0 0.5)) table) 44)
           (collect-numeric-types)
           (and (= (gethash 0.5d0 table) 41)
                (= (gethash 2 table) 42)
                (= (gethash 9223372036854775808.0d0 table) 43)
                (= (gethash (list #C(1 1/2)) table) 44)))",
        true,
    );
}
