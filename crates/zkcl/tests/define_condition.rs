use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn compound_condition_uses_a_viable_union_arm_when_precedence_orders_conflict() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(define-condition p () ()) (define-condition q () ())
                 (define-condition p-first (p q) ()) (define-condition q-first (q p) ())
                 (define-condition separate () ())",
    )
    .unwrap();
    let result = rt
        .eval(
            "(let* ((spec '(and (or p-first separate) q-first))
                                 (c (make-condition spec)))
                            (and (typep c spec) (typep c 'separate)
                                 (eq (class-of c) (class-of (make-condition spec)))))",
        )
        .unwrap();
    assert!(!result[0].is_nil());
}

#[test]
fn condition_intersections_without_a_named_class_obey_the_complete_type() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            r#"(let* ((spec '(and simple-error type-error))
        (c (make-condition spec :datum 17 :expected-type 'string :format-control "bad ~A"
                           :format-arguments '(17)))
        (again (make-condition '(and type-error simple-error))))
      (values (typep c spec) (typep c (type-of c))
              (eq (class-of c) (class-of again)) (null (class-name (class-of c)))
              (eql (type-error-datum c) 17) (eq (type-error-expected-type c) 'string)
              (equal (princ-to-string c) "bad 17")))"#,
        )
        .unwrap();
    assert!(values.iter().all(|v| !v.is_nil()));
}

#[test]
fn derived_condition_types_construct_the_signaled_class_in_the_callers_context() {
    let rt = Runtime::start().unwrap();
    rt.register_function("collect-during-condition-type", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    })
    .unwrap();
    rt.load_str(
        r#"
      (define-condition left-problem (warning) ((code :initarg :code :reader problem-code)))
      (define-condition right-problem (condition) ())
      (define-condition combined-problem (left-problem right-problem) ())
      (deftype problem-type (&optional (side :both))
        (collect-during-condition-type)
        (if (eq side :left) 'left-problem '(and left-problem right-problem)))
      (deftype escaped-condition () (throw :expansion 37))
    "#,
    )
    .unwrap();
    let results = rt.eval(r#"(let ((other (make-condition 'condition)))
      (values
        (eq (class-of (make-condition '(problem-type :left))) (find-class 'left-problem))
        (typep (make-condition '(and (or simple-error (problem-type :left)) right-problem)) 'combined-problem)
        (= 37 (catch :expansion (make-condition 'escaped-condition)))
        (handler-bind ((combined-problem
                        (lambda (c)
                          (assert (null (find-restart 'recover other)))
                          (invoke-restart (find-restart 'recover c) (problem-code c)))))
          (restart-case (warn 'problem-type :code 19) (recover (x) (= x 19))))
        (handler-case (error 'problem-type :code 31) (combined-problem (c) (= (problem-code c) 31)))))"#).unwrap();
    assert!(results.iter().all(|v| !v.is_nil()));
}

#[test]
fn host_errors_classify_user_conditions_by_inheritance_after_a_rename() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(define-condition renamed-warning (warning) () (:report "renamed"))
                        (setf (class-name (find-class 'renamed-warning)) 'type-error)"#,
        )
        .unwrap();
    let error = runtime
        .eval("(error (make-condition 'renamed-warning))")
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::Warning)
    );
    assert_eq!(error.message(), "renamed");
}

#[test]
fn handler_case_matches_each_member_of_a_derived_condition_type() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"
        (progn
          (define-condition handler-problem (error) ())
          (deftype handled-problem () '(or handler-problem simple-warning))
          (values
            (handler-case (signal (make-condition 'handler-problem))
              (handled-problem () t))
            (handler-case (signal (make-condition 'simple-warning))
              (handled-problem () t))))
    "#,
        )
        .unwrap();
    assert_eq!(values.len(), 2);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn a_condition_intersection_designates_simple_type_error() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval("(typep (make-condition '(and simple-condition type-error)) 'simple-type-error)")
        .unwrap();
    assert_eq!(values.len(), 1);
    assert!(!values[0].is_nil());
}

#[test]
fn make_condition_honors_the_leftmost_allow_other_keys_value() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(values
            (= (type-error-datum (make-condition 'type-error :datum 9 :ignored 1
                                 :allow-other-keys t :allow-other-keys nil)) 9)
            (handler-case (progn (make-condition 'type-error :ignored 1
                                 :allow-other-keys nil :allow-other-keys t) nil)
              (error () t)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 2);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn condition_class_object_keeps_its_identity_as_a_make_condition_designator() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(define-condition original-condition ()
               ((value :initarg :value :reader renamed-designator-value)))",
        )
        .unwrap();

    let values = runtime
        .eval(
            "(let ((class (find-class 'original-condition)))
               (setf (class-name class) 'renamed-condition)
               (let ((condition (make-condition class :value 19)))
                 (values
                   (eq (class-of condition) class)
                   (typep condition class)
                   (renamed-designator-value condition))))",
        )
        .unwrap();
    assert!(values[..2].iter().all(|value| !value.is_nil()));
    assert_eq!(values[2].as_fixnum(), Some(19));
}

#[test]
fn condition_type_names_use_find_class_associations() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(define-condition aliased-condition ()
               ((value :initarg :value :reader aliased-condition-value)))
             (setf (find-class 'simple-error-alias) (find-class 'simple-error))
             (setf (find-class 'condition-alias) (find-class 'aliased-condition))",
        )
        .unwrap();

    let values = runtime
        .eval(
            "(let ((builtin (make-condition 'simple-error-alias))
                   (user (make-condition 'condition-alias :value 23)))
               (values
                 (eq (class-of builtin) (find-class 'simple-error))
                 (eq (class-of user) (find-class 'aliased-condition))
                 (aliased-condition-value user)))",
        )
        .unwrap();
    assert!(values[..2].iter().all(|value| !value.is_nil()));
    assert_eq!(values[2].as_fixnum(), Some(23));
}

#[test]
fn condition_handler_specifiers_compare_class_identity() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(progn
               (define-condition identity-specifier-old () ())
               (define-condition identity-specifier-current () ())
               (let ((old (find-class 'identity-specifier-old))
                     (current (find-class 'identity-specifier-current)))
                 (setf (class-name old) 'shared-condition-name)
                 (setf (class-name current) 'shared-condition-name)
                 (setf (find-class 'old-condition-alias) old)
                 (setf (find-class 'current-condition-alias) current)
                 (let ((condition (make-condition current))
                       (old-called nil)
                       (current-called nil))
                   (handler-bind
                       ((old-condition-alias
                          (lambda (caught)
                            (declare (ignore caught))
                            (setq old-called t))))
                     (signal condition))
                   (handler-bind
                       ((current-condition-alias
                          (lambda (caught)
                            (declare (ignore caught))
                            (setq current-called t))))
                     (signal condition))
                   (values old-called current-called))))",
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert!(!values[1].is_nil());
}

#[test]
fn signaled_user_conditions_expose_inherited_slots_to_the_active_handler() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defparameter *zkcl-condition-reports* 0)
               (define-condition parent-problem (error)
                 ((value :initarg :value :reader problem-value :initform 17)
                  (note :initarg :note :reader problem-note))
                 (:report "parent report"))
               (define-condition auxiliary-problem ()
                 ((extra :initarg :extra :reader problem-extra :initform 3)))
               (let ((captured 42))
                 (define-condition child-problem (parent-problem auxiliary-problem)
                   ((value :initform captured)
                    (local :reader problem-local :initform (+ captured 1)))
                   (:default-initargs :note "default note")))
               (define-condition rendered-problem (parent-problem)
                 ()
                 (:report (lambda (condition output)
                            (incf *zkcl-condition-reports*)
                            (format output "problem ~D" (problem-value condition)))))
               (define-condition typed-problem (type-error)
                 ((detail :reader problem-detail :initform 7)))
               (define-condition inherited-simple (simple-error) ())
               (define-condition jumped-problem ()
                 ((value :initform (throw 'initialized 80))))"#,
        )
        .unwrap();

    let values = runtime
        .eval(
            "(let ((c (make-condition (find-class 'child-problem))))
               (values (problem-value c) (problem-local c) (problem-extra c)
                       (problem-note c)
                       (eq (class-of c) (find-class 'child-problem))
                       (typep c 'parent-problem) (typep c 'auxiliary-problem)
                       (typep c 'condition) (typep c 'error)
                       (typep c (find-class 'auxiliary-problem))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(values[1].as_fixnum(), Some(43));
    assert_eq!(values[2].as_fixnum(), Some(3));
    assert_eq!(runtime.string(&values[3]).as_deref(), Some("default note"));
    assert!(values[4..].iter().all(|value| !value.is_nil()));

    let caught = runtime
        .eval(
            "(handler-case
                 (handler-bind ((auxiliary-problem
                                 (lambda (c) (throw 'custom-signal (problem-value c)))))
                   (catch 'custom-signal (signal 'child-problem :value 55)))
               (error () 0))",
        )
        .unwrap();
    assert_eq!(caught[0].as_fixnum(), Some(55));
    let values = runtime
        .eval(
            "(handler-case
                 (error 'typed-problem :datum 4 :expected-type 'character)
               (type-error (c) (values (type-error-datum c)
                                       (type-error-expected-type c)
                                       (problem-detail c))))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(4));
    assert_eq!(
        runtime.symbol_name(&values[1]).as_deref(),
        Some("CHARACTER")
    );
    assert_eq!(values[2].as_fixnum(), Some(7));
    let inherited_report = runtime
        .eval(
            "(princ-to-string
               (make-condition 'inherited-simple
                 :format-control \"value ~D\" :format-arguments '(41)))",
        )
        .unwrap();
    assert_eq!(
        runtime.string(&inherited_report[0]).as_deref(),
        Some("value 41")
    );
    let restarted = runtime
        .eval(
            "(handler-bind
                 ((child-problem
                   (lambda (c)
                     (invoke-restart (find-restart 'replace c) (problem-value c)))))
               (restart-case (error 'child-problem :value 19)
                 (replace (value) value)))",
        )
        .unwrap();
    assert_eq!(restarted[0].as_fixnum(), Some(19));
    let jumped = runtime
        .eval("(catch 'initialized (make-condition 'jumped-problem))")
        .unwrap();
    assert_eq!(jumped[0].as_fixnum(), Some(80));

    let values = runtime
        .eval(
            "(progn
               (handler-case (error 'rendered-problem :value 2)
                 (rendered-problem () nil))
               (values *zkcl-condition-reports*
                       (princ-to-string (make-condition 'child-problem))
                       (format nil \"~A\" (make-condition 'rendered-problem :value 26))
                       *zkcl-condition-reports*))",
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(0));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("parent report"));
    assert_eq!(runtime.string(&values[2]).as_deref(), Some("problem 26"));
    assert_eq!(values[3].as_fixnum(), Some(1));
    let escaped = runtime
        .eval("(error 'rendered-problem :value 51)")
        .err()
        .unwrap();
    assert_eq!(
        escaped.kind(),
        EvalErrorKind::LispCondition(ConditionClass::Error)
    );
    assert_eq!(escaped.message(), "problem 51");

    let kept = runtime
        .eval("(make-condition 'child-problem :note (list 21 22))")
        .unwrap();
    let reader = runtime.eval("#'problem-note").unwrap();
    runtime.collect();
    let note = runtime.call(&reader[0], &[&kept[0]]).unwrap();
    let (head, _) = runtime.cons_fields(&note[0]).unwrap();
    assert_eq!(head.as_fixnum(), Some(21));
}

#[test]
fn standard_initializers_populate_the_reflected_slots_of_builtin_conditions() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str(r#"
      (defvar *constructor-arguments* nil)
      (defmethod initialize-instance :after ((object type-error) &key marker)
        (when marker (push marker *constructor-arguments*)))
      (defvar *new-arguments* nil)
      (defmethod reinitialize-instance :after ((object type-error) &key marker)
        (when marker (push marker *new-arguments*)))
      (defparameter *test-error*
        (make-instance 'simple-type-error :datum :old :expected-type 'integer
                       :format-control "~A" :format-arguments '(42) :marker :made))
      (assert (typep (make-instance 'error) 'error))
      (assert (equal *constructor-arguments* '(:made)))
      (assert (equal (simple-condition-format-arguments *test-error*) '(42)))
      (assert (eq (type-error-expected-type *test-error*) 'integer))
      (assert (eq :old (type-error-datum *test-error*)))
      (let* ((class (find-class 'simple-type-error))
             (slots (zk-mop:class-slots class))
             (datum (find-if (lambda (slot) (member :datum (zk-mop:slot-definition-initargs slot))) slots))
             (name (zk-mop:slot-definition-name datum))
             (raw (allocate-instance class)))
        (assert (not (slot-boundp raw name)))
        (assert (eq raw (initialize-instance raw :datum :first :marker :explicit)))
        (assert (eq :first (zk-mop:slot-value-using-class class raw datum)))
        (assert (eq raw (reinitialize-instance raw :datum :second :marker :again)))
        (assert (eq :second (slot-value raw name)))
        (assert (equal *constructor-arguments* '(:explicit :made)))
        (assert (equal *new-arguments* '(:again)))
        (assert (handler-case (progn (setf (zk-mop:slot-definition-allocation datum) :class) nil)
                  (error () t)))
        (assert (handler-case (progn (make-instance class :unexpected t) nil) (program-error () t)))
        (assert (typep (make-instance class :unexpected nil :allow-other-keys t) 'type-error)))
    "#).unwrap();
    runtime.collect();
    runtime
        .eval("(assert (eq (type-error-datum *test-error*) :old))")
        .unwrap();
}
