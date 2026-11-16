use zkcl::Runtime;

#[test]
fn atomic_cons_svref_symbol_and_fence_semantics() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *atomic-var* :initial)
             (defvar *unbound-atomic*)
             (defconstant +atomic-const+ :locked)",
        )
        .unwrap();

    // CAS returns (values previous swapped-p) on both match and mismatch.
    let cas_match = runtime
        .eval(
            "(let ((cell (cons :a :b)))
               (multiple-value-list (zk-atomic:cas-car cell :a :a2)))",
        )
        .unwrap();
    assert_eq!(runtime.repr(&cas_match[0]).unwrap(), "(:A T)");

    let cas_miss = runtime
        .eval(
            "(let ((cell (cons :a :b)))
               (multiple-value-bind (prev swapped)
                   (zk-atomic:cas-car cell :wrong :a2 :acq-rel)
                 (list prev swapped (zk-atomic:atomic-load-car cell :acquire))))",
        )
        .unwrap();
    assert_eq!(runtime.repr(&cas_miss[0]).unwrap(), "(:A NIL :A)");

    let cdr_cycle = runtime
        .eval(
            "(let ((cell (cons :head :tail)))
               (zk-atomic:atomic-store-cdr cell :stored :release)
               (multiple-value-bind (prev1 ok1)
                   (zk-atomic:cas-cdr cell :stored :cas-next :seq-cst)
                 (multiple-value-bind (prev2 ok2)
                     (zk-atomic:cas-cdr cell :stale :never :relaxed)
                   (list prev1 ok1 prev2 ok2 (zk-atomic:atomic-load-cdr cell :relaxed)))))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&cdr_cycle[0]).unwrap(),
        "(:STORED T :CAS-NEXT NIL :CAS-NEXT)"
    );

    let vec_cycle = runtime
        .eval(
            "(let ((v (vector 'one 'two 'three)))
               (zk-atomic:atomic-store-svref v 1 'stored :release)
               (multiple-value-bind (prev1 ok1)
                   (zk-atomic:cas-svref v 1 'stored 'cas-val :acq-rel)
                 (multiple-value-bind (prev2 ok2)
                     (zk-atomic:cas-svref v 1 'wrong 'never :acquire)
                   (list prev1 ok1 prev2 ok2 (zk-atomic:atomic-load-svref v 1 :acquire)))))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&vec_cycle[0]).unwrap(),
        "(STORED T CAS-VAL NIL CAS-VAL)"
    );

    let sym_cycle = runtime
        .eval(
            "(progn
               (zk-atomic:atomic-store-symbol-value '*atomic-var* :written :release)
               (multiple-value-bind (prev1 ok1)
                   (zk-atomic:cas-symbol-value '*atomic-var* :written :cas-sym :seq-cst)
                 (multiple-value-bind (prev2 ok2)
                     (zk-atomic:cas-symbol-value '*atomic-var* :wrong :never)
                   (list prev1 ok1 prev2 ok2
                         (zk-atomic:atomic-load-symbol-value '*atomic-var* :acquire)
                         (zk-atomic:atomic-load-symbol-value '+atomic-const+ :acquire)
                         (zk-atomic:atomic-load-symbol-value nil :acquire)
                         (zk-atomic:fence :acquire)
                         (zk-atomic:fence :release)
                         (zk-atomic:fence :acq-rel)
                         (zk-atomic:fence)))))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&sym_cycle[0]).unwrap(),
        "(:WRITTEN T :CAS-SYM NIL :CAS-SYM :LOCKED NIL NIL NIL NIL NIL)"
    );
}

#[test]
fn atomic_primitives_validate_orders_targets_bounds_and_variables() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defvar *atomic-unbound*)
             (defconstant +immutable-atomic+ 123)",
        )
        .unwrap();

    for form in [
        // Invalid memory orders
        "(let ((c (cons 1 2))) (zk-atomic:atomic-load-car c :release))",
        "(let ((c (cons 1 2))) (zk-atomic:atomic-load-cdr c :acq-rel))",
        "(let ((c (cons 1 2))) (zk-atomic:atomic-store-car c 9 :acquire))",
        "(let ((c (cons 1 2))) (zk-atomic:atomic-store-cdr c 9 :acq-rel))",
        "(let ((v (vector 1))) (zk-atomic:atomic-load-svref v 0 :release))",
        "(let ((v (vector 1))) (zk-atomic:atomic-store-svref v 0 2 :acquire))",
        "(zk-atomic:atomic-load-symbol-value '*atomic-unbound* :release)",
        "(zk-atomic:fence :relaxed)",
        "(let ((c (cons 1 2))) (zk-atomic:atomic-load-car c :bogus))",
        "(let ((c (cons 1 2))) (zk-atomic:atomic-load-car c 'seq-cst))",
        // Target type errors
        "(zk-atomic:atomic-load-car 42)",
        "(zk-atomic:cas-cdr nil 1 2)",
        "(let ((adj (make-array 2 :adjustable t))) (zk-atomic:atomic-load-svref adj 0))",
        "(let ((mat (make-array '(2 2)))) (zk-atomic:cas-svref mat 0 nil t))",
        "(zk-atomic:atomic-store-svref \"str\" 0 #\\a)",
        // Bounds / index errors
        "(let ((v (vector 1 2))) (zk-atomic:atomic-load-svref v 2))",
        "(let ((v (vector 1 2))) (zk-atomic:cas-svref v -1 1 2))",
        // Symbol constant / unbound checks
        "(zk-atomic:cas-symbol-value '+immutable-atomic+ 123 456)",
        "(zk-atomic:atomic-store-symbol-value '+immutable-atomic+ 456)",
        "(zk-atomic:atomic-store-symbol-value :locked-keyword 1)",
        "(zk-atomic:atomic-store-symbol-value nil t)",
        "(zk-atomic:cas-symbol-value nil nil t)",
        "(zk-atomic:atomic-load-symbol-value '*atomic-unbound*)",
        "(zk-atomic:cas-symbol-value '*atomic-unbound* nil t)",
    ] {
        assert!(
            runtime.eval(form).is_err(),
            "expected error for invalid atomic operation: {form}"
        );
    }

    // Storing into an initially unbound variable binds it atomically.
    let bound = runtime
        .eval(
            "(progn
               (zk-atomic:atomic-store-symbol-value '*atomic-unbound* :now-bound :seq-cst)
               *atomic-unbound*)",
        )
        .unwrap();
    assert_eq!(runtime.repr(&bound[0]).unwrap(), ":NOW-BOUND");
}

