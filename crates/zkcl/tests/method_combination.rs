use zkcl::Runtime;

#[test]
fn standard_combination_runs_methods_in_specificity_order() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defparameter *method-log* nil)
            (defgeneric combined (object &optional argument))

            (defmethod combined :around ((object integer) &optional argument)
              (push (list :integer-around (next-method-p)) *method-log*)
              (multiple-value-prog1 (call-next-method (+ object 1) argument)
                (push :around-exit *method-log*)))
            (defmethod combined :around (object &optional argument)
              (push (list :general-around object (next-method-p)) *method-log*)
              (call-next-method))
            (defmethod combined :before ((object integer) &optional argument)
              (push (list :integer-before object argument) *method-log*))
            (defmethod combined :before (object &optional argument)
              (push :general-before *method-log*))
            (defmethod combined ((object integer) &optional argument)
              (push (list :integer-primary (next-method-p)) *method-log*)
              (setq object 999)
              (call-next-method))
            (defmethod combined (object &optional argument)
              (push (list :general-primary (next-method-p)) *method-log*)
              (values object argument))
            (defmethod combined :after ((object integer) &optional argument)
              (push :integer-after *method-log*)
              (values :discarded :also-discarded))
            (defmethod combined :after (object &optional argument)
              (push :general-after *method-log*))

            "#,
        )
        .unwrap();

    let results = runtime
        .eval(
            r#"(let* ((*method-log* nil)
                      (results (multiple-value-list (combined 2 9))))
                 (values
                   (equal results '(3 9))
                   (equal *method-log*
                          '(:around-exit :integer-after :general-after
                            (:general-primary nil) (:integer-primary t)
                            :general-before (:integer-before 3 9)
                            (:general-around 3 t) (:integer-around t)))))"#,
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|value| !value.is_nil()));
}

