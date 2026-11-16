use zkcl::{EvaluationPoll, ExecutionStop, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::assert_truthy;

fn assert_all_forms_are_true(runtime: &Runtime, sources: &[&str]) {
    for &source in sources {
        assert_truthy(runtime, source);
    }
}

#[test]
fn interleaved_class_finalizations_see_their_own_proposed_subclasses() {
    let rt = Runtime::start().unwrap();
    rt.register_function("check-left-class-update", |rt, _| {
        Ok(rt.eval("(and (proposed-child-p 'left-child) (not (proposed-child-p 'right-child)))")?)
    })
    .unwrap();
    rt.load_str(
        r#"
        (defclass shared-parent () ())
        (defclass interleaving-metaclass (standard-class) ())
        (defmethod zk-mop:validate-superclass ((c interleaving-metaclass) (p standard-class)) t)
        (defvar *left-entered* (zk-async:make-event))
        (defvar *right-entered* (zk-async:make-event))
        (defvar *left-published* (zk-async:make-event))
        (defun proposed-child-p (name)
          (member name (mapcar #'class-name
                               (zk-mop:class-direct-subclasses (find-class 'shared-parent)))))
        (defmethod zk-mop:finalize-inheritance :before ((c interleaving-metaclass))
          (ecase (class-name c)
            (left-child
             (zk-async:set-event *left-entered*)
             (zk-async:wait-event *right-entered*)
             (assert (check-left-class-update)))
            (right-child
             (zk-async:wait-event *left-entered*)
             (assert (proposed-child-p 'right-child))
             (assert (not (proposed-child-p 'left-child)))
             (zk-async:set-event *right-entered*)
             (zk-async:wait-event *left-published*))))
        "#,
    )
    .unwrap();
    assert_truthy(
        &rt,
        r#"(progn
          (zk-async:with-task-group (group)
            (zk-async:spawn-task (group)
              (defclass left-child (shared-parent) () (:metaclass interleaving-metaclass))
              (zk-async:set-event *left-published*))
            (zk-async:spawn-task (group)
              (defclass right-child (shared-parent) () (:metaclass interleaving-metaclass))))
          (and (proposed-child-p 'left-child)
               (proposed-child-p 'right-child)
               (zk-mop:class-finalized-p (find-class 'left-child))
               (zk-mop:class-finalized-p (find-class 'right-child))))"#,
    );
}

#[test]
fn hard_abort_discards_the_suspended_class_update() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defclass abort-parent () ())
        (defclass aborting-metaclass (standard-class) ())
        (defmethod zk-mop:validate-superclass ((c aborting-metaclass) (p standard-class)) t)
        (defvar *block-finalization* t)
        (defmethod zk-mop:finalize-inheritance :before ((c aborting-metaclass))
          (when *block-finalization*
            (sleep 3600)))
        "#,
    )
    .unwrap();
    let definition = "(zk-async:with-task-group (group)
                         (zk-async:spawn-task (group)
                           (defclass abort-child (abort-parent) () (:metaclass aborting-metaclass))))";
    let mut evaluation = rt.start_eval(definition).unwrap();
    loop {
        match evaluation.resume(None).unwrap() {
            EvaluationPoll::Runnable => continue,
            EvaluationPoll::Waiting(Some(_)) => break,
            _ => panic!("expected class finalization to remain pending"),
        }
    }
    rt.cancellation_handle().cancel();
    let Err(error) = evaluation.resume(None) else {
        panic!("expected hard abort");
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Aborted));
    drop(evaluation);
    rt.cancellation_handle().reset();
    assert_truthy(
        &rt,
        "(null (zk-mop:class-direct-subclasses (find-class 'abort-parent)))",
    );
    rt.load_str("(setf *block-finalization* nil)").unwrap();
    rt.load_str(definition).unwrap();
    assert_truthy(
        &rt,
        "(equal (mapcar #'class-name (zk-mop:class-direct-subclasses (find-class 'abort-parent)))
                '(abort-child))",
    );
}

#[test]
fn competing_first_definitions_share_one_class_name() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defclass race-parent () ())
        (defclass race-meta (standard-class) ())
        (defmethod zk-mop:validate-superclass ((class race-meta) (parent standard-class)) t)
        (defvar *defer-binding* nil)
        (defvar *first-layout-published* (zk-async:make-event))
        (defvar *release-first* (zk-async:make-event))
        (defmethod zk-mop:ensure-class-using-class :around
            ((class null) (name (eql 'race-child)) &rest args)
          (declare (ignore args))
          (assert (null (find-class name nil)))
          (let ((result (call-next-method)))
            (assert (eq result (find-class name)))
            result))
        (defmethod initialize-instance :after ((class race-meta) &rest initargs)
          (declare (ignore initargs))
          (when *defer-binding*
            (assert (zk-mop:class-finalized-p class))
            (assert (null (find-class (class-name class) nil)))
            (zk-async:set-event *first-layout-published*)
            (zk-async:wait-event *release-first*)))
        "#,
    )
    .unwrap();
    assert_truthy(
        &rt,
        r#"(let ((winner
                  (zk-async:with-task-group (group)
                    (let ((first (zk-async:spawn-task (group)
                                   (let ((*defer-binding* t))
                                     (defclass race-child (race-parent) () (:metaclass race-meta)))))
                          (second (zk-async:spawn-task (group)
                                    (zk-async:wait-event *first-layout-published*)
                                    (unwind-protect
                                         (progn
                                           (zk:gc)
                                           (assert (null (find-class 'race-child nil)))
                                           (assert
                                            (handler-case
                                                (progn (defclass race-child (race-parent) ()
                                                         (:metaclass race-meta)) nil)
                                              (program-error () t)))
                                           (assert
                                            (handler-case
                                                (progn (setf (find-class 'race-child)
                                                             (find-class 'race-parent)) nil)
                                              (program-error () t)))
                                           (defclass unrelated-to-claim () ()))
                                      (zk-async:set-event *release-first*)))))
                      (zk-async:await second)
                      (zk-async:await first)))))
          (and (eq winner (find-class 'race-child))
               (eq winner (defclass race-child (race-parent) () (:metaclass race-meta)))
               (equal (list winner)
                      (zk-mop:class-direct-subclasses (find-class 'race-parent)))))"#,
    );
}

#[test]
fn class_redefinition_reuses_existing_metaobjects() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
        (defclass tagged-class (standard-class)
          ((tag :initarg :tag :initform :default :accessor class-tag)))
        (defmethod zk-mop:validate-superclass ((c tagged-class) (p standard-class)) t)
        (defvar *class-init-events* nil)
        (defmethod initialize-instance :before ((c tagged-class) &rest args)
          (push (list :initial (getf args :direct-superclasses) (getf args :tag)) *class-init-events*))
        (defmethod reinitialize-instance :after ((c tagged-class) &rest args)
          (declare (ignore args)) (push :redefined *class-init-events*))
        (defclass root-record () ((x :initform 7 :accessor read-x))
          (:metaclass tagged-class) (:tag :tracked))
        (defclass child-record (root-record) () (:metaclass tagged-class))
        (defparameter *record-class* (find-class 'root-record))
        (defparameter *record* (make-instance 'child-record))
        (defmethod zk-mop:slot-value-using-class :around ((c tagged-class) o s)
           (+ 10 (call-next-method)))
        (defparameter *reader* (find-method #'read-x nil (list *record-class*)))
        (defparameter *writer* (find-method #'(setf read-x) nil (list (find-class t) *record-class*)))
        (defparameter *slot* (car (zk-mop:class-direct-slots *record-class*)))
        "#).unwrap();
    rt.collect();
    assert_all_forms_are_true(&rt, &[
        "(and (eq (class-of *record-class*) (find-class 'tagged-class))
              (typep *record-class* 'class) (typep *record-class* 'tagged-class)
              (equal (class-tag *record-class*) '(:tracked))
              (eq (class-tag (class-of *record*)) :default) (= (read-x *record*) 17))",
        "(equal (reverse *class-init-events*) (list '(:initial nil (:tracked)) (list :initial (list *record-class*) nil)))",
        "(and (typep *reader* 'zk-mop:standard-reader-method)
              (typep *writer* 'zk-mop:standard-writer-method)
              (eq (zk-mop:accessor-method-slot-definition *reader*) *slot*)
              (eq (zk-mop:accessor-method-slot-definition *writer*) *slot*))",
        "(progn (zk-mop:ensure-class 'root-record :metaclass (find-class 'tagged-class) :tag :new)
                (and (eq *record-class* (find-class 'root-record)) (eq :new (class-tag *record-class*))
                     (eq (first *class-init-events*) :redefined)
                     (eq *reader* (find-method #'read-x nil (list *record-class*)))
                     (= (read-x *record*) 17)))",
        "(progn (defclass tagged-class (standard-class)
                  ((tag :initarg :tag :initform :default :accessor class-tag)
                   (extra :initform 55 :reader extra)))
                (and (= (extra *record-class*) 55) (eq :new (class-tag *record-class*))))",
        "(let* ((c (make-instance 'standard-class :direct-slots
                      (list (list :name 'local :readers '(anonymous-local)))))
                (object (make-instance c)))
           (and (null (class-name c)) (eq c (class-of object))
                (progn (setf (slot-value object 'local) 19) (= 19 (anonymous-local object)))))",
    ]);
}

#[test]
fn finalization_commits_the_computed_class_layout_as_one_unit() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
       (defclass ordered-class (standard-class) ())
       (defmethod zk-mop:validate-superclass ((c ordered-class) (p standard-class)) t)
       (defclass left-base () ((value :initform :left)))
       (defclass right-base () ((value :initform :right)))
       (defparameter *reject-layout* nil)
       (defmethod zk-mop:compute-class-precedence-list ((c ordered-class))
         (let ((standard (call-next-method)))
           (if (eq (class-name c) 'ordered)
               (list* c (third standard) (second standard) (cdddr standard))
               standard)))
       (defmethod zk-mop:compute-slots ((c ordered-class))
         (when *reject-layout* (throw 'failed :aborted))
         (sort (copy-list (call-next-method)) #'string< :key #'zk-mop:slot-definition-name))
       (defmethod zk-mop:compute-default-initargs ((c ordered-class))
         (cons (list :a 41 (lambda () 41)) (call-next-method)))
       (defclass ordered (left-base right-base) ((z) (a :initarg :a)) (:metaclass ordered-class))
       (defparameter *ordered* (make-instance 'ordered))
       (defparameter *ordered-c* (class-of *ordered*))
       (defparameter *old-slots* (zk-mop:class-slots *ordered-c*))
       (defmethod which ((x left-base)) :left)
       (defmethod which ((x right-base)) :right)
       "#,
    )
    .unwrap();
    rt.collect();
    assert_all_forms_are_true(&rt, &[
        "(and (equal (mapcar #'class-name (zk-mop:class-precedence-list *ordered-c*))
                      '(ordered right-base left-base standard-object t))
              (eq :right (which *ordered*)) (eq :right (slot-value *ordered* 'value))
              (equal '(a value z) (mapcar #'zk-mop:slot-definition-name *old-slots*))
              (equal '(0 1 2) (mapcar #'zk-mop:slot-definition-location *old-slots*))
              (= (zk-mop:standard-instance-access *ordered* 0) 41)
              (= (setf (zk-mop:standard-instance-access *ordered* 0) 69) (slot-value *ordered* 'a)))",
        "(let ((*reject-layout* t))
           (eq :aborted (catch 'failed
             (defclass ordered (left-base right-base) ((lost)) (:metaclass ordered-class)))))",
        "(and (every #'eq *old-slots* (zk-mop:class-slots *ordered-c*))
              (not (slot-exists-p *ordered* 'lost)) (= 69 (slot-value *ordered* 'a)))",
        "(eq (zk-mop:class-prototype *ordered-c*) (zk-mop:class-prototype *ordered-c*))",
    ]);
}

#[test]
fn funcallable_instances_use_their_replacement_function_after_collection() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
        (defclass accumulator () ((n :initform 4 :accessor n)) (:metaclass zk-mop:funcallable-standard-class))
        (defparameter *fn* (make-instance 'accumulator))
        (zk-mop:set-funcallable-instance-function *fn* (lambda (x) (values (+ (n *fn*) x) x)))
        "#).unwrap();
    rt.collect();
    assert_all_forms_are_true(&rt, &[
        "(and (eq (class-of (find-class 'accumulator)) (find-class 'zk-mop:funcallable-standard-class))
              (functionp *fn*) (typep *fn* 'function) (typep *fn* 'standard-object)
              (typep *fn* 'zk-mop:funcallable-standard-object)
              (equal (multiple-value-list (apply *fn* '(9))) '(13 9)))",
        "(progn (setf (zk-mop:funcallable-standard-instance-access *fn* 0) 10)
                (zk-mop:set-funcallable-instance-function *fn* (lambda () (throw 'done (n *fn*))))
                (= 10 (catch 'done (funcall *fn*))))",
    ]);
}

#[test]
fn refused_forward_class_resolution_can_retry_on_the_same_metaobject() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defclass future-meta (standard-class) ((stamp :initform 3 :reader stamp)))
        (defvar *refuse* nil)
        (defmethod zk-mop:validate-superclass ((class future-meta) (super standard-class))
          (when *refuse* (throw 'failed :refused)) t)
        (defclass waiting (later) () (:metaclass future-meta))
        (defparameter *forward* (find-class 'later))
        "#,
    )
    .unwrap();
    rt.collect();
    assert_all_forms_are_true(
        &rt,
        &[
            "(let ((*refuse* t))
           (and (eq :refused (catch 'failed (defclass later () () (:metaclass future-meta))))
                (typep *forward* 'zk-mop:forward-referenced-class)
                (not (zk-mop:class-finalized-p (find-class 'waiting)))))",
            "(progn (defclass later () ((x :initform 22)) (:metaclass future-meta))
                (and (eq *forward* (find-class 'later)) (= 3 (stamp *forward*))
                     (= 22 (slot-value (make-instance 'waiting) 'x))))",
        ],
    );
}

#[test]
fn forward_resolution_changes_the_existing_class_before_reinitialization() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass forward-meta (standard-class)
        ((stamp :initarg :stamp :initform 13)))
      (defmethod zk-mop:validate-superclass ((class forward-meta) (super standard-class)) t)
      (defclass waiting-child (waiting-parent) ())
      (defparameter *forward* (find-class 'waiting-parent))
      (defmethod forward-specializer-reader ((instance waiting-parent)) :matched)
      (zk-mop:add-dependent *forward* :listener)
      (zk-mop:add-dependent *forward* :keep)
      (defmethod zk-mop:update-dependent ((owner (eql *forward*)) listener &rest args) nil)
      (defvar *stages* nil)
      (defvar *abort* nil)
      (defmethod change-class :around
          ((old zk-mop:forward-referenced-class) (new (eql (find-class 'forward-meta))) &rest args)
        (push (list :change args) *stages*)
        (prog1 (call-next-method)
          (push (list :changed (slot-value old 'stamp)) *stages*)))
      (defmethod update-instance-for-different-class :around
          ((old zk-mop:forward-referenced-class) (new forward-meta) &rest args)
        (zk:gc)
        (assert (and (not (eq old new)) (eq new *forward*) (eq (class-name old) (class-name new))))
        (assert (equal (zk-mop:class-direct-subclasses old) (zk-mop:class-direct-subclasses new)))
        (assert (equal (zk-mop:specializer-direct-methods old) (zk-mop:specializer-direct-methods new)))
        (let ((previous nil) (current nil))
          (zk-mop:map-dependents old (lambda (object) (push object previous)))
          (zk-mop:map-dependents new (lambda (object) (push object current)))
          (assert (equal previous current))
          (when (member :listener previous)
            (zk-mop:remove-dependent old :listener)
            (setf current nil)
            (zk-mop:map-dependents new (lambda (object) (push object current)))
            (assert (member :listener current))))
        (push (list :update args) *stages*)
        (when (eq *abort* :update) (throw 'abort *abort*))
        (call-next-method))
      (defmethod reinitialize-instance :before ((class forward-meta) &rest args)
        (push (list :reinitialize args) *stages*)
        (when (eq *abort* :reinitialize) (throw 'abort *abort*)))
      (defun resolve-parent ()
        (zk-mop:ensure-class 'waiting-parent :metaclass 'forward-meta :stamp 31))
      (dolist (*abort* '(:update :reinitialize))
        (assert (eq *abort* (catch 'abort (resolve-parent))))
        (assert (eq (class-of *forward*) (find-class 'zk-mop:forward-referenced-class)))
        (assert (equal (zk-mop:class-direct-subclasses *forward*) (list (find-class 'waiting-child))))
        (assert (not (zk-mop:class-finalized-p (find-class 'waiting-child)))))
      (setf *stages* nil)
      (assert (eq (resolve-parent) *forward*))
      (assert (equal (reverse *stages*)
               '((:change (:stamp 31)) (:update (:stamp 31)) (:changed 31) (:reinitialize (:stamp 31)))))
      (assert (eq (find-class 'waiting-parent) *forward*))
      (assert (typep (make-instance 'waiting-child) *forward*))
      (assert (eq :matched (forward-specializer-reader (make-instance 'waiting-child))))
      (assert (handler-case (progn (zk-mop:ensure-class 'waiting-parent :metaclass 'standard-class) nil)
                (error () t)))
      (assert (= 31 (slot-value *forward* 'stamp)))
      (defclass traced-forward (zk-mop:forward-referenced-class) ((stamp :initarg :stamp)))
      (let ((c (zk-mop:ensure-class 'explicit-forward :metaclass 'traced-forward :stamp :saved)))
        (assert (eq c (find-class 'explicit-forward)))
        (assert (not (zk-mop:class-finalized-p c)))
        (let ((*forward* c) (*abort* :reinitialize))
          (assert (eq :reinitialize
                    (catch 'abort (zk-mop:ensure-class 'explicit-forward :metaclass 'forward-meta)))))
        (assert (eq (class-of c) (find-class 'traced-forward)))
        (assert (eq :saved (slot-value c 'stamp)))
        (let ((*forward* c))
          (assert (eq c (change-class c (find-class 'forward-meta))))
          (assert (eq :saved (slot-value c 'stamp)))
          (assert (not (zk-mop:class-finalized-p c)))
          (assert (eq c (zk-mop:ensure-class 'explicit-forward :metaclass 'forward-meta)))
          (assert (zk-mop:class-finalized-p c))))
      (defmethod zk-mop:ensure-class-using-class :around
          ((class null) (name (eql 'use-existing)) &rest options)
        (declare (ignore class name options))
        *forward*)
      (assert (eq *forward* (zk-mop:ensure-class 'use-existing)))
      (assert (not (find-class 'use-existing nil)))
      (let ((class (zk-mop:ensure-class-using-class nil 'direct-ensure)))
        (assert (eq class (find-class 'direct-ensure))))
    "#).unwrap();
}

#[test]
fn subclass_hooks_observe_staged_edges_before_layout_commit() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass old-parent () ((shared :allocation :class :initform 8)))
      (defclass new-parent () ((shared :allocation :class :initform 13)))
      (defclass third-parent () ())
      (defclass moving-child (old-parent) ((local :reader local-reader :initform 2)))
      (defparameter *instance* (make-instance 'moving-child))
      (defvar *action* nil)
      (defmethod zk-mop:add-direct-subclass :around
          ((super (eql (find-class 'new-parent))) (child (eql (find-class 'moving-child))))
        (zk:gc)
        (assert (equal (zk-mop:class-direct-superclasses child) (list super)))
        (assert (member child (zk-mop:class-direct-subclasses super)))
        (assert (not (member child (zk-mop:class-direct-subclasses (find-class 'old-parent)))))
        (call-next-method)
        (case *action*
          (:abort (throw 'abort :refused))
          (:move (zk-mop:ensure-class 'moving-child :direct-superclasses '(third-parent)))))
      (let ((*action* :abort))
        (assert (eq :refused (catch 'abort
          (defclass moving-child (new-parent) ((replacement :reader new-reader)))))))
      (assert (typep *instance* 'old-parent))
      (assert (= 8 (slot-value *instance* 'shared)))
      (assert (= 2 (local-reader *instance*)))
      (assert (not (fboundp 'new-reader)))
      (assert (null (zk-mop:class-direct-subclasses (find-class 'new-parent))))
      (let ((*action* :move))
        (assert (handler-case (progn (zk-mop:ensure-class 'moving-child :direct-superclasses '(new-parent)) nil)
                  (program-error () t))))
      (assert (typep *instance* 'third-parent))
      (assert (null (zk-mop:class-direct-subclasses (find-class 'new-parent))))
      (assert (null (zk-mop:class-direct-subclasses (find-class 'old-parent))))
      (assert (equal (zk-mop:class-direct-subclasses (find-class 'third-parent))
                     (list (find-class 'moving-child))))
    "#).unwrap();
}

#[test]
fn effective_slot_definitions_carry_custom_options_into_slot_access() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
        (defclass tagged-class (standard-class) ())
        (defmethod zk-mop:validate-superclass ((c tagged-class) (parent standard-class)) t)
        (defclass tagged-direct (zk-mop:standard-direct-slot-definition)
          ((tags :initarg :tag :reader slot-tags)))
        (defclass tagged-effective (zk-mop:standard-effective-slot-definition)
          ((tags :initform nil :accessor effective-tags)))
        (defmethod zk-mop:direct-slot-definition-class ((c tagged-class) &rest args)
          (declare (ignore args)) (find-class 'tagged-direct))
        (defmethod zk-mop:effective-slot-definition-class ((c tagged-class) &rest args)
          (declare (ignore args)) (find-class 'tagged-effective))
        (defmethod zk-mop:compute-effective-slot-definition :around ((c tagged-class) name slots)
          (let ((effective (call-next-method)))
            (setf (effective-tags effective) (slot-tags (first slots)))
            effective))
        (defmethod zk-mop:slot-value-using-class :around ((c tagged-class) o (slot tagged-effective))
          (if (member :numeric (effective-tags slot)) (1+ (call-next-method)) (call-next-method)))
        (defclass tagged-data () ((value :tag :numeric :tag :public :initform 5 :reader tagged-value))
          (:metaclass tagged-class))
        (defparameter *c* (find-class 'tagged-data))
        (defparameter *obj* (make-instance *c*))
        "#).unwrap();
    rt.collect();
    assert_all_forms_are_true(&rt, &[
        "(let ((s (first (zk-mop:class-slots *c*))) (d (first (zk-mop:class-direct-slots *c*))))
           (and (typep s 'tagged-effective) (typep d 'tagged-direct)
                (equal (effective-tags s) '(:numeric :public))
                (eq d (zk-mop:accessor-method-slot-definition (find-method #'tagged-value nil (list *c*))))
                (= (tagged-value *obj*) 6)
                (= (zk-mop:standard-instance-access *obj* (zk-mop:slot-definition-location s)) 5)))",
    ]);
}

#[test]
fn slot_initfunctions_distinguish_bound_nil_from_uninitialized_slots() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass member-meta (standard-class) ())
      (defmethod zk-mop:validate-superclass ((m member-meta) (s standard-class)) t)
      (defclass member-slot (zk-mop:standard-direct-slot-definition) ())
      (defmethod zk-mop:direct-slot-definition-class ((m member-meta) &rest args) 'member-slot)
      (defmethod initialize-instance ((s member-slot) &rest args &key name initform &allow-other-keys)
        (when (eq name 'required)
          (assert (not initform))
          (setf (getf args :initform) '(error "required")))
        (apply #'call-next-method s args))
      (let ((value 41))
        (defclass packet () ((required) (explicit :initform nil) (local :initform (incf value)))
          (:metaclass member-meta)))
      (let* ((o (make-instance 'packet))
             (slot (find 'required (zk-mop:class-direct-slots (class-of o))
                         :key #'zk-mop:slot-definition-name)))
        (assert (equal (zk-mop:slot-definition-initform slot) '(error "required")))
        (assert (null (zk-mop:slot-definition-initfunction slot)))
        (assert (not (slot-boundp o 'required)))
        (assert (and (slot-boundp o 'explicit) (null (slot-value o 'explicit))))
        (assert (= 42 (slot-value o 'local)))
        (assert (= 43 (slot-value (make-instance 'packet) 'local))))
      (dolist (args '((:initform nil) (:initfunction nil) (:initform x :initfunction 17)))
        (assert (handler-case
                  (progn (apply #'make-instance 'zk-mop:standard-direct-slot-definition :name 'x args) nil)
                  (error () t))))
      (let ((s (make-instance 'zk-mop:standard-effective-slot-definition
                              :name 'x :initform :metadata :initfunction nil)))
        (assert (eq :metadata (zk-mop:slot-definition-initform s)))
        (assert (null (zk-mop:slot-definition-initfunction s))))
    "#).unwrap();
}

#[test]
fn compiled_metaclasses_rebuild_slot_protocols_in_a_fresh_runtime() {
    let directory = TempDir::new("metaclass-protocol");
    let source = directory.path().join("classes.lisp");
    std::fs::write(&source, r#"
        (defclass configured-class (standard-class) ((setting :initarg :setting :reader setting)))
        (defmethod zk-mop:validate-superclass ((c configured-class) (s standard-class)) t)
        (let ((seed 7))
          (defclass compiled-object () ((x :initform (+ seed 2) :accessor compiled-slot))
            (:metaclass configured-class) (:setting foo bar)))
        (defclass compiled-callable () ((x :initform 13)) (:metaclass zk-mop:funcallable-standard-class))
        "#).unwrap();
    let fasl = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    std::fs::remove_file(&source).unwrap();
    let rt = Runtime::start().unwrap();
    rt.load_file(&fasl).unwrap();
    rt.collect();
    assert_all_forms_are_true(
        &rt,
        &[
            "(and (equal (setting (find-class 'compiled-object)) '(foo bar))
                       (= 9 (compiled-slot (make-instance 'compiled-object)))
                       (let ((f (make-instance 'compiled-callable)))
                         (zk-mop:set-funcallable-instance-function f (lambda () (slot-value f 'x)))
                         (= 13 (funcall f))))",
        ],
    );
}

#[test]
fn finalization_rejects_layout_changes_conflicting_with_a_reentrant_hook() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
        (defclass watched-meta (standard-class) ())
        (defmethod zk-mop:validate-superclass ((c watched-meta) (p standard-class)) t)
        (defclass base () ((b :initform 10 :reader base-b)))
        (defclass observed (base) ((a :initform 42)) (:metaclass watched-meta))
        (defvar *during* nil)
        (defvar *finished* 0)
        (defvar *staged-prototype* nil)
        (defmethod zk-mop:compute-slots :around ((c watched-meta))
          (when *during*
            (let ((action *during*) (*during* nil)) (funcall action)))
          (call-next-method))
        (defmethod zk-mop:finalize-inheritance :after ((c watched-meta))
          (assert (zk-mop:class-finalized-p c))
          (setf *staged-prototype* (zk-mop:class-prototype c))
          (assert (every (lambda (slot) (slot-exists-p *staged-prototype* (zk-mop:slot-definition-name slot))) (zk-mop:class-slots c)))
          (incf *finished*))
        (defparameter *one* (make-instance 'observed))
        "#).unwrap();
    assert_all_forms_are_true(&rt, &[
        "(let ((*during* (lambda () (defclass unrelated () ()))))
           (zk-mop:finalize-inheritance (find-class 'observed))
           (and (= *finished* 1) (eq *staged-prototype* (zk-mop:class-prototype (find-class 'observed))) (find-class 'unrelated nil) (= 42 (slot-value *one* 'a))))",
        "(let ((*during* (lambda () (defclass base () ((b :initform 10 :reader base-b) (extra :initform 9))))))
           (and (handler-case (progn (defclass observed (base) ((should-not-publish)) (:metaclass watched-meta)) nil)
                  (program-error () t))
                (not (slot-exists-p *one* 'should-not-publish)) (= 9 (slot-value *one* 'extra))
                (= 42 (slot-value *one* 'a)) (= 10 (base-b *one*))))",
        "(progn (defgeneric wrong-arity (a b))
                (and (handler-case (progn (defclass base () ((replacement :reader wrong-arity))) nil)
                       (program-error () t))
                     (= 10 (base-b *one*)) (slot-exists-p *one* 'a)))",
        "(and (handler-case (progn (defclass impossible-meta (standard-class zk-mop:standard-direct-slot-definition) ()) nil)
                (program-error () t)) (null (find-class 'impossible-meta nil)))",
        "(handler-case (progn (reinitialize-instance (first (zk-mop:class-direct-slots (find-class 'base))) :name 'changed) nil)
           (error () t))",
    ]);
}

#[test]
fn compute_slots_assigns_locations_before_returning_to_the_caller() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass location-meta (standard-class) ())
      (defmethod zk-mop:validate-superclass ((c location-meta) (s standard-class)) t)
      (defvar *inside* nil)
      (defmethod zk-mop:compute-slots :around ((c location-meta))
        (let ((slots (call-next-method)))
          (push (mapcar #'zk-mop:slot-definition-location
                        (remove-if-not (lambda (slot) (eq :instance (zk-mop:slot-definition-allocation slot))) slots))
                *inside*)
          (reverse slots)))
      (defclass placed () ((a :initform 7) (common :allocation :class :initform 9)
                           (b :initform 11)) (:metaclass location-meta))
      (defclass callable-placed () ((a :initform 17) (b :initform 19))
        (:metaclass zk-mop:funcallable-standard-class))
      (defun check-locations (instance access)
        (dolist (slot (zk-mop:compute-slots (class-of instance)))
          (when (eq :instance (zk-mop:slot-definition-allocation slot))
            (assert (eql (slot-value instance (zk-mop:slot-definition-name slot))
                         (funcall access instance (zk-mop:slot-definition-location slot)))))))
      (defvar *placed* (make-instance 'placed))
      (check-locations *placed* #'zk-mop:standard-instance-access)
      (assert (equal (first *inside*) '(0 1)))
      (check-locations (make-instance 'callable-placed) #'zk-mop:funcallable-standard-instance-access)
      (slot-makunbound *placed* 'common)
      (defclass placed () ((b :initform 101) (common :initform 102)
                           (a :allocation :class :initform 103)) (:metaclass location-meta))
      (assert (not (slot-boundp *placed* 'common)))
      (assert (= (slot-value *placed* 'a) 103))
      (assert (= (slot-value *placed* 'b) 11))
      (setf (slot-value *placed* 'common) 23)
      (check-locations *placed* #'zk-mop:standard-instance-access)
    "#).unwrap();
}

#[test]
fn change_class_rejects_installed_metaobjects_before_reclassifying_them() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass extended-generic (standard-generic-function) ()
        (:metaclass zk-mop:funcallable-standard-class))
      (defclass extended-method (standard-method) ())
      (defgeneric observed (v))
      (defmethod observed ((v t)) (list v))
      (let* ((gf #'observed) (m (find-method gf nil (list (find-class t)))))
        (dolist (entry (list (list m 'extended-method) (list gf 'extended-generic)))
          (let* ((obj (first entry)) (old (class-of obj)))
            (assert (handler-case (progn (change-class obj (find-class (second entry))) nil) (error () t)))
            (assert (eq (class-of obj) old))))
        (assert (eq m (find-method gf nil (list (find-class t)))))
        (assert (eq gf (zk-mop:method-generic-function m)))
        (assert (equal '(12) (funcall gf 12))))
    "#).unwrap();
}
