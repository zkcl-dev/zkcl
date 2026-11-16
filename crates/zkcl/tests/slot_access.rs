use zkcl::Runtime;

#[test]
fn bootstrap_exposes_instance_storage_access() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defclass ordinary-instance () ((value :initform 11)))
             (defclass callable-instance () ((value :initform 12))
               (:metaclass zk-mop:funcallable-standard-class))",
        )
        .unwrap();
    for source in [
        "(let* ((instance (make-instance 'ordinary-instance))
                (slot (first (zk-mop:class-slots (class-of instance))))
                (location (zk-mop:slot-definition-location slot)))
           (and (= 11 (zk-mop:standard-instance-access instance location))
                (= 21 (setf (zk-mop:standard-instance-access instance location) 21))
                (= 21 (slot-value instance 'value))))",
        "(let* ((instance (make-instance 'callable-instance))
                (slot (first (zk-mop:class-slots (class-of instance))))
                (location (zk-mop:slot-definition-location slot)))
           (zk-mop:set-funcallable-instance-function instance (lambda (n) (+ n 20)))
           (and (= 12 (zk-mop:funcallable-standard-instance-access instance location))
                (= 22 (setf (zk-mop:funcallable-standard-instance-access instance location) 22))
                (= 22 (slot-value instance 'value))
                (= 23 (funcall instance 3))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn warmed_accessors_observe_changes_to_the_slot_protocol() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass warm-slot () ((payload :initform 9 :accessor warm-payload)))
        (defparameter *warm-object* (make-instance 'warm-slot))
        (dotimes (i 3) (assert (= (warm-payload *warm-object*) 9)))
        (let ((method (defmethod zk-mop:slot-value-using-class :around
                           (class (object (eql *warm-object*)) slot)
                         (declare (ignore class slot))
                         (+ 100 (call-next-method)))))
          (assert (= (warm-payload *warm-object*) 109))
          (remove-method #'zk-mop:slot-value-using-class method)
          (assert (= (warm-payload *warm-object*) 9)))
        (let ((method (defmethod (setf zk-mop:slot-value-using-class) :around
                          ((new (eql :skip)) class (object warm-slot) slot)
                        (declare (ignore class slot))
                        (throw 'skip-write :skipped))))
          (assert (eq (catch 'skip-write (setf (warm-payload *warm-object*) :skip)) :skipped))
          (assert (= (warm-payload *warm-object*) 9))
          (remove-method #'(setf zk-mop:slot-value-using-class) method))
        (let* ((method (find-method #'warm-payload nil (list (find-class 'warm-slot))))
               (public (zk-mop:method-function method)))
          (assert (= (funcall public (list *warm-object*) nil) 9)))
        (let ((gf #'zk-mop:slot-value-using-class))
          (zk-mop:set-funcallable-instance-function gf
            (lambda (class object slot) (declare (ignore class object slot)) :replacement))
          (assert (eq (warm-payload *warm-object*) :replacement)))
        "#,
        )
        .unwrap();
}

#[test]
fn an_accessor_method_initializer_can_replace_the_public_function() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass wrapping-reader (zk-mop:standard-reader-method) ())
        (defmethod zk-mop:reader-method-class :around (class slot &rest options)
          (declare (ignore class options))
          (if (eq (zk-mop:slot-definition-name slot) 'wrapped-payload)
              (find-class 'wrapping-reader)
              (call-next-method)))
        (defmethod initialize-instance :around ((m wrapping-reader) &rest initargs &key function)
          (apply #'call-next-method m
                 :function (lambda (args next) (list :wrapped (funcall function args next)))
                 initargs))
        (defclass wrapped-slot () ((wrapped-payload :initform 19 :reader wrapped-reader)))
        (let* ((object (make-instance 'wrapped-slot))
               (method (find-method #'wrapped-reader nil (list (class-of object)))))
          (dotimes (i 3) (assert (equal (wrapped-reader object) '(:wrapped 19))))
          (assert (equal (funcall (zk-mop:method-function method) (list object) nil)
                         '(:wrapped 19))))
        "#,
        )
        .unwrap();
}

#[test]
fn slot_value_dispatches_through_effective_slot_definitions() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defclass mop-protocol-base ()
              ((shared :allocation :class :initform 31
                       :accessor mop-protocol-shared)
               (local :initarg :local)))
            (defclass mop-protocol-child (mop-protocol-base) ())

            (defmethod slot-unbound
                ((class standard-class) (object mop-protocol-child) name)
              (if (eq name 'local)
                  (throw 'mop-protocol-unbound 77)
                  (call-next-method)))
            (defparameter *mop-protocol-reads* nil)
            (defmethod zk-mop:slot-value-using-class :around
                ((class standard-class) (object mop-protocol-child) slot)
              (push (zk-mop:slot-definition-name slot) *mop-protocol-reads*)
              (call-next-method))

            (defparameter *mop-protocol-class* (find-class 'mop-protocol-child))
            (defparameter *mop-protocol-object* (make-instance *mop-protocol-class*))
            (defparameter *mop-protocol-slots*
              (zk-mop:class-slots *mop-protocol-class*))
            "#,
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(and (typep #'zk-mop:slot-value-using-class 'generic-function)
              (typep #'(setf zk-mop:slot-value-using-class) 'generic-function))",
        "(let* ((class *mop-protocol-class*)
                (object *mop-protocol-object*)
                (shared (find 'shared *mop-protocol-slots*
                              :key #'zk-mop:slot-definition-name))
                (other (make-instance class)))
           (and (= (zk-mop:slot-value-using-class class object shared) 31)
                (eq (setf (zk-mop:slot-value-using-class class object shared) 41)
                    41)
                (= (zk-mop:slot-value-using-class class other shared) 41)))",
        "(let ((object *mop-protocol-object*))
           (setq *mop-protocol-reads* nil)
           (and (= (slot-value object 'shared) 41)
                (= (mop-protocol-shared object) 41)
                (equal (reverse *mop-protocol-reads*) '(shared shared))))",
        "(let* ((class *mop-protocol-class*)
                (object *mop-protocol-object*)
                (local (find 'local *mop-protocol-slots*
                             :key #'zk-mop:slot-definition-name)))
           (and (not (zk-mop:slot-boundp-using-class class object local))
                (= (catch 'mop-protocol-unbound
                     (slot-value object 'local))
                   77)))",
        "(let* ((class *mop-protocol-class*)
                (prototype (zk-mop:class-prototype class))
                (local (find 'local (zk-mop:class-slots class)
                             :key #'zk-mop:slot-definition-name)))
           (and (typep prototype 'mop-protocol-child)
                (not (zk-mop:slot-boundp-using-class
                       class prototype local))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }

    runtime.collect();
    let values = runtime
        .eval(
            "(let* ((class *mop-protocol-class*)
                    (object *mop-protocol-object*)
                    (shared (find 'shared (zk-mop:class-slots class)
                                  :key #'zk-mop:slot-definition-name)))
               (and (= (zk-mop:slot-value-using-class class object shared) 41)
                    (eq (class-of object) class)))",
        )
        .unwrap();
    assert!(!values[0].is_nil());
}

#[test]
fn condition_slot_access_uses_effective_slot_definitions() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (define-condition mop-type-problem (simple-type-error)
          ((extra :initform 7 :reader mop-problem-extra)))
        (defmethod zk-mop:slot-value-using-class :around
            ((class class) (object mop-type-problem) slot)
          (if (eq (zk-mop:slot-definition-name slot) 'extra)
              (1+ (call-next-method))
              (call-next-method)))
        (defparameter *mop-type-class* (find-class 'simple-type-error))
        (defparameter *mop-type-slots* (zk-mop:class-slots *mop-type-class*))
    "#,
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(and (= (length (zk-mop:class-direct-slots (find-class 'type-error))) 2)
              (null (zk-mop:class-direct-slots *mop-type-class*))
              (= (length *mop-type-slots*) 4)
              (every #'eq *mop-type-slots* (zk-mop:class-slots *mop-type-class*)))",
        "(let* ((prototype (zk-mop:class-prototype *mop-type-class*))
                (condition (make-condition *mop-type-class*))
                (datum (find :datum *mop-type-slots*
                             :key (lambda (slot)
                                    (car (zk-mop:slot-definition-initargs slot))))))
           (and (eq (class-of prototype) *mop-type-class*)
                (not (zk-mop:slot-boundp-using-class *mop-type-class* condition datum))
                (progn (setf (zk-mop:slot-value-using-class *mop-type-class* condition datum) 42)
                       (= (type-error-datum condition) 42))
                (eq (zk-mop:slot-makunbound-using-class *mop-type-class* condition datum) condition)
                (not (slot-boundp condition (zk-mop:slot-definition-name datum)))))",
        "(let ((condition (make-condition 'mop-type-problem :datum 9)))
           (and (= (mop-problem-extra condition) 8)
                (= (type-error-datum condition) 9)))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn shared_initialize_uses_the_slot_protocol_in_the_callers_control_context() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"
      (progn
        (defclass initialized-via-mop ()
          ((value :initarg :value :initform 7) (skipped :initform (error "unexpected initform"))))
        (defmethod zk-mop:slot-boundp-using-class :around
            ((class standard-class) (object initialized-via-mop) slot)
          (or (eq (zk-mop:slot-definition-name slot) 'skipped)
              (call-next-method)))
        (defmethod (setf zk-mop:slot-value-using-class) :around
            (value (class standard-class) (object initialized-via-mop) slot)
          (if (eq value :exit)
              (throw 'slot-init :escaped)
              (call-next-method (1+ value) class object slot)))
        (let ((defaulted (make-instance 'initialized-via-mop))
              (explicit (make-instance 'initialized-via-mop :value 3)))
          (values
            (= 8 (slot-value defaulted 'value))
            (= 4 (slot-value explicit 'value))
            (= 6 (slot-value (reinitialize-instance explicit :value 5) 'value))
            (eq :escaped (catch 'slot-init
                           (make-instance 'initialized-via-mop :value :exit))))))
    "#,
        )
        .unwrap();
    assert_eq!(values.len(), 4);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn slot_protocol_observes_the_current_layout_after_redefinition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass mop-layout () ((old :initform 3)))
        (defparameter *mop-layout-object* (make-instance 'mop-layout))
        (defparameter *mop-layout-old-slot* (car (zk-mop:class-slots (find-class 'mop-layout))))
        (defparameter *mop-layout-reads* nil)
        (defmethod zk-mop:slot-value-using-class :around
            ((class standard-class) (object mop-layout) slot)
          (push slot *mop-layout-reads*)
          (call-next-method))
        (defclass mop-layout () ((new :initform 7) (old)))
    "#,
        )
        .unwrap();
    runtime.collect();

    let values = runtime.eval(r#"
        (let* ((class (find-class 'mop-layout))
               (object *mop-layout-object*)
               (slots (zk-mop:class-slots class))
               (old (find 'old slots :key #'zk-mop:slot-definition-name))
               (new (find 'new slots :key #'zk-mop:slot-definition-name)))
          (and (= 3 (slot-value object 'old))
               (= 7 (slot-value object 'new))
               (equal (list new old) *mop-layout-reads*)
               (handler-case (progn (zk-mop:slot-value-using-class class object *mop-layout-old-slot*) nil)
                 (error () t))))
    "#).unwrap();
    assert!(!values[0].is_nil());
}
