use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

fn eval_numbers(runtime: &Runtime, source: &str) -> Vec<Option<isize>> {
    runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error:?}"))
        .iter()
        .map(|value| value.as_fixnum())
        .collect()
}

#[test]
fn setf_writes_the_place_captured_before_value_evaluation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-write", |runtime, _| {
            runtime.collect();
            Ok(vec![runtime.fixnum(42).unwrap()])
        })
        .unwrap();

    assert_eq!(
        eval_numbers(
            &runtime,
            "(let* ((x (list 1 2)) (old (cdr x)) (state 0))
               (setf (cadr (progn (setq state (+ state 1)) x))
                     (progn (setq state (* state 10))
                            (rplacd x (list 7 8))
                            (collect-during-write)))
               (values state (cadr x) (car old)))",
        ),
        [Some(10), Some(7), Some(42)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x (list 1 2 3 4)) (order 0))
               (setf (nth (progn (setq order 1) 2)
                          (progn (setq order (+ (* order 10) 2)) x))
                     (progn (setq order (+ (* order 10) 3)) 42)
                     (fourth x) (+ (third x) 1))
               (values order (third x) (fourth x)))",
        ),
        [Some(123), Some(42), Some(43)]
    );
}

#[test]
fn setf_values_places_bind_missing_values_to_nil() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x 0) (y 0) (z (list 0)))
               (multiple-value-bind (a b c)
                   (setf (values (values x y) (car z)) (values 12 42 99))
                 (values x y (car z) a b c)))",
        ),
        [Some(12), None, Some(42), Some(12), Some(42), None]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x 0) (y 1) (z 2))
               (psetf (values x y) (values y)
                      z x)
               (values x y z))",
        ),
        [Some(1), None, Some(0)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x (list 1 2)) (order 0))
               (psetf (car (progn (setq order (+ order 1)) x))
                      (progn (setq order (* order 10)) (cadr x))
                      (cadr (progn (setq order (+ order 2)) x))
                      (progn (setq order (* order 10)) (car x)))
               (values order (car x) (cadr x)))",
        ),
        [Some(120), Some(2), Some(1)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x 1))
               (values (setf (symbol-value 'place-value) 2)
                       (setf x (values 3 4))
                       (symbol-value 'place-value)
                       x))",
        ),
        [Some(2), Some(3), Some(2), Some(3)]
    );
    assert_eq!(
        eval_numbers(&runtime, "(setf (values) (setq ignored-place-value 9))"),
        []
    );
    assert_eq!(
        eval_reprs(&runtime, "(let ((x 5)) (values (setf x (values)) x))"),
        ["NIL", "NIL"]
    );
    assert_eq!(eval_numbers(&runtime, "ignored-place-value"), [Some(9)]);
    assert_eq!(eval_numbers(&runtime, "(psetf)"), [None]);
}

#[test]
fn incf_reads_the_place_after_evaluating_the_increment() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((cell (list 4)))
               (values (incf (car cell) (progn (rplaca cell 100) 1))
                       (decf (car cell))
                       (car cell)))",
        ),
        [Some(101), Some(100), Some(100)]
    );
}

#[test]
fn push_pop_evaluate_place_subforms_in_order() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((wrap (list (list 9))) (order 0))
               (values (car (push (setq order 1)
                                  (car (progn (setq order (+ (* order 10) 2)) wrap))))
                       (pop (car (progn (setq order (+ (* order 10) 3)) wrap)))
                       (caar wrap)
                       order))",
        ),
        [Some(1), Some(1), Some(9), Some(123)]
    );
}

#[test]
fn pop_preserves_the_remaining_circular_list() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((cycle (list 9)))
               (rplacd cycle cycle)
               (values (pop cycle) (if (eq cycle (cdr cycle)) 1 0)))",
        ),
        [Some(9), Some(1)]
    );
}

