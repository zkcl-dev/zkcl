use std::fs;

use zkcl::{ConditionClass, EvalErrorKind, ExecutionStop, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::{eval, eval_error};

fn runtime_with_collection_callback() -> Runtime {
    let rt = Runtime::start().expect("runtime start");
    rt.register_function("HOST-FORCE-COLLECT", |runtime, args| {
        runtime.collect();
        Ok(args.to_vec())
    })
    .unwrap();
    rt
}

#[test]
fn named_call_cache_is_invalidated_by_redefinition() {
    let rt = Runtime::start().expect("runtime start");
    // 1. CallNamed2 IC invalidation on redefinition, wrong arity ProgramError, and saved closure identity.
    rt.load_str(
        "(defun ic-step (a b) (+ a b))
         (defun ic-caller (n)
           (let ((acc 0))
             (dotimes (i n acc)
               (setq acc (ic-step acc i)))))",
    )
    .unwrap();
    assert_eq!(eval(&rt, "(ic-caller 10)")[0].as_fixnum(), Some(45));
    let saved = eval(&rt, "#'ic-step").pop().unwrap();
    rt.load_str("(defun ic-step (a b) (+ a (* b 2)))").unwrap();
    assert_eq!(eval(&rt, "(ic-caller 10)")[0].as_fixnum(), Some(90));
    rt.collect();
    assert_eq!(
        rt.call(&saved, &[&rt.fixnum(3).unwrap(), &rt.fixnum(4).unwrap()])
            .unwrap()[0]
            .as_fixnum(),
        Some(7)
    );
    rt.load_str("(defun ic-step (a) a)").unwrap();
    let arity_err = eval_error(&rt, "(ic-caller 2)");
    assert_eq!(
        arity_err.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );
}

#[test]
fn fasl_notinline_calls_use_the_current_definition() {
    let rt = Runtime::start().expect("runtime start");
    // 2. NOTINLINE preservation across source-deleted FASL reload.
    let files = TempDir::new("vm-specialization");
    let lisp_path = files.path().join("notinline_mod.lisp");
    let fasl_path = files.path().join("notinline_mod.zkclfasl");
    fs::write(
        &lisp_path,
        "(declaim (notinline fasl-target))
         (defun fasl-target (x) (+ x 1))
         (defun fasl-caller (x)
           (declare (notinline fasl-target))
           (fasl-target x))
         (defun fasl-cmp-notinline (a b)
           (declare (notinline <))
           (if (< a b) :less :not-less))",
    )
    .unwrap();
    rt.compile_file(&lisp_path, Some(&fasl_path)).unwrap();
    fs::remove_file(&lisp_path).unwrap();
    rt.load_file(&fasl_path).unwrap();
    assert_eq!(eval(&rt, "(fasl-caller 10)")[0].as_fixnum(), Some(11));
    rt.load_str("(defun fasl-target (x) (+ x 100))").unwrap();
    assert_eq!(eval(&rt, "(fasl-caller 10)")[0].as_fixnum(), Some(110));
    assert_eq!(
        rt.repr(&eval(&rt, "(fasl-cmp-notinline 1 2)")[0]).unwrap(),
        ":LESS"
    );
}

#[test]
fn comparison_fusion_preserves_the_predicate_result() {
    let rt = runtime_with_collection_callback();
    // 3. CmpBranch fusion soundness & negative cases (multi-use, Complex =, TypeError order, flet, loop+GC).
    assert_eq!(
        rt.repr(&eval(&rt, "(let ((b (< 3 10))) (if b (list b 1) (list b 2)))")[0])
            .unwrap(),
        "(T 1)"
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(let ((b (< 10 3))) (if b (list b 1) (list b 2)))")[0])
            .unwrap(),
        "(NIL 2)"
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(if (= 1 #c(1 0)) :eq :ne)")[0])
            .unwrap(),
        ":EQ"
    );
    let cmp_type_err = eval_error(&rt, "(if (< 2 1 'bad) :yes :no)");
    assert_eq!(
        cmp_type_err.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(flet ((< (a b) (> a b))) (if (< 1 2) :lt :gt))")[0])
            .unwrap(),
        ":GT"
    );
    assert_eq!(
        eval(
            &rt,
            "(let ((i 0) (s 0))
               (tagbody
                 again
                   (if (= i 100) (go done))
                   (if (= i 50) (host-force-collect nil))
                   (setq s (+ s i))
                   (setq i (+ i 1))
                   (go again)
                 done)
               s)"
        )[0]
        .as_fixnum(),
        Some(4950)
    );
}

#[test]
fn guarded_addition_retries_after_fixnum_overflow() {
    let rt = runtime_with_collection_callback();
    // 4. ArithFixnumGuarded 61-bit overflow with out == lhs, mixed fixnum+float IC, GC relocation, TypeError.
    let overflow = eval(
        &rt,
        "(let ((x most-positive-fixnum))
           (setq x (+ x 1))
           (host-force-collect x)
           (+ x 1))",
    )
    .pop()
    .unwrap();
    assert_eq!(
        rt.integer(&overflow).unwrap(),
        rt.integer(&eval(&rt, "(+ most-positive-fixnum 2)").pop().unwrap())
            .unwrap()
    );
}

#[test]
fn specialized_arithmetic_accepts_floats_after_fixnums() {
    let rt = Runtime::start().expect("runtime start");
    rt.load_str("(defun mix-add (a b) (+ a b))").unwrap();
    assert_eq!(eval(&rt, "(mix-add 1 2)")[0].as_fixnum(), Some(3));
    assert_eq!(eval(&rt, "(mix-add 3 4)")[0].as_fixnum(), Some(7));
    assert_eq!(rt.repr(&eval(&rt, "(mix-add 1 2.5)")[0]).unwrap(), "3.5");
    assert_eq!(eval(&rt, "(mix-add 10 20)")[0].as_fixnum(), Some(30));
    assert_eq!(
        eval_error(&rt, "(mix-add 1 'bad)").kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
}

#[test]
fn tail_recursion_handles_deep_call_chains() {
    let rt = Runtime::start().expect("runtime start");
    rt.load_str(
        "(defun tail-sum (n acc)
           (if (= n 0) acc (tail-sum (- n 1) (+ acc n))))
         (defun my-even-p (n)
           (if (= n 0) t (my-odd-p (- n 1))))
         (defun my-odd-p (n)
           (if (= n 0) nil (my-even-p (- n 1))))",
    )
    .unwrap();
    assert_eq!(
        rt.integer(&eval(&rt, "(tail-sum 200000 0)")[0])
            .unwrap()
            .to_string(),
        "20000100000"
    );
    assert_eq!(rt.repr(&eval(&rt, "(my-even-p 100000)")[0]).unwrap(), "T");
    assert_eq!(rt.repr(&eval(&rt, "(my-odd-p 100000)")[0]).unwrap(), "NIL");

    assert_eq!(
        eval(
            &rt,
            "(labels ((walk (i acc)
                       (if (= i 0) acc (walk (- i 1) (+ acc 2)))))
               (walk 150000 0))"
        )[0]
        .as_fixnum(),
        Some(300000)
    );
}

