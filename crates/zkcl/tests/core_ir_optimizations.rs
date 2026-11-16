use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::{eval_error, eval_reprs};

fn eval_one_fixnum(rt: &Runtime, src: &str) -> isize {
    let vals = rt
        .eval(src)
        .unwrap_or_else(|e| panic!("eval failed for `{src}`: {e:?}"));
    assert_eq!(vals.len(), 1, "expected 1 return value for `{src}`");
    vals[0].as_fixnum().unwrap_or_else(|| {
        panic!(
            "expected fixnum for `{src}`, got {}",
            rt.repr(&vals[0]).unwrap()
        )
    })
}

fn eval_one_repr(rt: &Runtime, src: &str) -> String {
    let reprs = eval_reprs(rt, src);
    assert_eq!(
        reprs.len(),
        1,
        "expected 1 return value for `{src}`, got {reprs:?}"
    );
    reprs.into_iter().next().unwrap()
}

fn assert_type_error(rt: &Runtime, src: &str) {
    let err = match rt.eval(src) {
        Err(err) => err,
        Ok(vals) => {
            let reprs: Vec<String> = vals
                .iter()
                .map(|v| rt.repr(v).unwrap_or_else(|_| "<repr-err>".into()))
                .collect();
            panic!("expected TYPE-ERROR for `{src}`, got Ok({reprs:?})");
        }
    };
    assert!(
        matches!(
            err.kind(),
            EvalErrorKind::LispCondition(ConditionClass::TypeError)
        ),
        "expected TYPE-ERROR for `{src}`, got {err:?}"
    );
}

