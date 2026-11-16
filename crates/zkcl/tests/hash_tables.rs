use zkcl::Runtime;

#[test]
fn gethash_distinguishes_a_stored_nil_from_a_missing_entry() {
    let runtime = Runtime::start().unwrap();
    let source = r#"
        (let ((h (make-hash-table :size 0 :rehash-size 1 :rehash-threshold 1))
              (trace nil))
          (assert (and (hash-table-p h) (typep h 'hash-table)))
          (assert (eq (hash-table-test h) 'eql))
          (assert (= (hash-table-rehash-size h) 1))
          (assert (= (hash-table-rehash-threshold h) 1))
          (assert (eq (fdefinition '(setf gethash)) #'(setf gethash)))
          (assert (fboundp '(setf gethash)))
          (multiple-value-bind (value present) (gethash nil h :missing)
            (assert (eq value :missing))
            (assert (null present)))
          (flet ((mark (tag value) (setq trace (cons tag trace)) value))
            (assert (equal (multiple-value-list
                            (setf (gethash (mark :key nil)
                                           (mark :table h)
                                           (mark :fallback :ignored))
                                  (values (mark :new-value nil) :extra)))
                           '(nil))))
          (assert (equal trace '(:new-value :fallback :table :key)))
          (multiple-value-bind (value present) (gethash nil h :missing)
            (assert (null value))
            (assert present))
          (assert (= (hash-table-count h) 1))
          (assert (= (funcall #'(setf gethash) 42 :key h :ignored) 42))
          (assert (= (gethash :key h) 42))
          (assert (remhash nil h))
          (assert (null (remhash nil h)))
          (assert (eq (clrhash h) h))
          (assert (= (hash-table-count h) 0))
          (assert (null (hash-table-p 42)))
          (handler-case (gethash :key nil)
            (type-error (error)
              (assert (eq (type-error-expected-type error) 'hash-table))))
          t)
    "#;
    assert!(!runtime.eval(source).unwrap()[0].is_nil());
}

#[test]
fn maphash_retains_the_updated_entry_when_its_callback_collects() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("hash-test-collect", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();

    let source = r#"
        (let ((identity (make-hash-table :test 'eq :size 1))
              (structural (make-hash-table :test #'equal :size 1))
              (caseless (make-hash-table :test 'equalp))
              (key (list (gensym) "value"))
              (value (list 42))
              (replacement (list 55)))
          (setf (gethash key identity) value)
          (setf (gethash (list "Å" :value) structural) value)
          (setf (gethash "ÅΒ" caseless) value)
          (dotimes (i 193) (setf (gethash i identity) i))
          (hash-test-collect)
          (assert (eq (gethash key identity) value))
          (assert (null (gethash (list (car key) "value") identity)))
          (assert (eq (gethash (list "Å" :value) structural) value))
          (assert (eq (gethash "åβ" caseless) value))
          (assert (= (hash-table-count identity) 194))
          (assert (null (< (hash-table-size identity) (hash-table-count identity))))
          (let ((count 0))
            (assert (null (maphash (lambda (k v)
                                     (incf count)
                                     (if (eq k key)
                                         (progn
                                           (hash-test-collect)
                                           (setf (gethash k identity) replacement))
                                         (remhash k identity)))
                                   identity)))
            (assert (= count 194)))
          (assert (= (hash-table-count identity) 1))
          (let ((result (catch :hash-result
                          (maphash (lambda (k v) (throw :hash-result (cons k v))) identity)
                          nil)))
            (assert (eq (car result) key))
            (assert (eq (cdr result) replacement)))
          (let ((result (restart-case
                            (maphash (lambda (k v)
                                       (invoke-restart 'return-entry v))
                                     identity)
                          (return-entry (entry) entry))))
            (assert (eq result replacement)))
          (assert (= (hash-table-count identity) 1))
          (assert (eq (gethash key identity) replacement))
          t)
    "#;
    assert!(!runtime.eval(source).unwrap()[0].is_nil());
}

#[test]
fn weak_hash_tables_trace_conditional_entries_to_a_fixed_point() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(include_str!("common/weak_hash_tables.lisp"))
        .unwrap();
    assert!(
        !runtime
            .eval(
                "(let ((invalid (make-symbol \"KEY\")))
               (handler-case (make-hash-table :weakness invalid)
                 (type-error (c)
                   (and (eq (type-error-datum c) invalid)
                        (typep :key (type-error-expected-type c))))))"
            )
            .unwrap()[0]
            .is_nil()
    );
}

#[test]
fn rehash_hints_preserve_additive_growth_and_validate_exact_thresholds() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(let ((largest-step (1- (ash 1 63))))
           (dolist (step (list (1+ most-positive-fixnum) largest-step
                               (1+ largest-step) (ash 1 100)))
             (let ((actual (hash-table-rehash-size (make-hash-table :rehash-size step))))
               (assert (typep actual '(integer 1 *)))
               (assert (= actual (min step largest-step))))))
         (dolist (ratio (list (/ (1+ (ash 1 100)) (ash 1 100)) (/ -1 (ash 1 1200))))
           (assert (handler-case (progn (make-hash-table :rehash-threshold ratio) nil)
                     (type-error () t))))",
        )
        .unwrap();
}
