use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn moving_a_method_changes_which_generic_function_dispatches_to_it() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "
            (defgeneric methods-test-order (a b &optional c))
            (defparameter *method-default*
              (defmethod methods-test-order (a b &optional c) :default))
            (defparameter *method-number*
              (defmethod methods-test-order ((a number) b &optional c) :number))
            (defparameter *method-integer-before*
              (defmethod methods-test-order :before ((a integer) b &optional c) nil))
            (defparameter *method-literal*
              (defmethod methods-test-order ((a (eql 9)) b &optional c) :literal))
            (defgeneric methods-test-destination (a b &optional c))
            (defparameter *method-replaced*
              (defmethod methods-test-destination ((a (eql 9)) b &optional c) :replaced))
            (defgeneric methods-test-wrong-arity (x))",
        )
        .unwrap();
    runtime.collect();

    for source in [
        "(eq (find-method #'methods-test-order '(:before)
                          (list (find-class 'integer) (find-class t)))
             *method-integer-before*)",
        "(eq (find-method #'methods-test-order nil (list '(eql 9) (find-class t)))
             *method-literal*)",
        "(let ((qualifiers (method-qualifiers *method-integer-before*)))
           (setf (car qualifiers) :modified)
           (equal (method-qualifiers *method-integer-before*) '(:before)))",
        "(equal (compute-applicable-methods #'methods-test-order '(9 nil))
                 (list *method-literal* *method-integer-before* *method-number* *method-default*))",
        "(equal (compute-applicable-methods #'methods-test-order '(7 nil :optional))
                 (list *method-integer-before* *method-number* *method-default*))",
        "(null (compute-applicable-methods #'methods-test-wrong-arity '(7)))",
        "(null (find-method #'methods-test-order '(:after)
                           (list (find-class 'integer) (find-class t))
                           nil))",
        "(handler-case (progn (add-method #'methods-test-destination *method-literal*) nil)
           (program-error () t))",
        "(and (eq (remove-method #'methods-test-destination *method-literal*) #'methods-test-destination)
             (eq (car (compute-applicable-methods #'methods-test-order '(9 nil)))
                 *method-literal*))",
        "(and (eq (remove-method #'methods-test-order *method-literal*) #'methods-test-order)
             (eq (car (compute-applicable-methods #'methods-test-order '(9 nil)))
                 *method-integer-before*))",
        "(handler-case (progn (add-method #'methods-test-wrong-arity *method-literal*) nil)
           (error () t))",
        "(and (eq (add-method #'methods-test-destination *method-literal*) #'methods-test-destination)
             (eq (methods-test-destination 9 nil) :literal)
             (equal (compute-applicable-methods #'methods-test-destination '(9 nil))
                    (list *method-literal*)))",
        "(and (eq (add-method #'methods-test-order *method-replaced*) #'methods-test-order)
             (eq (methods-test-order 9 nil) :replaced))",
    ] {
        let value = runtime.eval(source).unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!value[0].is_nil(), "{source}");
    }

    for source in [
        "(find-method #'methods-test-order nil nil nil)",
        "(find-method #'methods-test-order '(:after) (list (find-class 'integer) (find-class t)))",
    ] {
        let error = runtime
            .eval(source)
            .err()
            .expect("expected a method query error");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ProgramError),
            "{source}"
        );
    }

    let error = runtime
        .eval("(compute-applicable-methods #'methods-test-order '(9 . nil-argument))")
        .err()
        .expect("expected a proper argument list");
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
}