#[test]
fn runtime_type_checks_follow_lexical_optimization_declarations() {
    let rt = Runtime::start().unwrap();
    // Basic THE and values checking at normal/high safety
    assert_eq!(eval_one_fixnum(&rt, "(the fixnum (+ 1 2))"), 3);
    assert_type_error(&rt, "(the fixnum \"bad\")");
    assert_type_error(
        &rt,
        "(locally (declare (optimize (safety 3))) (the fixnum \"bad\"))",
    );

    assert_eq!(
        eval_reprs(&rt, "(the (values fixnum symbol) (values 10 'ok))"),
        vec!["10", "OK"]
    );
    assert_eq!(
        eval_reprs(
            &rt,
            "(locally (declare (optimize (safety 3))) (the (values fixnum) (values 41 42)))"
        ),
        vec!["41", "42"]
    );
    assert_eq!(
        eval_reprs(
            &rt,
            "(the (values fixnum symbol &optional string) (values 1 'a))"
        ),
        vec!["1", "A"]
    );
    assert_eq!(
        eval_reprs(&rt, "(the (values fixnum &rest symbol) (values 1 'a 'b))"),
        vec!["1", "A", "B"]
    );
    assert_type_error(&rt, "(the (values fixnum &rest symbol) (values 1 'a 2))");
    assert_type_error(&rt, "(the (values fixnum symbol) (values 1 \"not-sym\"))");

    // Parameter TYPE declaration & atomic type shorthand
    rt.load_str(
        "(defun opt-param-typed (x)
           (declare (optimize (safety 3)) (type fixnum x))
           (1+ x))
         (defun opt-param-shorthand (x)
           (declare (fixnum x))
           (+ x 10))",
    )
    .unwrap();
    assert_eq!(eval_one_fixnum(&rt, "(opt-param-typed 5)"), 6);
    assert_type_error(&rt, "(opt-param-typed :bad)");
    assert_eq!(eval_one_fixnum(&rt, "(opt-param-shorthand 7)"), 17);
    assert_type_error(&rt, "(opt-param-shorthand \"bad\")");

    // LET / LET* TYPE checks and SETQ enforcement
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(let ((x 10) (y 20))
               (declare (type fixnum x y))
               (setq x (+ x y))
               x)"
        ),
        30
    );
    assert_type_error(
        &rt,
        "(let ((x \"bad\"))
           (declare (type fixnum x))
           x)",
    );
    assert_type_error(
        &rt,
        "(let ((x 10))
           (declare (type fixnum x))
           (setq x \"bad\")
           x)",
    );
    assert_type_error(
        &rt,
        "(let* ((x 1) (y (+ x 2)))
           (declare (type fixnum x y))
           (setq y :bad)
           y)",
    );

    // Free TYPE declaration on variable read & SETQ
    assert_type_error(
        &rt,
        "(let ((x \"bad\"))
           (locally (declare (type fixnum x))
             x))",
    );
    assert_type_error(
        &rt,
        "(let ((x 1))
           (locally (declare (type fixnum x))
             (setq x \"bad\"))
           x)",
    );

    // Shadowed lexical variable must NOT inherit outer TYPE declaration
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((x 10))
               (declare (type fixnum x))
               (let ((x \"inner-ok\"))
                 x))"
        ),
        "\"inner-ok\""
    );

    // DEFTYPE compound types in THE and DECLARE TYPE
    rt.load_str("(deftype opt-pos-small () '(integer 1 100))")
        .unwrap();
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(let ((x 42))
               (declare (type opt-pos-small x))
               (the opt-pos-small x))"
        ),
        42
    );
    assert_type_error(&rt, "(the opt-pos-small 0)");
    assert_type_error(
        &rt,
        "(let ((x 101))
           (declare (type opt-pos-small x))
           x)",
    );

    // (optimize (speed 3) (safety 0)) trusts hints and skips runtime THE checks
    rt.load_str(
        "(defun opt-fast-pred (x)
           (declare (optimize (speed 3) (safety 0)) (type fixnum x))
           (if (integerp x) (+ x 1) :dead))",
    )
    .unwrap();
    assert_eq!(eval_one_fixnum(&rt, "(opt-fast-pred 41)"), 42);
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((v \"unsafe\"))
               (locally (declare (optimize (speed 3) (safety 0)))
                 (the fixnum v)))"
        ),
        "\"unsafe\""
    );

    // NOTINLINE prevents folding/inlining while preserving semantics
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(let ((x 10))
               (declare (notinline +))
               (+ x 20))"
        ),
        30
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(locally (declare (notinline integerp +))
               (list (integerp 5) (+ 1 2)))"
        ),
        "(T 3)"
    );

    // DECLAIM + COMPILE-FILE + LOAD_FILE
    let directory = TempDir::new("core-ir-optimizations");
    let src_path = directory.path().join("declaim.lisp");
    std::fs::write(
        &src_path,
        "(declaim (optimize (speed 3) (safety 1)))
         (declaim (ftype (function (fixnum) fixnum) add-ten))
         (defun add-ten (x)
           (declare (type fixnum x))
           (+ x 10))
         (defun cf-opt-caller (x)
           (add-ten x))
         (declaim (optimize (speed 1) (safety 1)))",
    )
    .unwrap();
    let fasl_path = rt.compile_file(&src_path, None).unwrap();
    rt.load_file(&fasl_path).unwrap();
    drop(directory);

    assert_eq!(eval_one_fixnum(&rt, "(cf-opt-caller 32)"), 42);
    assert_type_error(&rt, "(add-ten :bad)");
}

#[test]
fn dead_code_elimination_keeps_side_effects_in_order() {
    let rt = Runtime::start().unwrap();
    // Unused pure bindings drop while impure initforms execute in exact left-to-right order
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((log nil))
               (let ((a (+ 1 2))
                     (b (progn (push :first log) 10))
                     (c (* 3 4))
                     (d (progn (push :second log) 20)))
                 (declare (ignorable a b c d))
                 (nreverse log)))"
        ),
        "(:FIRST :SECOND)"
    );

    // Dead pure void subexpressions drop while impure ones execute in order
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((log nil))
               (progn
                 (+ 1 2)
                 (integerp 5)
                 (cons 1 2)
                 (list 1 2 3)
                 (list* 1 2 '(3))
                 (push :kept log)
                 (= 1 2)
                 (eql 'a 'b)
                 (push :also-kept log)
                 (nreverse log)))"
        ),
        "(:KEPT :ALSO-KEPT)"
    );
}

