use std::fs;

use zkcl::{ConditionClass, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::eval;

#[test]
fn structure_class_effective_slots_include_the_inherited_layout() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defstruct reflected-parent (value (+ 2 3) :type integer) (locked 8 :read-only t))
             (defstruct (reflected-child (:include reflected-parent (value 17 :type fixnum)))
               extra)
             (defstruct (reflected-inherited (:include reflected-parent) (:constructor nil)))
             (defstruct (reflected-list (:type list) :named) value)
             (defstruct (reflected-vector (:type vector) :named) value)
             (defparameter *reflected-instance* (make-reflected-child :extra :local))
             (defparameter *reflected-class* (class-of *reflected-instance*))
             (defparameter *reflected-direct-slot*
               (first (zk-mop:class-direct-slots *reflected-class*)))
             (defparameter *reflected-effective-slot*
               (first (zk-mop:class-slots *reflected-class*)))",
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(and (typep *reflected-class* 'structure-class)
              (null (find-class 'reflected-list nil))
              (null (find-class 'reflected-vector nil))
              (equal (mapcar #'zk-mop:slot-definition-name
                             (zk-mop:class-direct-slots *reflected-class*))
                     '(value extra))
              (null (zk-mop:class-direct-slots (find-class 'reflected-inherited)))
              (equal (mapcar #'zk-mop:slot-definition-name
                             (zk-mop:class-slots *reflected-class*))
                     '(value locked extra))
              (equal (mapcar #'zk-mop:slot-definition-location
                             (zk-mop:class-slots *reflected-class*)) '(0 1 2)))",
        "(let ((direct *reflected-direct-slot*) (effective *reflected-effective-slot*))
           (and (typep direct 'zk-mop:direct-slot-definition)
                (typep effective 'zk-mop:effective-slot-definition)
                (eq direct (first (zk-mop:class-direct-slots *reflected-class*)))
                (eq effective (first (zk-mop:class-slots *reflected-class*)))
                (eq (zk-mop:slot-definition-allocation effective) :instance)
                (eq (zk-mop:slot-definition-type effective) 'fixnum)
                (eql (zk-mop:slot-definition-initform effective) 17)
                (equal (zk-mop:slot-definition-initargs effective) '(:value))
                (equal (zk-mop:slot-definition-readers direct) '(reflected-child-value))
                (null (zk-mop:slot-definition-initargs
                       (first (zk-mop:class-slots (find-class 'reflected-inherited)))))))",
        "(let ((names (mapcar #'zk-mop:slot-definition-name
                             (zk-mop:class-slots *reflected-class*))))
           (equal (mapcar (lambda (name) (slot-value *reflected-instance* name)) names)
                  '(17 8 :local)))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn structure_slot_access_uses_the_reflected_native_location() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defstruct protocol-parent (value 10))
             (defstruct (protocol-child (:include protocol-parent)) (extra 20))
             (defparameter *record* (make-protocol-child))
             (defparameter *record-class* (class-of *record*))
             (defparameter *parent-class* (find-class 'protocol-parent))
             (defparameter *record-slot* (first (zk-mop:class-slots *record-class*)))
             (defvar *visits* 0)
             (defmethod zk-mop:slot-value-using-class :around
                 ((class (eql *record-class*)) (object protocol-child) slot)
               (when (eq slot *record-slot*) (incf *visits*))
               (call-next-method))",
        )
        .unwrap();
    runtime.collect();
    assert_true(
        &runtime,
        "(and (= (slot-value *record* 'value) 10)
              (= (zk-mop:slot-value-using-class *record-class* *record* *record-slot*) 10)
              (= *visits* 2)
              (zk-mop:slot-boundp-using-class *record-class* *record* *record-slot*)
              (= (setf (zk-mop:slot-value-using-class *record-class* *record* *record-slot*) 73) 73)
              (= (protocol-parent-value *record*) 73)
              (handler-case
                  (progn
                    (zk-mop:slot-value-using-class *record-class* *record*
                                                  (first (zk-mop:class-slots *parent-class*)))
                    nil)
                (error () t)))",
    );
}

#[test]
fn included_structures_preserve_the_parent_slot_layout() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *zkcl-struct-default-calls* 0)
             (let ((seed 42))
               (defstruct (struct-parent (:conc-name nil))
                 (struct-parent-value
                   (progn (incf *zkcl-struct-default-calls*) seed))
                 (struct-parent-note (list 7) :read-only t)))
             (let ((seed 19))
               (defstruct (struct-child
                            (:include struct-parent (struct-parent-value seed))
                            (:conc-name nil))
                 (struct-child-marker 11)))
             (defstruct (struct-options
                          (:constructor build-struct-options)
                          (:constructor build-struct-boa
                             (x &optional y (z 13 supplied)
                              &rest tail &aux (sum (+ x y z))))
                          (:predicate struct-options?)
                          (:copier nil))
                \"Constructor and predicate aliases.\"
                x (y 17) z supplied tail sum)",
        )
        .unwrap();

    let numbers = runtime
        .eval(
            "(let ((parent (make-struct-parent))
                   (child (make-struct-child)))
               (make-struct-parent :struct-parent-value 10)
               (values (struct-parent-value parent)
                       (struct-parent-value child)
                       (struct-child-marker child)
                       *zkcl-struct-default-calls*))",
        )
        .unwrap();
    assert_eq!(
        numbers
            .iter()
            .map(|root| root.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(42), Some(19), Some(11), Some(1)]
    );

    assert_true(
        &runtime,
        "(let* ((child (make-struct-child))
                (copy (copy-struct-parent child)))
           (and (struct-parent-p child)
                (struct-child-p copy)
                (typep copy 'struct-parent)
                (typep copy 'structure-object)
                (eq (type-of copy) 'struct-child)
                (not (eq child copy))
                (eq (struct-parent-note child) (struct-parent-note copy))
                (= (setf (struct-parent-value copy) 33) 33)
                (= (struct-parent-value child) 19)
                (= (struct-parent-value copy) 33)))",
    );
    assert_true(
        &runtime,
        "(let ((defaulted (build-struct-boa 1))
               (supplied (build-struct-boa 1 2 3 4 5))
               (keyword (build-struct-options :x 52)))
            (and (struct-options? defaulted)
                 (not (fboundp 'make-struct-options))
                 (not (fboundp 'copy-struct-options))
                 (= (struct-options-y defaulted) 17)
                 (= (struct-options-sum defaulted) 31)
                 (not (struct-options-supplied defaulted))
                 (struct-options-supplied supplied)
                 (equal (struct-options-tail supplied) '(4 5))
                 (= (struct-options-sum supplied) 6)
                 (= (struct-options-x keyword) 52)))",
    );
    assert_true(
        &runtime,
        "(= (struct-parent-value #S(struct-parent :struct-parent-value 83)) 83)",
    );
    assert_true(
        &runtime,
        "(= (struct-options-x #S(struct-options :x 83)) 83)",
    );
    let doc = runtime
        .eval("(documentation 'struct-options 'type)")
        .unwrap();
    assert_eq!(
        runtime.string(&doc[0]).as_deref(),
        Some("Constructor and predicate aliases.")
    );
    let error = runtime.eval("(struct-parent-value 1)").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    assert!(
        runtime
            .eval("(setf (struct-parent-note (make-struct-parent)) nil)")
            .is_err()
    );
    assert!(
        runtime
            .eval("(defstruct bad-structure duplicate duplicate)")
            .is_err()
    );

    assert!(
        runtime
            .eval("(defstruct (illegal-accessor (:conc-name nil)) car)")
            .is_err()
    );
    assert_true(
        &runtime,
        "(let ((pair (list 1))) (= (setf (car pair) 2) 2))",
    );
    runtime.collect();
    assert_true(
        &runtime,
        "(= (struct-parent-value (make-struct-parent)) 42)",
    );
}

#[test]
fn loaded_structure_accessors_reach_inherited_slots() {
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("defstruct");
    let input = directory.path().join("structure.lisp");
    fs::write(
        &input,
        "(defstruct fasl-parent (value 30))
         (defstruct (fasl-child (:include fasl-parent (value 40))) (tag 2))
         (defun fasl-structure-answer ()
           (let ((instance (make-fasl-child)))
             (incf (fasl-parent-value instance) (fasl-child-tag instance))))",
    )
    .unwrap();
    let output = runtime.compile_file(&input, None).unwrap();
    fs::remove_file(input).unwrap();
    runtime.collect();
    runtime.load_file(&output).unwrap();
    assert_true(&runtime, "(= (fasl-structure-answer) 42)");
}

#[test]
fn printer_options_define_inheritable_print_object_methods() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defstruct (printed (:print-object emit-printed)) payload)
               (defun emit-printed (object stream)
                 (write-string (if *print-escape* "E/" "A/") stream)
                 (write (printed-payload object) :stream stream))
               (defstruct (printed-child (:include printed)))
               (defstruct (default-child (:include printed) (:print-object)))
               (defstruct (default-function (:include printed) (:print-function)))
               (defstruct (printed-leaf (:include printed)
                             (:print-object (lambda (o s) (declare (ignore o))
                                              (write-string "leaf" s)))))
               (defstruct (printed-depth (:print-function emit-depth)))
               (defun emit-depth (object stream depth)
                 (declare (ignore object))
                 (format stream "D~D" depth))
               (defstruct (printed-setf (:print-object (setf printer))))
               (defun (setf printer) (object stream)
                 (declare (ignore object))
                 (write-string "setf" stream))"#,
        )
        .unwrap();
    assert_true(
        &runtime,
        r#"(let ((parent (make-printed :payload "v"))
                 (child (make-printed-child :payload "v"))
                 (plain (make-default-child :payload 7))
                 (defaulted (make-default-function :payload 11))
                 (leaf (make-printed-leaf))
                 (depth (make-printed-depth)))
             (and (string= (prin1-to-string parent) "E/\"v\"")
                  (string= (princ-to-string child) "A/v")
                  (string= (princ-to-string (list parent leaf)) "(A/v leaf)")
                  (string= (prin1-to-string depth) "D0")
                  (string= (prin1-to-string (list depth)) "(D1)")
                  (string= (with-output-to-string (s) (print-object leaf s)) "leaf")
                  (string= (prin1-to-string (make-printed-setf)) "setf")
                  (= (printed-payload (read-from-string (prin1-to-string plain))) 7)
                  (= (printed-payload (read-from-string (prin1-to-string defaulted))) 11)
                  (null (find-method #'print-object nil
                                     (list (find-class 'printed-child) (find-class 't)) nil))
                  (find-method #'print-object nil
                               (list (find-class 'default-child) (find-class 't)))))"#,
    );
    for invalid in [
        "(defstruct (bad-printers (:print-function) (:print-object)))",
        "(defstruct (bad-typed-printer (:type list) :named (:print-object emit-printed)))",
        "(defstruct (bad-typed-function (:print-function) (:type vector)))",
    ] {
        assert!(runtime.eval(invalid).is_err(), "{invalid}");
    }
}

fn assert_true(runtime: &Runtime, source: &str) {
    let values = eval(runtime, source);
    assert_eq!(values.len(), 1);
    assert!(!values[0].is_nil(), "{source}");
}
