use zkcl::Runtime;

#[test]
fn make_load_form_reconstructs_saved_object_references() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
        (defvar *init-count* 0)
        (defclass persistent-base () ((inherited :initform (incf *init-count*))))
        (defclass persistent (persistent-base)
          ((shared :allocation :class :initform :first) self alias missing))
        (defstruct persistent-struct (value (incf *init-count*) :read-only t) self)
        (defmethod make-load-form ((obj persistent) &optional environment)
          (make-load-form-saving-slots obj :environment environment))
        (defmethod make-load-form ((obj persistent-struct) &optional environment)
          (make-load-form-saving-slots obj :environment environment))
        (defun reconstruct (original create init)
          (let ((new (eval create)))
            (eval (subst new original init))
            new))
        "#,
        )
        .unwrap();

    for form in [
        "(every (lambda (class)
                  (find-method #'make-load-form nil (list (find-class class)) nil))
                '(standard-object structure-object condition class))",
        "(let* ((obj (make-instance 'persistent))
                (shared (list :payload)))
           (setf (slot-value obj 'inherited) shared (slot-value obj 'alias) shared
                 (slot-value obj 'self) obj)
           (multiple-value-bind (create init) (make-load-form obj)
             (setf (slot-value obj 'shared) :second)
             (let ((new (reconstruct obj create init)))
               (and (= *init-count* 1)
                    (eq new (slot-value new 'self))
                    (eq (slot-value new 'inherited) (slot-value new 'alias))
                    (not (slot-boundp new 'missing))
                    (eq :second (slot-value new 'shared))))))",
        "(let ((obj (allocate-instance (find-class 'persistent))))
           (slot-makunbound obj 'shared)
           (multiple-value-bind (create init)
               (make-load-form-saving-slots obj :slot-names '(shared))
             (setf (slot-value obj 'shared) :other)
             (let ((new (reconstruct obj create init)))
               (and (not (slot-boundp new 'shared))
                    (not (slot-boundp new 'inherited))))))",
        "(let ((obj (make-persistent-struct :value 42)))
           (setf (persistent-struct-self obj) obj)
           (multiple-value-bind (create init) (make-load-form obj)
             (let ((new (reconstruct obj create init)))
               (and (= 42 (persistent-struct-value new))
                    (eq new (persistent-struct-self new))
                    (= *init-count* 1)))))",
        "(let ((obj (make-instance 'persistent)))
           (multiple-value-bind (create init)
               (make-load-form-saving-slots obj :slot-names nil)
             (let ((new (reconstruct obj create init)))
               (not (slot-boundp new 'inherited)))))",
        "(every (lambda (name)
                  (let ((class (find-class name)))
                    (eq class (eval (make-load-form class)))))
                '(persistent-struct persistent number))",
        "(let ((obj (make-instance 'persistent)))
           (defclass persistent (persistent-base)
             ((shared :allocation :class) self alias missing (later :initform :new)))
           (multiple-value-bind (create init) (make-load-form obj)
             (eq :new (slot-value (reconstruct obj create init) 'later))))",
        "(let ((class (find-class 'persistent)))
           (setf (class-name class) 'different-name)
           (handler-case (progn (make-load-form class) nil) (error () t)))",
    ] {
        let result = runtime
            .eval(form)
            .unwrap_or_else(|err| panic!("{form}: {err}"));
        assert!(!result[0].is_nil(), "{form}");
    }
}