#[test]
fn constant_propagation_preserves_pure_results() {
    let rt = Runtime::start().unwrap();

    // Multi-round constant & copy propagation + pure fixnum & predicate folding
    rt.load_str(
        "(defun opt-const-fold ()
           (let* ((a 10)
                  (b a)
                  (c (+ b (abs -5) (1+ 2) (min 9 4 7) (ash 1 3))))
             (list c
                   (evenp c)
                   (plusp c)
                   (= c 30)
                   (zerop (- c 30))
                   (mod c 7)
                   (rem -17 5)
                   (/= c 29)
                   (not (minusp c))
                   (identity (signum -9)))))",
    )
    .unwrap();
    assert_eq!(
        eval_one_repr(&rt, "(opt-const-fold)"),
        "(30 T T T T 2 -2 T T -1)"
    );

    // Copy propagation preserves literal identity without duplicating fresh objects
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let* ((s (copy-seq \"abc\"))
                    (alias s))
               (list (eq s alias) (equal alias \"abc\")))"
        ),
        "(T T)"
    );
}

#[test]
fn function_return_type_checks_follow_the_safety_declaration() {
    let rt = Runtime::start().unwrap();

    // SAFETY 0 trusts a declared return type. SAFETY 3 checks it before use.
    rt.load_str(
        "(declaim (ftype (function (fixnum) fixnum) ftype-add1))
         (defun ftype-add1 (x) (1+ x))
         (defun ftype-use-fast (x)
           (declare (optimize (speed 3) (safety 0)))
           (if (integerp (ftype-add1 x)) :proven-int :not-int))
         (declaim (ftype (function (t) fixnum) ftype-lie))
         (defun ftype-lie (x) x)
         (defun ftype-use-safe (x)
           (declare (optimize (safety 3)))
           (if (integerp (ftype-lie x)) :int :not-int))",
    )
    .unwrap();
    assert_eq!(eval_one_repr(&rt, "(ftype-use-fast 5)"), ":PROVEN-INT");
    assert_type_error(&rt, "(ftype-use-safe \"nope\")");
}

#[test]
fn ftype_checks_named_calls_in_the_declared_lexical_scope() {
    let rt = Runtime::start().unwrap();
    let temp = TempDir::new("ftype-calls");
    let source = temp.write(
        "ftype-calls.lisp",
        "(defun ftype-target (x &optional (y \"default\") &rest tail)
           (values x y tail))
         (defun ftype-caller (x y z)
           (declare (optimize (safety 3))
                    (notinline ftype-target)
                    (ftype (function (fixnum &optional string &rest symbol)
                                     (values fixnum string list)) ftype-target))
           (if y (ftype-target x y z) (ftype-target x)))
         (defun ftype-shadow (x)
           (declare (optimize (safety 3))
                    (ftype (function (fixnum) fixnum) ftype-target))
           (values (flet ((ftype-target (v) v))
                     (ftype-target x))
                   (macrolet ((ftype-target (v) (declare (ignore v)) :macro))
                     (ftype-target x))))
         (defun ftype-flet-definition ()
           (declare (optimize (safety 3))
                    (ftype (function (symbol) *) ftype-target))
           (flet ((ftype-target (v) (ftype-target :outer)))
             (declare (ftype (function (fixnum) *) ftype-target))
             (ftype-target 7)))
         (defun ftype-local (x)
           (declare (optimize (safety 3)))
           (flet ((ftype-target (v) v))
             (declare (ftype (function (fixnum) *) ftype-target))
             (ftype-target x)))
         (defun ftype-recursive (x)
           (declare (optimize (safety 3)))
           (labels ((recur (v) (if (eql v 0) (recur x) v)))
             (declare (ftype (function (fixnum) *) recur))
             (recur 0)))
         (defun ftype-wild-result (x)
           (declare (optimize (safety 3))
                    (ftype (function * (values t string)) ftype-target))
           (ftype-target :unchecked x))
         (defun ftype-keyword-target (&key x) x)
         (defun ftype-keyword-caller (key value)
           (declare (optimize (safety 3))
                    (ftype (function (&key (:x fixnum)) fixnum) ftype-keyword-target))
           (ftype-keyword-target key value))
         (defun ftype-protected (x)
           (declare (optimize (safety 3)))
           (flet ((local (x) x))
             (declare (ftype (function (fixnum) *) local))
             (unwind-protect (local x) (values))))
         (defun ftype-ordered (step)
           (declare (optimize (safety 3)))
           (flet ((target (x y) (funcall step :call) (values x y)))
             (declare (notinline target)
                      (ftype (function (fixnum fixnum) *) target))
             (target (funcall step :left) (funcall step :right))))
         (defun ftype-global (x) x)
         (declaim (ftype (function (fixnum) *) ftype-global))",
    );
    let compiled = rt.compile_file(&source, None).unwrap();
    rt.load_file(&compiled).unwrap();

    assert_eq!(
        eval_reprs(&rt, "(ftype-caller 2 nil nil)"),
        ["2", "\"default\"", "NIL"]
    );
    assert_eq!(
        eval_reprs(&rt, "(ftype-caller 2 \"present\" :tail)"),
        ["2", "\"present\"", "(:TAIL)"]
    );
    assert_type_error(&rt, "(ftype-caller :bad nil nil)");
    assert_type_error(&rt, "(ftype-caller 2 :bad :tail)");
    assert_type_error(&rt, "(ftype-caller 2 \"ok\" 3)");
    assert_eq!(
        eval_reprs(&rt, "(ftype-shadow :local)"),
        [":LOCAL", ":MACRO"]
    );
    assert_eq!(
        eval_reprs(&rt, "(ftype-flet-definition)"),
        [":OUTER", "\"default\"", "NIL"]
    );
    assert_type_error(&rt, "(ftype-local :bad)");
    assert_type_error(&rt, "(ftype-recursive :bad)");
    assert_eq!(eval_one_fixnum(&rt, "(ftype-recursive 1)"), 1);
    assert_eq!(
        eval_reprs(&rt, "(ftype-wild-result \"ok\")"),
        [":UNCHECKED", "\"ok\"", "NIL"]
    );
    assert_type_error(&rt, "(ftype-wild-result :bad)");
    assert_eq!(eval_one_repr(&rt, "(ftype-keyword-caller :x 7)"), "7");
    assert_type_error(&rt, "(ftype-protected :bad)");
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((trace nil))
               (let ((result (multiple-value-list
                                (ftype-ordered (lambda (step) (push step trace) 7)))))
                 (list result (reverse trace))))",
        ),
        "((7 7) (:LEFT :RIGHT :CALL))"
    );
    assert_eq!(
        eval_reprs(
            &rt,
            "(catch 'ftype-exit
               (ftype-ordered (lambda (step) (throw 'ftype-exit (values step :kept)))))",
        ),
        [":LEFT", ":KEPT"]
    );
    let source = "(locally (declare (optimize (safety 3))) (ftype-global :bad))";
    let error = eval_error(&rt, source);
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert_eq!(error.offset(), source.find(":bad").unwrap());
}

#[test]
fn constant_branch_elimination_keeps_effectful_tests() {
    let rt = Runtime::start().unwrap();
    // Untaken branches do not execute
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((hit nil))
               (if t :yes (setf hit :bad1))
               (if nil (setf hit :bad2) :no)
               (if 42 :num (setf hit :bad3))
               (let ((x 7))
                 (declare (type fixnum x))
                 (if x :typed-truthy (setf hit :bad4)))
               hit)"
        ),
        "NIL"
    );

    // Side-effecting test expression STILL runs even when its truthiness eliminates the false arm
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((log nil))
               (list
                 (if (progn (push :side log) 123)
                     :taken
                     (progn (push :untaken log) :dead))
                 (nreverse log)))"
        ),
        "(:TAKEN (:SIDE))"
    );
}

