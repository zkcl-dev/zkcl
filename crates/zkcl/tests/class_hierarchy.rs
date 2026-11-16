use zkcl::Runtime;

#[test]
fn class_direct_subclasses_lists_immediate_children() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defstruct tree-root)
             (defstruct (branch (:include tree-root)))
             (defstruct (leaf (:include branch)))
             (defstruct (tagged-list (:type list) :named))
             (define-condition tree-error (type-error) ())",
        )
        .unwrap();
    for source in [
        "(typep #'zk-mop:class-direct-subclasses 'generic-function)",
        "(let ((children (zk-mop:class-direct-subclasses (find-class 'number))))
           (and (= 2 (length children))
                (member (find-class 'complex) children)
                (member (find-class 'real) children)))",
        "(let ((children (zk-mop:class-direct-subclasses (find-class 'zk-mop:specializer))))
           (and (member (find-class 'zk-mop:eql-specializer) children)
                (member (find-class 'class) children)))",
        "(let ((children (zk-mop:class-direct-subclasses (find-class 'type-error))))
           (and (member (find-class 'simple-type-error) children)
                (member (find-class 'tree-error) children)
                (member (find-class 'simple-type-error)
                        (zk-mop:class-direct-subclasses (find-class 'simple-condition)))))",
        "(let ((children (zk-mop:class-direct-subclasses (find-class 'structure-object))))
           (and (member (find-class 'tree-root) children)
                (not (member (find-class 'branch) children))
                (not (find 'tagged-list children :key #'class-name))))",
        "(and (equal (zk-mop:class-direct-subclasses (find-class 'tree-root))
                     (list (find-class 'branch)))
              (equal (zk-mop:class-direct-subclasses (find-class 'branch))
                     (list (find-class 'leaf)))
              (null (zk-mop:class-direct-subclasses (find-class 'leaf))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn class_direct_subclasses_tracks_class_objects_across_redefinition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defclass child (future-parent) ())
             (defparameter *parent* (find-class 'future-parent))
             (defparameter *first-child* (find-class 'child))
             (defparameter *forward-children* (zk-mop:class-direct-subclasses *parent*))
             (defclass future-parent () ())
             (setf (find-class 'child) nil)
             (defclass child (future-parent) ())",
        )
        .unwrap();
    runtime.collect();
    for source in [
        "(equal *forward-children* (list *first-child*))",
        "(let ((children (zk-mop:class-direct-subclasses *parent*)))
           (and (= 2 (length children))
                (not (eq *first-child* (find-class 'child)))
                (member *first-child* children)
                (member (find-class 'child) children)))",
        "(progn (defclass child () ())
                (equal (zk-mop:class-direct-subclasses *parent*) (list *first-child*)))",
        "(progn
           (defmethod zk-mop:class-direct-subclasses :around ((class (eql *parent*)))
             (values (call-next-method) :observed))
           (equal (multiple-value-list (zk-mop:class-direct-subclasses *parent*))
                  (list (list *first-child*) :observed)))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}