#[test]
fn tail_calls_from_fasl_switch_closures_and_forward_multiple_values() {
    let rt = runtime_with_collection_callback();
    let files = TempDir::new("tail-calls");
    let source = files.write(
        "tail-calls.lisp",
        "(defun tail-make-hop (tag)
           (lambda (n self other a b)
             (if (= n 0)
                 (values tag a b)
                 (funcall other (1- n) other self b
                          (if (= n 31) (host-force-collect a) a)))))
         (defun tail-invoke-values (n)
           (labels ((walk (n left right)
                      (if (= n 0)
                          (values left right)
                          (multiple-value-call #'walk
                            (values (1- n) right) (values left)))))
             (walk n :left :right)))
         (defun tail-optional (n &optional (value (list n) supplied-p))
           (if (= n 0) (values value supplied-p) (tail-optional (1- n))))",
    );
    let compiled = rt.compile_file(&source, None).unwrap();
    fs::remove_file(source).unwrap();
    rt.load_file(&compiled).unwrap();

    for (source, expected) in [
        (
            "(let ((a (tail-make-hop :a)) (b (tail-make-hop :b)))
            (funcall a 50001 a b (list 1) (list 2)))",
            [":B", "(2)", "(1)"].as_slice(),
        ),
        ("(tail-invoke-values 50001)", &[":RIGHT", ":LEFT"]),
        ("(tail-optional 50000 :original)", &["(0)", "NIL"]),
    ] {
        let actual: Vec<_> = eval(&rt, source)
            .iter()
            .map(|value| rt.repr(value).unwrap())
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn tail_frame_replacement_preserves_unwind_and_checks_the_execution_budget() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defun tail-loop () (tail-loop))
         (defun tail-throw () (throw 'tag (values :escaped 42)))
         (defun tail-catch () (catch 'tag (tail-throw)))",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(&rt, "(multiple-value-list (tail-catch))")[0])
            .unwrap(),
        "(:ESCAPED 42)"
    );
    let result = eval(
        &rt,
        "(flet ((escape ()
                  (block done
                    (multiple-value-call #'list
                      (values :discarded)
                      (return-from done (values :escaped 42))))))
           (multiple-value-list (escape)))",
    );
    assert_eq!(rt.repr(&result[0]).unwrap(), "(:ESCAPED 42)");
    let function = eval(&rt, "#'tail-loop").pop().unwrap();
    rt.set_step_budget(Some(1000));
    let error = rt.call(&function, &[]).err().unwrap();
    assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));
    rt.set_step_budget(None);
}