#[test]
fn warmed_dispatch_uses_the_current_most_specific_method() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-during-dispatch", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            "
            (defclass cached-left () ())
            (defclass cached-right () ())
            (defclass cached-parent (cached-left) ())
            (defclass cached-item (cached-parent)
              ((payload :initform 4 :reader cached-payload)))
            (defparameter *cached-item* (make-instance 'cached-item))
            (defgeneric cached-dispatch (object))
            (defmethod cached-dispatch ((object cached-left))
              (values :left (cached-payload object)))
            (defmethod cached-dispatch ((object cached-right))
              (values :right (cached-payload object) (slot-value object 'added)))
            (defmethod cached-dispatch (object) :default)",
        )
        .unwrap();

    let check = |source| {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    };
    for source in [
        "(progn (dotimes (i 3) (cached-dispatch *cached-item*))
                (equal (multiple-value-list (cached-dispatch *cached-item*)) '(:left 4)))",
        "(progn (defclass cached-parent (cached-right) ((added :initform 17)))
                (equal (multiple-value-list (cached-dispatch *cached-item*)) '(:right 4 17)))",
        "(progn (defclass cached-parent (cached-left) ())
                (equal (multiple-value-list (cached-dispatch *cached-item*)) '(:left 4)))",
        "(progn (defparameter *cached-remove* nil)
                (defparameter *cached-method*
                  (defmethod cached-dispatch ((object cached-left))
                    (if *cached-remove*
                        (let ((*cached-remove* nil))
                          (remove-method #'cached-dispatch *cached-method*)
                          (collect-during-dispatch)
                          (values (cached-dispatch object) (call-next-method)))
                        :replacement)))
                (eq (cached-dispatch *cached-item*) :replacement))",
        "(let ((*cached-remove* t))
           (equal (multiple-value-list (cached-dispatch *cached-item*)) '(:default :default)))",
        "(progn (add-method #'cached-dispatch *cached-method*)
                (eq (cached-dispatch *cached-item*) :replacement))",
        "(progn (defparameter *cached-eql-method*
                  (defmethod cached-dispatch ((object (eql *cached-item*))) :exact))
                (and (eq (cached-dispatch (make-instance 'cached-item)) :replacement)
                     (eq (cached-dispatch *cached-item*) :exact)))",
    ] {
        check(source);
    }
    runtime.collect();
    check(
        "(progn (remove-method #'cached-dispatch *cached-eql-method*)
                (eq (cached-dispatch *cached-item*) :replacement))",
    );
}

#[test]
fn dispatch_for_multiple_classes_tracks_redefinition_and_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defclass dispatch-left () ())
            (defclass dispatch-right () ())
            (defclass dispatch-derived-left (dispatch-left) ())
            (defclass dispatch-derived-right (dispatch-right) ())
            (defparameter *dispatch-objects*
              (mapcar #'make-instance
                      '(dispatch-left dispatch-right dispatch-derived-left dispatch-derived-right)))
            (defgeneric dispatch-value (object))
            (defmethod dispatch-value ((object dispatch-left)) (values :left 1))
            (defmethod dispatch-value ((object dispatch-right)) (values :right 2))
            (defun dispatch-values ()
              (mapcar (lambda (object) (multiple-value-list (dispatch-value object)))
                      *dispatch-objects*))
            "#,
        )
        .unwrap();

    let check = |expected| {
        let source = format!("(equal (dispatch-values) '{expected})");
        assert!(!runtime.eval(&source).unwrap()[0].is_nil(), "{source}");
    };
    for _ in 0..3 {
        check("((:left 1) (:right 2) (:left 1) (:right 2))");
    }
    runtime
        .load_str("(defmethod dispatch-value ((object dispatch-right)) (values :new 3))")
        .unwrap();
    for _ in 0..3 {
        check("((:left 1) (:new 3) (:left 1) (:new 3))");
    }
    runtime
        .load_str("(defclass dispatch-derived-left (dispatch-right) ())")
        .unwrap();
    check("((:left 1) (:new 3) (:new 3) (:new 3))");
    runtime.collect();
    check("((:left 1) (:new 3) (:new 3) (:new 3))");
}

#[test]
fn saved_call_next_method_uses_the_original_chain_after_redefinition() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defgeneric saved-next (x y) (:argument-precedence-order y x))
         (defmethod saved-next ((x number) y) (list :base x y))
         (defmethod saved-next ((x integer) y)
           (values #'call-next-method (next-method-p)))
         (saved-next 3 :warm)
         (multiple-value-bind (next has-next) (saved-next 5 :original)
           (assert has-next)
           (defparameter *saved-next* next))
         (assert (equal (funcall *saved-next* 9 :changed) '(:base 9 :changed)))
         (assert (handler-case (progn (funcall *saved-next* 1.5 :changed) nil)
                   (program-error () t)))
         (defmethod saved-next ((x number) y) (list :replacement x y))
         (defmethod saved-next (x (y (eql :priority))) :priority)",
        )
        .unwrap();
    runtime.collect();
    runtime
        .load_str(
            "(assert (equal (funcall *saved-next*) '(:base 5 :original)))
         (assert (eq (saved-next 1 :priority) :priority))
         (assert (equal (funcall (saved-next 7 :fresh)) '(:replacement 7 :fresh)))",
        )
        .unwrap();
}

#[test]
fn setf_dispatch_selects_the_most_specific_writer_for_both_arguments() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defclass writer-left () ())
         (defclass writer-right () ())
         (defclass writer-child (writer-left) ())
         (defparameter *writer* (make-instance 'writer-child))
         (defgeneric (setf writer-value) (value object))
         (defmethod (setf writer-value) (value (object writer-left)) :left)
         (defmethod (setf writer-value) (value (object writer-right)) :right)
         (dotimes (i 3) (assert (eq (setf (writer-value *writer*) i) :left)))
         (defclass writer-child (writer-right) ())
         (assert (eq (setf (writer-value *writer*) :value) :right))
         (defmethod (setf writer-value) ((value (eql :exact)) (object writer-child)) :exact)
         (dotimes (i 3)
           (assert (eq (setf (writer-value *writer*) :value) :right))
           (assert (eq (setf (writer-value *writer*) :exact) :exact)))",
        )
        .unwrap();
}

#[test]
fn ensure_generic_function_rejects_non_generic_parameter_syntax() {
    let runtime = Runtime::start().unwrap();
    for source in [
        "(ensure-generic-function 'invalid-ensure
                                   :lambda-list '(object &optional (value 1)))",
        "(ensure-generic-function 'invalid-ensure
                                   :lambda-list '(object &optional (value nil supplied-p)))",
        "(ensure-generic-function 'invalid-ensure
                                   :lambda-list '(object &aux value))",
    ] {
        assert!(
            runtime.eval(source).is_err(),
            "generic lambda list was accepted: {source}"
        );
    }
    assert!(
        runtime
            .eval(
                "(ensure-generic-function 'valid-ensure
                                           :lambda-list
                                           '(object &optional value &key option))"
            )
            .is_ok()
    );
}

#[test]
fn dispatch_uses_the_installed_generic_function_parameters() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *signature* (list 'a 'b '&optional 'extra '&key
                                             (list (list :tag 'option))))
             (defparameter *order* (list 'a 'b))
             (ensure-generic-function 'ordered-call :lambda-list *signature*
                                                   :argument-precedence-order *order*)
             (defmethod ordered-call ((a integer) (b number) &optional extra &key tag)
               (list :first extra tag))
             (defmethod ordered-call ((a number) (b integer) &optional extra &key tag)
               (list :second extra tag))
             (setf (first *order*) 'b
                   (caaar (last *signature*)) :other)
             (let ((list (zk-mop:generic-function-lambda-list #'ordered-call)))
               (setf (caaar (last list)) :different))",
        )
        .unwrap();
    runtime.collect();
    for source in [
        "(equal (ordered-call 1 2) '(:first nil nil))",
        "(equal (ordered-call 1 2 3 :tag 4) '(:first 3 4))",
        "(progn (ensure-generic-function 'ordered-call :argument-precedence-order '(b a))
                (equal (ordered-call 1 2 3 :tag 4) '(:second 3 4)))",
        "(handler-case (ensure-generic-function 'ordered-call :lambda-list '(a b))
           (program-error () (equal (ordered-call 1 2 3 :tag 4) '(:second 3 4))))",
    ] {
        let values = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert!(!values[0].is_nil(), "{source}");
    }
}

#[test]
fn warmed_dispatch_observes_reentrant_class_precedence_changes() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defclass reentrant-left () ())
            (defclass reentrant-right () ())
            (defclass reentrant-parent (reentrant-left) ())
            (defclass reentrant-item (reentrant-parent) ())
            (defparameter *reentrant-item* (make-instance 'reentrant-item))
            (defparameter *reentrant-trigger* nil)

            (defgeneric reentrant-dispatch (object))
            (defmethod reentrant-dispatch ((object reentrant-left))
              (if *reentrant-trigger*
                  (progn
                    (setq *reentrant-trigger* nil)
                    (defclass reentrant-parent (reentrant-right) ())
                    (values :old (reentrant-dispatch object)))
                  :left))
            (defmethod reentrant-dispatch ((object reentrant-right)) :right)
            (defmethod reentrant-dispatch (object) :default)
            "#,
        )
        .unwrap();

    for _ in 0..3 {
        let result = runtime
            .eval("(eq (reentrant-dispatch *reentrant-item*) :left)")
            .unwrap();
        assert!(!result[0].is_nil());
    }
    let result = runtime
        .eval(
            "(progn
               (setq *reentrant-trigger* t)
               (equal (multiple-value-list (reentrant-dispatch *reentrant-item*))
                      '(:old :right)))",
        )
        .unwrap();
    assert!(!result[0].is_nil());
}

#[test]
fn warmed_dispatch_is_invalidated_when_method_combination_changes() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defgeneric cached-combination (object))
            (defmethod cached-combination (object) :standard)
            "#,
        )
        .unwrap();
    for _ in 0..3 {
        let result = runtime
            .eval("(eq (cached-combination 1) :standard)")
            .unwrap();
        assert!(!result[0].is_nil());
    }

    assert!(
        !runtime
            .eval(
                "(eq
                   (handler-case
                       (progn
                         (defgeneric cached-combination (object)
                           (:method-combination +))
                         (cached-combination 1))
                     (error () :combination-error))
                   :combination-error)"
            )
            .unwrap()[0]
            .is_nil()
    );
    assert!(
        !runtime
            .eval(
                "(progn
                   (defgeneric cached-combination (object))
                   (eq (cached-combination 1) :standard))"
            )
            .unwrap()[0]
            .is_nil()
    );
}
