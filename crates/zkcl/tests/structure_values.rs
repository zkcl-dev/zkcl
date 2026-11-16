use zkcl::Runtime;

#[test]
fn structure_copies_preserve_shared_slots_after_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("force-structure-collection", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            r#"(defstruct moving-structure "Documented." value)
               (defstruct (restricted-structure
                            (:include moving-structure (value nil :read-only t))))
               (defstruct (boa-structure (:constructor build-boa (value))) value)"#,
        )
        .unwrap();
    let values = runtime
        .eval(
            r#"(let* ((shared (list 41))
                        (item (make-moving-structure :value shared))
                        (copy (copy-structure item)))
                   (force-structure-collection)
                   (values
                     (and (not (eq copy item))
                          (equalp copy item)
                          (eq (moving-structure-value copy) shared)
                          (eq (type-of copy) 'moving-structure))
                     (and (setf (moving-structure-value item) (list 42))
                          (= (car (moving-structure-value item)) 42)
                          (= (car (moving-structure-value copy)) 41))
                     (handler-case
                         (moving-structure-value nil)
                       (type-error (error)
                         (eq (type-error-expected-type error) 'moving-structure)))
                     (handler-case
                         (setf (moving-structure-value (make-restricted-structure)) 7)
                       (program-error () t))
                     (string= (documentation 'moving-structure 'structure) "Documented.")
                     (handler-case
                         (write-to-string (build-boa 7) :readably t)
                       (print-not-readable () t))
                     (write-to-string item :readably t)))"#,
        )
        .unwrap();
    for value in &values[..6] {
        assert!(!value.is_nil());
    }
    let printed = runtime.string(&values[6]).unwrap();
    assert_eq!(printed, "#S(MOVING-STRUCTURE :VALUE (42))");
    let read = runtime.eval(&printed).unwrap();
    assert_eq!(runtime.repr(&read[0]).unwrap(), printed);
}

#[test]
fn self_referential_structures_are_not_readable() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defstruct moving-structure value)")
        .unwrap();
    let values = runtime
        .eval(
            r#"(let ((item #S(moving-structure "VALUE" 42)))
                 (let ((correct (= (moving-structure-value item) 42)))
                   (setf (moving-structure-value item) item)
                   (values correct
                           (handler-case
                               (write-to-string item :readably t)
                             (print-not-readable () t))
                           item)))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert_eq!(
        runtime.repr(&values[2]).unwrap(),
        "#S(MOVING-STRUCTURE :VALUE #<CIRCULAR-STRUCTURE>)"
    );
}