#[test]
fn optimized_expressions_preserve_values_in_multiple_value_contexts() {
    let rt = Runtime::start().unwrap();

    // Multiple values preserved in tail and non-tail positions across IF/LET/THE/FLET
    rt.load_str(
        "(defun mv-tail (flag)
           (if t
               (if flag (values 1 2 3) (values 4 5))
               (values 9 9)))
         (defun mv-inlined-let-the (x)
           (flet ((mv (n) (values n (+ n 1) (+ n 2))))
             (the (values fixnum fixnum fixnum)
                  (let ((y (+ x 1)))
                    (if 1 (mv y) :dead)))))",
    )
    .unwrap();
    assert_eq!(eval_reprs(&rt, "(mv-tail t)"), vec!["1", "2", "3"]);
    assert_eq!(eval_reprs(&rt, "(mv-tail nil)"), vec!["4", "5"]);
    assert_eq!(
        eval_reprs(&rt, "(mv-inlined-let-the 9)"),
        vec!["10", "11", "12"]
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(multiple-value-list
               (let ((x 10))
                 (declare (ignorable x))
                 (if nil :dead (values 7 8 9))))"
        ),
        "(7 8 9)"
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(multiple-value-bind (a b c)
                 (the (values fixnum fixnum fixnum)
                      (if 1 (values 10 20 30) :dead))
               (list a b c))"
        ),
        "(10 20 30)"
    );
    // Single-value truncation when secondary values must be discarded
    assert_eq!(eval_reprs(&rt, "(identity (values 11 22))"), vec!["11"]);
    assert_eq!(eval_reprs(&rt, "(values (mv-tail t))"), vec!["1"]);
    assert_eq!(
        eval_one_repr(&rt, "(multiple-value-bind (x) (the symbol (values)) x)"),
        "NIL"
    );
    assert_type_error(
        &rt,
        "(multiple-value-bind (x) (the (values fixnum fixnum) (values 1 :bad)) x)",
    );
}

