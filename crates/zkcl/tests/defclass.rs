use zkcl::Runtime;

mod common;

use common::files::TempDir;

#[test]
fn standard_instances_inherit_slots_in_class_precedence_order() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defclass food () ((taste :initarg :taste :initform :food :accessor taste)))
           (defclass fruit (food) ((taste :initarg :fruit-taste :initform :fruit))
             (:default-initargs :taste :ripe))
           (defclass apple (fruit) ())
           (defclass spice (food) ((taste :initarg :spice-taste :initform :spice)))
           (defclass cinnamon (spice) ())
           (defclass pie (apple cinnamon) ((glaze :initarg :glaze))
             (:documentation "Fruit and spice."))
           (defparameter *pie-class* (find-class 'pie))
           (defparameter *pie* (make-instance *pie-class*))
           (defmethod layers ((food food)) (list :food))
           (defmethod layers ((apple apple)) (cons :apple (call-next-method)))
           (defmethod layers ((fruit fruit)) (cons :fruit (call-next-method)))
           (defmethod layers ((spice spice)) (cons :spice (call-next-method)))
           (defmethod layers ((cinnamon cinnamon)) (cons :cinnamon (call-next-method)))
           (defmethod layers ((pie pie)) (cons :pie (call-next-method)))
           (defclass spare-dessert (foretold-dessert) ())
           (defparameter *spare-type* (find-class 'spare-dessert))
           (defclass foretold-dessert () ((surprise :initform 17)))
           (defparameter *shared-init-count* 0)
           (let ((seed 7))
             (defclass lexical-initializers ()
               ((counter :allocation :class :initform (progn (incf *shared-init-count*) seed))
                (lexical :initarg :lexical :initform seed))
               (:default-initargs :lexical (1+ seed))))
           (defclass shared-child (lexical-initializers) ())
           (defclass local-child (lexical-initializers) ((counter)))
           (defclass independent-item () ((flavor :initarg :flavor :accessor taste)))"#,
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(eq (class-of *pie*) *pie-class*)",
        "(eq (class-of (make-instance 'standard-object)) (find-class 'standard-object))",
        "(eq (type-of *pie*) 'pie)",
        "(typep *pie* 'fruit)",
        "(typep *pie* (find-class 'cinnamon))",
        "(typep *pie* 'standard-object)",
        "(typep *pie-class* 'standard-class)",
        "(not (typep (make-instance 'fruit) *pie-class*))",
        "(string= (documentation *pie-class* t) \"Fruit and spice.\")",
        "(string= (documentation 'pie 'type) \"Fruit and spice.\")",
        "(equal (layers *pie*) '(:pie :apple :fruit :cinnamon :spice :food))",
        "(eq (taste *pie*) :ripe)",
        "(eq (taste (make-instance 'pie :spice-taste :borrowed)) :borrowed)",
        "(eq (setf (taste *pie*) :modified) :modified)",
        "(eq (taste *pie*) :modified)",
        "(eq (taste (make-instance 'independent-item :flavor :other)) :other)",
        "(eq (find-class 'spare-dessert) *spare-type*)",
        "(= (slot-value (make-instance *spare-type*) 'surprise) 17)",
        "(= *shared-init-count* 1)",
        "(= (slot-value (make-instance 'shared-child) 'lexical) 8)",
        "(= (slot-value (make-instance 'local-child) 'counter) 7)",
        "(let ((a (make-instance 'shared-child)) (b (make-instance 'lexical-initializers)))
            (setf (slot-value a 'counter) 9)
            (and (= (slot-value b 'counter) 9)
                 (= (slot-value (make-instance 'local-child) 'counter) 7)))",
    ] {
        assert!(
            !runtime
                .eval(source)
                .unwrap_or_else(|error| panic!("{source}: {error}"))[0]
                .is_nil(),
            "{source}"
        );
    }

    runtime
        .load_str("(defclass reverse-pie (cinnamon apple) ())")
        .unwrap();
    assert!(
        runtime
            .eval("(defclass impossible (pie reverse-pie) ())")
            .is_err()
    );
    assert!(runtime.eval("(find-class 'impossible nil)").unwrap()[0].is_nil());
    assert!(runtime.eval("(defclass food (apple) ())").is_err());
    assert!(!runtime.eval("(eq *pie-class* (class-of *pie*))").unwrap()[0].is_nil());
    runtime
        .load_str(
            "(defclass food () ((taste :initarg :taste :accessor taste) (addition :initform 44)))",
        )
        .unwrap();
    assert_eq!(
        runtime.eval("(slot-value *pie* 'addition)").unwrap()[0].as_fixnum(),
        Some(44)
    );

    runtime
        .load_str(
            "(defclass accessor-change () ((value :accessor changed-value)))
             (defmethod changed-value ((object accessor-change)) 91)
             (defclass accessor-change () ((value)))",
        )
        .unwrap();
    let accessors = runtime
        .eval(
            "(and (= (changed-value (make-instance 'accessor-change)) 91)
                  (null (find-method #'(setf changed-value) nil
                                     (list (find-class t) (find-class 'accessor-change)) nil)))",
        )
        .unwrap();
    assert!(!accessors[0].is_nil());

    let unwound = runtime
        .eval(
            "(eq (catch 'class-initializer
                   (defclass interrupted ()
                     ((value :allocation :class :initform (throw 'class-initializer :done)))))
                 :done)",
        )
        .unwrap();
    assert!(!unwound[0].is_nil());
    runtime
        .load_str("(defclass interrupted () ((value :allocation :class :initform 19)))")
        .unwrap();
    assert_eq!(
        runtime
            .eval("(slot-value (make-instance 'interrupted) 'value)")
            .unwrap()[0]
            .as_fixnum(),
        Some(19)
    );

    let temp_dir = TempDir::new("defclass");
    let input = temp_dir.path().join("classes.lisp");
    std::fs::write(
        &input,
        r#"
        (defclass compiled-parent ()
          ((shared :allocation :class :initform (incf *shared-init-count*))))
        (defclass compiled-child (compiled-parent)
          ((local :initform 9 :reader compiled-value)))
        (eval-when (:compile-toplevel)
          (unless (and (find-class 'compiled-parent nil) (find-class 'compiled-child nil))
            (error "Classes must be usable during compilation.")))
        (defmethod compiled-list ((child compiled-child))
           (list (slot-value child 'shared) (compiled-value child)))
    "#,
    )
    .unwrap();
    let output = runtime.compile_file(&input, None).unwrap();
    assert_eq!(
        runtime.eval("*shared-init-count*").unwrap()[0].as_fixnum(),
        Some(3)
    );
    runtime.collect();
    runtime.load_file(&output).unwrap();
    assert_eq!(
        runtime.eval("*shared-init-count*").unwrap()[0].as_fixnum(),
        Some(4)
    );
    assert!(
        !runtime
            .eval("(equal (compiled-list (make-instance 'compiled-child)) '(4 9))")
            .unwrap()[0]
            .is_nil()
    );

    assert!(
        !runtime
            .eval(
                "(let* ((slots (zk-mop:class-direct-slots (find-class 'compiled-child)))
                (local (car slots)))
           (and (eql 9 (zk-mop:slot-definition-initform local))
                (eql 9 (funcall (zk-mop:slot-definition-initfunction local)))
                (equal '(compiled-value) (zk-mop:slot-definition-readers local))))"
            )
            .unwrap()[0]
            .is_nil()
    );
}

#[test]
fn obsolete_instances_migrate_on_first_access() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass migration-base ()
          ((keep :initarg :keep)
           (gone :initarg :gone)
           (unbound)
           (becomes-shared :initarg :becomes-shared)
           (becomes-local :allocation :class :initform :shared)))
        (defclass migration-child (migration-base) ())
        (defparameter *migration-object*
          (make-instance 'migration-child :keep :kept :gone 41 :becomes-shared :local))
        (defparameter *migration-observations* nil)
        (defmethod update-instance-for-redefined-class :before
            ((object migration-child) added discarded values &rest initargs)
          (declare (ignore initargs))
          (push (list added discarded values) *migration-observations*)
          (when (member 'new added)
            (setf (slot-value object 'new) (1+ (getf values 'gone)))))
        (defclass migration-base ()
          ((new :initform :default) (keep :initarg :keep)
           (becomes-shared :allocation :class :initform :class)
           (becomes-local)))
    "#,
        )
        .unwrap();
    runtime.collect();
    for source in [
        "(null *migration-observations*)",
        "(progn (reinitialize-instance *migration-object* :keep :updated)
                (and (eql 42 (slot-value *migration-object* 'new))
                     (eq :updated (slot-value *migration-object* 'keep))
                     (eq :shared (slot-value *migration-object* 'becomes-local))
                     (eq :class (slot-value *migration-object* 'becomes-shared))))",
        "(destructuring-bind ((added discarded values)) *migration-observations*
           (and (equal added '(new))
                (= (length discarded) 3)
                (member 'gone discarded)
                (member 'unbound discarded)
                (member 'becomes-shared discarded)
                (= (length values) 4)
                (eql 41 (getf values 'gone))
                (eq :local (getf values 'becomes-shared))))",
        "(progn (make-instances-obsolete 'migration-child)
                (and (= 1 (length *migration-observations*))
                     (= 42 (slot-value *migration-object* 'new))
                     (equal '(nil nil nil) (first *migration-observations*))
                     (= 2 (length *migration-observations*)) ))",
        "(progn (make-instances-obsolete 'migration-base)
                (slot-value *migration-object* 'new)
                (= 3 (length *migration-observations*)))",
    ] {
        let value = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!value[0].is_nil(), "{source}");
    }
}

#[test]
fn superclass_edges_keep_class_identity_across_name_reuse() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval(
            "(progn
               (defclass original-parent () ((retained :initform :original)))
               (defclass other-parent () ())
               (defclass descendant (original-parent other-parent) ())
               (let* ((original (find-class 'original-parent))
                      (object (make-instance 'descendant)))
                 (setf (find-class 'original-parent) nil)
                 (defclass original-parent () ((replacement :initform :replacement)))
                 (defclass other-parent () ((added :initform :added)))
                 (setf (find-class 'parent-alias) original)
                 (defclass alias-descendant (parent-alias) ())
                 (let ((replacement (find-class 'original-parent))
                       (descendant (class-of object)))
                   (and (not (eq original replacement))
                        (typep object original)
                        (not (typep object replacement))
                        (not (subtypep 'descendant 'original-parent))
                        (equal (multiple-value-list (subtypep original replacement)) '(nil t))
                        (equal (multiple-value-list (subtypep descendant replacement)) '(nil t))
                        (equal (multiple-value-list (subtypep 'descendant original)) '(t t))
                        (eq (car (zk-mop:class-direct-superclasses descendant)) original)
                        (eq (slot-value object 'retained) :original)
                        (eq (slot-value object 'added) :added)
                        (not (slot-exists-p object 'replacement))
                        (typep (make-instance 'alias-descendant) original)
                        (progn (setf (class-name original) 'retired-parent)
                               (equal (multiple-value-list (subtypep descendant original))
                                      '(t t)))))))",
        )
        .unwrap();
    assert!(!result[0].is_nil());
}

#[test]
fn class_aliases_designate_the_same_superclass() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defclass association-base () ())
        (setf (find-class 'association-alias) (find-class 'association-base))
        (defclass association-child (association-alias) ())
        (defclass association-other () ())
    "#,
        )
        .unwrap();
    let result = runtime
        .eval(
            "(and
               (typep (make-instance 'association-base) 'association-alias)
               (typep (make-instance 'association-child) 'association-alias)
               (not (typep (make-instance 'association-other) 'association-alias))
               (equal (multiple-value-list
                        (subtypep 'association-child 'association-alias))
                      '(t t))
               (equal (multiple-value-list
                        (subtypep 'association-alias 'association-base))
                      '(t t))
               (equal (multiple-value-list
                        (subtypep 'association-base 'association-alias))
                      '(t t))
               (eq (car (zk-mop:class-direct-superclasses
                          (find-class 'association-child)))
                   (find-class 'association-base)))",
        )
        .unwrap();
    assert!(!result[0].is_nil());
}