#[test]
fn notinline_tail_calls_see_midflight_redefinitions() {
    let rt = Runtime::start().expect("runtime start");
    // Dynamic redefinition of NOTINLINE tail callee at runtime.
    rt.load_str(
        "(declaim (notinline tail-hop))
         (defun tail-hop (n)
           (declare (notinline tail-hop))
           (if (= n 5)
               (progn
                 (defun tail-hop (k) (+ k 1000))
                 (tail-hop n))
               (if (= n 0)
                   0
                   (tail-hop (- n 1)))))",
    )
    .unwrap();
    assert_eq!(eval(&rt, "(tail-hop 10)")[0].as_fixnum(), Some(1005));
}

#[test]
fn cleanup_retains_the_dynamic_binding_in_tail_position() {
    let rt = Runtime::start().expect("runtime start");
    // UNWIND-PROTECT and dynamic special bindings MUST survive around tail-position forms.
    rt.load_str(
        "(defvar *tail-dyn* :outer)
         (defvar *tail-log* nil)
         (defun dyn-leaf () *tail-dyn*)
         (defun tail-with-protect ()
           (let ((*tail-dyn* :inner))
             (unwind-protect
                 (dyn-leaf)
               (setq *tail-log* (cons *tail-dyn* *tail-log*)))))",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(&rt, "(tail-with-protect)")[0]).unwrap(),
        ":INNER"
    );
    assert_eq!(rt.repr(&eval(&rt, "*tail-dyn*")[0]).unwrap(), ":OUTER");
    assert_eq!(rt.repr(&eval(&rt, "*tail-log*")[0]).unwrap(), "(:INNER)");
}

#[test]
fn tail_calls_forward_all_return_values() {
    let rt = Runtime::start().expect("runtime start");
    // Multiple values across tail calls.
    rt.load_str(
        "(defun mv-leaf () (values 11 22 33))
         (defun mv-proxy () (mv-leaf))",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(&rt, "(multiple-value-list (mv-proxy))")[0])
            .unwrap(),
        "(11 22 33)"
    );
}

#[test]
fn inlining_renames_shadowed_bindings() {
    let rt = Runtime::start().expect("runtime start");
    // 1. Alpha-renamed FLET / LABELS / immediate LAMBDA inlining, shadowing, block collision, side-effect order, NOTINLINE.
    rt.load_str(
        "(defvar *order* nil)
         (defun push-log (v)
           (setq *order* (cons v *order*))
           v)
         (defun inline-alpha-test (x y)
           (flet ((step (x)
                    (block step
                      (if (= x 0)
                          (return-from step 999))
                      (+ x y))))
             (labels ((step-and-add (a b) (+ (step a) b)))
               (list (block step
                       (+ x (step-and-add (push-log 10)
                                    ((lambda (y)
                                       (push-log 20)
                                       (+ y 1))
                                     5))))
                     (step 0)
                     (locally (declare (notinline step))
                       (step 3))))))",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(&rt, "(inline-alpha-test 1 100)")[0]).unwrap(),
        "(117 999 103)"
    );
    assert_eq!(rt.repr(&eval(&rt, "*order*")[0]).unwrap(), "(20 10)");
}

