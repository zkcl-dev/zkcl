use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn defpackage_resolves_symbol_conflicts_in_option_processing_order() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defpackage #:source-a (:use :cl) (:intern #:hidden)
              (:export #:x #:clash #:shared))
            (defpackage #:source-b (:export #:clash #:shared))
            (defpackage "Target"
              (:export #:hidden #:x #:shared #:nil)
              (:import-from #:source-a #:hidden)
              (:import-from #:cl #:nil)
              (:use #:source-a #:source-b #:cl)
              (:shadow #:clash)
              (:shadowing-import-from #:source-b #:shared)
              (:intern #:private-name)
              (:nicknames "alias" #:target-short)
              (:documentation "a documented package")
              (:size 10))
            "#,
        )
        .unwrap();

    let values = runtime
        .eval(
            r#"
            (let ((package (find-package "alias")))
              (multiple-value-bind (symbol status) (find-symbol "PRIVATE-NAME" package)
                (values
                  (eq package (find-package "Target"))
                  (eq package (find-package :target-short))
                  (eq (symbol-package '|Target|::clash) package)
                  (eq '|Target|:hidden 'source-a::hidden)
                  (eq '|Target|:shared 'source-b:shared)
                  (eq '|Target|:x 'source-a:x)
                  (eq '|Target|:nil nil)
                  (eq (symbol-package symbol) package)
                  (eq status :internal)
                  (documentation package t))))
            "#,
        )
        .unwrap();
    assert_eq!(values.len(), 10);
    assert!(values[..9].iter().all(|value| !value.is_nil()));
    runtime.collect();
    assert_eq!(
        runtime.string(&values[9]).as_deref(),
        Some("a documented package")
    );
    assert_eq!(
        runtime
            .string(
                &runtime
                    .eval(r#"(documentation (find-package "Target") t)"#)
                    .unwrap()[0]
            )
            .as_deref(),
        Some("a documented package")
    );

    let values = runtime
        .eval(
            r#"(progn
                 (macroexpand-1 '(defpackage #:unexpanded-target))
                 (find-package "UNEXPANDED-TARGET"))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());

    let values = runtime
        .eval(
            r#"
            (handler-bind ((package-error (lambda (condition) (continue condition))))
              (defpackage #:recoverable (:import-from #:source-a #:absent) (:export #:visible))
              (values (find-package "RECOVERABLE")
                      (find-symbol "ABSENT" "SOURCE-A")
                      (find-symbol "VISIBLE" "RECOVERABLE")))
            "#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil());
    assert!(runtime.symbol_name(&values[2]).is_some());

    for options in [
        "(:intern \"A\") (:export \"A\")",
        "(:shadow \"A\") (:import-from :source-a \"A\")",
        "(:size 1) (:size 2)",
        "(:documentation \"a\") (:documentation \"b\")",
        "(:unknown)",
    ] {
        let values = runtime
            .eval(&format!(
                "(handler-case (macroexpand-1 '(defpackage :invalid {options}))
                   (program-error () t))"
            ))
            .unwrap();
        assert_eq!(values.len(), 1);
        assert!(!values[0].is_nil(), "{options}");
    }

    let result = runtime
        .eval(r#"(defpackage "Target" (:nicknames "SOURCE-A"))"#)
        .err()
        .expect("a nickname cannot designate a different package");
    assert_eq!(
        result.kind(),
        EvalErrorKind::LispCondition(ConditionClass::PackageError)
    );

    runtime
        .load_str(r#"(defpackage "Target" (:nicknames "Replacement"))"#)
        .unwrap();
    let renamed = runtime
        .eval(
            r#"(values (find-package "alias")
                         (find-package "target-short")
                         (find-package "Replacement"))"#,
        )
        .unwrap();
    assert!(renamed[0].is_nil());
    assert!(renamed[1].is_nil());
    assert!(renamed[2].same_identity(&runtime.eval(r#"(find-package "Target")"#).unwrap()[0]));

    runtime.load_str(r#"(defpackage "Target")"#).unwrap();
    let nicknames = runtime
        .eval(
            r#"(values (package-nicknames (find-package "Target"))
                         (find-package "Replacement"))"#,
        )
        .unwrap();
    assert!(nicknames[0].is_nil());
    assert!(nicknames[1].is_nil());
}

#[test]
fn defpackage_preserves_its_name_when_redefining_through_a_nickname() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"
            (defpackage "LIBRARY" (:nicknames "LIB" "OLD-LIB") (:export "ENTRY"))
            (defpackage "LIB" (:nicknames "NEW-LIB") (:export "ENTRY")
                             (:size 1180591620717411303424))
            "#,
        )
        .unwrap();
    let values = runtime
        .eval(
            r#"(let ((defined (find-package "LIBRARY")))
                 (values (eq defined (find-package "LIB"))
                         (eq defined (find-package "NEW-LIB"))
                         (not (find-package "OLD-LIB"))
                         (eq 'library:entry 'lib:entry)))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 4);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn defpackage_with_a_conflicting_nickname_cannot_modify_its_owner() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(r#"(defpackage "OTHER-LIBRARY" (:nicknames "OTHER-LIB"))"#)
        .unwrap();
    let values = runtime
        .eval(
            r#"(let ((owner (find-package "OTHER-LIBRARY")))
                 (values
                   (handler-case
                       (handler-bind ((package-error #'continue))
                         (defpackage "NEW-LIBRARY" (:nicknames "OTHER-LIB")
                                                   (:export "ACCIDENTAL"))
                         nil)
                     (package-error (condition)
                       (eq (package-error-package condition) owner)))
                   (not (find-package "NEW-LIBRARY"))
                   (not (find-symbol "ACCIDENTAL" owner))))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|value| !value.is_nil()));
}

#[test]
fn defpackage_rechecks_a_missing_import_after_its_handler_continues() {
    let runtime = Runtime::start().unwrap();
    runtime.eval(r#"(defpackage "LAZY-API")"#).unwrap();
    let values = runtime
        .eval(
            r#"(handler-bind ((package-error (lambda (condition)
                                (intern "NEW-OPERATOR" "LAZY-API")
                                (continue condition))))
                 (defpackage "API-CLIENT" (:import-from "LAZY-API" "NEW-OPERATOR"))
                 (let ((operator (find-symbol "NEW-OPERATOR" "LAZY-API")))
                   (and operator (eq operator (find-symbol "NEW-OPERATOR" "API-CLIENT")))))"#,
        )
        .unwrap();
    assert_eq!(values.len(), 1);
    assert!(!values[0].is_nil());
}

#[test]
fn local_nicknames_resolve_relative_to_the_current_package() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str(r#"
      (defpackage :primary (:export :entry) (:intern :secret))
      (defpackage :short (:export :entry))
      (defpackage :consumer (:use :cl)
        (:local-nicknames (:short :primary) ("MiX" :short)))
      (defpackage :other (:use :cl) (:local-nicknames (:short :short)))
      (assert (member :package-local-nicknames *features*))
      (let ((primary (find-package :primary))
            (global (find-package :short))
            (owner (find-package :consumer)))
        (let ((*package* owner))
          (assert (eq primary (find-package :short)))
          (assert (eq global (find-package global)))
          (assert (eq (find-symbol "ENTRY" primary) (read-from-string "short:entry")))
          (assert (eq (find-symbol "SECRET" primary) (read-from-string "short::secret")))
          (assert (string= "SHORT:ENTRY" (prin1-to-string (find-symbol "ENTRY" primary))))
          (assert (string= "|MiX|:ENTRY" (prin1-to-string (find-symbol "ENTRY" global))))
          (assert (eq (find-symbol "ENTRY" global) (read-from-string "|MiX|:ENTRY")))
          (assert (eq owner (zk:add-package-local-nickname "TWO" :short)))
          (assert (eq global (find-package :two)))
          (assert (zk:remove-package-local-nickname :two))
          (assert (not (zk:remove-package-local-nickname :two)))
          (assert (handler-case (progn (zk:add-package-local-nickname :short global) nil)
                    (package-error () t)))
          (handler-bind ((package-error (lambda (e) (invoke-restart 'zk::keep-old))))
            (zk:add-package-local-nickname :short global))
          (assert (eq primary (find-package :short)))
          (handler-bind ((package-error (lambda (e) (invoke-restart 'zk::change-nick))))
            (zk:add-package-local-nickname :short global))
          (assert (eq global (find-package :short)))
          (zk:remove-package-local-nickname :short)
          (zk:add-package-local-nickname :short primary)
          (assert (handler-case (progn (zk:add-package-local-nickname "CL" global) nil)
                    (package-error () t)))
          (handler-bind ((package-error #'continue)) (zk:add-package-local-nickname "CL" global))
          (assert (eq global (find-package (copy-seq "CL"))))
          (zk:remove-package-local-nickname "CL"))
        (let ((*package* (find-package :other)))
          (assert (eq global (find-package :short))))
        (assert (equal (list owner) (zk:package-locally-nicknamed-by-list primary)))
        (rename-package primary :new-primary)
        (assert (eq primary (cdr (assoc "SHORT" (zk:package-local-nicknames owner) :test #'string=))))
        (delete-package primary)
        (make-package :new-primary)
        (let ((*package* owner)) (assert (eq global (find-package :short))))
        (defpackage :consumer (:use :cl))
        (assert (null (zk:package-local-nicknames owner))))
    "#).unwrap();
}