#[test]
fn inlined_local_calls_preserve_lexical_bindings() {
    let rt = Runtime::start().unwrap();
    // Shadowed lexical variable in FLET parameter and inner LET
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((x 100))
               (flet ((add-x (x) (+ x 1)))
                 (list (add-x 5) x)))"
        ),
        "(6 100)"
    );
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(let ((x 10))
               (flet ((f (y) (+ x y)))
                 (let ((x 999))
                   (f 5))))"
        ),
        15
    );

    // SETQ on inlined parameter does not mutate caller's variable
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((a 10))
               (flet ((bump (a) (setq a (+ a 5)) a))
                 (list (bump a) a)))"
        ),
        "(15 10)"
    );

    // SETQ on captured outer variable inside inlined FLET mutates caller's binding
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((acc 0))
               (flet ((inc (d) (setq acc (+ acc d)) acc))
                 (list (inc 3) (inc 4) acc)))"
        ),
        "(3 7 7)"
    );

    // Nested escaping closure inside inlined FLET captures distinct call bindings
    assert_eq!(
        eval_one_repr(
            &rt,
            "(flet ((make-adder (k) (lambda (n) (+ n k))))
               (let ((f1 (make-adder 10))
                     (f2 (make-adder 20)))
                 (list (funcall f1 1) (funcall f2 1))))"
        ),
        "(11 21)"
    );

    // BLOCK/RETURN-FROM inside FLET and non-local RETURN-FROM to outer block
    assert_eq!(
        eval_one_repr(
            &rt,
            "(block done
               (flet ((inner (flag)
                        (when flag (return-from inner :inner-ret))
                        :fallthrough))
                 (list (inner t) (inner nil))))"
        ),
        "(:INNER-RET :FALLTHROUGH)"
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(block outer
               (flet ((bail (x)
                        (if (zerop x) (return-from outer :bailed) (+ x 1))))
                 (list (bail 5) (bail 0) :unreachable)))"
        ),
        ":BAILED"
    );

    // Non-recursive LABELS chain, tail-recursive LABELS, and dead recursive LABELS
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(labels ((a (x) (+ x 1))
                      (b (x) (* (a x) 2)))
               (b 4))"
        ),
        10
    );
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(labels ((factorial (n acc)
                        (if (<= n 1) acc (factorial (1- n) (* n acc)))))
               (factorial 6 1))"
        ),
        720
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(labels ((dead-rec (n)
                        (if (zerop n) 0 (dead-rec (1- n)))))
               :alive)"
        ),
        ":ALIVE"
    );

    // NOTINLINE stops local function inlining
    assert_eq!(
        eval_one_fixnum(
            &rt,
            "(let ((x 10))
               (flet ((f (y) (+ x y)))
                 (declare (notinline f))
                 (setq x 20)
                 (f 5)))"
        ),
        25
    );
}