#[test]
fn parallel_assignments_resolve_symbol_macro_places() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((x 1) (y 2))
               (psetq x y y x)
               (values x y (multiple-value-setq (x y) (values 4 5 6)) x y))",
        ),
        [Some(2), Some(1), Some(4), Some(4), Some(5)]
    );
    assert_eq!(
        eval_numbers(&runtime, "(multiple-value-setq () (values 4 5))"),
        [Some(4)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((cell (list 10 20)))
               (symbol-macrolet ((first (car cell)) (second (cadr cell)))
                 (psetq first (+ second 1) second (+ first 1))
                 (values first second (multiple-value-setq (first second) (values 4 5))
                         first second)))",
        ),
        [Some(21), Some(11), Some(4), Some(4), Some(5)]
    );
}

#[test]
fn get_setf_expansion_resolves_places_in_the_lexical_environment() {
    let runtime = Runtime::start().unwrap();
    let expansion = runtime.eval("(get-setf-expansion 'place-value)").unwrap();
    assert_eq!(expansion.len(), 5);
    assert!(expansion[0].is_nil());
    assert!(expansion[1].is_nil());
    assert!(expansion[4].same_identity(&runtime.eval("'place-value").unwrap()[0]));
    let other = runtime.eval("(get-setf-expansion 'place-value)").unwrap();
    let (store, end) = runtime.cons_fields(&expansion[2]).unwrap();
    let (other_store, _) = runtime.cons_fields(&other[2]).unwrap();
    assert!(end.is_nil());
    assert!(!store.same_identity(&other_store));
    assert!(!runtime.eval("(macro-function 'setf)").unwrap()[0].is_nil());

    assert_eq!(
        eval_numbers(
            &runtime,
            r#"(let ((cell (cons 2 8)))
                 (multiple-value-bind (temps forms stores writer reader)
                     (get-setf-expansion (list 'car (list 'quote cell)))
                   (multiple-value-bind (stored obtained)
                       (eval `(let* ((,(car temps) ,(car forms)) (,(car stores) 65))
                                (values ,writer ,reader)))
                     (values stored obtained (car cell)))))"#,
        ),
        [Some(65), Some(65), Some(65)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            r#"(let ((cell (list 1 2)))
                 (macrolet ((head-place (target) `(car ,target))
                            (store-expanded (target &environment env)
                              (multiple-value-bind (temps forms stores writer reader)
                                  (get-setf-expansion target env)
                                `(let* ((,(car temps) ,(car forms)) (,(car stores) 12))
                                   ,writer))))
                   (setf (head-place cell) 42)
                   (values (car cell) (store-expanded (head-place cell)) (car cell))))"#,
        ),
        [Some(42), Some(12), Some(12)]
    );
}

#[test]
fn setf_reports_the_name_of_an_undefined_writer() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_numbers(
            &runtime,
            "(handler-case (setf (unknown-place 1) 0)
               (undefined-function (condition)
                 (when (equal (cell-error-name condition) '(setf unknown-place))
                   42)))",
        ),
        [Some(42)]
    );
}

#[test]
fn defsetf_defines_generalized_place_writers() {
    let runtime = &Runtime::start().unwrap();
    // Short form with function updater and docstring
    assert_eq!(
        eval_reprs(
            runtime,
            r#"(progn
                 (defun ds-short-acc (x) (cadr x))
                 (defun ds-short-upd (x val) (setf (cadr x) val))
                 (defsetf ds-short-acc ds-short-upd "Short doc")
                 (let ((x (list 1 2 3)))
                   (values (defsetf ds-short-acc ds-short-upd "Short doc")
                           (setf (ds-short-acc x) 4)
                           x
                           (documentation 'ds-short-acc 'setf))))"#,
        ),
        ["DS-SHORT-ACC", "4", "(1 4 3)", "\"Short doc\""]
    );

    // Short form with macro updater
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (defun ds-macro-acc (x) (cadr x))
               (defmacro ds-macro-upd (x val) `(setf (cadr ,x) ,val))
               (defparameter *ds-macro-ret*
                 (defsetf ds-macro-acc ds-macro-upd))
               (let ((x (list 10 20 30)))
                 (values *ds-macro-ret* (setf (ds-macro-acc x) 99) x)))",
        ),
        ["DS-MACRO-ACC", "99", "(10 99 30)"]
    );
}

#[test]
fn long_defsetf_binds_place_arguments_in_order() {
    let runtime = &Runtime::start().unwrap();
    // Long form: evaluation order, docstring, declare, return-from, lexical closure, multiple stores
    assert_eq!(
        eval_reprs(
            runtime,
            r#"(progn
                 (defun ds-long-elt (n seq) (elt seq n))
                 (defsetf ds-long-elt (n seq) (val)
                   (declare (ignorable n seq val))
                   "Long doc"
                   `(setf (elt ,seq ,n) ,val))
                 (let ((x (list 1 2 3 4))
                       (i 0)
                       (j nil)
                       (k nil))
                   (values
                    (setf (ds-long-elt (progn (setf j (incf i)) 2)
                                       (progn (setf k (incf i)) x))
                          (progn (incf i) :a))
                    x i j k
                    (documentation 'ds-long-elt 'setf))))"#,
        ),
        [":A", "(1 2 :A 4)", "3", "1", "2", "\"Long doc\""]
    );

    // Long form: block return-from and lexical closure capture
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (defun ds-ret (x) (car x))
               (defsetf ds-ret (y) (val)
                 (return-from ds-ret `(setf (car ,y) ,val))
                 '(error :unreachable))
               (defun ds-lex (x) (car x))
               (let ((z 'car))
                 (defsetf ds-lex (y) (val)
                   `(setf (,z ,y) ,val)))
               (let ((a (cons 1 2))
                     (b (cons 3 4)))
                 (values (setf (ds-ret a) 10) a
                         (setf (ds-lex b) 30) b)))",
        ),
        ["10", "(10 . 2)", "30", "(30 . 4)"]
    );

    // Long form: multiple store variables, &optional, &rest, &key, &environment
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (defsetf ds-multi (x) (v1 v2)
                 `(list ,x ,v1 ,v2))
               (defsetf ds-opt (x &optional (scale 1 scale-p) &environment env) (val)
                 (declare (ignore env))
                 `(list ,x ,scale ,scale-p ,val))
               (defsetf ds-key (x &key (k 0) &allow-other-keys) (val)
                 `(list ,x ,k ,val))
               (defsetf ds-rest (x &rest more) (val)
                 `(list ,x (list ,@more) ,val))
               (let ((order 0))
                 (values
                  (setf (ds-multi 1) (values 2 3))
                  (setf (ds-opt :a) 10)
                  (setf (ds-opt :b 5) 10)
                  (setf (ds-key (incf order) :k (incf order)) (incf order))
                  (setf (ds-rest :r 1 2 3) 4)
                  order)))",
        ),
        [
            "(1 2 3)",
            "(:A 1 NIL 10)",
            "(:B 5 T 10)",
            "(1 2 3)",
            "(:R (1 2 3) 4)",
            "3"
        ]
    );

    // Long form: &environment captures local macrolet and &key with non-keyword extra forms
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (defsetf ds-env (p &environment env) (val)
                 (let ((expanded (macroexpand '(local-op) env)))
                   `(progn (setf (car ,p) ,val) ,expanded)))
               (defsetf ds-rest-key (x &rest all &key (a 0) &allow-other-keys) (val)
                 `(list ,x ,a (list ,@all) ,val))
               (macrolet ((local-op () '':from-env))
                 (let ((cell (cons 1 2)))
                   (values (setf (ds-env cell) 77)
                           cell
                           (setf (ds-rest-key 1 :a 2) 9)
                           (setf (ds-rest-key 1 :b 2 :a 5) 9)))))",
        ),
        [
            ":FROM-ENV",
            "(77 . 2)",
            "(1 2 (:A 2) 9)",
            "(1 5 (:B 2 :A 5) 9)"
        ]
    );
}

#[test]
fn setf_expanders_compose_using_lexical_place_expansions() {
    let runtime = &Runtime::start().unwrap();
    // Nested places, return value, &environment, and my-assoc with return-from
    assert_eq!(
        eval_reprs(
            runtime,
            r#"(progn
                 (defun my-car (x) (car x))
                 (defparameter *dse-ret*
                   (multiple-value-list
                    (define-setf-expander my-car (place &environment env)
                      "My car expander"
                      (multiple-value-bind (temps vals stores set-form get-form)
                          (get-setf-expansion place env)
                        (declare (ignore stores set-form))
                        (let ((store (gensym))
                              (temp (gensym)))
                          (values
                           `(,@temps ,temp)
                           `(,@vals ,get-form)
                           `(,store)
                           `(progn (rplaca ,temp ,store) ,store)
                           `(my-car ,temp)))))))
                 (defun my-assoc (key alist)
                   (loop for pair in alist
                         when (and (consp pair) (eql key (car pair)))
                         return pair))
                 (define-setf-expander my-assoc (key place &environment env)
                   (multiple-value-bind (temps vals stores set-form get-form)
                       (get-setf-expansion place env)
                     (let ((store (gensym))
                           (key-temp (gensym))
                           (pair-temp (gensym))
                           (place-temp (gensym)))
                       (return-from my-assoc
                         (values
                          `(,@temps ,key-temp ,place-temp ,pair-temp)
                          `(,@vals ,key ,get-form (my-assoc ,key-temp ,place-temp))
                          `(,store)
                          `(if (null ,pair-temp)
                               (let ((,(car stores)
                                      (cons (cons ,key-temp ,store) ,place-temp)))
                                 ,set-form
                                 ,store)
                             (setf (cdr ,pair-temp) ,store))
                          `(cdr ,pair-temp))))))
                 (let ((nested (list (list 1)))
                       (alist nil))
                   (values
                    *dse-ret*
                    (documentation 'my-car 'setf)
                    (setf (documentation 'my-car 'setf) "Updated doc")
                    (documentation 'my-car 'setf)
                    (setf (my-car (my-car nested)) 2)
                    nested
                    (setf (my-assoc 'foo alist) 1)
                    (setf (my-assoc 'foo alist) 2)
                    (setf (my-assoc 'bar alist) 3)
                    alist)))"#,
        ),
        [
            "(MY-CAR)",
            "\"My car expander\"",
            "\"Updated doc\"",
            "\"Updated doc\"",
            "2",
            "((2))",
            "1",
            "2",
            "3",
            "((BAR . 3) (FOO . 2))",
        ]
    );

    // &whole, &body, &optional, &key, dotted tail, declare, and overriding a defmacro
    assert_eq!(
        eval_reprs(
            runtime,
            r#"(progn
                 (define-setf-expander dse-whole (&whole w a &optional (b 10) &body rest)
                   (declare (ignore a b rest))
                   (let ((s (gensym)))
                     (values nil nil (list s) `(list ',w ,s) `(quote ,w))))
                 (define-setf-expander dse-key (a &key (k 99) &environment env)
                   (declare (special env))
                   (let ((s (gensym)))
                     (values nil nil (list s) `(list ,a ,k ,s ,(not (null env))) a)))
                 (define-setf-expander dse-dot (a . tail)
                   (let ((s (gensym)))
                     (values nil nil (list s) `(list ,a ',tail ,s) a)))
                 (defmacro dse-override (x) `(car ,x))
                 (defsetf dse-override (x) (val) `(setf (cdr ,x) ,val))
                 (let ((cell (cons 1 2)))
                   (macrolet ((wrap (p) `(car ,p)))
                     (values
                      (setf (dse-whole 1 2 3 4) :v)
                      (setf (dse-key 5 :k 6) :w)
                      (setf (dse-dot 7 8 9) :z)
                      (setf (dse-override cell) 20)
                      cell))))"#,
        ),
        [
            "((DSE-WHOLE 1 2 3 4) :V)",
            "(5 6 :W T)",
            "(7 (8 9) :Z)",
            "20",
            "(1 . 20)",
        ]
    );

    // GET-SETF-EXPANSION: 5 values, explicit NIL env, symbol macros
    assert_eq!(
        eval_reprs(
            runtime,
            "(let ((vals-nil (multiple-value-list (get-setf-expansion '(car x) nil))))
               (symbol-macrolet ((sm (car cell)))
                 (macrolet ((inspect-sm (p &environment env)
                              (multiple-value-bind (temps vals stores writer reader)
                                  (get-setf-expansion p env)
                                `(list ,(length temps)
                                       ,(length vals)
                                       ,(length stores)
                                       (equal ',reader '(car ,(car temps)))))))
                   (values (length vals-nil)
                           (inspect-sm sm)))))",
        ),
        ["5", "(1 1 1 T)"]
    );
}

#[test]
fn setf_apply_accepts_array_accessor_designators() {
    let runtime = &Runtime::start().unwrap();
    // (SETF (APPLY #'aref / #'bit / #'sbit ...))
    assert_eq!(
        eval_reprs(
            runtime,
            "(let ((v (vector 0 1 2))
                   (a (make-array '(2 2) :initial-contents '((0 0) (0 0))))
                   (bv (copy-seq #*000000)))
               (setf (apply #'aref v '(1)) 10)
               (setf (apply #'aref a 1 1 nil) 99)
               (setf (apply #'bit bv 2 nil) 1)
               (setf (apply #'sbit bv '(4)) 1)
               (values v (aref a 1 1) bv))",
        ),
        ["#(0 10 2)", "99", "#*001010"]
    );
}

#[test]
fn shiftf_returns_the_leftmost_primary_value() {
    let runtime = &Runtime::start().unwrap();
    // SHIFTF: order, multiple values stored, single primary value returned
    assert_eq!(
        eval_reprs(
            runtime,
            "(let ((x (vector 'a 'b 'c 'd 'e 'f))
                   (i 1)
                   (p 'first)
                   (q 'second))
               (values
                (shiftf (aref x (incf i)) (aref x (incf i)) (incf i))
                x
                i
                (multiple-value-list (shiftf (values p q) (floor 10 3)))
                p q))",
        ),
        ["C", "#(A B D 4 E F)", "4", "(FIRST)", "3", "1"]
    );
}

#[test]
fn rotatef_reads_places_in_order_before_rotating() {
    let runtime = &Runtime::start().unwrap();
    // ROTATEF: 0, 1, 2, 3+ places, returns NIL, preserves left-to-right order
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (defparameter *rot-log* nil)
               (defun rot-slot (v idx) (aref v idx))
               (define-setf-expander rot-slot (v idx)
                 (let ((tv (gensym)) (ti (gensym)) (s (gensym)))
                   (values (list tv ti)
                           (list v idx)
                           (list s)
                           `(setf (aref ,tv ,ti) ,s)
                           `(progn (push ,ti *rot-log*) (aref ,tv ,ti)))))
               (let ((x (vector 'a 'b 'c 'd 'e))
                     (i 0)
                     (s 10)
                     (p 1)
                     (q 2))
                 (values
                  (rotatef)
                  (rotatef (aref x (incf i)))
                  i
                  (rotatef p q)
                  p q
                  (rotatef (rot-slot x 0) (rot-slot x 1) (rot-slot x 2) s)
                  x
                  s
                  (nreverse *rot-log*))))",
        ),
        [
            "NIL",
            "NIL",
            "1",
            "NIL",
            "2",
            "1",
            "NIL",
            "#(B C 10 D E)",
            "A",
            "(0 1 2)",
        ]
    );
}

#[test]
fn pushnew_evaluates_keyword_arguments_once() {
    let runtime = &Runtime::start().unwrap();
    // PUSHNEW: :test, :test-not, :key, single evaluation of subforms
    assert_eq!(
        eval_reprs(
            runtime,
            "(let ((holder (list (list '(1 :a) '(2 :b))))
                   (order 0))
               (pushnew (progn (setq order (+ (* order 10) 1)) (list 2 :dup))
                        (car (progn (setq order (+ (* order 10) 2)) holder))
                        :key (progn (setq order (+ (* order 10) 3)) #'car)
                        :test (progn (setq order (+ (* order 10) 4)) #'eql))
               (let ((len-after-dup (length (car holder))))
                 (pushnew (list 3 :c) (car holder) :key #'car :test #'eql)
                 (let ((nums (list 10 20)))
                   (pushnew 10 nums :test-not #'/=)
                   (pushnew 30 nums :test-not #'/=)
                   (values order len-after-dup (length (car holder)) nums))))",
        ),
        ["1234", "2", "3", "(30 10 20)"]
    );
}

#[test]
fn define_modify_macro_binds_arguments_before_updating_the_place() {
    let runtime = &Runtime::start().unwrap();
    // Required, &optional (with default and supplied-p), &rest, docstring, and evaluation order
    assert_eq!(
        eval_reprs(
            runtime,
            r#"(progn
                 (define-modify-macro dmm-appendf (&rest args)
                   append "Append lists onto a list")
                 (define-modify-macro dmm-incf (&optional (delta 1)) +)
                 (define-modify-macro dmm-mix (req &optional (opt 10 opt-p) (extra (if opt-p 100 200)))
                   list)
                 (let ((u (list 'p 'q))
                       (a (vector 0 0 0 0 0))
                       (i 1)
                       (defaulted 1)
                       (supplied 2))
                   (values
                    (define-modify-macro dmm-dummy () identity)
                    (dmm-appendf u '(r s) '(t))
                    u
                    (dmm-incf (aref a (incf i)) (incf i))
                    a
                    i
                    (dmm-mix defaulted 5)
                    (dmm-mix supplied 5 7))))"#,
        ),
        [
            "DMM-DUMMY",
            "(P Q R S T)",
            "(P Q R S T)",
            "3",
            "#(0 0 3 0 0)",
            "3",
            "(1 5 10 200)",
            "(2 5 7 100)",
        ]
    );

    // Check symbol-macrolet and multiple-value place with define-modify-macro
    assert_eq!(
        eval_reprs(
            runtime,
            "(progn
               (define-modify-macro dmm-add (&optional (delta 1)) +)
               (let ((cell (list 10 20)))
                 (symbol-macrolet ((first-elem (car cell)))
                   (values (dmm-add first-elem 5)
                           (car cell)))))",
        ),
        ["15", "15"]
    );
}

#[test]
fn define_modify_macro_rejects_invalid_lambda_lists() {
    let runtime = &Runtime::start().unwrap();
    // Invalid lambda-lists and arguments signal PROGRAM-ERROR
    for bad in [
        "(define-modify-macro)",
        "(define-modify-macro bad-1)",
        "(define-modify-macro bad-2 ())",
        "(define-modify-macro bad-3 () + 123)",
        "(define-modify-macro bad-4 () + \"doc\" \"extra\")",
        "(define-modify-macro bad-5 (&key a) +)",
        "(define-modify-macro bad-6 (&aux a) +)",
        "(define-modify-macro bad-7 (&body b) +)",
        "(define-modify-macro bad-8 (&whole w) +)",
        "(define-modify-macro bad-9 (&environment e) +)",
        "(define-modify-macro bad-10 (a . b) +)",
        "(define-modify-macro bad-11 ((a b)) +)",
        "(define-modify-macro bad-12 (a a) +)",
        "(define-modify-macro bad-13 (&rest) +)",
        "(define-modify-macro bad-14 (&rest a b) +)",
    ] {
        let code = format!(
            "(handler-case (macroexpand '{bad})
               (program-error () :program-error))"
        );
        assert_eq!(eval_reprs(runtime, &code), [":PROGRAM-ERROR"], "{bad}");
    }
}
