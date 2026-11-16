use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn accessor_places_in_cleanup_keep_their_lexical_expansion() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass stack () ((items :initform nil :accessor items)))
        (defun call-on-stack (stack thunk)
          (with-accessors ((top items)) stack
            (macrolet ((clear-top () '(pop top)))
              (push 42 top)
              (unwind-protect (funcall thunk top) (clear-top)))))
        "#,
        )
        .unwrap();
    let result = runtime
        .eval(
            r#"
        (let ((stack (make-instance 'stack)))
          (and (= 42 (call-on-stack stack #'car))
               (null (items stack))
               (= 42 (catch 'exit (call-on-stack stack (lambda (items) (throw 'exit (car items))))))
               (null (items stack))))
        "#,
        )
        .unwrap();
    assert!(!result[0].is_nil());
}

#[test]
fn instance_slot_values_survive_collection_during_initialization() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-instance-storage", |runtime, arguments| {
            runtime.collect();
            Ok(arguments.to_vec())
        })
        .unwrap();
    let repr = |source: &str| {
        let values = runtime
            .eval(&format!("(progn {source})"))
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        values
            .iter()
            .map(|value| runtime.repr(value).unwrap())
            .collect::<Vec<_>>()
    };
    for (source, expected) in [
        (
            "(let ((seed 40))
               (defclass record ()
                 ((value :initarg :value :initarg :alias :initform (+ seed 2))
                  (other :initarg :value)
                  (unset))))
             (let* ((class (find-class 'record))
                    (a (make-instance class))
                    (b (make-instance 'record :alias 8 :value 9 :alias 10)))
               (list (eq class (class-of a)) (typep a 'standard-object)
                     (slot-value a 'value) (slot-boundp a 'other)
                     (slot-value b 'value) (slot-value b 'other)
                     (slot-exists-p a 'unset) (slot-exists-p a 'missing)
                     (slot-exists-p 42 'value)))",
            "(T T 42 NIL 8 9 T NIL NIL)",
        ),
        (
            "(let ((default-runs 0))
               (defclass defaulted ()
                 ((item :initarg :item :initform 10)
                  (after :initform (collect-instance-storage (list 42))))
                 (:default-initargs :item (progn (incf default-runs) 12)))
               (let ((a (make-instance 'defaulted :ignored 19 :allow-other-keys t))
                     (b (make-instance 'defaulted :item nil)))
                 (list (slot-value a 'item) (slot-value a 'after)
                       (slot-value b 'item) (slot-boundp b 'item) default-runs)))",
            "(12 (42) NIL T 1)",
        ),
        (
            "(defclass base () ((shared :allocation :class :initarg :shared :initform 10)))
             (defclass derived (base) ((own :initarg :own)))
             (let* ((a (make-instance 'base))
                    (b (make-instance 'derived :own (list 42))))
               (setf (slot-value b 'shared) 19
                     (slot-value a 'shared) 20)
               (list (slot-value a 'shared) (slot-value b 'shared)))",
            "(20 20)",
        ),
        (
            "(defclass with-slots-storage () ((item)))
             (let ((instance (make-instance 'with-slots-storage)) (seen nil))
               (setf (slot-value (progn (push 1 seen) instance) (progn (push 2 seen) 'item))
                     (progn (push 3 seen) (list instance)))
               (collect-instance-storage nil)
               (let ((held (eq (car (slot-value instance 'item)) instance)))
                 (list held seen
                       (eq (slot-makunbound instance 'item) instance)
                       (slot-boundp instance 'item)
                       (handler-case (slot-value instance 'item)
                         (unbound-slot (c)
                           (and (eq (cell-error-name c) 'item)
                                (eq (unbound-slot-instance c) instance)))))))",
            "(T (3 2 1) T NIL T)",
        ),
        (
            "(defclass movable-standard ()
               ((kept :initarg :kept) (discarded :initform 21)
                (shared :allocation :class :initform 7) (still-unbound)))
             (let* ((class (find-class 'movable-standard))
                    (instance (make-instance class :kept (list 42))))
               (setf (slot-value instance 'shared) 8)
               (defclass movable-standard ()
                 ((added :initform (collect-instance-storage 43)) (shared)
                  (still-unbound :initform 100) (kept)))
               (list (eq class (class-of instance))
                     (slot-value instance 'kept) (slot-value instance 'added)
                     (slot-value instance 'shared) (slot-boundp instance 'still-unbound)
                     (slot-exists-p instance 'discarded)))",
            "(T (42) 43 8 NIL NIL)",
        ),
        (
            "(defclass throwing-instance () ((value :initform (throw 'instance-escape 42))))
             (let ((cleaned nil))
               (list (catch 'instance-escape
                       (unwind-protect (make-instance 'throwing-instance)
                         (setf cleaned t)))
                     cleaned))",
            "(42 T)",
        ),
        (
            "(defclass raw-instance () ((slot :initform 21)))
             (let* ((class (find-class 'raw-instance)) (instance (allocate-instance class)))
               (list (eq class (class-of instance)) (slot-exists-p instance 'slot)
                     (slot-boundp instance 'slot)))",
            "(T T NIL)",
        ),
        (
            "(let* ((object (make-instance 'standard-object))
                    (raw (allocate-instance (class-of object))))
               (list (typep object 'standard-object) (eq (class-of raw) (class-of object))
                     (slot-exists-p raw 'unknown)))",
            "(T T NIL)",
        ),
        (
            "(defclass accessors () ((item :accessor accessor-item :reader read-item :writer write-item)))
             (let ((a (make-instance 'accessors)))
               (list (if (fboundp '(setf accessor-item)) t nil)
                     (setf (accessor-item a) 21)
                     (write-item 42 a)
                     (read-item a)))",
            "(T 21 42 42)",
        ),
        (
            "(defclass virtual-instance () ((cache) (unbound)))
             (defmethod slot-missing ((class standard-class) (object virtual-instance) name operation
                                      &optional new-value)
               (if (eq name 'virtual)
                   (case operation
                     (slot-value (values (slot-value object 'cache) :extra))
                     (slot-boundp :true)
                     (slot-makunbound (slot-makunbound object 'cache))
                     (setf (setf (slot-value object 'cache) new-value) :ignored))
                   (call-next-method)))
             (defmethod slot-unbound ((class standard-class) (object virtual-instance) name)
               (if (eq name 'unbound) (values 99 :extra) (call-next-method)))
             (let ((a (make-instance 'virtual-instance)))
               (list (setf (slot-value a 'virtual) 42)
                     (multiple-value-list (slot-value a 'virtual))
                     (if (slot-boundp a 'virtual) t nil)
                     (not (slot-exists-p a 'virtual))
                     (multiple-value-list (slot-value a 'unbound))
                     (eq a (slot-makunbound a 'virtual))
                     (slot-boundp a 'cache)))",
            "(42 (42) T T (99) T NIL)",
        ),
    ] {
        assert_eq!(repr(source), [expected], "{source}");
    }
    for source in [
        "(make-instance 'record :other 10)",
        "(make-instance 'record :ignored 1 :allow-other-keys nil :allow-other-keys t)",
        "(make-instance 'record :value)",
        "(let ((a (make-instance 'record))) (slot-value a 'unknown))",
        "(let ((a (make-instance 'record))) (setf (slot-value a 'unknown) 42))",
        "(slot-value 42 'slot)",
    ] {
        assert!(runtime.eval(source).is_err(), "{source}");
    }
}

#[test]
fn initialization_keywords_follow_applicable_methods() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defclass initarg-before () ())
            (defclass initarg-after () ((value :initform nil)))
            (defmethod update-instance-for-different-class :after
                ((previous initarg-before) (current initarg-after) &key payload)
              (setf (slot-value current 'value) payload))
            (defmethod update-instance-for-different-class :after
                ((previous initarg-after) (current initarg-after) &key wrong)
              (declare (ignore wrong)))
            (defmethod shared-initialize :after
                ((current initarg-after) (slots (eql t)) &key make-only)
              (when make-only (setf (slot-value current 'value) make-only)))
            (defmethod shared-initialize :after
                ((current initarg-after) (slots null) &key reinitialize-only)
              (when reinitialize-only
                (setf (slot-value current 'value) reinitialize-only)))
            (defmethod shared-initialize :after
                ((current initarg-after) (slots cons) &key added-only)
              (when added-only (setf (slot-value current 'value) added-only)))
            (defparameter *initarg-object* (make-instance 'initarg-after))
            (defmethod reinitialize-instance :after
                ((object (eql *initarg-object*)) &key only-this)
              (when only-this (setf (slot-value object 'value) only-this)))
            (defclass initarg-redefined () ((value :initform nil)))
            (defmethod update-instance-for-redefined-class :around
                ((object initarg-redefined) added discarded properties &rest initargs)
              (declare (ignore initargs))
              (call-next-method object added discarded properties :redefine-only 29))
            (defmethod update-instance-for-redefined-class :after
                ((object initarg-redefined) (added cons) discarded properties &key redefine-only)
              (when redefine-only (setf (slot-value object 'value) redefine-only)))
            "#,
        )
        .unwrap();

    let value = runtime
        .eval(
            "(list
               (slot-value (change-class (make-instance 'initarg-before)
                                         'initarg-after :payload 13 :added-only t) 'value)
               (slot-value (make-instance 'initarg-after :make-only 17) 'value)
               (slot-value (reinitialize-instance *initarg-object* :only-this 19) 'value)
               (slot-value (reinitialize-instance (make-instance 'initarg-after)
                                                   :reinitialize-only 23) 'value)
               (let ((object (make-instance 'initarg-redefined)))
                 (defclass initarg-redefined () ((value) (new-slot)))
                 (slot-value object 'value)))",
        )
        .unwrap();
    assert_eq!(runtime.repr(&value[0]).unwrap(), "(13 17 19 23 29)");

    for source in [
        "(change-class (make-instance 'initarg-before) 'initarg-after :wrong t)",
        "(make-instance 'initarg-after :reinitialize-only t)",
        "(reinitialize-instance (make-instance 'initarg-after) :only-this 19)",
        "(reinitialize-instance (make-instance 'initarg-after) :make-only t)",
        "(let ((object (make-instance 'initarg-redefined)))
           (make-instances-obsolete 'initarg-redefined)
           (slot-value object 'value))",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .expect("expected invalid initarg");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ProgramError),
            "{source}"
        );
    }
}
