use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::lisp::{eval, eval_error};

#[test]
fn specialized_vm_operations_preserve_gc_roots() {
    let rt = Runtime::start().expect("runtime start");
    rt.register_function("HOST-FORCE-COLLECT", |runtime, args| {
        runtime.collect();
        Ok(args.to_vec())
    })
    .unwrap();

    check_rooting_of_multiple_values(&rt);
    check_cons_operands_remain_rooted(&rt);
    check_rooting_of_captured_mutable_bindings(&rt);
    check_steady_state_root_registrations(&rt);
}

fn check_rooting_of_multiple_values(rt: &Runtime) {
    rt.load_str(
        "(defun churn-heap (n)
           (let ((acc nil))
             (dotimes (i n (length acc))
               (setq acc (cons i nil)))))
         (defun mv-zero () (values))
         (defun mv-three (a b c)
           (values (cons a nil) (cons b nil) (cons c nil)))
         (defun mv-twelve ()
           (values (cons 1 :a) (cons 2 :b) (cons 3 :c) (cons 4 :d)
                   (cons 5 :e) (cons 6 :f) (cons 7 :g) (cons 8 :h)
                   (cons 9 :i) (cons 10 :j) (cons 11 :k) (cons 12 :l)))",
    )
    .unwrap();

    // 0 values
    assert_eq!(eval(rt, "(mv-zero)").len(), 0);
    assert!(eval(rt, "(multiple-value-list (mv-zero))")[0].is_nil());

    // >1 values + CallValues + SaveValues + forced moving GC + heavy consing
    let mv_call_res = eval(
        rt,
        "(multiple-value-call
           (lambda (&rest xs)
             (mapcar #'car xs))
           (mv-zero)
           (mv-three 10 20 30)
           (progn
             (churn-heap 15000)
             (host-force-collect)
             (mv-three 40 50 60))
           (mv-zero))",
    );
    assert_eq!(mv_call_res.len(), 1);
    assert_eq!(rt.repr(&mv_call_res[0]).unwrap(), "(10 20 30 40 50 60)");

    // >8 values growth through Values::select and Values::append across GC
    let twelve = eval(
        rt,
        "(multiple-value-call
           (lambda (&rest xs) (mapcar #'car xs))
           (mv-twelve)
           (progn (churn-heap 12000) (host-force-collect) (mv-zero))
           (mv-three 13 14 15))",
    );
    assert_eq!(
        rt.repr(&twelve[0]).unwrap(),
        "(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15)"
    );

    // SaveValues / Cleanup inside UNWIND-PROTECT across forced moving GC
    let uwp_mv = eval(
        rt,
        "(multiple-value-list
           (unwind-protect
               (multiple-value-prog1
                   (mv-twelve)
                 (churn-heap 10000)
                 (host-force-collect))
             (churn-heap 10000)
             (host-force-collect (cons :cleanup 99))))",
    );
    assert_eq!(
        rt.repr(&uwp_mv[0]).unwrap(),
        "((1 . :A) (2 . :B) (3 . :C) (4 . :D) (5 . :E) (6 . :F) (7 . :G) (8 . :H) (9 . :I) (10 . :J) (11 . :K) (12 . :L))"
    );

    // Non-local unwind preserving multiple values through nested UNWIND-PROTECT with GC in cleanup
    let uwp_throw = eval(
        rt,
        "(multiple-value-list
           (catch :out
             (unwind-protect
                 (throw :out (mv-three 7 8 9))
               (churn-heap 8000)
               (host-force-collect))))",
    );
    assert_eq!(rt.repr(&uwp_throw[0]).unwrap(), "((7) (8) (9))");
}

