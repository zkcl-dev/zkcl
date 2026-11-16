use std::fs;
use zkcl::Runtime;

#[test]
fn pattern_compiler_reports_invalid_definitions_or_nonexhaustive_matches() {
    let rt = Runtime::start().unwrap();
    rt.load_str(include_str!("patterns/api.lisp")).unwrap();
    rt.load_str(include_str!("patterns/coverage.lisp")).unwrap();
    for (form, message) in [
        (
            "(zk:match (the reply (ok 1)) ((pending) 1) ((ok _) 2))",
            "ERR",
        ),
        (
            "(zk:match (the reply (ok 1)) ((pending) 1) ((ok _) :when nil 2) ((err _) 3))",
            "guard",
        ),
        (
            "(zk:match 7 ((ok x) x) ((err x) x) ((pending) nil))",
            "Non-exhaustive",
        ),
        (
            "(zk:match 7 ((or (ok x) (err y)) x) (_ nil))",
            "different variables",
        ),
        ("(zk:ematch '(a . b) ((cons x x) t))", "Duplicate"),
        ("(zk:match (:inputs 1 2) ((a) a))", "columns"),
        ("(zk:ematch 3 ((:const missing-constant) t))", "constant"),
        ("(defstruct (not-a-reply (:include reply)))", "sealed"),
        ("(allocate-instance (find-class 'reply))", "constructor"),
    ] {
        let err = rt
            .eval(form)
            .err()
            .unwrap_or_else(|| panic!("unexpected success: {form}"));
        assert!(
            err.to_string()
                .to_lowercase()
                .contains(&message.to_lowercase()),
            "{form}: {err}"
        );
    }
    rt.load_str(
        r#"
      ;; Unchecked declaration violation against exhaustive zk:match signals zk:match-error at runtime.
      (assert (eq :caught
                  (handler-case
                      (funcall
                       (compile nil
                                '(lambda (v)
                                   (declare (optimize (safety 0)) (type boolean v))
                                   (zk:match v (t :yes) (nil :no))))
                       42)
                    (zk:match-error () :caught))))

      ;; Redefining a variant warns via style-warning, cleans removed constructors/readers, and succeeds.
      (let ((warned nil))
        (handler-bind ((style-warning (lambda (w) (setf warned t) (muffle-warning w))))
          (eval '(zk:defvariant reply (pending) (ok value))))
        (assert warned)
        (assert (not (fboundp 'err)))
        (assert (not (fboundp 'err-reason)))
        (assert (= 11 (zk:match (ok 11) ((ok v) v) ((pending) 0)))))
    "#,
    )
    .unwrap();
}

#[test]
fn separately_compiled_packages_resolve_patterns_by_identity() {
    let dir = std::env::temp_dir().join(format!("zk-match-files-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let defs = dir.join("defs.lisp");
    let client = dir.join("client.lisp");
    fs::write(
        &defs,
        r#"(defpackage :protocol (:use :cl) (:export :reply :ok :err :single :+limit+ :+token+))
(in-package :protocol)
(defconstant +limit+ 7)
(defconstant +token+ (gensym "TOKEN"))
(zk:defvariant reply (ok value) (err reason))
(zk:defpattern single (x) (list 'ok x))"#,
    )
    .unwrap();
    fs::write(&client, r#"(defpackage :consumer (:use :cl) (:import-from :protocol :reply :ok :err :single :+limit+) (:export :decode))
(in-package :consumer)
(defgeneric decode (r))
(defmethod decode ((r reply))
  (zk:match (the reply r)
    ((single (:const +limit+)) :limit)
    ((ok n) n)
    ((err why) why)))"#).unwrap();
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(&client).unwrap();
    writeln!(
        file,
        "(defun token-p (x) (zk:match x ((:const protocol:+token+) t) (_ nil)))"
    )
    .unwrap();
    write!(file, "\n(defun classify-key (x) (zk:match x ").unwrap();
    for n in 0..48 {
        write!(file, "(:event-{n} {n}) ").unwrap();
    }
    writeln!(file, "(:none nil) (_ :unknown)))").unwrap();
    write!(file, "(defun dense-result (x) (zk:match x ").unwrap();
    for n in -12..12 {
        let result = if n % 2 == 0 { "nil" } else { ":odd" };
        write!(file, "({n} {result}) ").unwrap();
    }
    writeln!(file, "(_ (values :outside x))))").unwrap();
    write!(file, "(defun dynamic-result (x n) (zk:match x ").unwrap();
    for n in 0..24 {
        write!(file, "(:dynamic-{n} (+ n {n})) ").unwrap();
    }
    writeln!(file, "(:empty (values)) (:pair (values n n)) (_ :miss)))").unwrap();
    drop(file);
    let (defs_fasl, client_fasl);
    {
        let rt = Runtime::start().unwrap();
        defs_fasl = rt.compile_file(&defs, None).unwrap();
        rt.load_file(&defs_fasl).unwrap();
        client_fasl = rt.compile_file(&client, None).unwrap();
    }
    // Verify the second load cannot recover by reading either source file.
    fs::remove_file(defs).unwrap();
    fs::remove_file(client).unwrap();
    let rt = Runtime::start().unwrap();
    rt.load_file(&defs_fasl).unwrap();
    rt.load_file(&client_fasl).unwrap();
    rt.load_str(
        "(assert (eq :limit (consumer:decode (protocol:ok 7))))
                 (assert (= 13 (consumer:decode (protocol:ok 13))))
                 (assert (eq :io (consumer:decode (protocol:err :io))))
                 (in-package :consumer)
                 (unintern 'ok) (unintern '+limit+)",
    )
    .unwrap();
    rt.eval("(assert (consumer::token-p protocol:+token+))")
        .unwrap();
    for n in 0..48 {
        let values = rt
            .eval(&format!("(consumer::classify-key :event-{n})"))
            .unwrap();
        assert_eq!(values[0].as_fixnum(), Some(n));
    }
    rt.eval("(assert (eq :unknown (consumer::classify-key :absent)))")
        .unwrap();
    rt.load_str("(assert (equal '(nil) (multiple-value-list (consumer::classify-key :none))))
                 (assert (equal '(nil) (multiple-value-list (consumer::dense-result -12))))
                 (assert (eq :odd (consumer::dense-result 11)))
                 (dolist (k '(-13 12 1.0 :other))
                   (assert (equal (list :outside k) (multiple-value-list (consumer::dense-result k)))))
                 (assert (= 30 (consumer::dynamic-result :dynamic-23 7)))
                 (assert (null (multiple-value-list (consumer::dynamic-result :empty 7))))
                 (assert (equal '(7 7) (multiple-value-list (consumer::dynamic-result :pair 7))))")
        .unwrap();
    for (form, expected) in [
        (
            "(zk:ematch (protocol:ok 2) ((ok n) n))",
            "Unknown pattern head",
        ),
        (
            "(zk:ematch 7 ((:const +limit+) t))",
            "Unknown named constant",
        ),
    ] {
        let err = rt.eval(form).err().expect("removed import must fail");
        assert!(err.to_string().contains(expected), "{err}");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compiled_matchers_execute_without_loading_the_pattern_compiler() {
    let dir = std::env::temp_dir().join(format!("zk-match-demo-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("api.lisp");
    fs::write(&source, include_str!("patterns/api.lisp")).unwrap();
    let fasl = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    fs::remove_file(source).unwrap();
    let rt = Runtime::start().unwrap();
    rt.load_file(&fasl).unwrap();
    // Executing a compiled matcher must not bootstrap the source-pattern compiler.
    assert!(
        rt.eval("(fboundp (find-symbol \"COMPILE-MATCH\" :zk-pattern-compiler))")
            .unwrap()[0]
            .is_nil()
    );
    rt.eval("(assert (equal (match-examples::transition :idle (match-examples::ok 2) :scale 3) '(:start 6)))").unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn match_exhaustiveness_agrees_with_boolean_truth_tables() {
    let rt = Runtime::start().unwrap();
    // Every subset of the four Boolean pairs, with no default row.
    let domain = [("nil", "nil"), ("nil", "t"), ("t", "nil"), ("t", "t")];
    for mask in 0..16u8 {
        let rows: String = domain
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(i, (a, b))| format!("(({a} {b}) {i}) "))
            .collect();
        let source =
            format!("(lambda (a b) (declare (type boolean a b)) (zk:match (:inputs a b) {rows}))");
        let result = rt.eval(&source);
        assert_eq!(result.is_ok(), mask == 15, "{source}");
    }
    // Independent truth tables for combinations, types, constructors, and
    // nested complements. Bit i denotes the i-th Boolean pair above.
    let alternatives = [
        ("(_ _)", 0b1111),
        ("(nil _)", 0b0011),
        ("(_ nil)", 0b0101),
        ("(t _)", 0b1100),
        ("(_ t)", 0b1010),
        ("((or t nil) (type null))", 0b0101),
        ("((and (type boolean) t) (or nil t))", 0b1100),
        ("((and nil t) _)", 0),
        ("((type (eql nil)) (type (eql t)))", 0b0010),
    ];
    for (left, left_mask) in alternatives {
        for (right, right_mask) in alternatives {
            let expression = format!(
                "(lambda (a b) (declare (type boolean a b)) (zk:match (:inputs a b) ({left} 1) ({right} 2)))"
            );
            assert_eq!(
                rt.eval(&expression).is_ok(),
                left_mask | right_mask == 15,
                "{expression}"
            );
        }
    }
}

#[test]
fn match_coverage_respects_lexical_shadowing() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(zk:defvariant answer (yes data) (no))
        (defun nested (a) (declare (type answer a))
          (zk:match a ((yes (cons x _)) x) ((yes v) v) ((no) nil)))
        (assert (= (nested (yes '(42))) 42))
        (assert (= (nested (yes 7)) 7))",
    )
    .unwrap();
    let err = rt.eval(
        "(lambda (a) (declare (type answer a))
                         (let ((a 17)) (zk:match a ((yes v) v) ((no) 0))))",
    );
    assert!(
        err.is_err(),
        "a fresh binding must not inherit an outer type proof"
    );
    let err = rt.eval("(flet ((yes (v) v)) (zk:match (yes 7) ((yes v) v)))");
    assert!(
        err.is_err(),
        "a shadowed constructor cannot supply a closed domain"
    );
}

#[test]
fn indexed_coverage_reports_the_single_uncovered_pair() {
    let rt = Runtime::start().unwrap();
    let keys: Vec<_> = (0..24).map(|i| format!(":k{i}")).collect();
    let domain = keys.join(" ");
    for missing in [None, Some(0), Some(12), Some(23)] {
        let rows = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != missing)
            .map(|(_, key)| format!("(({key} nil) 1)"))
            .collect::<Vec<_>>()
            .join(" ");
        let form = format!(
            "(lambda (k flag) (declare (type (member {domain}) k) (type boolean flag))
               (zk:match (:inputs k flag) ((_ t) 0) {rows}))"
        );
        let result = rt.eval(&form);
        match missing {
            None => assert!(result.is_ok(), "{form}"),
            Some(i) => {
                let err = result.err().expect("one uncovered pair");
                assert!(err.to_string().contains(&format!(":K{i}")), "{err}");
            }
        }
    }
}

#[test]
fn match_coverage_preserves_exclusions_across_guarded_rows() {
    let rt = Runtime::start().unwrap();
    let keys: Vec<_> = (0..24).map(|i| format!(":k{i}")).collect();
    let domain = keys.join(" ");
    let rest = keys[1..]
        .iter()
        .map(|key| format!("(({key} _) 1)"))
        .collect::<Vec<_>>()
        .join(" ");
    // Under CL set-theoretic coverage + CLHS 3.6 mutability contract:
    // guarded rows do not contribute coverage on their own, nor do they
    // wipe exclusions established by earlier unguarded rows.
    let incomplete = format!(
        "(lambda (k cell guard)
        (declare (type (member {domain}) k) (type (cons boolean t) cell))
        (zk:match (:inputs k cell)
          ((:k0 (cons nil _)) 0)
          ((:k0 _) :when guard 1)
          {rest}))"
    );
    let err = rt
        .eval(&incomplete)
        .err()
        .expect("guarded row alone must not prove coverage");
    assert!(err.to_string().contains("guard"), "{err}");

    let complete = format!(
        "(lambda (k cell guard)
        (declare (type (member {domain}) k) (type (cons boolean t) cell))
        (zk:match (:inputs k cell)
          ((:k0 (cons nil _)) 0)
          ((:k0 _) :when guard 1)
          ((:k0 (cons t _)) 2)
          {rest}))"
    );
    rt.eval(&complete)
        .unwrap_or_else(|e| panic!("expected coverage pass across guarded row: {e}"));
}

#[test]
fn failed_guards_can_change_class_backed_type_tests() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (defclass guard-type-base () ())
      (defclass guard-type-target (guard-type-base) ())
      (defclass guard-type-source (guard-type-target) ())
      (deftype guard-type-alias () 'integer)

      (let* ((events nil)
             (subject (make-instance 'guard-type-source)))
        (assert (equal '(:fallback 42)
                       (multiple-value-list
                        (zk:ematch (the guard-type-base subject)
                          ((type guard-type-target)
                           :when (progn
                                   (push :guarded events)
                                   (handler-bind ((warning #'muffle-warning))
                                     (eval '(defclass guard-type-source
                                              (guard-type-base) ())))
                                   nil)
                           :unreachable)
                          ((type guard-type-target)
                           (push :cached-type events)
                           :unreachable)
                          ((type guard-type-base)
                           (push :fallback events)
                           (values :fallback 42))))))
        (assert (equal '(:guarded :fallback) (reverse events)))
        (assert (not (typep subject 'guard-type-target))))

      (handler-bind ((warning #'muffle-warning))
        (defclass guard-type-source (guard-type-target) ()))
      (let ((subject (make-instance 'guard-type-source))
            (view-calls 0))
        (assert (equal '(:fallback 43)
                       (multiple-value-list
                        (zk:ematch (the guard-type-base subject)
                          ((type guard-type-target
                                 (zk:view (lambda (value)
                                            (declare (ignore value))
                                            (incf view-calls)
                                            (handler-bind ((warning #'muffle-warning))
                                              (eval '(defclass guard-type-source
                                                       (guard-type-base) ())))
                                            nil)
                                          t))
                           :unreachable)
                          ((type guard-type-target) :unreachable)
                          ((type guard-type-base) (values :fallback 43))))))
        (assert (= view-calls 1)))

      (let ((guard-calls 0))
        (assert (equal '(:fallback 20)
                       (multiple-value-list
                        (zk:ematch (the t 17)
                          ((type guard-type-alias)
                           :when (progn
                                   (incf guard-calls)
                                   (eval '(deftype guard-type-alias () 'null))
                                   nil)
                           :unreachable)
                          ((type guard-type-alias) :unreachable)
                          ((type t) (values :fallback 20))))))
        (assert (= guard-calls 1)))
    "#,
        )
        .unwrap();
}

#[test]
fn structural_effects_bound_projection_sharing() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
      (let ((cell (list 1 2))
            (guard-calls 0))
        (assert (equal '(:reread 41)
                       (multiple-value-list
                        (zk:ematch cell
                          ((cons 1 _)
                           :when (progn
                                   (incf guard-calls)
                                   (rplaca cell 2)
                                   nil)
                           :guard-failed)
                          ((cons 2 _) (values :reread 41))
                          (_ :fallback)))))
        (assert (= guard-calls 1)))

      (let ((cell (cons :left 1))
            (events nil))
        (assert (equal '(2 :matched)
                       (multiple-value-list
                        (zk:ematch cell
                          ((cons (zk:view
                                  (lambda (value)
                                    (declare (ignore value))
                                    (rplacd cell 2)
                                    (push :view events)
                                    :left)
                                  _)
                                 observed)
                           (push :body events)
                           (values observed :matched))
                          (_ :fallback)))))
        (assert (equal '(:view :body) (reverse events))))

      (zk:defvariant mutable-tag (want (value integer)))
      (let ((subject (want 1))
            (guard-calls 0))
        (assert (equal '(:slot-reread 42)
                       (multiple-value-list
                        (zk:ematch (the mutable-tag subject)
                          ((want _)
                           :when (progn
                                   (incf guard-calls)
                                   (setf (slot-value subject 'value) 2)
                                   nil)
                           :guard-failed)
                          ((want 2) (values :slot-reread 42))
                          ((want _) :fallback)))))
        (assert (= guard-calls 1)))
    "#,
        )
        .unwrap();
}

#[test]
fn covers_only_effective_vector_length_and_typed_variant_slots() {
    let runtime = Runtime::start().unwrap();
    let error = runtime
        .eval(
            "(zk:match (the (vector t 3) (make-array 3 :fill-pointer 1)) ((vector _ _ _) :three))",
        )
        .err()
        .expect("fill-pointer vector must fail fixed 3-element vector coverage");
    assert!(
        error.message().contains("coverage") || error.message().contains("Non-exhaustive"),
        "{error}"
    );
    runtime
        .load_str(
            r#"
      (assert (eq :one (zk:match (the (vector t 3) (make-array 3 :fill-pointer 1))
                         ((vector _) :one) (_ :other))))
      (assert (eq :three (zk:match (the (simple-vector 3) #(1 2 3)) ((vector _ _ _) :three))))

      ;; Immutable typed defvariant slots enforce check-type at construction and refine nested coverage.
      (zk:defvariant (pkt (:predicate pkt-p) (:conc-name pkt-))
        (hdr (flag boolean) (tag (integer 0 1))))
      (assert (pkt-p (hdr t 0)))
      (assert (eq :type-err (handler-case (hdr :not-bool 0) (type-error () :type-err))))
      (assert (eq :type-err (handler-case (hdr t 2) (type-error () :type-err))))
      (assert (eq :high
                  (zk:match (the pkt (hdr nil 1))
                    ((hdr t _) :true)
                    ((hdr nil 0) :low)
                    ((hdr nil 1) :high))))
    "#,
        )
        .unwrap();
}

#[test]
fn structural_extensions_preserve_order_types_and_binding_identity() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str(r#"
      (assert (eq :upper (zk:match (the (integer 0 255) 128)
                          ((type (integer 0 127)) :lower)
                          ((zk:range 128 255) :upper))))
      (assert (char= #\B (zk:match #\B ((zk:range #\A #\Z letter) letter) (_ #\?))))
      (let ((expected 2) (calls nil))
        (assert (equal '(1 3) (zk:match '(1 2 3)
                        ((list expected (:= (progn (push :value calls) expected)
                                            :test (progn (push :test calls) #'eql)) result)
                         (list expected result))
                        (_ nil))))
        (assert (equal calls '(:test :value))))
      (let ((table (make-hash-table :test 'equal)))
        (assert (eq :absent (zk:match table ((zk:keys ("x" nil)) :bad)
                              ((zk:keys ("x" (:values _ nil))) :absent) (_ :miss))))
        (setf (gethash "x" table) nil)
        (assert (eq :present (zk:match table ((zk:keys ("x" nil)) :present) (_ :miss)))))
      (flet ((both (input) (values input nil 42)))
        (assert (equal '(9 42) (zk:match 9 ((zk:view both (:values value nil extra)) (list value extra)) (_ nil)))))
      (let ((input (make-array 5 :fill-pointer 4 :initial-contents '(1 2 3 4 5))))
        (assert (equalp '(1 #(2 3) 4) (zk:match input ((vector first (:rest middle) last) (list first middle last)) (_ nil))))
        (let ((calls 0))
          (flet ((inc-view (v) (incf calls) v))
            (assert (equal '(1 2)
                           (zk:match input
                             ((vector (zk:view inc-view 1) 2 9) :short)
                             ((vector (zk:view inc-view 1) 2 3 9) :miss)
                             ((vector 1 (zk:view inc-view 2) _ _) (list 1 2))
                             (_ :other))))
            (assert (= calls 2)))))
      (let ((packet (zk-bits:octets #xd2 3 0 65 66 67 #x7e)))
        (assert (equal '(unsigned-byte 8) (array-element-type packet)))
        (assert (typep packet '(simple-array (unsigned-byte 8) (7))))
        (assert (not (typep #(256) '(vector (unsigned-byte 8)))))
        (assert (equal '(2 (65 66 67))
                       (zk:match packet ((zk-bits:bits (u4be #xd) (u4be kind) (u16le size)
                                                     (:bytes size payload) (u8be #x7e))
                                         (list kind (coerce payload 'list))) (_ nil))))
        (setf (aref packet 1) 4)
        (assert (eq :miss (zk:match packet ((zk-bits:bits (u8be _) (u16le size) (:bytes size _) (u8be #x7e)) :bad) (_ :miss)))))
      (assert (= -1 (zk:match (zk-bits:octets #x80) ((zk-bits:bits (i1be signed) (:rest-bits _)) signed) (_ 0))))
      (let ((tail (zk:match (zk-bits:octets #xab #xcd) ((zk-bits:bits (u4be #xa) (:rest-bits bits)) bits) (_ nil))))
        (assert (eq :bit (zk-text:span-unit tail)))
        (assert (= 4 (zk-text:span-start tail)))
        (assert (= #xbcd (zk:match tail ((zk-bits:bits (u12be value)) value) (_ 0)))))
    "#).unwrap();
    let error = runtime
        .eval("(zk:match (the (integer 0 255) 0) ((zk:range 0 127) :a) ((zk:range 129 255) :b))")
        .err()
        .expect("uncovered value");
    assert!(error.message().contains("128"), "{error}");

    runtime
        .load_str(
            r#"
      ;; Type-shorthand declarations, scalar eql/member coverage, unsigned-byte/fixnum ranges, and 1D arrays.
      (defun classify-bool (flag)
        "Classify a boolean."
        (declare (boolean flag))
        (zk:match flag
          (t :yes)
          (nil :no)))
      (assert (eq :yes (classify-bool t)))
      (assert (eq :no (classify-bool nil)))

      (assert (eq :two (zk:match (the (integer 0 2) 2)
                         ((type (eql 0)) :zero)
                         ((type (member 1 2)) :two))))
      (assert (eq :neg (zk:match (the fixnum -5)
                         ((zk:range * -1) :neg)
                         ((zk:range 0 *) :nonneg))))
      (assert (eq :ok (zk:match (the unsigned-byte 7)
                        ((zk:range 0 *) :ok))))
      (assert (eq :empty (zk:match (the (simple-array (unsigned-byte 8) (*)) (zk-bits:octets))
                           ((vector) :empty)
                           ((vector _ (:rest _)) :nonempty))))

      ;; Multi-input constant folding & matrix coverage without false warning.
      (let ((warnings nil))
        (handler-bind ((warning (lambda (w) (push w warnings) (muffle-warning w))))
          (assert (eq :yes
                      (let ((tag :x))
                        (zk:match (:inputs (the boolean t) (the (member :x :y) :x))
                          ((t (:= tag)) :yes)
                          ((nil :x) :nx)
                          ((_ :y) :y)
                          ((t :x) :unreachable-fallback))))))
        (assert (null warnings)))
    "#,
        )
        .unwrap();
}