#[test]
fn the_values_assertions_preserve_every_returned_value() {
    let rt = Runtime::start().unwrap();
    rt.register_function("the-test-gc", |rt, _| {
        rt.collect();
        Ok(vec![rt.t()])
    })
    .unwrap();
    let temp = TempDir::new("the-values");
    let source = temp.write(
        "the-values.lisp",
        "(defun the-test-p (value)
           (when (eq value :escape) (throw 'the-test (values :escaped :kept)))
           (and (the-test-gc) (consp value)))
         (defun the-test-primary (producer)
           (the symbol (funcall producer)))
         (defun the-test-fixed (producer)
           (the (values (satisfies the-test-p) symbol) (funcall producer)))
         (defun the-test-one (producer)
           (list (the (values t fixnum) (funcall producer))))
         (defun the-test-tail (producer)
           (the (values fixnum &optional string &rest symbol) (funcall producer)))",
    );
    let compiled = rt.compile_file(&source, None).unwrap();
    rt.load_file(&compiled).unwrap();

    assert!(eval_reprs(&rt, "(the-test-primary #'values)").is_empty());
    assert_eq!(
        eval_reprs(&rt, "(the-test-primary (lambda () (values nil)))"),
        ["NIL"]
    );
    assert_eq!(
        eval_reprs(&rt, "(the-test-primary (lambda () (values :ok 2 3)))"),
        [":OK", "2", "3"]
    );
    assert_eq!(
        eval_reprs(
            &rt,
            "(let ((calls 0))
               (multiple-value-bind (a b c)
                   (the-test-fixed (lambda () (incf calls) (values (list :a) :b (list :c))))
                 (values a b c calls)))"
        ),
        ["(:A)", ":B", "(:C)", "1"]
    );
    assert_eq!(
        eval_reprs(&rt, "(the-test-fixed (lambda () (values (list :only))))"),
        ["(:ONLY)"]
    );
    assert_eq!(
        eval_reprs(
            &rt,
            "(catch 'the-test (the-test-fixed (lambda () (values :escape :ignored))))"
        ),
        [":ESCAPED", ":KEPT"]
    );
    assert_eq!(
        eval_one_repr(&rt, "(the-test-one (lambda () (values :first 2 3)))"),
        "(:FIRST)"
    );
    assert_type_error(&rt, "(the-test-one (lambda () (values :only)))");
    assert_type_error(&rt, "(the-test-one (lambda () (values :first :bad)))");
    assert_eq!(
        eval_reprs(&rt, "(the-test-tail (lambda () (values 1)))"),
        ["1"]
    );
    assert_eq!(
        eval_reprs(&rt, "(the-test-tail (lambda () (values 1 \"ok\" :tail)))"),
        ["1", "\"ok\"", ":TAIL"]
    );
    assert_type_error(&rt, "(the-test-tail (lambda () (values 1 nil)))");
    assert_type_error(&rt, "(the-test-tail (lambda () (values 1 \"ok\" 2)))");
    assert_eq!(
        eval_reprs(&rt, "(the (values fixnum &optional) (values 1 :extra))"),
        ["1", ":EXTRA"]
    );
}