#[test]
fn notinline_preserves_local_call_semantics() {
    let rt = Runtime::start().expect("runtime start");
    // Constant folding respect for NOTINLINE.
    rt.load_str(
        "(defun fold-check ()
           (list (+ 2 3 4)
                 (if (null nil) :yes :no)
                 (locally (declare (notinline +)) (+ 2 3))))",
    )
    .unwrap();
    assert_eq!(
        rt.repr(&eval(&rt, "(fold-check)")[0]).unwrap(),
        "(9 :YES 5)"
    );
}

#[test]
fn notinline_overrides_an_inline_proclamation() {
    let rt = Runtime::start().expect("runtime start");
    // 2. Explicit DECLAIM INLINE + DECLARE INLINE across eval and source-deleted FASL, vs default dynamic DEFUN.
    rt.load_str(
        "(declaim (inline inl-add))
         (defun inl-add (x) (+ x 10))
         (defun uses-proclaimed-inline (x) (inl-add x))
         (defun uses-notinline-override (x)
           (declare (notinline inl-add))
           (inl-add x))
         (defun dyn-target (x) (+ x 1))
         (defun uses-default-dynamic (x) (dyn-target x))
         (defun uses-declare-inline (x)
           (declare (inline dyn-target))
           (dyn-target x))",
    )
    .unwrap();
    assert_eq!(
        eval(&rt, "(uses-proclaimed-inline 5)")[0].as_fixnum(),
        Some(15)
    );
    assert_eq!(eval(&rt, "(uses-declare-inline 5)")[0].as_fixnum(), Some(6));
    rt.load_str(
        "(defun inl-add (x) (+ x 1000))
         (defun dyn-target (x) (+ x 50))",
    )
    .unwrap();
    assert_eq!(
        eval(&rt, "(uses-notinline-override 5)")[0].as_fixnum(),
        Some(1005)
    );
    assert_eq!(
        eval(&rt, "(uses-default-dynamic 5)")[0].as_fixnum(),
        Some(55)
    );

    let files = TempDir::new("vm-specialization");
    let lisp_path = files.path().join("inline_mod.lisp");
    let fasl_path = files.path().join("inline_mod.zkclfasl");
    fs::write(
        &lisp_path,
        "(declaim (inline fasl-inl))
         (defun fasl-inl (x) (* x 3))
         (defun fasl-inl-caller (y) (fasl-inl (+ y 1)))",
    )
    .unwrap();
    rt.compile_file(&lisp_path, Some(&fasl_path)).unwrap();
    fs::remove_file(&lisp_path).unwrap();
    rt.load_file(&fasl_path).unwrap();
    assert_eq!(eval(&rt, "(fasl-inl-caller 4)")[0].as_fixnum(), Some(15));
}