fn check_cons_operands_remain_rooted(rt: &Runtime) {
    rt.load_str(
        "(defun cons-pair (a b) (cons a b))
         (defun cons-noncontig (a unused b)
           (declare (ignore unused))
           (cons b a))
         (defun cons-loop-gc (n)
           (let ((acc nil))
             (dotimes (i n acc)
               (when (= (mod i 250) 0)
                 (host-force-collect acc))
               (setq acc (cons i acc)))))
         (defun cons-notinline (a b)
           (declare (notinline cons))
           (cons a b))
         (defun cons-flet-shadow (a b)
           (flet ((cons (x y) (list y x)))
             (cons a b)))",
    )
    .unwrap();

    // Warm and steady-state CONS, including arguments from nonadjacent lexical bindings.
    assert_eq!(rt.repr(&eval(rt, "(cons-pair 1 2)")[0]).unwrap(), "(1 . 2)");
    assert_eq!(
        rt.repr(&eval(rt, "(cons-pair :x :y)")[0]).unwrap(),
        "(:X . :Y)"
    );
    assert_eq!(
        rt.repr(&eval(rt, "(cons-noncontig 'first 'skip 'second)")[0])
            .unwrap(),
        "(SECOND . FIRST)"
    );
    assert_eq!(
        rt.repr(&eval(rt, "(cons-notinline 3 4)")[0]).unwrap(),
        "(3 . 4)"
    );
    assert_eq!(
        rt.repr(&eval(rt, "(cons-flet-shadow 10 20)")[0]).unwrap(),
        "(20 10)"
    );

    // Heavy consing + forced moving GC relocation in loop
    let loop_res = eval(
        rt,
        "(let ((xs (cons-loop-gc 2000)))
           (host-force-collect xs)
           (list (length xs) (car xs) (car (last xs))))",
    );
    assert_eq!(rt.repr(&loop_res[0]).unwrap(), "(2000 1999 0)");

    // Operands held in registers while GC runs inside cons allocation
    let nested = eval(
        rt,
        "(let ((a (cons 1 2))
               (b (cons 3 4)))
           (dotimes (i 5000)
             (setq a (cons i (cdr a))))
           (host-force-collect a b)
           (cons (car a) (car b)))",
    );
    assert_eq!(rt.repr(&nested[0]).unwrap(), "(4999 . 3)");

    // (setf (fdefinition 'cons) ...) fallback / protection safety:
    // CL:CONS cannot be redefined, and shadowed package CONS uses normal dynamic dispatch.
    let err = eval_error(rt, "(setf (fdefinition 'cons) (lambda (a b) (list a b)))");
    assert_eq!(
        err.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );
    assert_eq!(rt.repr(&eval(rt, "(cons-pair 5 6)")[0]).unwrap(), "(5 . 6)");

    rt.load_str(
        "(defpackage :cons-shadow-test
           (:use :cl)
           (:shadow #:cons))
         (in-package :cons-shadow-test)
         (defun cons (a b) (cl:list b a))
         (defun call-shadow (x y) (cons x y))
         (in-package :cl-user)",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(rt, "(cons-shadow-test::call-shadow 1 2)")[0])
            .unwrap(),
        "(2 1)"
    );
    rt.load_str(
        "(in-package :cons-shadow-test)
         (setf (fdefinition 'cons) (lambda (a b) (cl:list :redef a b)))
         (in-package :cl-user)",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(rt, "(cons-shadow-test::call-shadow 1 2)")[0])
            .unwrap(),
        "(:REDEF 1 2)"
    );
}

fn check_rooting_of_captured_mutable_bindings(rt: &Runtime) {
    rt.load_str(
        "(defun make-counter-and-peek (init)
           (let ((count init)
                 (frozen (cons :frozen init))
                 (cell (cons :head init)))
             (list
               ;; CaptureKind::MutableCell read/write + CaptureKind::Value read
               (lambda (delta)
                 (setq count (+ count delta))
                 (setq cell (cons (+ (cdr frozen) count) cell))
                 (when (= (mod count 5) 0)
                   (host-force-collect))
                 (list count (car cell) (car frozen)))
               ;; Nested closure reading/writing same MutableCell + outer ReadBoxLocal
               (lambda ()
                 (let ((inner (lambda (v) (setq count v))))
                   (funcall inner (+ count 10))
                   (list count (cdr frozen))))
               ;; Outer local read/write of boxed variable
               (lambda () count))))",
    )
    .unwrap();

    let res = eval(
        rt,
        "(let* ((fns (make-counter-and-peek 1))
                (step (first fns))
                (bump (second fns))
                (peek (third fns))
                (r1 (funcall step 4))
                (r2 (progn
                      (dotimes (i 8000) (cons i i))
                      (host-force-collect)
                      (funcall bump)))
                (r3 (funcall step 5))
                (r4 (funcall peek)))
           (list r1 r2 r3 r4))",
    );
    assert_eq!(
        rt.repr(&res[0]).unwrap(),
        "((5 6 :FROZEN) (15 1) (20 21 :FROZEN) 20)"
    );

    // Local boxed variable mutated and read inside the defining frame across GC
    let local_box = eval(
        rt,
        "(let ((x (cons 100 200)))
           (let ((reader (lambda () (car x)))
                 (writer (lambda (v) (setq x (cons v (cdr x))))))
             (host-force-collect)
             (funcall writer 300)
             (dotimes (i 5000) (cons i i))
             (host-force-collect)
             (setq x (cons (+ (car x) 1) (cdr x)))
             (list (funcall reader) (car x) (cdr x))))",
    );
    assert_eq!(rt.repr(&local_box[0]).unwrap(), "(301 301 200)");
}