#[test]
fn nested_functions_reach_the_implicit_block_of_an_inlined_function() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(declaim (inline inline-exit-wrapper))
         (defun inline-exit-wrapper (x)
           (flet ((outer (y)
                    (flet ((inner () (return-from outer (values y (1+ y)))))
                      (declare (notinline inner))
                      (inner)
                      :fell-through)))
             (declare (notinline outer))
             (outer x)))
         (defun call-inline-exit-wrapper (x) (inline-exit-wrapper x))",
    )
    .unwrap();

    assert_eq!(
        eval_reprs(&rt, "(call-inline-exit-wrapper 42)"),
        ["42", "43"]
    );
}

#[test]
fn lexical_type_checks_follow_the_declared_binding() {
    let rt = Runtime::start().unwrap();
    for form in [
        "(let ((x 1))
           (declare (fixnum x))
           (locally (declare (number x)) (setq x 1.0))
           x)",
        "(let ((x 1))
           (declare (fixnum x) (number x))
           (setq x 1.0))",
        "(let ((x 1.0)) (locally (declare (fixnum x) (number x)) x))",
        "(let* ((x 1) (y (setq x :bad))) (declare (fixnum x)) (list x y))",
        "(let ((x 1)) (declare (special x) (fixnum x)) (set 'x :bad) x)",
        "(multiple-value-bind (x y) (values 1 :bad)
           (declare (fixnum x y))
           (list x y))",
        "(let ((x 1))
           (declare (fixnum x))
           (multiple-value-bind (y) (values :bad) (setq x y)))",
        "(let ((x 1))
           (declare (special x) (fixnum x))
           (funcall (lambda ()
             (multiple-value-bind (y) (values :bad) (setq x y)))))",
        "(symbol-macrolet ((x :bad)) (declare (fixnum x)) x)",
        "(let ((cell (list 1)))
           (symbol-macrolet ((x (car cell))) (declare (fixnum x)) (setq x :bad))
           cell)",
        "(let ((cell (list 1)))
           (symbol-macrolet ((x (car cell))) (declare (fixnum x)) (setf x :bad))
           cell)",
    ] {
        assert_type_error(&rt, form);
        assert_type_error(&rt, &format!("(funcall (lambda () {form}))"));
    }

    let shadowed_special = "(let ((x 1))
      (declare (special x) (fixnum x))
      (let ((x :inner))
        (declare (special x))
        (setq x :new))
      x)";
    assert_eq!(eval_one_fixnum(&rt, shadowed_special), 1);
    assert_eq!(
        eval_one_fixnum(&rt, &format!("(funcall (lambda () {shadowed_special}))")),
        1
    );

    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((x :original))
               (locally (declare (fixnum x)) nil)
               x)"
        ),
        ":ORIGINAL"
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(let ((cell (list 1)))
              (symbol-macrolet ((x (car cell)))
                (declare (fixnum x) (optimize (safety 0)))
                (setq x :unchecked))
              cell)"
        ),
        "(:UNCHECKED)"
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(funcall (lambda ()
               (locally (declare (optimize (safety 0)))
                 (multiple-value-bind (x) (values :unchecked)
                   (declare (fixnum x))
                   x))))"
        ),
        ":UNCHECKED"
    );

    let parallel_exit = "(block done
      (let ((x :bad) (y (return-from done :early)))
        (declare (fixnum x) (ignore y))
        x))";
    assert_eq!(eval_one_repr(&rt, parallel_exit), ":EARLY");
    assert_eq!(
        eval_one_repr(&rt, &format!("(funcall (lambda () {parallel_exit}))")),
        ":EARLY"
    );
    assert_type_error(
        &rt,
        "(funcall (lambda ()
           (declare (optimize (safety 0)))
           (flet ((f (x) (declare (optimize (safety 3)) (fixnum x)) x))
             (declare (notinline f))
             (f :bad))))",
    );
}