#[test]
fn replacing_fdefinition_invalidates_the_inline_definition() {
    let rt = Runtime::start().expect("runtime start");
    // (setf (fdefinition ...)) clears stale inline definition.
    rt.load_str(
        "(declaim (inline fdef-target))
         (defun fdef-target (x) (+ x 2))
         (setf (fdefinition 'fdef-target) (lambda (x) (+ x 200)))
         (defun fdef-caller (x)
           (declare (inline fdef-target))
           (fdef-target x))",
    )
    .unwrap();
    assert_eq!(eval(&rt, "(fdef-caller 3)")[0].as_fixnum(), Some(203));
}

#[test]
fn nested_closures_keep_captured_values_after_collection() {
    let rt = runtime_with_collection_callback();
    // 3. Immutable CaptureKind::Value vs MutableCell + multi-level nested closures + GC relocation.
    rt.load_str(
        "(defun make-multi-adder (a pair)
           (let ((b (+ a 10)))
             (lambda (c)
               (lambda ()
                 (host-force-collect (cons (+ a b c) pair))))))
         (defun make-counter (start)
           (let ((n start))
             (values (lambda () n)
                     (lambda (d) (setq n (+ n d))))))
         (defun flet-capture-adder (a)
           (flet ((mk (b) (lambda (c) (+ a b c))))
             (mk 20)))",
    )
    .unwrap();
    let nested = eval(&rt, "(funcall (make-multi-adder 5 (list 1 2)) 7)")
        .pop()
        .unwrap();
    rt.collect();
    assert_eq!(
        rt.repr(&rt.call(&nested, &[]).unwrap().pop().unwrap())
            .unwrap(),
        "(27 1 2)"
    );
    assert_eq!(
        eval(&rt, "(funcall (flet-capture-adder 3) 4)")[0].as_fixnum(),
        Some(27)
    );
    let counter = eval(&rt, "(make-counter 10)");
    assert_eq!(rt.call(&counter[0], &[]).unwrap()[0].as_fixnum(), Some(10));
    assert_eq!(
        rt.call(&counter[1], &[&rt.fixnum(32).unwrap()]).unwrap()[0].as_fixnum(),
        Some(42)
    );
    rt.collect();
    assert_eq!(rt.call(&counter[0], &[]).unwrap()[0].as_fixnum(), Some(42));
}

#[test]
fn local_control_transfers_run_crossed_cleanup() {
    let rt = Runtime::start().expect("runtime start");
    // 4. Local non-escaping BLOCK / TAGBODY jumps, dynamic unwind, and escaping-block crossing guard.
    rt.load_str(
        "(defvar *blk-log* nil)
         (defun local-block-and-tagbody (n)
           (let ((acc 0) (i 0))
             (tagbody
               loop
                 (when (>= i n) (go done))
                 (let ((v (block step
                            (if (= i 3) (return-from step 100))
                            i)))
                   (setq acc (+ acc v)))
                 (setq i (+ i 1))
                 (go loop)
               done)
             acc))
         (defun escaping-crossed-by-outer-block ()
           (setq *blk-log* nil)
           (list
             (block outer
               (block inner
                 (let ((escape (lambda () (return-from inner :never))))
                   (declare (ignore escape))
                   (unwind-protect
                       (return-from outer :escaped)
                     (setq *blk-log* (cons :cleaned *blk-log*))))))
             *blk-log*))",
    )
    .unwrap();
    assert_eq!(
        eval(&rt, "(local-block-and-tagbody 6)")[0].as_fixnum(),
        Some(1 + 2 + 100 + 4 + 5)
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(escaping-crossed-by-outer-block)")[0])
            .unwrap(),
        "(:ESCAPED (:CLEANED))"
    );
}

#[test]
fn immediate_arithmetic_promotes_out_of_range_fixnums() {
    let rt = runtime_with_collection_callback();
    // 5. Specialized instructions: AddFixnumImm, SubFixnumImm, CarFast, CdrFast, NullBranch (loop+GC, TypeError, overflow).
    rt.load_str(
        "(defun imm-add1 (x) (+ x 1))
         (defun imm-add-neg (x) (+ x -5))
         (defun imm-sub-imm (x) (- x 7))
         (defun fast-car-cdr (p) (cons (car p) (cdr p)))
         (defun fast-null-check (x) (if (null x) :empty :non-empty))
         (defun fast-not-check (x) (if (not x) :falsy :truthy))
         (defun multi-null-check (x)
           (let ((n (null x)))
             (if n (list n 1) (list n 2))))
         (defun gc-car-loop (xs)
           (let ((sum 0) (cur xs))
             (tagbody
               again
                 (if (null cur) (go done))
                 (host-force-collect cur)
                 (setq sum (+ sum (car cur)))
                 (setq cur (cdr cur))
                 (go again)
               done)
             sum))",
    )
    .unwrap();
    assert_eq!(eval(&rt, "(imm-add1 41)")[0].as_fixnum(), Some(42));
    assert_eq!(eval(&rt, "(imm-add-neg 2)")[0].as_fixnum(), Some(-3));
    assert_eq!(eval(&rt, "(imm-sub-imm 10)")[0].as_fixnum(), Some(3));
    let min_over = eval(&rt, "(imm-sub-imm most-negative-fixnum)")
        .pop()
        .unwrap();
    assert_eq!(
        rt.integer(&min_over).unwrap(),
        rt.integer(&eval(&rt, "(- most-negative-fixnum 7)").pop().unwrap())
            .unwrap()
    );
    let max_over = eval(&rt, "(imm-add1 most-positive-fixnum)").pop().unwrap();
    assert_eq!(
        rt.integer(&max_over).unwrap(),
        rt.integer(&eval(&rt, "(+ most-positive-fixnum 1)").pop().unwrap())
            .unwrap()
    );
    assert_eq!(
        eval_error(&rt, "(imm-add1 'sym)").kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert_eq!(
        eval_error(&rt, "(imm-sub-imm 'sym)").kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );

    assert_eq!(
        rt.repr(&eval(&rt, "(fast-car-cdr nil)")[0]).unwrap(),
        "(NIL)"
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(fast-car-cdr (cons 1 2))")[0]).unwrap(),
        "(1 . 2)"
    );
    assert_eq!(
        eval_error(&rt, "(car 123)").kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert_eq!(
        eval_error(&rt, "(cdr 123)").kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(list (fast-null-check nil) (fast-null-check 1) (fast-not-check nil) (fast-not-check 0))")[0]).unwrap(),
        "(:EMPTY :NON-EMPTY :FALSY :TRUTHY)"
    );
    assert_eq!(
        rt.repr(&eval(&rt, "(list (multi-null-check nil) (multi-null-check 1))")[0])
            .unwrap(),
        "((T 1) (NIL 2))"
    );
    assert_eq!(
        eval(&rt, "(gc-car-loop '(10 20 30 40))")[0].as_fixnum(),
        Some(100)
    );
}

#[test]
fn trace_invalidates_specialized_vm_call_sites() {
    let rt = Runtime::start().unwrap();
    rt.register_function("COLLECT-DURING-TRACE", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    })
    .unwrap();
    rt.load_str(
        "(defun vm-trace-probe (a b)
           (let* ((*trace-output* (make-string-output-stream))
                  (pair (cons a b)))
             (collect-during-trace)
             (values
               (list (+ a b) (+ 7 a) (- a 3) (* a 7)
                     (car pair) (cdr pair) (rest pair)
                     (if (null a) :empty :nonempty)
                     (if (not b) :false :true)
                     (if (< a b) :less :not-less))
               (get-output-stream-string *trace-output*))))",
    )
    .unwrap();
    let probe = eval(&rt, "#'vm-trace-probe").pop().unwrap();
    let a = rt.fixnum(12).unwrap();
    let b = rt.fixnum(20).unwrap();
    let call = || {
        let values = rt.call(&probe, &[&a, &b]).unwrap();
        assert_eq!(
            rt.repr(&values[0]).unwrap(),
            "(32 19 9 84 12 20 20 :NONEMPTY :TRUE :LESS)"
        );
        rt.string(&values[1]).unwrap()
    };
    assert!(call().is_empty());
    assert!(call().is_empty());
    let operations = [
        ("+", 2),
        ("-", 1),
        ("*", 1),
        ("CONS", 1),
        ("CAR", 1),
        ("CDR", 1),
        ("REST", 1),
        ("NULL", 1),
        ("NOT", 1),
        ("<", 1),
    ];
    for (op, count) in operations {
        rt.load_str(&format!("(trace {op})")).unwrap();
        let trace = call();
        assert_eq!(
            trace.matches(&format!("TRACE {op}: enter")).count(),
            count,
            "{op}: {trace}"
        );
        assert_eq!(
            trace.matches(&format!("TRACE {op}: exit")).count(),
            count,
            "{op}: {trace}"
        );
        rt.load_str(&format!("(untrace {op})")).unwrap();
        assert!(call().is_empty());
    }
}
