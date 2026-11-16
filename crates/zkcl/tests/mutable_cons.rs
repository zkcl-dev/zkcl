use zkcl::Runtime;

#[test]
fn cons_writes_preserve_cell_identity_through_collection() {
    let runtime = Runtime::start().unwrap();

    let values = runtime
        .eval(
            "(let* ((link (cons :before :tail))
                    (alias link))
               (values (eq (rplaca link (list 41)) alias)
                       (eq (rplacd link (list (list 42))) alias)
                       link))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    runtime.collect();
    let (car, cdr) = runtime.cons_fields(&values[2]).unwrap();
    assert_eq!(runtime.cons_fields(&car).unwrap().0.as_fixnum(), Some(41));
    let child = runtime.cons_fields(&cdr).unwrap().0;
    assert_eq!(runtime.cons_fields(&child).unwrap().0.as_fixnum(), Some(42));
}

#[test]
fn nconc_reuses_input_cells_when_splicing_list_arguments() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(let ((first (list* 1 2 :dotted))
                   (second (cons 3 :dotted))
                   (tail (list 4)))
               (values (eq (nconc nil first nil second tail) first)
                       (eq (cddr first) second)
                       (eq (cdr second) tail)
                       (equal first '(1 2 3 4))))",
        )
        .unwrap();
    assert!(values.iter().all(|value| !value.is_nil()));
    let values = runtime
        .eval(
            "(let ((prefix (cons :car :cdr)))
               (values (cdr (nconc prefix nil))
                       (cdr (nconc prefix 7))
                       (nconc)
                       (nconc 42)))",
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert_eq!(values[1].as_fixnum(), Some(7));
    assert!(values[2].is_nil());
    assert_eq!(values[3].as_fixnum(), Some(42));
}

#[test]
fn nconc_can_create_a_cycle_that_survives_collection() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(let* ((head (list 11))
                    (ring (nconc head head)))
               (values (eq ring head) (eq (cdr ring) head) ring))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    runtime.collect();
    let (item, next) = runtime.cons_fields(&values[2]).unwrap();
    assert_eq!(item.as_fixnum(), Some(11));
    assert!(next.same_identity(&values[2]));
}

#[test]
fn list_operations_detect_circular_spines_when_traversal_must_end() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(let ((prefix (cons 11 :dotted))
                   (ring (list 20 21 22)))
               (rplacd (cddr ring) (cdr ring))
               (values (handler-case (nconc prefix ring nil)
                         (error () (eq (cdr prefix) :dotted)))
                       (handler-case (last ring) (error () t))
                       (handler-case (append ring nil) (type-error () t))
                       (handler-case (apply #'list ring) (type-error () t))
                       (handler-case (length ring) (type-error () t))
                       (nth 7 ring)
                       (handler-case (rplaca nil 0)
                         (type-error (condition)
                           (eq (type-error-expected-type condition) 'cons)))
                       (handler-case (rplacd 1 nil)
                         (type-error (condition)
                           (eq (type-error-datum condition) 1)))))",
        )
        .unwrap();
    assert!(values[..5].iter().all(|value| !value.is_nil()));
    assert_eq!(values[5].as_fixnum(), Some(21));
    assert!(!values[6].is_nil());
    assert!(!values[7].is_nil());

    let values = runtime
        .eval(
            "(let ((directory (list :absolute \"subdir\"))
                   (type (list 'or 'integer)))
               (rplacd (cdr directory) (cdr directory))
               (rplacd (cdr type) (cdr type))
               (values (handler-case (make-pathname :directory directory) (error () t))
                       (handler-case (typep 1 type) (error () t))))",
        )
        .unwrap();
    assert!(values.iter().all(|value| !value.is_nil()));
}
