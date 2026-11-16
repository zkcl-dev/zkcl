use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn type_of_structure_follows_its_proper_class_name() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(progn
               (defstruct typed-record value)
               (let* ((record (make-typed-record :value 42))
                      (class (class-of record)))
                 (setf (class-name class) 'renamed-record)
                 (let ((unbound (type-of record)))
                   (setf (find-class 'renamed-record) class)
                   (and (eq unbound class)
                        (typep record unbound)
                        (eq (type-of record) 'renamed-record)
                        (= (typed-record-value record) 42)))))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn core_builtin_prototypes_preserve_type_and_allocation_rules() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *prototype-classes*
               (mapcar #'find-class '(t null symbol list cons character function sequence
                                     array vector bit-vector string hash-table pathname
                                     readtable random-state)))
             (defparameter *builtin-prototypes*
               (mapcar #'zk-mop:class-prototype *prototype-classes*))",
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(loop for class in *prototype-classes*
               for prototype in *builtin-prototypes*
               always (and (typep prototype class)
                           (eq (zk-mop:class-prototype class) prototype)
                           (handler-case (progn (allocate-instance class) nil)
                             (error () t))
                           (handler-case (progn (make-instance class) nil)
                             (error () t))))",
        "(and (equal (array-dimensions (zk-mop:class-prototype (find-class 'array))) '(0 0))
              (= (length (zk-mop:class-prototype (find-class 'bit-vector))) 0)
              (= (hash-table-count (zk-mop:class-prototype (find-class 'hash-table))) 0)
              (null (symbol-package (zk-mop:class-prototype (find-class 'symbol))))
              (eq (funcall (zk-mop:class-prototype (find-class 'function)) :probe) :probe))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn class_objects_keep_their_identity_when_names_are_reinterned() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defstruct parent-record value)
             (defstruct (child-record (:include parent-record)) extra)
             (defparameter *child-record* (make-child-record :value 42))
             (defparameter *child-record-class* (class-of *child-record*))
             (defparameter *integer-class* (find-class 'integer))
             (defmacro class-in-expansion (&environment environment)
               (eq (find-class 'integer t environment) (find-class 'integer)))",
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(eq (class-of *child-record*) *child-record-class*)",
        "(eq *child-record-class* (find-class 'child-record))",
        "(eq (class-name *child-record-class*) 'child-record)",
        "(typep *child-record-class* 'structure-class)",
        "(typep *child-record-class* (find-class 'class))",
        "(typep *child-record-class* 'standard-object)",
        "(typep *child-record* (find-class 'parent-record))",
        "(typep *child-record* (find-class 'structure-object))",
        "(equal (multiple-value-list (subtypep *child-record-class* (find-class 'structure-object)))
                '(t t))",
        "(equal (multiple-value-list (subtypep *child-record-class* 'parent-record))
                '(t t))",
        "(not (typep (make-parent-record) *child-record-class*))",
        "(eq (class-name (class-of *integer-class*)) 'built-in-class)",
        "(typep *integer-class* (find-class 'built-in-class))",
        "(typep (class-of *integer-class*) 'standard-class)",
        "(eq (type-of *integer-class*) 'built-in-class)",
        "(eq *integer-class* (find-class 'integer))",
        "(typep 1 (find-class 'integer))",
        "(equal (multiple-value-list (subtypep '(integer 0 1) *integer-class*)) '(t t))",
        "(typep 10000000000000000000000 (find-class 'rational))",
        "(typep 2/3 (find-class 'real))",
        "(typep 1.0d0 (find-class 'float))",
        "(typep #C(0 1) (find-class 'complex))",
        "(not (typep #C(0 1) (find-class 'real)))",
        "(typep 1 (list 'and (find-class 'number) (find-class 'integer)))",
        "(typep nil (find-class 'list))",
        "(typep nil (find-class 'symbol))",
        "(typep '(0) (find-class 'cons))",
        "(not (typep 'symbol (find-class 'list)))",
        "(typep #\\A (find-class 'character))",
        "(typep #*01 (find-class 'bit-vector))",
        "(typep (make-array '(2 2)) (find-class 'array))",
        "(not (typep (make-array '(2 2)) (find-class 'sequence)))",
        "(typep #(0 1) (find-class 'vector))",
        "(typep \"text\" (find-class 'string))",
        "(typep \"text\" (find-class 'sequence))",
        "(typep (make-hash-table) (find-class 'hash-table))",
        "(typep (make-random-state) (find-class 'random-state))",
        "(typep (copy-readtable nil) (find-class 'readtable))",
        "(typep (find-package 'cl) (find-class 'package))",
        "(typep (pathname \"a\") (find-class 'pathname))",
        "(typep (make-string-output-stream) (find-class 'string-stream))",
        "(typep (make-string-input-stream \"x\") (find-class 'stream))",
        "(typep #'car (find-class 'function))",
        "(not (typep #'car (find-class 'generic-function)))",
        "(eq (class-of (make-condition 'simple-error)) (find-class 'simple-error))",
        "(typep (make-condition 'simple-error) (find-class 'simple-condition))",
        "(typep (make-condition 'simple-error) (find-class 'error))",
        "(not (typep (make-condition 'simple-error) (find-class 'standard-object)))",
        "(typep (make-condition 'reader-error) (find-class 'parse-error))",
        "(typep (make-condition 'reader-error) (find-class 'stream-error))",
        "(null (find-class 'no-such-class nil))",
        "(null (find-class (make-symbol \"INTEGER\") nil))",
        "(class-in-expansion)",
    ] {
        let result = runtime
            .eval(source)
            .unwrap_or_else(|failure| panic!("{source}: {failure}"));
        assert_eq!(result.len(), 1, "{source}");
        assert!(!result[0].is_nil(), "{source}");
    }

    // Every name in CLHS Figure 4-8 names a class, including classes for which
    // the runtime does not yet provide constructors.
    let result = runtime
        .eval("(let ((class (find-class 'class)))
                 (mapcar (lambda (symbol)
                           (let ((object (find-class symbol)))
                             (and (eq (class-name object) symbol) (typep object class))))
                         '(arithmetic-error array bit-vector broadcast-stream built-in-class
                           cell-error character class complex concatenated-stream condition cons
                           control-error division-by-zero echo-stream end-of-file error file-error
                           file-stream float floating-point-inexact floating-point-invalid-operation
                           floating-point-overflow floating-point-underflow function generic-function
                           hash-table integer list logical-pathname method method-combination null
                           number package package-error parse-error pathname print-not-readable
                           program-error random-state ratio rational reader-error readtable real restart
                           sequence serious-condition simple-condition simple-error simple-type-error
                           simple-warning standard-class standard-generic-function standard-method
                           standard-object storage-condition stream stream-error string string-stream
                           structure-class structure-object style-warning symbol synonym-stream t
                           two-way-stream type-error unbound-slot unbound-variable undefined-function
                           vector warning)))")
        .unwrap();
    let mut cursor = result.into_iter().next().unwrap();
    let mut count = 0;
    while let Some((found, rest)) = runtime.cons_fields(&cursor) {
        assert!(!found.is_nil(), "class at index {count}");
        cursor = rest;
        count += 1;
    }
    assert_eq!(count, 75);

    for source in ["(class-name 3)", "(find-class \"INTEGER\" nil)"] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError)
        );
    }
    assert!(runtime.eval("(find-class 'no-such-class)").is_err());
    let class = runtime.eval("(find-class 'integer)").unwrap();
    assert_eq!(
        runtime.repr(&class[0]).unwrap(),
        "#<BUILT-IN-CLASS INTEGER>"
    );

    let distinct_names = runtime
        .eval(
            "(let* ((old (class-name *child-record-class*))
                    (removed (unintern old))
                    (new (intern \"CHILD-RECORD\")))
               (and removed
                    (not (eq old new))
                    (eq (find-class old) *child-record-class*)
                    (null (find-class new nil))
                    (eq (class-of *child-record*) *child-record-class*)))",
        )
        .unwrap();
    assert!(!distinct_names[0].is_nil());
}
