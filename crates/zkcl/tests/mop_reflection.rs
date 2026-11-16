use zkcl::Runtime;

fn reflection_runtime() -> Runtime {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (let ((initial 12))
              (defclass mop-base ()
                ((value :initform (1+ initial) :initarg :value :type number
                        :allocation :class :accessor mop-value))
                (:default-initargs :value (+ initial 2))))
            (defclass mop-child (mop-base)
              ((value :type integer :allocation :instance :initarg :other)))
            (defclass mop-documented ()
              ((value :documentation "slot docs")))
            (defclass mop-unfinished (mop-later) ((pending)))
            (defparameter *base-class* (find-class 'mop-base))
            (defparameter *child-class* (find-class 'mop-child))
            (setf (find-class 'mop-alias) *child-class*)
            (defgeneric mop-select (x y) (:argument-precedence-order y x))
            (defparameter *mop-literal* (list :literal))
            (defparameter *literal-method*
              (defmethod mop-select ((x (eql *mop-literal*)) (y integer)) :literal))
            (defparameter *integer-method*
              (defmethod mop-select ((x cons) (y integer)) :integer))
            (defparameter *fallback-method*
              (defmethod mop-select ((x cons) y) :fallback))
            (defparameter *alias-method*
              (defmethod mop-select ((x mop-alias) y) :alias))
            (defgeneric mop-documented-generic (x)
              (:documentation "generic docs"))
            (defparameter *documented-method*
              (defmethod mop-documented-generic ((x mop-documented))
                "method docs"
                :documented))
            "#,
        )
        .unwrap();
    runtime.collect();

    runtime
}

fn assert_reflection_forms(sources: &[&str]) {
    let runtime = reflection_runtime();
    for source in sources {
        let value = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!value[0].is_nil(), "{source}");
    }
}

#[test]
fn inherited_slot_definitions_report_the_effective_class_layout() {
    assert_reflection_forms(&[
        "(and (zk-mop:class-finalized-p *child-class*)
              (equal (zk-mop:class-direct-superclasses *child-class*) (list *base-class*))
              (equal (mapcar #'class-name (zk-mop:class-precedence-list *child-class*))
                     '(mop-child mop-base standard-object t)))",
        "(let* ((direct (car (zk-mop:class-direct-slots *base-class*)))
                (effective (car (zk-mop:class-slots *child-class*))))
           (and (typep direct 'zk-mop:standard-direct-slot-definition)
                (typep effective 'zk-mop:effective-slot-definition)
                (eq effective (car (zk-mop:class-slots *child-class*)))
                (eq 'value (zk-mop:slot-definition-name effective))
                (equal '(mop-value) (zk-mop:slot-definition-readers direct))
                (equal '((setf mop-value)) (zk-mop:slot-definition-writers direct))
                (eq :class (zk-mop:slot-definition-allocation direct))
                (eq :instance (zk-mop:slot-definition-allocation effective))
                (equal '(and integer number) (zk-mop:slot-definition-type effective))
                (equal '(:other :value) (zk-mop:slot-definition-initargs effective))
                (equal '(1+ initial) (zk-mop:slot-definition-initform effective))
                (= 13 (funcall (zk-mop:slot-definition-initfunction effective)))
                (= 14 (slot-value (make-instance *child-class*) 'value))))",
        "(let ((defaults (zk-mop:class-default-initargs *child-class*)))
           (and (null (zk-mop:class-direct-default-initargs *child-class*))
                (equal '(:value (+ initial 2)) (subseq (car defaults) 0 2))
                (= 14 (funcall (third (car defaults))))))",
    ]);
}

#[test]
fn resolving_a_forward_class_finalizes_its_dependent_class() {
    assert_reflection_forms(&["(let* ((class (find-class 'mop-unfinished))
                (parent (car (zk-mop:class-direct-superclasses class))))
           (and (typep parent 'zk-mop:forward-referenced-class)
                (eq parent (find-class 'mop-later))
                (not (zk-mop:class-finalized-p parent))
                (not (zk-mop:class-finalized-p class))
                (handler-case (progn (zk-mop:class-slots class) nil)
                  (program-error () t))
                (progn (eval '(defclass mop-later () ())) (zk-mop:class-finalized-p class))
                (eq parent (find-class 'mop-later))
                (typep parent 'standard-class)
                (eq parent (car (zk-mop:class-direct-superclasses class)))))"]);
}

#[test]
fn method_metaobjects_track_applicability_after_method_removal() {
    assert_reflection_forms(&[
        "(let* ((generic #'mop-select)
                (specializer (car (zk-mop:method-specializers *literal-method*))))
           (and (typep #'find-method 'generic-function)
                (typep #'zk-mop:class-slots 'generic-function)
                (typep specializer 'zk-mop:eql-specializer)
                (eq (type-of specializer) 'zk-mop:eql-specializer)
                (eq specializer (zk-mop:intern-eql-specializer *mop-literal*))
                (eq *mop-literal* (zk-mop:eql-specializer-object specializer))
                (eq *literal-method* (find-method generic nil (list specializer (find-class 'integer))))
                (eq generic (zk-mop:method-generic-function *literal-method*))
                (member *literal-method* (zk-mop:generic-function-methods generic))
                (equal '(x y) (zk-mop:method-lambda-list *literal-method*))
                (equal '(y x) (zk-mop:generic-function-argument-precedence-order generic))
                (eq :literal (mop-select *mop-literal* 1))
                (eq :alias (mop-select (make-instance *child-class*) 1))
                (eq *alias-method* (find-method generic nil (list 'mop-alias (find-class t))))))",
        "(and (eq (zk-mop:intern-eql-specializer (+ (expt 2 70) 1))
                  (zk-mop:intern-eql-specializer (+ (expt 2 70) 1)))
              (not (eq (zk-mop:intern-eql-specializer (list :literal))
                       (zk-mop:intern-eql-specializer *mop-literal*))))",
        "(multiple-value-bind (methods known)
             (zk-mop:compute-applicable-methods-using-classes
              #'mop-select (list (find-class 'cons) (find-class 'integer)))
           (declare (ignore methods))
           (not known))",
        r#"(multiple-value-bind (methods known)
             (zk-mop:compute-applicable-methods-using-classes
              #'mop-select (list (find-class 'cons) (find-class 'string)))
           (and known (equal methods (list *fallback-method*))
                (equal methods (compute-applicable-methods #'mop-select '((nil) "a" :ignored)))))"#,
        "(progn (remove-method #'mop-select *literal-method*)
                (and (null (zk-mop:method-generic-function *literal-method*))
                     (not (member *literal-method* (zk-mop:generic-function-methods #'mop-select)))
                     (eq :integer (mop-select *mop-literal* 1))))",
    ]);
}

#[test]
fn mop_metaobjects_expose_their_lisp_documentation() {
    assert_reflection_forms(&["(let* ((class (find-class 'mop-documented))
                (slot (car (zk-mop:class-direct-slots class)))
                (prototype (zk-mop:class-prototype class)))
           (and (typep prototype 'mop-documented)
                (equal (zk-mop:slot-definition-documentation slot) \"slot docs\")
                (equal (zk-mop:generic-function-documentation #'mop-documented-generic)
                       \"generic docs\")
                (equal (zk-mop:method-documentation *documented-method*) \"method docs\")
                (null (zk-mop:method-qualifiers *documented-method*))))"]);
}

#[test]
fn slot_using_class_methods_observe_the_same_slot_state() {
    assert_reflection_forms(&["(let* ((class (find-class 'mop-documented))
                (slot (car (zk-mop:class-slots class)))
                (object (make-instance class)))
           (and (not (zk-mop:slot-boundp-using-class class object slot))
                (slot-exists-p object 'value)
                (eq (setf (zk-mop:slot-value-using-class class object slot) 9) 9)
                (= (zk-mop:slot-value-using-class class object slot) 9)
                (eq (zk-mop:slot-makunbound-using-class class object slot) object)
                (not (zk-mop:slot-boundp-using-class class object slot))))"]);
}