#[test]
fn call_method_distinguishes_an_omitted_tail_from_an_exhausted_chain() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defvar *next-request* nil)
            (defvar *exhausted-calls* nil)
            (defgeneric check-next (x))
            (defmethod check-next ((x integer))
              (unless (eq *next-request* :primary) (return-from check-next x))
              (assert (null (next-method-p)))
              (call-next-method (1+ x)))
            (defmethod check-next :before ((x integer))
              (when (eq *next-request* :before) (call-next-method :different)))
            (defmethod check-next :after ((x integer))
              (when (eq *next-request* :after) (call-next-method)))
            (defmethod no-next-method ((gf (eql #'check-next)) method &rest arguments)
              (declare (ignore gf method))
              (push arguments *exhausted-calls*)
              (values :exhausted arguments))
            (defun check-next-contexts ()
              (let ((*exhausted-calls* nil))
                (dolist (*next-request* '(:before :after))
                  (assert (eq :blocked (handler-case (check-next 5)
                                         (control-error () :blocked)))))
                (let ((*next-request* :primary))
                  (assert (equal '(:exhausted (6)) (multiple-value-list (check-next 5)))))
                (assert (equal *exhausted-calls* '((6))))))
            (check-next-contexts)
            (defmethod zk-mop:compute-effective-method :around
                ((gf (eql #'check-next)) combination methods)
              (declare (ignore gf combination methods))
              (call-next-method))
            (check-next-contexts)

            (define-method-combination select-next-tail (mode) ((primary ()))
              (ecase mode
                (:omitted `(call-method ,(first primary)))
                (:explicit `(call-method ,(first primary) nil))
                (:synthetic `(call-method (make-method (next-method-p)) nil))))
            (defgeneric omitted-tail (x) (:method-combination select-next-tail :omitted)
              (:method (x) (assert (null (next-method-p))) (call-next-method)))
            (defgeneric explicit-tail (x) (:method-combination select-next-tail :explicit)
              (:method ((x integer)) (assert (null (next-method-p))) (call-next-method (1+ x))))
            (defmethod no-next-method ((gf (eql #'explicit-tail)) method &rest arguments)
              (declare (ignore gf method))
              arguments)
            (defgeneric synthetic-tail (x) (:method-combination select-next-tail :synthetic)
              (:method (x) x))
            (assert (eq :blocked (handler-case (omitted-tail nil) (control-error () :blocked))))
            (assert (equal '(6) (explicit-tail 5)))
            (assert (null (synthetic-tail nil)))
            "#,
        )
        .unwrap();
}

#[test]
fn saved_next_method_retains_its_arguments_after_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defparameter *method-log* nil)

            (defmethod escaped-next (x &optional (y :default y-p))
              (values x y y-p))
            (defmethod escaped-next ((x integer) &optional y)
              (values #'call-next-method #'next-method-p))
            (multiple-value-bind (next next-p) (escaped-next 7 :given)
              (defparameter *saved-next* next)
              (defparameter *saved-next-p* next-p))

            (defmethod skip-primary :around ((x integer))
              (push :never *method-log*))
            (defmethod change-applicable ((x integer))
              (call-next-method :different))
            (defmethod change-applicable (x)
              (push :never *method-log*))
            (defmethod exhausted-changed ((x integer))
              (call-next-method :different))
            (defmethod no-next-method ((gf (eql #'exhausted-changed)) method &rest arguments)
              (push :never *method-log*))

            (defmethod zero-values (x) (values))
            (defmethod zero-values :after (x) (values :discarded))

            (defmethod escape ((x integer))
              (throw 'return-from-method (values :escaped 19)))
            (defmethod escape :after ((x integer))
              (push :never *method-log*))

            (defmethod missing-method ((x integer)) x)
            (defmethod missing-next ((x integer)) (call-next-method))
            "#,
        )
        .unwrap();

    runtime.collect();
    let results = runtime
        .eval(
            r#"(let ((*method-log* nil))
                 (values
                   (funcall *saved-next-p*)
                   (equal (multiple-value-list (funcall *saved-next*)) '(7 :given t))
                   (equal (multiple-value-list (funcall *saved-next* 4)) '(4 :default nil))
                   (eq (handler-case (skip-primary 1) (error () :rejected)) :rejected)
                   (eq (handler-case (change-applicable 1) (error () :rejected)) :rejected)
                   (eq (handler-case (exhausted-changed 1) (error () :rejected)) :rejected)
                   (null (multiple-value-list (zero-values 1)))
                   (equal (multiple-value-list
                            (catch 'return-from-method (escape 1)))
                          '(:escaped 19))
                   (null *method-log*)
                   (handler-case (missing-method :absent) (type-error () t))
                   (handler-case (missing-next 1) (error () t))))"#,
        )
        .unwrap();
    assert_eq!(results.len(), 11);
    assert!(results.iter().all(|value| !value.is_nil()));

    runtime
        .load_str(
            r#"
            (defmethod no-applicable-method ((gf (eql #'missing-method)) &rest arguments)
              (values :no-applicable arguments))
            (defmethod no-next-method ((gf (eql #'missing-next)) (method standard-method)
                                       &rest arguments)
              (values :no-next arguments))
            "#,
        )
        .unwrap();
    let results = runtime
        .eval(
            r#"(values
                 (equal (multiple-value-list (missing-method :absent))
                        '(:no-applicable (:absent)))
                 (equal (multiple-value-list (missing-next 12))
                        '(:no-next (12))))"#,
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|value| !value.is_nil()));
}

#[test]
fn cached_single_primary_preserves_the_next_method_protocol() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "
            (defclass solo-argument () ())
            (defparameter *solo-first* (make-instance 'solo-argument))
            (defparameter *solo-second* (make-instance 'solo-argument))
            (defmethod solo ((object solo-argument))
              (values #'call-next-method (next-method-p)))
            (defmethod no-next-method ((generic (eql #'solo)) method &rest arguments)
              (values :exhausted (car arguments)))
            (solo *solo-first*)
            (multiple-value-bind (next available) (solo *solo-first*)
              (defparameter *solo-next* next)
              (defparameter *solo-available* available))",
        )
        .unwrap();
    runtime.collect();

    let results = runtime
        .eval(
            "(values (null *solo-available*)
                     (equal (multiple-value-list (funcall *solo-next*))
                            (list :exhausted *solo-first*))
                     (equal (multiple-value-list (funcall *solo-next* *solo-second*))
                            (list :exhausted *solo-second*))
                     (handler-case (funcall *solo-next* :different)
                       (program-error () t)))",
        )
        .unwrap();
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|value| !value.is_nil()));
}

#[test]
fn cached_primary_follows_binding_method_and_layout_changes() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defclass cache-left () ())
            (defclass cache-right () ())
            (defclass cache-object (cache-left)
              ((payload :initarg :payload :accessor cache-payload)))
            (defvar *cache-object* (make-instance 'cache-object :payload 7))
            (defvar *cache-migrations* 0)
            (defmethod update-instance-for-redefined-class :after
                ((object cache-object) added discarded old-values &rest initargs)
              (declare (ignore object added discarded old-values initargs))
              (incf *cache-migrations*))

            (defmethod cache-read ((object cache-left))
              (values :left (cache-payload object)))
            (defmethod cache-read ((object cache-right))
              (values :right (cache-payload object)))
            (defvar *cache-reader* #'cache-read)
            (defvar *cache-collecting* nil)
            (defgeneric cache-collect (object))
            (defmethod cache-collect ((object cache-object))
              (when *cache-collecting* (zk:gc))
              (values object 23))
            (defun cache-collect-call (object)
              (multiple-value-list (cache-collect object)))
            (defun cache-named-call (object) (multiple-value-list (cache-read object)))
            (defun cache-value-call (reader object) (multiple-value-list (funcall reader object)))
            (defun check-cache (named value)
              (dotimes (i 4)
                (assert (equal named (cache-named-call *cache-object*)))
                (assert (equal value (cache-value-call *cache-reader* *cache-object*)))))

            (check-cache '(:left 7) '(:left 7))
            (dotimes (i 8)
              (assert (equal (cache-collect-call *cache-object*)
                             (list *cache-object* 23))))
            (let ((*cache-collecting* t))
              (assert (equal (cache-collect-call *cache-object*)
                             (list *cache-object* 23))))
            (let ((hook (defmethod zk-mop:compute-effective-method :around
                            ((generic (eql #'cache-read)) combination methods)
                          (declare (ignore combination methods))
                          '(values :mop 12))))
              (check-cache '(:mop 12) '(:mop 12))
              (remove-method #'zk-mop:compute-effective-method hook))
            (check-cache '(:left 7) '(:left 7))
            (setf (fdefinition 'cache-read) (lambda (object) (declare (ignore object)) :replacement))
            (check-cache '(:replacement) '(:left 7))
            (setf (fdefinition 'cache-read) *cache-reader*)
            (defmethod cache-read ((object cache-left))
              (values :new-left (cache-payload object)))
            (check-cache '(:new-left 7) '(:new-left 7))

            (defmethod cache-read :around ((object cache-object))
              (multiple-value-bind (origin payload) (call-next-method)
                (values :around origin payload)))
            (check-cache '(:around :new-left 7) '(:around :new-left 7))
            (remove-method #'cache-read (find-method #'cache-read '(:around) (list (find-class 'cache-object))))
            (check-cache '(:new-left 7) '(:new-left 7))

            (defclass cache-object (cache-right)
              ((inserted :initform :inserted)
               (payload :initarg :payload :accessor cache-payload)))
            (check-cache '(:right 7) '(:right 7))
            (setf (cache-payload *cache-object*) 8)
            (assert (= 1 *cache-migrations*))
            (assert (eq :inserted (slot-value *cache-object* 'inserted)))
            (check-cache '(:right 8) '(:right 8))
            "#,
        )
        .unwrap();

    runtime.collect();
    runtime
        .load_str(
            "(check-cache '(:right 8) '(:right 8))
             (assert (handler-case (funcall *cache-reader*) (program-error () t)))",
        )
        .unwrap();
}

#[test]
fn long_method_combination_evaluates_group_options_in_their_binding_scope() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (define-method-combination option-order
                (&optional (order :most-specific-first))
              ((primary (option-order) :order order :required t))
              `(list ,@(mapcar (lambda (method)
                                 `(call-method ,method))
                               primary)))

            (defgeneric option-order-default (object)
              (:method-combination option-order))
            (defmethod option-order-default option-order ((object integer)) :integer)
            (defmethod option-order-default option-order ((object number)) :number)

            (defgeneric option-order-reversed (object)
              (:method-combination option-order :most-specific-last))
            (defmethod option-order-reversed option-order ((object integer)) :integer)
            (defmethod option-order-reversed option-order ((object number)) :number)

            (defparameter *option-order-evaluations* 0)
            (define-method-combination counted-option-order
                (&optional
                 (order (progn
                          (incf *option-order-evaluations*)
                          :most-specific-first)))
              ((primary (counted-option-order) :order order :required t))
              `(list ,@(mapcar (lambda (method)
                                 `(call-method ,method))
                               primary)))
            (defgeneric option-order-counted (object)
              (:method-combination counted-option-order))
            (defmethod option-order-counted counted-option-order
                ((object integer))
              :integer)
            (defmethod option-order-counted counted-option-order
                ((object number))
              :number)
            "#,
        )
        .unwrap();
    runtime.collect();

    for (source, expected) in [
        ("(option-order-default 1)", "(:integer :number)"),
        ("(option-order-reversed 1)", "(:number :integer)"),
        (
            "(progn (setq *option-order-evaluations* 0)
                    (and (equal (option-order-counted 1) '(:integer :number))
                         (= *option-order-evaluations* 1)))",
            "t",
        ),
    ] {
        let result = runtime
            .eval(&format!("(equal {source} '{expected})"))
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!result[0].is_nil(), "{source}");
    }
}

#[test]
fn long_method_argument_sections_follow_the_generic_lambda_list() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (define-method-combination argument-sections () ((primary ()))
              (:arguments &whole all x excess
                          &optional (y (list :default x) y-p) (extra (list y-p y))
                          &rest tail &key ((:alias key) (list :missing y) key-p)
                          &aux (summary (list x y key)))
              `(list ,all ,x ,excess ,y ,y-p ,extra ,tail ,key ,key-p ,summary
                     (call-method ,(first primary))))
            (defgeneric empty-arguments () (:method-combination argument-sections))
            (defmethod empty-arguments () :empty)
            (defgeneric rest-arguments (x &optional y &rest tail)
              (:method-combination argument-sections))
            (defmethod rest-arguments (x &optional y &rest tail) (list x y tail))
            (assert (equal (empty-arguments)
                           '(nil nil nil (:default nil) nil (nil (:default nil))
                             nil (:missing (:default nil)) nil
                             (nil (:default nil) (:missing (:default nil))) :empty)))
            (assert (equal (rest-arguments 1 nil :alias nil :alias 5 :unknown 3)
                           '((1 nil :alias nil :alias 5 :unknown 3) 1 nil nil t (t nil)
                             (:alias nil :alias 5 :unknown 3) nil t (1 nil nil)
                             (1 nil (:alias nil :alias 5 :unknown 3)))))
            (assert (equal (rest-arguments 1 2 3 4)
                           '((1 2 3 4) 1 nil 2 t (t 2) (3 4) (:missing 2) nil
                             (1 2 (:missing 2)) (1 2 (3 4)))))

            (define-method-combination short-arguments () ((primary ()))
              (:arguments first &rest remaining &key (known :default known-p))
              `(list ,first ,remaining ,known ,known-p))
            (defgeneric more-arguments (a b &optional c &rest rest)
              (:method-combination short-arguments))
            (defmethod more-arguments (a b &optional c &rest rest))
            (assert (equal (more-arguments 1 2 3 :known 4) '(1 (:known 4) 4 t)))
            "#,
        )
        .unwrap();
}

#[test]
fn long_method_arguments_follow_changed_arguments_in_synthetic_methods() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defvar *method-default-count* 0)
            (define-symbol-macro arg :global)
            (define-method-combination changed-arguments () ((around (:around)) (primary ()))
              (:arguments arg &optional (opt (progn (incf *method-default-count*) arg) opt-p))
              `(call-method ,(first around)
                            ((make-method (list ,arg ,opt ,opt ,opt-p arg
                                                (let ((arg :inner)) (list arg ,arg))
                                                'arg (call-method ,(first primary)))))))
            (defgeneric change-arguments (x &optional y) (:method-combination changed-arguments))
            (defmethod change-arguments :around (x &optional y)
              (case y
                (:skip :skipped)
                (:repeat (list (call-next-method (+ x 10)) (call-next-method (+ x 20))))
                (:provided (call-next-method (+ x 10) nil))
                (otherwise (call-next-method (+ x 10)))))
            (defmethod change-arguments (x &optional (y :method-default)) (list x y))
            (let ((*method-default-count* 0))
              (assert (equal (change-arguments 1 :provided) '(11 nil nil t :global (:inner 11) arg (11 nil))))
              (assert (zerop *method-default-count*))
              (assert (equal (change-arguments 2) '(12 12 12 nil :global (:inner 12) arg (12 :method-default))))
              (assert (= 1 *method-default-count*))
              (assert (eq (change-arguments 3 :skip) :skipped))
              (assert (= 1 *method-default-count*))
              (assert (equal (change-arguments 3 :repeat)
                             '((13 13 13 nil :global (:inner 13) arg (13 :method-default))
                               (23 23 23 nil :global (:inner 23) arg (23 :method-default)))))
              (assert (= 3 *method-default-count*)))
            "#,
        )
        .unwrap();
}

#[test]
fn effective_method_macros_use_the_lexical_expansion_environment() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (defmacro indirect-call (method &environment env)
        (assert (macro-function 'call-method env))
        (list 'call-method method))
      (define-method-combination lexical-methods () ((primary () :required t))
        `(macrolet ((delegate () '(indirect-call ,(first primary))))
           (let ((call-method :variable))
             (values (delegate) call-method '(call-method quoted-data)))))
      (defgeneric indirect (x) (:method-combination lexical-methods))
      (defmethod indirect ((x integer)) (1+ x))
      (assert (equal (multiple-value-list (indirect 7))
                     '(8 :variable (call-method quoted-data))))
    "#,
    )
    .unwrap();
}

#[test]
fn method_combination_metaobjects_keep_the_argument_lambda_list_of_their_definition() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
      (define-method-combination schema () ((methods () :required t))
        (:arguments input &optional offset)
        (declare (ignore input offset)) (list 'call-method (first methods)))
      (defgeneric reader (input &optional offset) (:method-combination schema))
      (defmethod reader (input &optional offset) (+ input (or offset 0)))
      (defparameter *old-combination* (zk-mop:generic-function-method-combination #'reader))
      (assert (= 7 (reader 5 2)))
      (assert (eq 'schema (zk-mop:method-combination-name *old-combination*)))
      (let ((args (zk-mop:method-combination-arglist *old-combination*)))
        (setf (first args) :private-copy))
      (define-method-combination schema () ((methods () :required t))
        (list 'call-method (first methods)))
      (assert (equal (multiple-value-list (zk-mop:method-combination-arglist *old-combination*))
                     '((input &optional offset) t)))
      (let ((long (zk-mop:find-method-combination #'reader 'schema nil))
            (short (zk-mop:find-method-combination #'reader '+ nil)))
        (assert (equal '(nil t) (multiple-value-list (zk-mop:method-combination-arglist long))))
        (assert (eq '+ (zk-mop:method-combination-name short)))
        (assert (equal '(nil nil) (multiple-value-list (zk-mop:method-combination-arglist short)))))
    "#,
    )
    .unwrap();
}