fn check_steady_state_root_registrations(rt: &Runtime) {
    rt.load_str(
        "(defun steady-mv (x) (values x (+ x 1) (+ x 2)))
         (defun steady-uwp-cons-box (n)
           (let ((acc nil)
                 (sum 0))
             (let ((step (lambda (i)
                           (unwind-protect
                               (multiple-value-call
                                   (lambda (a b c)
                                     (setq sum (+ sum a b c))
                                     (setq acc (cons i acc)))
                                 (steady-mv i))
                             (values i)))))
               (dotimes (i n (list sum (car acc)))
                 (funcall step i)))))",
    )
    .unwrap();

    // Warm up IC sites once.
    assert_eq!(
        rt.repr(&eval(rt, "(steady-uwp-cons-box 2)")[0]).unwrap(),
        "(9 1)"
    );

    let fn_root = eval(rt, "#'steady-uwp-cons-box").pop().unwrap();
    let arg_small = rt.fixnum(200).unwrap();

    #[cfg(feature = "vm-metrics")]
    zkcl::metrics::reset();

    let res = rt.call(&fn_root, &[&arg_small]).unwrap();
    assert_eq!(rt.repr(&res[0]).unwrap(), "(60300 199)");

    #[cfg(feature = "vm-metrics")]
    {
        let arg_large = rt.fixnum(2000).unwrap();
        let m_small = zkcl::metrics::snapshot();
        assert_eq!(
            m_small.root_frames, 0,
            "unexpected heap RootFrame churn in steady state: {m_small:?}"
        );
        assert!(
            m_small.root_registrations <= 1,
            "expected only the 1 returned host Root registration: {m_small:?}"
        );
        assert_eq!(
            m_small.slow_paths, 0,
            "unexpected slow_paths in steady state (ConsFast/ReadBox/WriteBox): {m_small:?}"
        );

        zkcl::metrics::reset();
        let res_large = rt.call(&fn_root, &[&arg_large]).unwrap();
        assert_eq!(rt.repr(&res_large[0]).unwrap(), "(6003000 1999)");
        let m_large = zkcl::metrics::snapshot();
        assert_eq!(m_large.root_frames, 0);
        assert_eq!(
            m_large.root_registrations, m_small.root_registrations,
            "root_registrations must stay O(1) across 10x iterations: small={m_small:?} large={m_large:?}"
        );
        assert_eq!(m_large.slow_paths, 0);
    }
}