#[test]
fn type_proclamations_apply_only_to_dynamic_bindings() {
    let rt = Runtime::start().unwrap();
    rt.eval("(proclaim '(type (integer 0 7) proclaimed-cell))")
        .unwrap();
    assert_eq!(eval_one_fixnum(&rt, "(setq proclaimed-cell 4)"), 4);
    assert_type_error(&rt, "(setq proclaimed-cell 9)");
    assert_eq!(
        eval_one_repr(&rt, "(let ((proclaimed-cell :lexical)) proclaimed-cell)"),
        ":LEXICAL"
    );
    assert_eq!(
        eval_one_repr(
            &rt,
            "(funcall (compile nil '(lambda (proclaimed-cell) proclaimed-cell)) :lexical)"
        ),
        ":LEXICAL"
    );
    for form in [
        "(let ((proclaimed-cell 9)) (declare (special proclaimed-cell)) 42)",
        "(funcall (lambda () (let ((proclaimed-cell 9)) (declare (special proclaimed-cell)) 42)))",
        "(multiple-value-bind (proclaimed-cell) (values 9) (declare (special proclaimed-cell)) 42)",
        "(destructuring-bind (proclaimed-cell) '(9) (declare (special proclaimed-cell)) 42)",
        "(funcall (lambda (proclaimed-cell) (declare (special proclaimed-cell)) 42) 9)",
        "(defparameter proclaimed-cell 9)",
    ] {
        assert_type_error(&rt, form);
    }

    rt.load_str(
        "(defun write-proclaimed-cell (value)
           (locally (declare (type (integer 3 9) proclaimed-cell))
             (setq proclaimed-cell value)))",
    )
    .unwrap();
    assert_eq!(eval_one_fixnum(&rt, "(write-proclaimed-cell 5)"), 5);
    assert_type_error(&rt, "(write-proclaimed-cell 2)");
    assert_type_error(&rt, "(write-proclaimed-cell 8)");

    rt.eval("(declaim (type number proclaimed-cell))").unwrap();
    assert_eq!(eval_one_repr(&rt, "(setq proclaimed-cell 1.0)"), "1.0");
    assert_type_error(
        &rt,
        "(progn (declaim (type fixnum pending-proclaimed-cell))
                (setq pending-proclaimed-cell :bad))",
    );
}

#[test]
fn fasl_loads_type_proclamations_without_accumulating_duplicate_checks() {
    let directory = TempDir::new("proclaimed-variable-types");
    let compiler = Runtime::start().unwrap();
    let path = directory.write(
        "proclaimed.lisp",
        "(defparameter *proclamation-probe-count* 0)
         (defun proclamation-probe (value)
           (incf *proclamation-probe-count*)
           (integerp value))
         (declaim (type (satisfies proclamation-probe) compiled-cell))
         (defun write-compiled-cell (value) (setq compiled-cell value))",
    );
    let fasl = compiler.compile_file(&path, None).unwrap();
    let rt = Runtime::start().unwrap();
    rt.load_file(&fasl).unwrap();
    assert_type_error(&rt, "(write-compiled-cell :bad)");
    rt.load_file(&fasl).unwrap();
    assert_eq!(eval_one_fixnum(&rt, "(setq compiled-cell 3)"), 3);
    assert_eq!(eval_one_fixnum(&rt, "*proclamation-probe-count*"), 1);

    let path = directory.write(
        "ordinary.lisp",
        "(proclaim '(type fixnum runtime-only-cell))",
    );
    let fasl = compiler.compile_file(&path, None).unwrap();
    assert_eq!(
        eval_one_repr(&compiler, "(setq runtime-only-cell :before-load)"),
        ":BEFORE-LOAD"
    );
    rt.load_file(&fasl).unwrap();
    assert_type_error(&rt, "(setq runtime-only-cell :after-load)");

    let path = directory.write(
        "compile-time.lisp",
        "(declaim (type fixnum compile-time-cell))
         (eval-when (:compile-toplevel :load-toplevel)
           (proclaim '(type string compile-time-cell)))
         (defun write-compile-time-cell (value) (setq compile-time-cell value))",
    );
    let fasl = compiler.compile_file(&path, None).unwrap();
    rt.load_file(&fasl).unwrap();
    assert_eq!(
        eval_one_repr(&rt, "(write-compile-time-cell \"ok\")"),
        "\"ok\""
    );
    assert_type_error(&rt, "(write-compile-time-cell 9)");
}
