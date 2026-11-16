use zkcl::Runtime;

mod common;

use common::files::TempDir;
use common::lisp::assert_truthy;

#[test]
fn method_functions_keep_their_captured_bindings_after_collection() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass labelled-method (standard-method) ((label :initarg :label :reader method-label)))
      (defclass labelled-function (standard-generic-function) ((memo :initform :live :accessor memo))
        (:metaclass zk-mop:funcallable-standard-class))
      (defgeneric evaluate-labelled (x) (:generic-function-class labelled-function) (:method-class labelled-method))
      (defmethod zk-mop:make-method-lambda :around ((g labelled-function) (m labelled-method) body environment)
        (multiple-value-bind (expression initargs) (call-next-method)
          (values expression (list* :label :expanded initargs))))
      (defmethod evaluate-labelled ((x t)) (* 2 x))
      (let ((captured 3))
        (defmethod evaluate-labelled ((x integer)) (values (+ captured (call-next-method)) (next-method-p))))
      (defparameter *base* (find-method #'evaluate-labelled nil (list (find-class t))))
      (defparameter *specific* (find-method #'evaluate-labelled nil (list (find-class 'integer))))
      (defparameter *anonymous* (make-instance 'standard-generic-function :lambda-list '(x)))
      (defparameter *fallback* (make-instance 'standard-method :lambda-list '(x)
           :specializers (list (find-class t)) :function (lambda (args next) (declare (ignore next)) (car args))))
      (defparameter *next-list* nil)
      (add-method *anonymous* *fallback*)
      (add-method *anonymous* (make-instance 'standard-method :lambda-list '(x) :specializers (list (find-class 'integer))
          :function (lambda (arguments next)
                      (setq *next-list* next)
                      (1+ (funcall (zk-mop:method-function (first next)) arguments (rest next))))))
    "#).unwrap();
    assert_truthy(
        &rt,
        "(let ((method (first (zk-mop:generic-function-methods #'class-name))))
                        (and (eq (zk-mop:method-function method) (zk-mop:method-function method))
                             (eq (zk-mop:generic-function-method-combination #'class-name)
                                 (zk-mop:generic-function-method-combination #'class-name))))",
    );
    rt.collect();
    assert_truthy(&rt,"(and (eq (class-of #'evaluate-labelled) (find-class 'labelled-function))
                     (eq (memo #'evaluate-labelled) :live) (eq (method-label *specific*) :expanded)
                     (equal (multiple-value-list (evaluate-labelled 5)) '(13 t))
                     (equal (multiple-value-list (funcall (zk-mop:method-function *specific*) '(6) (list *base*))) '(15 t))
                     (= (funcall *anonymous* 9) 10) (equal *next-list* (list *fallback*)))");
}

#[test]
fn call_method_forwards_extra_arguments_from_the_effective_method() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass alternate-function (standard-generic-function) () (:metaclass zk-mop:funcallable-standard-class))
      (defgeneric alternate (x) (:generic-function-class alternate-function))
      (defmethod alternate ((x t)) :fallback)
      (defvar *hits* nil)
      (defmethod zk-mop:compute-applicable-methods-using-classes ((g alternate-function) classes)
        (declare (ignore classes)) (push :classes *hits*) (values nil nil))
      (defmethod compute-applicable-methods :after ((g alternate-function) args)
        (declare (ignore args)) (push :values *hits*))
      (defmethod zk-mop:compute-effective-method ((g alternate-function) c methods)
        (declare (ignore c)) (push :effective *hits*)
        (list 'call-method (first methods) nil :extra))
      (add-method #'alternate
        (make-instance 'standard-method :lambda-list '(x) :specializers (list (find-class 'integer))
          :function (lambda (args next marker) (values (car args) next marker))))
    "#).unwrap();
    rt.collect();
    assert_truthy(
        &rt,
        "(let ((*hits* nil)) (and (equal (multiple-value-list (alternate 8)) '(8 nil :extra))
                                        (equal (reverse *hits*) '(:classes :values :effective))))",
    );
    assert_truthy(&rt,"(progn (zk-mop:set-funcallable-instance-function #'alternate (let ((n 1)) (lambda (x) (incf n x))))
                      (= 6 (alternate 5)))");
    rt.load_str(
        "(defgeneric ordinary-hook (x)) (defmethod ordinary-hook (x) x)
                 (defmethod zk-mop:compute-effective-method :around ((g (eql #'ordinary-hook)) c ms)
                   (declare (ignore c ms)) '(values :adapted 2))",
    )
    .unwrap();
    assert_truthy(
        &rt,
        "(equal (multiple-value-list (ordinary-hook 1)) '(:adapted 2))",
    );
}

#[test]
fn method_replacement_notifies_dependents_in_removal_then_addition_order() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defclass listener () ())
      (defvar *changes* nil)
      (defmethod zk-mop:update-dependent ((gf generic-function) (who listener) &rest action)
        (declare (ignore who)) (push action *changes*))
      (defgeneric watched (x y))
      (defparameter *listener* (make-instance 'listener))
      (zk-mop:add-dependent #'watched *listener*)
      (zk-mop:add-dependent #'watched *listener*)
      (defparameter *original-method* (defmethod watched ((x integer) (y integer)) x))
      (defparameter *replacement-method* (defmethod watched ((x integer) (y integer)) y))
      (defparameter *special* (zk-mop:intern-eql-specializer 7))
      (defparameter *eql-specialized-method* (defmethod watched ((x (eql 7)) (y integer)) x))
    "#,
    )
    .unwrap();
    rt.collect();
    assert_truthy(&rt,"(and (equal (reverse *changes*) (list (list 'add-method *original-method*) (list 'remove-method *original-method*)
                                                      (list 'add-method *replacement-method*) (list 'add-method *eql-specialized-method*)))
                     (= 1 (count *replacement-method* (zk-mop:specializer-direct-methods (find-class 'integer))))
                     (not (member *original-method* (zk-mop:specializer-direct-methods (find-class 'integer))))
                     (equal (zk-mop:specializer-direct-generic-functions *special*) (list #'watched))
                     (progn (remove-method #'watched *eql-specialized-method*) (null (zk-mop:specializer-direct-methods *special*))))");
    assert_truthy(
        &rt,
        "(progn (zk-mop:remove-dependent #'watched *listener*) (setq *changes* nil)
                      (remove-method #'watched *replacement-method*) (null *changes*))",
    );
}

#[test]
fn extension_allocations_use_the_metaclass_slot_protocol() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass table-meta (standard-class) ((values :initform (make-hash-table :test 'eq) :reader table-values)))
      (defmethod zk-mop:validate-superclass ((m table-meta) (parent standard-class)) t)
      (defclass remote-slot (zk-mop:standard-effective-slot-definition) ())
      (defmethod zk-mop:effective-slot-definition-class ((c table-meta) &rest options)
        (if (eq (getf options :allocation) :table) (find-class 'remote-slot) (call-next-method)))
      (defmethod zk-mop:slot-value-using-class ((class table-meta) o (slot remote-slot))
        (gethash o (table-values class)))
      (defmethod (setf zk-mop:slot-value-using-class) (v (class table-meta) o (slot remote-slot))
        (setf (gethash o (table-values class)) v))
      (defmethod zk-mop:slot-boundp-using-class ((class table-meta) o (slot remote-slot))
        (nth-value 1 (gethash o (table-values class))))
      (defclass remote () ((x :initarg :x :allocation :table :accessor remote-x) (local :initform 2))
        (:metaclass table-meta))
      (defparameter *r* (make-instance 'remote :x 9))
    "#).unwrap();
    rt.collect();
    assert_truthy(&rt,"(let ((slot (find 'x (zk-mop:class-slots (class-of *r*)) :key #'zk-mop:slot-definition-name)))
                 (and (eq (zk-mop:slot-definition-allocation slot) :table)
                      (null (zk-mop:slot-definition-location slot)) (= (remote-x *r*) 9)
                      (= 7 (setf (remote-x *r*) 7)) (= 2 (slot-value *r* 'local))))");
}

#[test]
fn file_compilation_uses_declared_method_classes_without_publishing_the_generic() {
    let directory = TempDir::new("method-protocol");
    let source = directory.path().join("method.lisp");
    std::fs::write(&source,r#"
       (eval-when (:compile-toplevel :load-toplevel :execute)
         (defclass file-function (standard-generic-function) () (:metaclass zk-mop:funcallable-standard-class))
         (defclass file-method (standard-method) ((tag :initarg :tag :reader file-tag)))
         (defmethod zk-mop:make-method-lambda :around ((g file-function) (m file-method) expression env)
           (multiple-value-bind (body options) (call-next-method)
             (values body (list* :tag :from-compiler options)))))
       (defgeneric file-call (x) (:generic-function-class file-function) (:method-class file-method))
       (let ((seed 7))
         (defmethod file-call ((x integer)) (+ seed x)))
       (defgeneric file-inline (x) (:generic-function-class file-function) (:method-class file-method)
         (:method ((x integer)) (* x 2)))
    "#).unwrap();
    let compiler = Runtime::start().unwrap();
    let fasl = compiler.compile_file(&source, None).unwrap();
    assert_truthy(&compiler, "(not (fboundp 'file-call))");
    std::fs::remove_file(&source).unwrap();
    let fresh = Runtime::start().unwrap();
    fresh.load_file(&fasl).unwrap();
    fresh.collect();
    assert_truthy(&fresh,"(and (= (file-call 5) 12) (= (file-inline 8) 16)
                       (eq (file-tag (find-method #'file-call nil (list (find-class 'integer)))) :from-compiler)
                       (eq (file-tag (find-method #'file-inline nil (list (find-class 'integer)))) :from-compiler))");
}

#[test]
fn direct_method_links_reflect_membership_even_when_a_notification_exits() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
       (defclass read-gf (standard-generic-function) () (:metaclass zk-mop:funcallable-standard-class))
       (defgeneric get-field (o) (:generic-function-class read-gf))
       (defclass record-a () ((field :reader get-field)))
       (defparameter *old-reader* (find-method #'get-field nil (list (find-class 'record-a))))
       (defparameter *observed* nil)
       (defmethod remove-method :before ((g read-gf) (m zk-mop:standard-reader-method))
         (push (eq g (zk-mop:method-generic-function m)) *observed*))
       (defclass record-a () ((other)))
       (defclass counted-dependent () ())
       (defgeneric notify-exit (o))
       (zk-mop:add-dependent #'notify-exit (make-instance 'counted-dependent))
       (defmethod zk-mop:update-dependent ((g generic-function) (d counted-dependent) &rest args)
         (declare (ignore g d)) (throw 'notification args))
       (defparameter *registered* (second (catch 'notification (defmethod notify-exit ((o record-a)) :ok))))
    "#).unwrap();
    rt.collect();
    assert_truthy(&rt,"(and (equal *observed* '(t)) (null (zk-mop:method-generic-function *old-reader*))
                (not (member *old-reader* (zk-mop:specializer-direct-methods (find-class 'record-a))))
                (eq :ok (notify-exit (make-instance 'record-a)))
                (member *registered* (zk-mop:specializer-direct-methods (find-class 'record-a)))
                (equal (catch 'notification (remove-method #'notify-exit *registered*)) (list 'remove-method *registered*))
                (not (member *registered* (zk-mop:specializer-direct-methods (find-class 'record-a)))))");
}

#[test]
fn user_specializers_participate_in_custom_method_lookup() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass even-specializer (zk-mop:specializer) ())
      (defclass parity-gf (standard-generic-function) () (:metaclass zk-mop:funcallable-standard-class))
      (defgeneric parity (x) (:generic-function-class parity-gf))
      (defparameter *even* (make-instance 'even-specializer))
      (defmethod zk-mop:compute-applicable-methods-using-classes ((gf parity-gf) classes)
        (declare (ignore gf classes)) (values nil nil))
      (defmethod compute-applicable-methods ((gf parity-gf) args)
        (remove-if-not
          (lambda (m) (let ((s (first (zk-mop:method-specializers m))))
                        (if (eq s *even*) (evenp (first args)) (typep (first args) s))))
          (zk-mop:generic-function-methods gf)))
      (defmethod parity (x) :other)
      (defparameter *even-method*
        (make-instance 'standard-method :lambda-list '(x) :specializers (list *even*)
                       :function (lambda (args next) (declare (ignore args next)) :even)))
      (add-method #'parity *even-method*)
    "#).unwrap();
    rt.collect();
    assert_truthy(&rt, "(and (eq :even (parity 4)) (eq :other (parity 3))
                    (eq *even-method* (find-method #'parity nil (list *even*)))
                    (equal (zk-mop:specializer-direct-generic-functions *even*) (list #'parity))
                    (progn (remove-method #'parity *even-method*) (null (zk-mop:specializer-direct-methods *even*))))");
}

#[test]
fn effective_method_options_bind_each_generic_lambda_list_section() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
       (defgeneric options-gf (a b &optional c &rest keys))
       (defmethod options-gf (a b &optional c &rest keys) (declare (ignore a b c keys)) :primary)
       (defmethod zk-mop:compute-effective-method ((g (eql #'options-gf)) combination methods)
         (declare (ignore g combination))
         (values (list 'list 'first-arg 'opt 'supplied 'tail 'tag 'whole 'owner
                       (list 'call-method (first methods) nil))
                 '((:arguments &whole whole first-arg &optional (opt 77 supplied) &rest tail &key tag)
                   (:generic-function owner))))
    "#).unwrap();
    assert_truthy(&rt,"(and (equal (options-gf 1 2) (list 1 77 nil nil nil '(1 2) #'options-gf :primary))
                   (equal (options-gf 1 2 3 :tag 4 :other 9)
                     (list 1 3 t '(:tag 4 :other 9) 4 '(1 2 3 :tag 4 :other 9) #'options-gf :primary)))");
}

#[test]
fn extract_specializer_names_returns_unevaluated_forms_from_valid_bindings() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defvar *evaluations* 0)
      (let* ((input '((x (eql (incf *evaluations*))) y &optional (z 2 zp) &rest args
                      &key ((:tag value) nil vp) &allow-other-keys &aux a))
             (saved (copy-tree input)))
        (assert (equal (zk-mop:extract-lambda-list input) (list* 'x 'y (cddr input))))
        (assert (equal (zk-mop:extract-specializer-names input) '((eql (incf *evaluations*)) t)))
        (assert (and (equal saved input) (zerop *evaluations*))))
      (assert (equal (zk-mop:extract-specializer-names '((x 42) (y (eql a b))))
                     '(42 (eql a b))))
      (dolist (input '(((x integer extra)) ((x)) ((x integer) x) (x &rest)
                       (x &optional (x 1)) (x . tail) (&key ((:p p) nil p))))
        (dolist (reader (list #'zk-mop:extract-lambda-list #'zk-mop:extract-specializer-names))
          (assert (handler-case (progn (funcall reader input) nil) (error () t)))))
    "#,
    )
    .unwrap();
}

fn load_compiled_forms(forms: &str) -> Runtime {
    let directory = TempDir::new("compiled-forms");
    let source = directory.write("forms.lisp", forms);
    let compiler = Runtime::start().unwrap();
    let fasl = compiler.compile_file(&source, None).unwrap();
    let rt = Runtime::start().unwrap();
    rt.load_file(&fasl).unwrap();
    rt
}

#[test]
fn class_specializer_objects_preserve_identity_and_load_by_proper_name() {
    let rt = load_compiled_forms(
        r#"
        (eval-when (:compile-toplevel :load-toplevel :execute)
          (defclass direct-specializer () ()))
        (defclass alternate-specializer () ())
        (defmethod compiled-specializer ((x #.(find-class 'direct-specializer))) :direct)
        (defmethod compiled-specializer ((x #.(find-class 'integer))) :builtin)
        (defmethod dynamic-specializer (x) :fallback)
        (defparameter *selected-class* (find-class 'direct-specializer))
        (unwind-protect
            (progn
              (setf (find-class 'direct-specializer) (find-class 'alternate-specializer))
              (eval (list 'defmethod 'dynamic-specializer
                          (list (list 'x *selected-class*)) :direct)))
          (setf (find-class 'direct-specializer) *selected-class*))
    "#,
    );
    rt.collect();
    assert_truthy(
        &rt,
        "(let* ((members (list *selected-class*))
                (compiled (find-method #'compiled-specializer nil members))
                (dynamic (find-method #'dynamic-specializer nil members))
                (target (make-instance *selected-class*)))
           (and (eq *selected-class* (find-class 'direct-specializer))
                (eq *selected-class* (first (zk-mop:method-specializers compiled)))
                (eq *selected-class* (first (zk-mop:method-specializers dynamic)))
                (eq :direct (compiled-specializer target))
                (eq :builtin (compiled-specializer 42))
                (eq :direct (dynamic-specializer target))
                (eq :fallback (dynamic-specializer (make-instance 'alternate-specializer)))))",
    );
}

#[test]
fn compiled_defclass_passes_dotted_option_tails_to_the_metaclass() {
    let rt = load_compiled_forms(
        r#"
       (defclass option-meta (standard-class)
         ((tag :initarg tag :reader option-tag) (path :initarg :path :reader option-path)))
       (defmethod zk-mop:validate-superclass ((c option-meta) (s standard-class)) t)
       (defclass option-holder () () (:metaclass option-meta) (tag . t) (:path root . leaf))
    "#,
    );
    rt.load_str(
        r#"
      (let ((c (find-class 'option-holder)))
        (assert (eq t (option-tag c)))
        (assert (equal '(root . leaf) (option-path c))))
      (dolist (bad '((:metaclass . standard-class) (:documentation . "bad")
                     (:default-initargs :x . 1)))
        (assert (handler-case (progn (macroexpand-1 (list 'defclass 'invalid nil nil bad)) nil)
                  (error () t))))
    "#,
    )
    .unwrap();
}

#[test]
fn compiled_handler_case_selects_a_clause_by_its_compound_type() {
    let rt = load_compiled_forms(
        r#"
       (defun handle-option-error (condition)
         (handler-case (error condition)
           ((or arithmetic-error cell-error) () :matched)
           (error () :fallback)))
    "#,
    );
    rt.load_str(
        r#"
      (assert (eq :matched (handle-option-error (make-condition 'unbound-variable :name 'missing))))
      (assert (eq :fallback (handle-option-error (make-condition 'error))))
    "#,
    )
    .unwrap();
}

#[test]
fn effective_class_allocation_shares_the_defining_class_cell_across_finalization() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defvar *cell-inits* 0)
      (defclass shared-meta (standard-class) ())
      (defmethod zk-mop:validate-superclass ((c shared-meta) (s standard-class)) t)
      (defmethod zk-mop:compute-effective-slot-definition :around ((c shared-meta) name direct)
        (declare (ignore direct))
        (let ((slot (call-next-method)))
          (when (eq name 'v)
            (setf (zk-mop:slot-definition-allocation slot) :class))
          slot))
      (defclass owner () ((v :initform (list (incf *cell-inits*)) :accessor value-of))
        (:metaclass shared-meta))
      (defclass child (owner) () (:metaclass shared-meta))
      (defparameter *owner* (make-instance 'owner))
      (defparameter *child* (make-instance 'child))
      (assert (and (= 1 *cell-inits*) (eq (value-of *owner*) (value-of *child*))))
      (setf (value-of *child*) :saved)
      (defclass owner () ((v :initform (incf *cell-inits*) :accessor value-of) extra)
        (:metaclass shared-meta))
    "#,
    )
    .unwrap();
    rt.collect();
    rt.load_str(r#"
      (assert (and (= 1 *cell-inits*) (eq :saved (value-of *owner*))
                   (eq :saved (value-of (make-instance 'child)))))
      (let ((slot (find 'v (zk-mop:class-slots (class-of *owner*)) :key #'zk-mop:slot-definition-name)))
        (assert (eq :class (zk-mop:slot-definition-allocation slot)))
        (assert (handler-case (progn (setf (zk-mop:slot-definition-allocation slot) :instance) nil)
                  (error () t))))
      (defclass shadow-owner (owner) ((v :initform :shadow)) (:metaclass shared-meta))
      (assert (eq :shadow (value-of (make-instance 'shadow-owner))))
      (assert (eq :saved (value-of *owner*)))
    "#).unwrap();
}

#[test]
fn loading_a_compiled_class_initializes_its_preexisting_class_object() {
    let directory = TempDir::new("provisional-metaclass-defaults");
    let source = directory.path().join("classes.lisp");
    std::fs::write(
        &source,
        r#"
      (defvar *default-count* 0)
      (defvar *made* nil)
      (defclass defaulting-meta (standard-class)
        ((label :initarg :label :reader class-label))
        (:default-initargs :label (incf *default-count*)))
      (defmethod zk-mop:validate-superclass ((c defaulting-meta) (s standard-class)) t)
      (defmethod make-instance :around ((c (eql (find-class 'defaulting-meta))) &rest initargs)
        (push (getf initargs :name) *made*) (call-next-method))
      (defmethod zk-mop:ensure-class-using-class :around (old (name (eql 'inferred)) &rest args)
        (apply #'call-next-method old name :metaclass 'defaulting-meta args))
      (defclass inferred () ())
      (defclass reserved () () (:metaclass defaulting-meta))
      (defclass explicit () () (:metaclass defaulting-meta) (:label supplied))
    "#,
    )
    .unwrap();
    let rt = Runtime::start().unwrap();
    let fasl = rt.compile_file(&source, None).unwrap();
    rt.eval("(defparameter *reserved* (find-class 'reserved))")
        .unwrap();
    rt.load_file(&fasl).unwrap();
    rt.load_str(
        r#"
      (assert (eq *reserved* (find-class 'reserved)))
      (assert (eq (class-of (find-class 'inferred)) (find-class 'defaulting-meta)))
      (assert (= 1 (class-label (find-class 'inferred))))
      (assert (= 2 *default-count* (class-label *reserved*)))
      (assert (equal (class-label (find-class 'explicit)) '(supplied)))
      (assert (equal *made* '(explicit reserved inferred)))
    "#,
    )
    .unwrap();
}

#[test]
fn finalization_hooks_observe_initialized_prototypes_without_restarting_the_layout() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defclass observing-meta (standard-class) ())
      (defmethod zk-mop:validate-superclass ((c observing-meta) (s standard-class)) t)
      (defgeneric observe (o))
      (defmethod observe (o) (slot-value o 'shared))
      (defvar *inits* 0)
      (defvar *observed* nil)
      (defvar *observing* nil)
      (defmethod zk-mop:finalize-inheritance :after ((c observing-meta))
        (when (zk-mop:class-finalized-p c)
          (assert (not *observing*))
          (let ((*observing* t))
            (shared-initialize c nil)
            (push (observe (zk-mop:class-prototype c)) *observed*))))
      (defclass observed () ((shared :allocation :class :initform (incf *inits*)))
        (:metaclass observing-meta))
      (defclass observed-child (observed) () (:metaclass observing-meta))
      (defclass observed () ((shared :allocation :class :initform (incf *inits*)) new-slot)
        (:metaclass observing-meta))
      (assert (= 1 *inits*))
      (assert (and *observed* (every (lambda (n) (eql n 1)) *observed*)))
    "#,
    )
    .unwrap();
}

#[test]
fn funcallable_instance_documentation_survives_function_replacement() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defclass callable-note () () (:metaclass zk-mop:funcallable-standard-class))
      (defparameter *note* (make-instance 'callable-note))
      (setf (fdefinition 'documented-call) *note*)
      (setf (documentation 'documented-call 'function) (concatenate 'string "instance" " doc"))
      (zk-mop:set-funcallable-instance-function *note* (lambda (x) (1+ x)))
    "#,
    )
    .unwrap();
    rt.collect();
    rt.load_str(
        r#"
      (assert (string= (documentation *note* t) "instance doc"))
      (assert (string= (documentation 'documented-call 'function) "instance doc"))
      (assert (= 5 (documented-call 4)))
      (zk-mop:set-funcallable-instance-function *note* #'identity)
      (assert (string= (documentation *note* 'function) "instance doc"))
      (setf (documentation *note* t) nil)
      (assert (null (documentation 'documented-call 'function)))
    "#,
    )
    .unwrap();
}

#[test]
fn inherited_dispatch_calls_only_currently_installed_reader_hooks() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass tracked-gf (standard-generic-function) ()
        (:metaclass zk-mop:funcallable-standard-class))
      (defclass tracked-method (standard-method) ())
      (defgeneric read-through (x) (:generic-function-class tracked-gf) (:method-class tracked-method))
      (defmethod read-through ((x integer)) (1+ x))
      (defvar *reads* nil)
      (assert (= 2 (read-through 1)))
      (let ((hook (defmethod zk-mop:generic-function-method-combination :around ((g tracked-gf))
                    (push :combination *reads*) (call-next-method))))
        (setf *reads* nil)
        (assert (= 3 (read-through 2)))
        (assert (member :combination *reads*))
        (remove-method #'zk-mop:generic-function-method-combination hook))
      (let ((hook (defmethod method-qualifiers :around ((m tracked-method))
                    (push :qualifiers *reads*) (call-next-method))))
        (setf *reads* nil)
        (assert (= 4 (read-through 3)))
        (assert (member :qualifiers *reads*))
        (remove-method #'method-qualifiers hook))
      (setf *reads* nil)
      (assert (= 5 (read-through 4)))
      (assert (null *reads*))
    "#).unwrap();
}

#[test]
fn direct_method_hooks_receive_repeated_specializers_in_parameter_order() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defclass tracked-method (standard-method) ())
      (defclass tracked-class () ())
      (defgeneric tracked (a b c d) (:method-class tracked-method))
      (defvar *events* nil)
      (defun log-link (step s m)
        (push (list step (typep s 'zk-mop:eql-specializer)
                         (not (null (member m (zk-mop:specializer-direct-methods s))))
                         (eq #'tracked (zk-mop:method-generic-function m)))
              *events*))
      (defmethod zk-mop:add-direct-method :around (s (m tracked-method))
        (log-link :adding s m)
        (call-next-method)
        (log-link :added s m))
      (defmethod zk-mop:remove-direct-method :around (s (m tracked-method))
        (log-link :removing s m)
        (call-next-method)
        (log-link :removed s m))
      (defmethod tracked ((a tracked-class) (b (eql 1)) (c tracked-class) (d (eql 1))) :old)
      (defvar *add-log*
        '((:adding nil nil t) (:added nil t t) (:adding t nil t) (:added t t t)
          (:adding nil t t) (:added nil t t) (:adding t t t) (:added t t t)))
      (assert (equal *add-log* (nreverse *events*)))
      (setf *events* nil)
      (defvar *replacement*
        (defmethod tracked ((a tracked-class) (b (eql 1)) (c tracked-class) (d (eql 1))) :new))
      (assert (equal (append '((:removing nil t nil) (:removed nil nil nil)
                               (:removing t t nil) (:removed t nil nil)
                               (:removing nil nil nil) (:removed nil nil nil)
                               (:removing t nil nil) (:removed t nil nil)) *add-log*)
                     (nreverse *events*)))
      (dolist (s (list (find-class 'tracked-class) (zk-mop:intern-eql-specializer 1)))
        (assert (= 1 (count *replacement* (zk-mop:specializer-direct-methods s)))))
    "#,
    )
    .unwrap();
}

#[test]
fn method_hook_reentry_leaves_links_consistent_with_the_final_owner() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass first-spec () ())
      (defclass last-spec () ())
      (defclass observed-method (standard-method) ())
      (defgeneric observed (a b) (:method-class observed-method))
      (defvar *action* nil)
      (defmethod zk-mop:add-direct-method :before ((s (eql (find-class 'first-spec))) (m observed-method))
        (when (eq *action* :remove-before) (remove-method #'observed m)))
      (defmethod zk-mop:add-direct-method :after ((s (eql (find-class 'first-spec))) (m observed-method))
        (case *action*
          (:exit (throw 'abort m))
          (:remove (remove-method #'observed m))))
      (defmethod zk-mop:remove-direct-method :before ((s (eql (find-class 'first-spec))) (m observed-method))
        (case *action*
          (:exit-remove (throw 'abort m))
          (:reattach (let ((*action* nil)) (add-method #'observed m)))))
      (defun installed () (defmethod observed ((a first-spec) (b last-spec)) t))
      (defun detached (m)
        (and (not (zk-mop:method-generic-function m))
             (not (member m (zk-mop:generic-function-methods #'observed)))
             (every (lambda (s) (not (member m (zk-mop:specializer-direct-methods s))))
                    (zk-mop:method-specializers m))))
      (let ((*action* :exit)) (assert (detached (catch 'abort (installed)))))
      (let ((*action* :remove)) (assert (detached (installed))))
      (let ((*action* :remove-before)) (assert (detached (installed))))
      (let ((m (installed)) (*action* :reattach))
        (remove-method #'observed m)
        (assert (eq #'observed (zk-mop:method-generic-function m)))
        (dolist (s (zk-mop:method-specializers m))
          (assert (member m (zk-mop:specializer-direct-methods s))))
        (let ((*action* nil)) (remove-method #'observed m)))
      (let ((m (installed)) (*action* :exit-remove))
        (assert (eq m (catch 'abort (remove-method #'observed m))))
        (assert (detached m)))

      ;; A nested replacement installed by REMOVE-METHOD remains registered.
      (defclass nested-function (standard-generic-function) ()
        (:metaclass zk-mop:funcallable-standard-class))
      (defgeneric nested (arg) (:generic-function-class nested-function))
      (defvar *insert* nil)
      (defmethod remove-method :after ((g nested-function) m)
        (when *insert* (add-method g *insert*)))
      (defvar *inner* (defmethod nested ((arg last-spec)) :inner))
      (defmethod nested ((arg last-spec)) :original)
      (let ((*insert* *inner*))
        (assert (handler-case (progn (defmethod nested ((arg last-spec)) :outer) nil)
                  (program-error () t))))
      (assert (equal (zk-mop:generic-function-methods #'nested) (list *inner*)))
      (assert (eq :inner (nested (make-instance 'last-spec))))
      (assert (member *inner* (zk-mop:specializer-direct-methods (find-class 'last-spec))))

      ;; A failed move must leave the destination free to infer a different arity.
      (let ((destination (make-instance 'standard-generic-function)))
        (assert (handler-case (progn (add-method destination *inner*) nil)
                  (program-error () t)))
        (add-method destination (make-instance 'standard-method :specializers nil :lambda-list nil
                                  :function (lambda (args next) (declare (ignore args next)) 17)))
        (assert (= (funcall destination) 17)))

      (defclass reader-observer () ())
      (defgeneric tracked-reader (object))
      (defclass observed-record () ())
      (defmethod zk-mop:update-dependent ((g (eql #'tracked-reader)) (d reader-observer) &rest change)
        (when (eq (first change) 'add-method) (throw 'abort (second change))))
      (zk-mop:add-dependent #'tracked-reader (make-instance 'reader-observer))
      (defvar *stale* (catch 'abort (defclass observed-record () ((x :reader tracked-reader)))))
      (assert (eq #'tracked-reader (zk-mop:method-generic-function *stale*)))
      (defclass observed-record () ())
      (assert (null (zk-mop:method-generic-function *stale*)))
      (assert (not (member *stale* (zk-mop:specializer-direct-methods (find-class 'observed-record)))))

      (defgeneric early-reader (o))
      (defgeneric late-reader (o))
      (defmethod zk-mop:update-dependent ((g (eql #'early-reader)) (d reader-observer) &rest change)
        (when (eq (first change) 'add-method)
          (defclass observed-record () ())
          (assert (null (zk-mop:method-generic-function (second change))))))
      (zk-mop:add-dependent #'early-reader (make-instance 'reader-observer))
      (defclass observed-record () ((x :reader early-reader) (y :reader late-reader)))
      (assert (null (zk-mop:generic-function-methods #'early-reader)))
      (assert (null (zk-mop:generic-function-methods #'late-reader)))
    "#).unwrap();
}

#[test]
fn eql_specializer_reader_uses_slot_unbound_before_initialization() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defvar *blank* (allocate-instance (find-class 'zk-mop:eql-specializer)))
        (assert (handler-case (progn (zk-mop:eql-specializer-object *blank*) nil)
                  (unbound-slot (c) (and (eq *blank* (unbound-slot-instance c))
                                         (eq 'zk-mop::object (cell-error-name c))))))
        (defmethod slot-unbound (class (s (eql *blank*)) name)
          (assert (and (eq class (class-of s)) (eq name 'zk-mop::object)))
          (zk:gc)
          (values :supplied :secondary))
        (assert (equal '(:supplied) (multiple-value-list (zk-mop:eql-specializer-object *blank*))))
        (initialize-instance *blank* :object nil)
        (assert (equal '(nil) (multiple-value-list (zk-mop:eql-specializer-object *blank*))))
        (assert (handler-case (progn (zk-mop:eql-specializer-object 42) nil)
                  (type-error (c) (eql 42 (type-error-datum c)))))
        "#,
    )
    .unwrap();
}

#[test]
fn eql_specializer_interning_keeps_only_reachable_entries() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defun observe-unused-specializer ()
        (let* ((key (list :transient)) (s (zk-mop:intern-eql-specializer key)))
          (list (zk:make-weak-pointer key) (zk:make-weak-pointer s))))
      (defvar *number-digits* "123456789012345678901234567890123456789")
      (defun fresh-equal-number () (parse-integer *number-digits*))
      (defparameter *number-specializer* (zk-mop:intern-eql-specializer (fresh-equal-number)))
      (defparameter *number-observer*
        (zk:make-weak-pointer (zk-mop:eql-specializer-object *number-specializer*)))
      (defparameter *unused* (observe-unused-specializer))
      (defparameter *key* (list :live-key))
      (defgeneric weak-dispatch (object))
      (defmethod weak-dispatch ((object (eql *key*))) :hit)
      (defparameter *observer* (zk:make-weak-pointer (zk-mop:intern-eql-specializer *key*)))
      (assert (eq :hit (weak-dispatch *key*)))
    "#,
    )
    .unwrap();
    rt.collect();
    rt.load_str(
        r#"
      (assert (eq (zk:weak-pointer-value *number-observer*)
                  (zk-mop:eql-specializer-object *number-specializer*)))
      (assert (eq *number-specializer* (zk-mop:intern-eql-specializer (fresh-equal-number))))
      (setf *number-specializer* nil)
      (assert (every (lambda (w) (not (nth-value 1 (zk:weak-pointer-value w)))) *unused*))
      (let* ((s (zk:weak-pointer-value *observer*))
             (method (find-method #'weak-dispatch nil (list s))))
        (assert (and s (eq s (zk-mop:intern-eql-specializer *key*))))
        (assert (eq *key* (zk-mop:eql-specializer-object s)))
        (assert (eq s (first (zk-mop:method-specializers method))))
        (remove-method #'weak-dispatch method))
    "#,
    )
    .unwrap();
    rt.collect();
    rt.eval(
        "(assert (every (lambda (w) (not (nth-value 1 (zk:weak-pointer-value w))))
                     (list *number-observer* *observer*)))",
    )
    .unwrap();
}

#[test]
fn nested_method_updates_preserve_owned_snapshots_and_the_latest_discriminator() {
    let rt = Runtime::start().unwrap();
    rt.load_str(r#"
      (defclass snapshot-function (standard-generic-function) ()
        (:metaclass zk-mop:funcallable-standard-class))
      (defclass snapshot-argument () ())
      (defclass snapshot-observer () ())
      (defgeneric snapshot-size (x) (:generic-function-class snapshot-function))
      (defvar *operation* nil)
      (defvar *notifications* nil)
      (defmethod zk-mop:update-dependent ((g (eql #'snapshot-size)) (d snapshot-observer) &rest args)
        (push (first args) *notifications*))
      (zk-mop:add-dependent #'snapshot-size (make-instance 'snapshot-observer))
      (defmethod zk-mop:compute-discriminating-function ((g snapshot-function))
        (let ((size (length (zk-mop:generic-function-methods g))))
          (when (member *operation* '(:exit-removal :exit-refresh)) (throw 'stop-update :discriminator))
          (when (eq *operation* :recursive-retry) (assert (eq :integer (funcall g 7))))
          (when (eq *operation* :add-during-refresh)
            (let ((*operation* nil)) (defmethod snapshot-size ((x integer)) :integer)))
          (lambda (x) (declare (ignore x)) size)))
      (defmethod zk-mop:add-direct-method :after ((s (eql (find-class 'snapshot-argument))) m)
        (when (eq #'snapshot-size (zk-mop:method-generic-function m))
          (case *operation*
            (:reattach-then-exit
              (let ((*operation* nil))
                (remove-method #'snapshot-size m)
                (add-method #'snapshot-size m))
              (throw 'stop-update m))
            (:add-other-then-exit
              (defmethod snapshot-size ((x symbol)) :symbol)
              (throw 'stop-update m)))))
      (defmethod zk-mop:remove-direct-method :before ((s (eql (find-class 'snapshot-argument))) m)
        (when (eq *operation* :exit-removal) (throw 'stop-update m)))
      (defun new-snapshot-method () (defmethod snapshot-size ((x snapshot-argument)) t))

      (let ((*operation* :add-during-refresh))
        (defmethod snapshot-size ((x string)) :string))
      (assert (= 2 (snapshot-size nil)))
      (let* ((*operation* :reattach-then-exit)
             (*notifications* nil)
             (m (catch 'stop-update (new-snapshot-method)))
             (events (reverse *notifications*))
             (neighbor (let ((*operation* nil))
                         (defmethod snapshot-size :before ((x snapshot-argument)) nil)))
             (methods (zk-mop:generic-function-methods #'snapshot-size))
             (specializer (find-class 'snapshot-argument))
             (direct (zk-mop:specializer-direct-methods specializer))
             (generics (zk-mop:specializer-direct-generic-functions specializer)))
        (assert (equal events '(remove-method add-method)))
        (assert (eq #'snapshot-size (zk-mop:method-generic-function m)))
        (assert (= 4 (snapshot-size nil)))
        (let ((*operation* :exit-removal))
          (assert (eq m (catch 'stop-update (remove-method #'snapshot-size m)))))
        (assert (null (zk-mop:method-generic-function m)))
        (assert (= 3 (snapshot-size nil)))
        (remove-method #'snapshot-size neighbor)
        (assert (null (zk-mop:specializer-direct-generic-functions specializer)))
        (assert (and (member m methods) (member m direct) (member #'snapshot-size generics)))
        (assert (not (member m (zk-mop:specializer-direct-methods specializer)))))
      (let* ((*operation* :add-other-then-exit)
             (discarded (catch 'stop-update (new-snapshot-method))))
        (assert (null (zk-mop:method-generic-function discarded)))
        (assert (= 3 (length (zk-mop:generic-function-methods #'snapshot-size)) (snapshot-size nil))))
      (let* ((m (new-snapshot-method)) (*operation* :exit-refresh))
        (assert (eq :discriminator (catch 'stop-update (remove-method #'snapshot-size m))))
        (assert (eq :discriminator (catch 'stop-update (snapshot-size nil)))))
      (let ((*operation* :recursive-retry)) (assert (= 3 (snapshot-size nil))))
    "#).unwrap();
}

#[test]
fn defgeneric_removes_its_inline_methods_before_changing_arity() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defclass inline-function (standard-generic-function) ()
        (:metaclass zk-mop:funcallable-standard-class))
      (defgeneric inline-target (x) (:generic-function-class inline-function)
        (:method ((x integer)) :old))
      (defvar *removal* nil)
      (defmethod remove-method :around ((g inline-function) m)
        (case *removal*
          (:veto (throw 'veto (funcall g 7)))
          (:skip g)
          (otherwise (call-next-method))))
      (defun change-arity ()
        (defgeneric inline-target (x y) (:generic-function-class inline-function)))
      (let ((*removal* :veto)) (assert (eq :old (catch 'veto (change-arity)))))
      (assert (equal '(x) (zk-mop:generic-function-lambda-list #'inline-target)))
      (let ((*removal* :skip))
        (assert (handler-case (progn (change-arity) nil) (error () t))))
      (assert (eq :old (inline-target 7)))
      (change-arity)
      (assert (null (zk-mop:generic-function-methods #'inline-target)))
      (assert (equal '(x y) (zk-mop:generic-function-lambda-list #'inline-target)))

      (defgeneric moved-source (x) (:method (x) :moved))
      (defgeneric moved-target (x))
      (let ((m (find-method #'moved-source nil (list (find-class t)))))
        (remove-method #'moved-source m)
        (add-method #'moved-target m)
        (defgeneric moved-target (x))
        (assert (eq :moved (moved-target 1)))
        (remove-method #'moved-target m)
        (add-method #'moved-source m)
        (defgeneric moved-source (x))
        (assert (null (zk-mop:method-generic-function m)))
        (add-method #'moved-source m)
        (defgeneric moved-source (x))
        (assert (eq :moved (moved-source 2))))
    "#,
    )
    .unwrap();
}
