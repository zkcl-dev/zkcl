use zkcl::Runtime;

#[test]
fn inspector_slots_use_mop_reads_writes_and_preserve_identity_across_gc() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defclass inspected () ((item :initform nil) (unset)
                               (shared :allocation :class :initform 10)))
        (defparameter *inspected* (make-instance 'inspected))
        (defparameter *inspected-other* (make-instance 'inspected))
        (defparameter *inspection-writes* 0)
        (defmethod (setf zk-mop:slot-value-using-class) :around
            (value (class standard-class) (object inspected) slot)
          (incf *inspection-writes*)
          (zk:gc)
          (call-next-method))
        (defmethod zk-mop:slot-value-using-class :around
            ((class standard-class) (object inspected) slot)
          (zk:gc)
          (if (eq (zk-mop:slot-definition-name slot) 'shared)
              (1+ (call-next-method))
              (call-next-method)))
        "#,
    )
    .unwrap();
    let object = rt.eval("*inspected*").unwrap().remove(0);
    let page = rt.inspect_object(&object, 0, 10).unwrap();
    assert_eq!(page.total, 3);
    assert!(page.parts.iter().all(|part| part.writable));
    assert!(page.parts[1].value.is_none());
    assert_eq!(page.parts[2].value.as_ref().unwrap().as_fixnum(), Some(11));
    rt.replace_inspected_part(&object, 0, &object).unwrap();
    let value = rt.eval("42").unwrap().remove(0);
    rt.replace_inspected_part(&object, 1, &value).unwrap();
    rt.replace_inspected_part(&object, 2, &value).unwrap();
    rt.collect();
    let values = rt
        .eval(
            "(values (eq *inspected* (slot-value *inspected* 'item))
                     (slot-value *inspected* 'unset)
                     (slot-value *inspected-other* 'shared)
                     *inspection-writes*)",
        )
        .unwrap();
    assert!(values[0].is_t());
    assert_eq!(values[1].as_fixnum(), Some(42));
    assert_eq!(values[2].as_fixnum(), Some(43));
    assert_eq!(values[3].as_fixnum(), Some(3));
    // The original page owns its values even after subsequent Lisp collections.
    assert_eq!(page.parts[2].value.as_ref().unwrap().as_fixnum(), Some(11));
}

#[test]
fn inspector_migrates_obsolete_instances_before_selecting_slots() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defclass changing () ((removed :initform 1) (kept :initform 2)))
         (defparameter *changing* (make-instance 'changing))
         (defparameter *inspection-updates* 0)
         (defmethod update-instance-for-redefined-class :after
             ((object changing) added discarded plist &rest initargs)
           (declare (ignore added discarded plist initargs))
           (incf *inspection-updates*) (zk:gc))",
    )
    .unwrap();
    let object = rt.eval("*changing*").unwrap().remove(0);
    rt.load_str("(defclass changing () ((kept :initform 2) (added :initform 3)))")
        .unwrap();
    let value = rt.eval("99").unwrap().remove(0);
    rt.replace_inspected_part(&object, 0, &value).unwrap();
    let page = rt.inspect_object(&object, 0, 10).unwrap();
    assert_eq!(page.parts[0].label, "KEPT");
    assert_eq!(page.parts[0].value.as_ref().unwrap().as_fixnum(), Some(99));
    assert_eq!(page.parts[1].label, "ADDED");
    assert_eq!(page.parts[1].value.as_ref().unwrap().as_fixnum(), Some(3));
    assert_eq!(
        rt.eval("*inspection-updates*").unwrap()[0].as_fixnum(),
        Some(1)
    );
}

#[test]
fn inspector_includes_custom_allocations_and_uses_their_slot_protocol() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defclass inspector-table-meta (standard-class)
          ((values :initform (make-hash-table :test 'eq) :reader inspector-table-values)))
        (defmethod zk-mop:validate-superclass
            ((class inspector-table-meta) (parent standard-class)) t)
        (defclass inspector-remote-slot (zk-mop:standard-effective-slot-definition) ())
        (defmethod zk-mop:effective-slot-definition-class
            ((class inspector-table-meta) &rest options)
          (if (eq (getf options :allocation) :table)
              (find-class 'inspector-remote-slot) (call-next-method)))
        (defmethod zk-mop:slot-value-using-class
            ((class inspector-table-meta) object (slot inspector-remote-slot))
          (gethash object (inspector-table-values class)))
        (defmethod (setf zk-mop:slot-value-using-class)
            (value (class inspector-table-meta) object (slot inspector-remote-slot))
          (zk:gc)
          (setf (gethash object (inspector-table-values class)) value))
        (defmethod zk-mop:slot-boundp-using-class
            ((class inspector-table-meta) object (slot inspector-remote-slot))
          (nth-value 1 (gethash object (inspector-table-values class))))
        (defclass inspector-remote () ((remote :allocation :table) (local :initform 2))
          (:metaclass inspector-table-meta))
        (defparameter *inspector-remote* (make-instance 'inspector-remote))
        "#,
    )
    .unwrap();
    let object = rt.eval("*inspector-remote*").unwrap().remove(0);
    let page = rt.inspect_object(&object, 0, 10).unwrap();
    assert_eq!(page.total, 2);
    let remote = page
        .parts
        .iter()
        .find(|part| part.label == "REMOTE")
        .unwrap();
    assert!(remote.writable);
    assert!(remote.value.is_none());
    rt.replace_inspected_part(&object, remote.index, &object)
        .unwrap();
    assert!(
        rt.eval("(eq *inspector-remote* (slot-value *inspector-remote* 'remote))")
            .unwrap()[0]
            .is_t()
    );
}

#[test]
fn inspector_hash_values_are_writable_and_keys_keep_their_identity() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defparameter *inspected-table* (make-hash-table :test 'equal))
         (defparameter *inspected-key* (list :original))
         (setf (gethash *inspected-key* *inspected-table*) (list 1))",
    )
    .unwrap();
    let table = rt.eval("*inspected-table*").unwrap().remove(0);
    let page = rt.inspect_object(&table, 0, 1).unwrap();
    assert_eq!(page.total, 2);
    assert!(!page.parts[0].writable);
    let key = page.parts[0].value.as_ref().unwrap();
    let page = rt.inspect_object(&table, 1, 1).unwrap();
    assert!(page.parts[0].writable);
    rt.replace_inspected_part(&table, 1, key).unwrap();
    assert!(rt.replace_inspected_part(&table, 0, key).is_err());
    assert!(rt.replace_inspected_part(&table, 3, key).is_err());
    rt.collect();
    assert!(
        rt.eval("(eq *inspected-key* (gethash '(:original) *inspected-table*))")
            .unwrap()[0]
            .is_t()
    );
}

#[test]
fn inspector_symbol_bindings_use_standard_setters() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defparameter *inspected-symbol* (make-symbol \"EDITABLE\"))")
        .unwrap();
    let symbol = rt.eval("*inspected-symbol*").unwrap().remove(0);
    let value = rt.eval("(list 42)").unwrap().remove(0);
    let function = rt.eval("(lambda () 43)").unwrap().remove(0);
    let plist = rt.eval("(list :key 44)").unwrap().remove(0);
    let page = rt.inspect_object(&symbol, 0, 4).unwrap();
    assert!(page.parts[0].value.is_none());
    assert!(page.parts[1].value.is_none());
    assert!(page.parts[0..3].iter().all(|part| part.writable));
    assert!(!page.parts[3].writable);
    rt.replace_inspected_part(&symbol, 0, &value).unwrap();
    rt.replace_inspected_part(&symbol, 1, &function).unwrap();
    rt.replace_inspected_part(&symbol, 2, &plist).unwrap();
    assert!(rt.replace_inspected_part(&symbol, 1, &value).is_err());
    assert!(rt.replace_inspected_part(&symbol, 3, &value).is_err());
    rt.collect();
    let values = rt
        .eval(
            "(values (symbol-value *inspected-symbol*)
                       (funcall *inspected-symbol*) (get *inspected-symbol* :key))",
        )
        .unwrap();
    assert_eq!(rt.repr(&values[0]).unwrap(), "(42)");
    assert_eq!(values[1].as_fixnum(), Some(43));
    assert_eq!(values[2].as_fixnum(), Some(44));
    let constant = rt.eval(":constant").unwrap().remove(0);
    assert!(!rt.inspect_object(&constant, 0, 1).unwrap().parts[0].writable);
    assert!(rt.replace_inspected_part(&constant, 0, &value).is_err());
    let operator = rt.eval("'car").unwrap().remove(0);
    assert!(!rt.inspect_object(&operator, 1, 1).unwrap().parts[0].writable);
    assert!(rt.replace_inspected_part(&operator, 1, &function).is_err());
}

#[test]
fn inspector_mutable_captures_change_the_shared_lexical_cell() {
    let rt = Runtime::start().unwrap();
    let functions = rt
        .eval(
            "(let ((value 1))
               (values (lambda () value) (lambda (new) (setq value new))))",
        )
        .unwrap();
    let getter = &functions[0];
    let page = rt.inspect_object(getter, 0, 10).unwrap();
    let capture = page.parts.iter().find(|part| part.writable).unwrap();
    let value = rt.eval("(list 42)").unwrap().remove(0);
    rt.replace_inspected_part(getter, capture.index, &value)
        .unwrap();
    rt.collect();
    assert_eq!(rt.repr(&rt.call(getter, &[]).unwrap()[0]).unwrap(), "(42)");
    let replacement = rt.eval("43").unwrap().remove(0);
    rt.call(&functions[1], &[&replacement]).unwrap();
    assert_eq!(rt.call(getter, &[]).unwrap()[0].as_fixnum(), Some(43));
    for part in page.parts.iter().filter(|part| !part.writable) {
        assert!(
            rt.replace_inspected_part(getter, part.index, &replacement)
                .is_err()
        );
    }
}

#[test]
fn inspector_copied_captures_remain_read_only() {
    let rt = Runtime::start().unwrap();
    let function = rt
        .eval("(flet ((stored () (list 1))) (lambda () (stored)))")
        .unwrap()
        .remove(0);
    let page = rt.inspect_object(&function, 0, 10).unwrap();
    let capture = page
        .parts
        .iter()
        .find(|part| part.label == "Capture 0")
        .unwrap();
    assert!(!capture.writable);
    let value = rt.eval("42").unwrap().remove(0);
    assert!(
        rt.replace_inspected_part(&function, capture.index, &value)
            .is_err()
    );
    assert_eq!(
        rt.repr(&rt.call(&function, &[]).unwrap()[0]).unwrap(),
        "(1)"
    );
}

#[test]
fn inspector_rejects_objects_from_another_runtime() {
    let rt = Runtime::start().unwrap();
    let other = Runtime::start().unwrap();
    let local = rt.eval("(cons 1 2)").unwrap().remove(0);
    let foreign = other.eval("42").unwrap().remove(0);
    assert!(rt.replace_inspected_part(&local, 0, &foreign).is_err());
    assert!(rt.replace_inspected_part(&foreign, 0, &local).is_err());
}