#[test]
fn atomic_writes_trigger_generational_write_barrier_for_mature_containers() {
    let runtime = Runtime::builder()
        .heap_size_bytes(8 * 1024 * 1024)
        .start()
        .unwrap();
    runtime
        .load_str(
            "(defvar *mature-cons* (cons :old-car :old-cdr))
             (defvar *mature-vec* (vector :old-0 :old-1 :old-2 :old-3))
             (defvar *mature-sym* :old-val)
             (defvar *garbage* nil)",
        )
        .unwrap();

    // Promote containers and symbol to mature generation.
    runtime.collect();
    runtime.collect();
    assert!(runtime.heap_stats().major_collections >= 1);

    let before_minor = runtime.heap_stats().minor_collections;
    runtime
        .eval(
            "(progn
               (zk-atomic:cas-car *mature-cons* :old-car (list :young-cas-car 101))
               (zk-atomic:atomic-store-cdr *mature-cons* (list :young-store-cdr 102) :release)
               (zk-atomic:cas-svref *mature-vec* 0 :old-0 (list :young-cas-svref 201) :acq-rel)
               (zk-atomic:atomic-store-svref *mature-vec* 1 (list :young-store-svref 202) :release)
               (zk-atomic:cas-symbol-value '*mature-sym* :old-val (list :young-cas-sym 301))
               (dotimes (i 25000)
                 (setq *garbage* (cons i nil))))",
        )
        .unwrap();

    // Ensure at least one minor GC ran after storing young objects into mature containers.
    if runtime.heap_stats().minor_collections == before_minor {
        runtime
            .eval("(dotimes (i 100000) (setq *garbage* (cons i nil)))")
            .unwrap();
    }
    assert!(
        runtime.heap_stats().minor_collections > before_minor,
        "expected young-gen churn to trigger minor GC"
    );

    let checked = runtime
        .eval(
            "(list (zk-atomic:atomic-load-car *mature-cons* :acquire)
                   (zk-atomic:atomic-load-cdr *mature-cons* :acquire)
                   (zk-atomic:atomic-load-svref *mature-vec* 0 :acquire)
                   (zk-atomic:atomic-load-svref *mature-vec* 1 :acquire)
                   (zk-atomic:atomic-load-symbol-value '*mature-sym* :acquire))",
        )
        .unwrap();
    assert_eq!(
        runtime.repr(&checked[0]).unwrap(),
        "((:YOUNG-CAS-CAR 101) (:YOUNG-STORE-CDR 102) (:YOUNG-CAS-SVREF 201) (:YOUNG-STORE-SVREF 202) (:YOUNG-CAS-SYM 301))"
    );

    // Also exercise ATOMIC-STORE-SYMBOL-VALUE on a promoted symbol followed by another minor GC.
    let minor_two = runtime.heap_stats().minor_collections;
    runtime
        .eval(
            "(progn
               (zk-atomic:atomic-store-symbol-value '*mature-sym* (list :young-store-sym 302) :seq-cst)
               (dotimes (i 120000) (setq *garbage* (cons i nil))))",
        )
        .unwrap();
    assert!(runtime.heap_stats().minor_collections > minor_two);
    let reloaded = runtime
        .eval("(zk-atomic:atomic-load-symbol-value '*mature-sym*)")
        .unwrap();
    assert_eq!(runtime.repr(&reloaded[0]).unwrap(), "(:YOUNG-STORE-SYM 302)");
}
